use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;

use led_state::file_search::{FileGroup, SearchHit};

pub fn run_search(
    query: &str,
    root: &std::path::Path,
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

    let walker = WalkBuilder::new(root)
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
        let path = entry.path().to_path_buf();

        let mut hits = Vec::new();
        let m = &matcher;
        let _ = searcher.search_path(
            m,
            &path,
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
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
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
