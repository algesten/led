use tokio::sync::mpsc;

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;

use led_core::Waker;

use crate::types::{FileGroup, SearchHit, SearchRequest};

pub(crate) async fn search_worker(
    mut rx: mpsc::UnboundedReceiver<SearchRequest>,
    tx: mpsc::UnboundedSender<Vec<FileGroup>>,
    waker: Option<Waker>,
) {
    while let Some(req) = rx.recv().await {
        // Drain any queued requests, only process the latest
        let mut latest = req;
        while let Ok(newer) = rx.try_recv() {
            latest = newer;
        }

        let results = tokio::task::spawn_blocking(move || run_search(&latest))
            .await
            .unwrap_or_default();
        let _ = tx.send(results);
        if let Some(ref w) = waker {
            w();
        }
    }
}

fn run_search(req: &SearchRequest) -> Vec<FileGroup> {
    let pattern = if req.use_regex {
        req.query.clone()
    } else {
        regex_syntax::escape(&req.query)
    };

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!req.case_sensitive)
        .build(&pattern);

    let matcher = match matcher {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    let walker = WalkBuilder::new(&req.root)
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
                // Find all matches in this line
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
                            // Move past this match
                            byte_offset = abs_end;
                            if mat.start() == mat.end() {
                                // Zero-width match, advance by one byte to avoid infinite loop
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
                .strip_prefix(&req.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            groups.push(FileGroup {
                path,
                relative,
                hits,
            });
        }

        // Cap results to avoid huge result sets
        let total_hits: usize = groups.iter().map(|g| g.hits.len()).sum();
        if total_hits > 1000 {
            break;
        }
    }

    groups.sort_by(|a, b| a.relative.cmp(&b.relative));
    groups
}
