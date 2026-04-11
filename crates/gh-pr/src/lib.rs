use std::collections::HashMap;
use std::hash::{DefaultHasher, Hasher};
use std::process::Stdio;

use led_core::IssueCategory;
use led_core::git::LineStatus;
use led_core::rx::Stream;
use led_core::{CanonPath, PersistedContentHash, PrNumber, Row, UserPath};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum GhPrOut {
    LoadPr {
        branch: String,
        root: CanonPath,
    },
    /// Conditional poll: check if PR changed since last ETag.
    PollPr {
        api_endpoint: String,
        etag: Option<String>,
        root: CanonPath,
    },
}

/// A review comment from a PR thread, as returned by the driver.
#[derive(Clone, Debug)]
pub struct ReviewComment {
    pub line: Row,
    pub body: String,
    pub author: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub enum GhPrIn {
    PrLoaded {
        number: PrNumber,
        state: String,
        url: String,
        api_endpoint: String,
        etag: Option<String>,
        diff_lines: HashMap<CanonPath, Vec<LineStatus>>,
        comments: HashMap<CanonPath, Vec<ReviewComment>>,
        file_hashes: HashMap<CanonPath, PersistedContentHash>,
    },
    /// 304 Not Modified — PR hasn't changed.
    PrUnchanged,
    NoPr,
    GhUnavailable,
}

pub fn driver(out: Stream<GhPrOut>, gh_binary: Option<String>) -> Stream<GhPrIn> {
    let stream: Stream<GhPrIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<GhPrOut>(16);
    let (result_tx, mut result_rx) = mpsc::channel::<GhPrIn>(16);

    out.on(move |opt: Option<&GhPrOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    let gh_bin = gh_binary.unwrap_or_else(|| "gh".to_string());
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            let bin = gh_bin.clone();
            let result = match cmd {
                GhPrOut::LoadPr { branch, root } => {
                    tokio::task::spawn_blocking(move || load_pr(&branch, &root, &bin)).await
                }
                GhPrOut::PollPr {
                    api_endpoint,
                    etag,
                    root,
                } => {
                    tokio::task::spawn_blocking(move || {
                        poll_pr(&bin, &api_endpoint, etag.as_deref(), &root)
                    })
                    .await
                }
            };
            match result {
                Ok(ev) => {
                    result_tx.send(ev).await.ok();
                }
                Err(_) => {}
            }
        }
    });

    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}

fn load_pr(branch: &str, root: &CanonPath, gh_bin: &str) -> GhPrIn {
    let meta = match run_gh(
        gh_bin,
        &[
            "pr",
            "view",
            "--json",
            "number,state,url,reviews,headRefOid",
        ],
        root,
    ) {
        GhResult::Ok(output) => output,
        GhResult::NotInstalled => return GhPrIn::GhUnavailable,
        GhResult::Failed => return GhPrIn::NoPr,
    };

    let parsed: serde_json::Value = match serde_json::from_str(&meta) {
        Ok(v) => v,
        Err(_) => return GhPrIn::NoPr,
    };

    let number = parsed["number"].as_u64().unwrap_or(0) as u32;
    let state = parsed["state"].as_str().unwrap_or("OPEN").to_string();
    let url = parsed["url"].as_str().unwrap_or("").to_string();

    let api_endpoint = parse_github_url(&url)
        .map(|(owner, repo)| format!("repos/{owner}/{repo}/pulls/{number}"))
        .unwrap_or_default();

    // Fetch the initial ETag via a HEAD-like request
    let etag = fetch_etag(gh_bin, &api_endpoint);

    let comments = load_review_threads(gh_bin, number, &url, root);

    let diff_lines = match run_gh(gh_bin, &["pr", "diff"], root) {
        GhResult::Ok(output) => parse_unified_diff(&output, root),
        _ => HashMap::new(),
    };

    let head_oid = parsed["headRefOid"].as_str().unwrap_or("");
    let all_paths: Vec<&CanonPath> = diff_lines.keys().chain(comments.keys()).collect();
    let file_hashes = compute_file_hashes(root, head_oid, &all_paths);

    log::debug!(
        "[gh-pr] loaded PR #{number} for branch {branch}: {state}, {} files, {} files with comments",
        diff_lines.len(),
        comments.len(),
    );

    GhPrIn::PrLoaded {
        number: PrNumber(number),
        state,
        url,
        api_endpoint,
        etag,
        diff_lines,
        comments,
        file_hashes,
    }
}

/// Conditional poll: GET the PR endpoint with `If-None-Match`.
/// Returns `PrUnchanged` on 304, full `PrLoaded` on 200.
fn poll_pr(gh_bin: &str, api_endpoint: &str, etag: Option<&str>, root: &CanonPath) -> GhPrIn {
    use std::process::Command;

    let mut args = vec!["api", api_endpoint, "--include"];
    let header;
    if let Some(tag) = etag {
        header = format!("If-None-Match: {tag}");
        args.extend(["-H", &header]);
    }

    let result = Command::new(gh_bin)
        .args(&args)
        .current_dir(root.as_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let output = match result {
        Ok(o) => o,
        Err(_) => return GhPrIn::PrUnchanged,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    // --include puts HTTP headers before the body, separated by a blank line
    let (headers_section, body) = match stdout.find("\r\n\r\n") {
        Some(pos) => (&stdout[..pos], &stdout[pos + 4..]),
        None => match stdout.find("\n\n") {
            Some(pos) => (&stdout[..pos], &stdout[pos + 2..]),
            None => return GhPrIn::PrUnchanged,
        },
    };

    // Check for 304
    if headers_section.contains("304") {
        return GhPrIn::PrUnchanged;
    }

    // Not 304 — PR changed. Extract new ETag.
    let new_etag = headers_section
        .lines()
        .find(|l| l.to_lowercase().starts_with("etag:"))
        .map(|l| l.split_once(':').unwrap().1.trim().to_string());

    // Parse the PR JSON body
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return GhPrIn::PrUnchanged,
    };

    let number = parsed["number"].as_u64().unwrap_or(0) as u32;
    let state = parsed["state"].as_str().unwrap_or("open").to_string();
    let url = parsed["html_url"].as_str().unwrap_or("").to_string();

    // Map REST API state to the same format as `gh pr view`
    let state = match state.as_str() {
        "open" => "OPEN".to_string(),
        "closed" => {
            if parsed["merged"].as_bool() == Some(true) {
                "MERGED".to_string()
            } else {
                "CLOSED".to_string()
            }
        }
        other => other.to_uppercase(),
    };

    let api_endpoint_new = parse_github_url(&url)
        .map(|(owner, repo)| format!("repos/{owner}/{repo}/pulls/{number}"))
        .unwrap_or_else(|| api_endpoint.to_string());

    let comments = load_review_threads(gh_bin, number, &url, root);

    let diff_lines = match run_gh(gh_bin, &["pr", "diff"], root) {
        GhResult::Ok(output) => parse_unified_diff(&output, root),
        _ => HashMap::new(),
    };

    let head_oid = parsed["head"]
        .get("sha")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let all_paths: Vec<&CanonPath> = diff_lines.keys().chain(comments.keys()).collect();
    let file_hashes = compute_file_hashes(root, head_oid, &all_paths);

    log::info!(
        "[gh-pr] poll detected change: PR #{number} {state}, {} files",
        diff_lines.len()
    );

    GhPrIn::PrLoaded {
        number: PrNumber(number),
        state,
        url,
        api_endpoint: api_endpoint_new,
        etag: new_etag,
        diff_lines,
        comments,
        file_hashes,
    }
}

/// Fetch the ETag for a PR endpoint (used on initial load).
fn fetch_etag(gh_bin: &str, api_endpoint: &str) -> Option<String> {
    use std::process::Command;

    let result = Command::new(gh_bin)
        .args(["api", api_endpoint, "--include"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&result.stdout);
    let headers = stdout
        .split_once("\r\n\r\n")
        .or_else(|| stdout.split_once("\n\n"))
        .map(|(h, _)| h)?;

    headers
        .lines()
        .find(|l| l.to_lowercase().starts_with("etag:"))
        .map(|l| l.split_once(':').unwrap().1.trim().to_string())
}

enum GhResult {
    Ok(String),
    NotInstalled,
    Failed,
}

fn run_gh(gh_bin: &str, args: &[&str], root: &CanonPath) -> GhResult {
    use std::process::Command;

    let result = Command::new(gh_bin)
        .args(args)
        .current_dir(root.as_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match result {
        Ok(output) if output.status.success() => {
            GhResult::Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(_) => GhResult::Failed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GhResult::NotInstalled,
        Err(_) => GhResult::Failed,
    }
}

/// Extract `(owner, repo)` from a GitHub PR URL like
/// `https://github.com/owner/repo/pull/42`.
fn parse_github_url(url: &str) -> Option<(&str, &str)> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some((owner, repo))
}

/// Fetch line-level review thread comments via the GraphQL API.
fn load_review_threads(
    gh_bin: &str,
    pr_number: u32,
    url: &str,
    root: &CanonPath,
) -> HashMap<CanonPath, Vec<ReviewComment>> {
    let Some((owner, repo)) = parse_github_url(url) else {
        return HashMap::new();
    };

    let query = format!(
        r#"query {{ repository(owner:"{owner}",name:"{repo}") {{ pullRequest(number:{pr_number}) {{ reviewThreads(first:100) {{ nodes {{ path line isOutdated comments(first:5) {{ nodes {{ body url author {{ login }} }} }} }} }} }} }} }}"#,
    );

    let output = match run_gh(
        gh_bin,
        &["api", "graphql", "-f", &format!("query={query}")],
        root,
    ) {
        GhResult::Ok(s) => s,
        _ => return HashMap::new(),
    };

    let parsed: serde_json::Value = match serde_json::from_str(&output) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let mut result: HashMap<CanonPath, Vec<ReviewComment>> = HashMap::new();

    let threads = parsed
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(|v| v.as_array());

    let Some(threads) = threads else {
        return result;
    };

    for thread in threads {
        let is_outdated = thread
            .get("isOutdated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_outdated {
            continue;
        }

        let Some(path_str) = thread.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(line) = thread.get("line").and_then(|v| v.as_u64()) else {
            continue;
        };
        // GraphQL returns 1-based line numbers; convert to 0-based Row
        let row = Row(line.saturating_sub(1) as usize);

        let (body, author, comment_url) = thread
            .pointer("/comments/nodes/0")
            .map(|comment| {
                let body = comment
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let author = comment
                    .pointer("/author/login")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let url = comment
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                (body, author, url)
            })
            .unwrap_or_default();

        let abs_path = UserPath::new(root.as_path().join(path_str)).canonicalize();
        result.entry(abs_path).or_default().push(ReviewComment {
            line: row,
            body,
            author,
            url: comment_url,
        });
    }

    result
}

/// Compute content hashes for PR files at a given commit, using the same
/// algorithm as `TextDoc::content_hash()` (DefaultHasher over bytes).
fn compute_file_hashes(
    root: &CanonPath,
    head_oid: &str,
    paths: &[&CanonPath],
) -> HashMap<CanonPath, PersistedContentHash> {
    let mut result = HashMap::new();
    if head_oid.is_empty() || paths.is_empty() {
        return result;
    }

    let repo = match git2::Repository::open(root.as_path()) {
        Ok(r) => r,
        Err(_) => return result,
    };

    let oid = match git2::Oid::from_str(head_oid) {
        Ok(o) => o,
        Err(_) => return result,
    };

    let commit = match repo.find_commit(oid) {
        Ok(c) => c,
        Err(_) => return result,
    };

    let tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return result,
    };

    for path in paths {
        // Convert absolute path to repo-relative
        let rel = match path.as_path().strip_prefix(root.as_path()) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let entry = match tree.get_path(rel) {
            Ok(e) => e,
            Err(_) => continue, // file is new in the PR (not in tree) — no hash
        };

        let blob = match repo.find_blob(entry.id()) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let mut hasher = DefaultHasher::new();
        hasher.write(blob.content());
        result.insert((*path).clone(), PersistedContentHash(hasher.finish()));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{Doc, TextDoc};

    /// The hash we compute from a git blob must match what `TextDoc::content_hash`
    /// produces for the same bytes. If this test fails, `file_hashes` will never
    /// match buffer hashes, and PR annotations will always be suppressed.
    #[test]
    fn blob_hash_matches_textdoc_content_hash() {
        let content = b"line1\nline2\nline3\n";

        // The gh-pr side: hash the raw blob bytes.
        let mut hasher = DefaultHasher::new();
        hasher.write(content);
        let blob_hash = PersistedContentHash(hasher.finish());

        // The buffer side: load into a TextDoc and call content_hash().
        let doc = TextDoc::from_reader(&content[..]).unwrap();
        let doc_hash = PersistedContentHash(doc.content_hash().0);

        assert_eq!(
            blob_hash, doc_hash,
            "blob hash {blob_hash:?} must match TextDoc hash {doc_hash:?}"
        );
    }

    #[test]
    fn blob_hash_matches_textdoc_content_hash_large() {
        // A larger content that forces rope to split into multiple chunks.
        let line = "hello world, this is a fairly long line that the rope will split up.\n";
        let content: String = line.repeat(500);
        let bytes = content.as_bytes();

        let mut hasher = DefaultHasher::new();
        hasher.write(bytes);
        let blob_hash = PersistedContentHash(hasher.finish());

        let doc = TextDoc::from_reader(bytes).unwrap();
        let doc_hash = PersistedContentHash(doc.content_hash().0);

        assert_eq!(
            blob_hash, doc_hash,
            "blob hash {blob_hash:?} must match TextDoc hash {doc_hash:?} for large content"
        );
    }
}

fn parse_unified_diff(diff: &str, root: &CanonPath) -> HashMap<CanonPath, Vec<LineStatus>> {
    let mut result: HashMap<CanonPath, Vec<LineStatus>> = HashMap::new();
    let mut current_path: Option<CanonPath> = None;

    for line in diff.lines() {
        // Detect file header: +++ b/path/to/file
        if let Some(path_str) = line.strip_prefix("+++ b/") {
            let abs_path = UserPath::new(root.as_path().join(path_str)).canonicalize();
            current_path = Some(abs_path);
            continue;
        }

        // Detect hunk header: @@ -old_start,old_count +new_start,new_count @@
        if line.starts_with("@@ ") {
            // We process hunks line-by-line below, nothing to do here
            // except that we track current_new_line via the + lines
            continue;
        }

        // Skip if we don't have a file context yet
        let Some(ref path) = current_path else {
            continue;
        };

        // We need to track line numbers from hunk headers.
        // Re-parse: look for @@ lines to set context.
        // Actually, let's restructure: parse properly with state.
        // The simple approach below won't track line numbers.
        // Let me break out of this loop and use a proper stateful parser.
        let _ = path;
        break;
    }

    // Proper stateful parse
    result.clear();
    current_path = None;
    let mut new_line: usize = 0;
    let mut hunk_has_deletes = false;

    for line in diff.lines() {
        if let Some(path_str) = line.strip_prefix("+++ b/") {
            let abs_path = UserPath::new(root.as_path().join(path_str)).canonicalize();
            current_path = Some(abs_path);
            continue;
        }

        if line.starts_with("--- ") || line.starts_with("diff ") {
            continue;
        }

        if line.starts_with("@@ ") {
            // Parse +new_start from @@ -a,b +c,d @@
            if let Some(plus_part) = line.split('+').nth(1) {
                let num_str = plus_part.split(&[',', ' '][..]).next().unwrap_or("1");
                new_line = num_str.parse::<usize>().unwrap_or(1).saturating_sub(1);
            }
            hunk_has_deletes = false;
            // Pre-scan hunk for deletes to determine Added vs Modified
            // We'll do a simpler approach: check if there are any - lines
            // Actually, let's just scan ahead in the diff. For simplicity,
            // mark all PR diff lines as PrDiff (no Added/Modified distinction).
            continue;
        }

        let Some(ref path) = current_path else {
            continue;
        };

        if let Some(stripped) = line.strip_prefix('-') {
            // Deleted line — doesn't appear in new file
            let _ = stripped;
            hunk_has_deletes = true;
            continue;
        }

        if let Some(_added) = line.strip_prefix('+') {
            let row = new_line;
            let statuses = result.entry(path.clone()).or_default();
            let category = IssueCategory::PrDiff;

            if let Some(last) = statuses.last_mut() {
                if last.category == category && last.rows.end == row {
                    last.rows.end = row + 1;
                    new_line += 1;
                    continue;
                }
            }
            statuses.push(LineStatus {
                category,
                rows: row..row + 1,
            });
            new_line += 1;
            continue;
        }

        // Context line (space prefix or no prefix)
        if line.starts_with(' ') || (!line.starts_with('\\') && current_path.is_some()) {
            new_line += 1;
        }
    }

    // Suppress the unused variable warning
    let _ = hunk_has_deletes;

    result
}
