use std::collections::HashMap;
use std::process::Stdio;

use led_core::git::{LineStatus, LineStatusKind};
use led_core::rx::Stream;
use led_core::{CanonPath, PrNumber, Row, UserPath};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum GhPrOut {
    LoadPr { branch: String, root: CanonPath },
}

#[derive(Clone, Debug)]
pub enum GhPrIn {
    PrLoaded {
        number: PrNumber,
        state: String,
        url: String,
        diff_lines: HashMap<CanonPath, Vec<LineStatus>>,
        comments: HashMap<CanonPath, Vec<(Row, String, String)>>,
    },
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
            match cmd {
                GhPrOut::LoadPr { branch, root } => {
                    let bin = gh_bin.clone();
                    let result =
                        tokio::task::spawn_blocking(move || load_pr(&branch, &root, &bin)).await;
                    match result {
                        Ok(ev) => {
                            result_tx.send(ev).await.ok();
                        }
                        Err(e) => {
                            log::warn!("[gh-pr] task panicked: {e}");
                        }
                    }
                }
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
        &["pr", "view", "--json", "number,state,url,reviews"],
        root,
    ) {
        GhResult::Ok(output) => output,
        GhResult::NotInstalled => return GhPrIn::GhUnavailable,
        GhResult::Failed => return GhPrIn::NoPr,
    };

    let parsed: serde_json::Value = match serde_json::from_str(&meta) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[gh-pr] failed to parse gh output: {e}");
            return GhPrIn::NoPr;
        }
    };

    let number = parsed["number"].as_u64().unwrap_or(0) as u32;
    let state = parsed["state"].as_str().unwrap_or("OPEN").to_string();
    let url = parsed["url"].as_str().unwrap_or("").to_string();

    // `gh pr view --json reviews` provides review-level data (approve/reject)
    // but not line-level review threads. Line-level comments require the
    // GraphQL API. For now, comments are empty.
    let comments = HashMap::new();

    let diff_lines = match run_gh(gh_bin, &["pr", "diff"], root) {
        GhResult::Ok(output) => parse_unified_diff(&output, root),
        _ => HashMap::new(),
    };

    log::debug!(
        "[gh-pr] loaded PR #{number} for branch {branch}: {state}, {} files, {} files with comments",
        diff_lines.len(),
        comments.len(),
    );

    GhPrIn::PrLoaded {
        number: PrNumber(number),
        state,
        url,
        diff_lines,
        comments,
    }
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
        .stderr(Stdio::piped())
        .output();

    match result {
        Ok(output) if output.status.success() => {
            GhResult::Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log::warn!(
                "[gh-pr] `gh {}` failed (status={}): {}",
                args.join(" "),
                output.status,
                stderr.trim()
            );
            GhResult::Failed
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => GhResult::NotInstalled,
        Err(_) => GhResult::Failed,
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
            let kind = LineStatusKind::PrDiff;

            if let Some(last) = statuses.last_mut() {
                if last.kind == kind && last.rows.end == row {
                    last.rows.end = row + 1;
                    new_line += 1;
                    continue;
                }
            }
            statuses.push(LineStatus {
                kind,
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
