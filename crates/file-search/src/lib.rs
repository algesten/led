mod search;

use led_core::CanonPath;
use led_core::rx::Stream;
use led_state::file_search::{FileGroup, ReplaceScope};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum FileSearchOut {
    Search {
        query: String,
        root: CanonPath,
        case_sensitive: bool,
        use_regex: bool,
    },
    Replace {
        query: String,
        replacement: String,
        root: CanonPath,
        case_sensitive: bool,
        use_regex: bool,
        scope: ReplaceScope,
        skip_paths: Vec<CanonPath>,
    },
}

#[derive(Clone, Debug)]
pub enum FileSearchIn {
    Results {
        results: Vec<FileGroup>,
    },
    ReplaceComplete {
        results: Vec<FileGroup>,
        replaced_count: usize,
    },
}

pub fn driver(out: Stream<FileSearchOut>) -> Stream<FileSearchIn> {
    let stream: Stream<FileSearchIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<FileSearchOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<FileSearchIn>(64);

    // Bridge: rx::Stream → channel
    out.on(move |opt: Option<&FileSearchOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async task: coalescing search worker
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            // Drain queue, only process the latest request
            let mut latest = cmd;
            while let Ok(newer) = cmd_rx.try_recv() {
                latest = newer;
            }

            match latest {
                FileSearchOut::Search {
                    query,
                    root,
                    case_sensitive,
                    use_regex,
                } => {
                    let results = tokio::task::spawn_blocking(move || {
                        search::run_search(&query, &root, case_sensitive, use_regex)
                    })
                    .await
                    .unwrap_or_default();

                    result_tx.send(FileSearchIn::Results { results }).await.ok();
                }
                FileSearchOut::Replace {
                    query,
                    replacement,
                    root,
                    case_sensitive,
                    use_regex,
                    scope,
                    skip_paths,
                } => {
                    let (results, replaced_count) = tokio::task::spawn_blocking(move || {
                        search::run_replace(
                            &query,
                            &replacement,
                            &root,
                            case_sensitive,
                            use_regex,
                            &scope,
                            &skip_paths,
                        )
                    })
                    .await
                    .unwrap_or_default();

                    result_tx
                        .send(FileSearchIn::ReplaceComplete {
                            results,
                            replaced_count,
                        })
                        .await
                        .ok();
                }
            }
        }
    });

    // Bridge: channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}
