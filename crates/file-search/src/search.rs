use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;

use led_core::{CanonPath, UserPath};
use led_state::file_search::{FileGroup, ReplaceScope, SearchHit};

pub fn run_search(
    query: &str,
    root: &CanonPath,
    case_sensitive: bool,
    use_regex: bool,
) -> Vec<FileGroup> {
    let pattern = if use_regex {
        query.to_string()
    } else {
        regex_syntax::escape(query)
    };

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        .build(&pattern);

    let matcher = match matcher {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    let walker = WalkBuilder::new(root.as_path())
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut groups: Vec<FileGroup> = Vec::new();
    let mut searcher = grep_searcher::SearcherBuilder::new()
        .binary_detection(grep_searcher::BinaryDetection::quit(0x00))
        .build();

    for entry in walker.flatten() {
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            continue;
        }
        let path = UserPath::new(entry.path()).canonicalize();

        let mut hits = Vec::new();
        let m = &matcher;
        let _ = searcher.search_path(
            m,
            path.as_path(),
            UTF8(|line_num, line_text| {
                let line_text = line_text.trim_end_matches('\n').trim_end_matches('\r');
                let mut byte_offset = 0;
                loop {
                    let hay = &line_text.as_bytes()[byte_offset..];
                    if hay.is_empty() {
                        break;
                    }
                    match m.find(hay) {
                        Ok(Some(mat)) => {
                            let abs_start = byte_offset + mat.start();
                            let abs_end = byte_offset + mat.end();
                            let col = line_text[..abs_start].chars().count();
                            hits.push(SearchHit {
                                row: (line_num as usize).saturating_sub(1),
                                col,
                                line_text: line_text.to_string(),
                                match_start: abs_start,
                                match_end: abs_end,
                            });
                            byte_offset = abs_end;
                            if mat.start() == mat.end() {
                                byte_offset += 1;
                            }
                        }
                        _ => break,
                    }
                }
                Ok(true)
            }),
        );

        if !hits.is_empty() {
            let relative = path
                .strip_prefix(root)
                .map(|p: &std::path::Path| p.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());
            groups.push(FileGroup {
                path,
                relative,
                hits,
            });
        }

        let total_hits: usize = groups.iter().map(|g| g.hits.len()).sum();
        if total_hits > 1000 {
            break;
        }
    }

    groups.sort_by(|a, b| a.relative.cmp(&b.relative));
    groups
}

pub fn run_replace(
    query: &str,
    replacement: &str,
    root: &CanonPath,
    case_sensitive: bool,
    use_regex: bool,
    scope: &ReplaceScope,
    skip_paths: &[CanonPath],
) -> (Vec<FileGroup>, usize) {
    let pattern = if use_regex {
        query.to_string()
    } else {
        regex_syntax::escape(query)
    };

    let re = match regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
    {
        Ok(r) => r,
        Err(_) => return (run_search(query, root, case_sensitive, use_regex), 0),
    };

    let mut replaced_count: usize = 0;

    match scope {
        ReplaceScope::Single {
            path,
            row,
            match_start,
            match_end,
        } => {
            if let Ok(content) = std::fs::read_to_string(path.as_path()) {
                let lines: Vec<&str> = content.lines().collect();
                if *row < lines.len() {
                    let line = lines[*row];
                    if *match_start <= line.len() && *match_end <= line.len() {
                        let mut new_content = String::with_capacity(content.len());
                        for (i, l) in content.lines().enumerate() {
                            if i > 0 {
                                new_content.push('\n');
                            }
                            if i == *row {
                                new_content.push_str(&l[..*match_start]);
                                new_content.push_str(replacement);
                                new_content.push_str(&l[*match_end..]);
                                replaced_count += 1;
                            } else {
                                new_content.push_str(l);
                            }
                        }
                        // Preserve trailing newline
                        if content.ends_with('\n') {
                            new_content.push('\n');
                        }
                        write_atomic(path.as_path(), &new_content);
                    }
                }
            }
        }
        ReplaceScope::All => {
            let walker = WalkBuilder::new(root.as_path())
                .hidden(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .build();

            for entry in walker.flatten() {
                if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                    continue;
                }
                let path = UserPath::new(entry.path()).canonicalize();
                if skip_paths.contains(&path) {
                    continue;
                }

                let Ok(content) = std::fs::read_to_string(path.as_path()) else {
                    continue;
                };

                let new_content = re.replace_all(&content, replacement);
                if new_content != content {
                    let count = re.find_iter(&content).count();
                    replaced_count += count;
                    write_atomic(path.as_path(), &new_content);
                }
            }
        }
    }

    // Re-search to get updated results
    let results = run_search(query, root, case_sensitive, use_regex);
    (results, replaced_count)
}

fn write_atomic(path: &std::path::Path, content: &str) {
    let dir = path.parent().unwrap_or(path);
    let tmp = dir.join(format!(".led-replace-{}", std::process::id()));
    if std::fs::write(&tmp, content).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
