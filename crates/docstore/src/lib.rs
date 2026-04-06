use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Cursor;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{
    Alert, CanonPath, Doc, FileWatcher, Registration, TextDoc, WatchEvent, WatchEventKind,
    WatchMode,
};
use tokio::sync::mpsc;

#[derive(Clone)]
pub enum DocStoreOut {
    Open {
        path: CanonPath,
        /// When true, opening a non-existent path creates an empty buffer
        /// instead of reporting OpenFailed.  Used for user-initiated opens
        /// (CLI arg, find-file); session restore passes false.
        create_if_missing: bool,
    },
    Save {
        path: CanonPath,
        doc: Arc<dyn Doc>,
    },
    SaveAs {
        path: CanonPath,
        doc: Arc<dyn Doc>,
        new_path: CanonPath,
    },
}

#[derive(Clone)]
pub enum DocStoreIn {
    /// Driver acknowledged the open request; materialization in progress.
    Opening {
        path: CanonPath,
    },
    Opened {
        path: CanonPath,
        doc: Arc<dyn Doc>,
    },
    Saved {
        path: CanonPath,
        doc: Arc<dyn Doc>,
    },
    SavedAs {
        path: CanonPath,
        doc: Arc<dyn Doc>,
    },
    ExternalChange {
        path: CanonPath,
        doc: Arc<dyn Doc>,
    },
    ExternalRemove {
        path: CanonPath,
    },
    OpenFailed {
        path: CanonPath,
    },
}

impl fmt::Debug for DocStoreIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocStoreIn::Opening { path } => f.debug_struct("Opening").field("path", path).finish(),
            DocStoreIn::Opened { path, .. } => {
                f.debug_struct("Opened").field("path", path).finish()
            }
            DocStoreIn::Saved { path, .. } => f.debug_struct("Saved").field("path", path).finish(),
            DocStoreIn::SavedAs { path, .. } => {
                f.debug_struct("SavedAs").field("path", path).finish()
            }
            DocStoreIn::ExternalChange { path, .. } => f
                .debug_struct("ExternalChange")
                .field("path", path)
                .finish(),
            DocStoreIn::ExternalRemove { path } => f
                .debug_struct("ExternalRemove")
                .field("path", path)
                .finish(),
            DocStoreIn::OpenFailed { path } => {
                f.debug_struct("OpenFailed").field("path", path).finish()
            }
        }
    }
}

/// Read a file and construct a TextDoc. Async read, sync Rope construction from memory.
async fn read_doc(path: &CanonPath) -> std::io::Result<TextDoc> {
    let bytes = tokio::fs::read(path.as_path()).await?;
    TextDoc::from_reader(Cursor::new(bytes))
}

/// Start the docstore driver. Takes a stream of commands, returns a stream of results.
///
/// `file_watcher`: when `Some`, the driver registers parent directories of
/// opened files so that external changes are detected.  Pass `None` to
/// disable watching (tests that don't need it).
pub fn driver(
    out: Stream<DocStoreOut>,
    file_watcher: Arc<FileWatcher>,
) -> Stream<Result<DocStoreIn, Alert>> {
    let stream: Stream<Result<DocStoreIn, Alert>> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<DocStoreOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<Result<DocStoreIn, Alert>>(64);

    // Bridge out: rx::Stream → channel
    out.on(move |opt: Option<&DocStoreOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async driver task (spawn_local so it is scheduled by the LocalSet
    // alongside the rest of the app on the single-threaded runtime).
    tokio::task::spawn_local(async move {
        let (watcher_tx, mut watcher_rx) = mpsc::channel::<WatchEvent>(256);

        let mut registrations: HashMap<CanonPath, Registration> = HashMap::new();
        // Canonical paths of files we've opened — used to filter watcher events
        // so we only report external changes for files we care about.
        let mut watched_paths: HashSet<CanonPath> = HashSet::new();

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        DocStoreOut::Open { path, create_if_missing } => {
                            log::debug!("[docstore] Open: {}", path.display());

                            register_watcher(
                                &path, &file_watcher, &watcher_tx,
                                &mut registrations,
                            );

                            let _ = result_tx.send(Ok(DocStoreIn::Opening { path: path.clone() })).await;

                            let doc_result = match read_doc(&path).await {
                                Ok(doc) => Ok(doc),
                                Err(e) if create_if_missing
                                    && e.kind() == std::io::ErrorKind::NotFound =>
                                {
                                    Ok(TextDoc::from_reader(Cursor::new(b"" as &[u8])).unwrap())
                                }
                                Err(e) => Err(e),
                            };

                            match doc_result {
                                Ok(doc) => {
                                    watched_paths.insert(path.clone());
                                    let doc: Arc<dyn Doc> = Arc::new(doc);
                                    let _ = result_tx.send(Ok(DocStoreIn::Opened { path, doc })).await;
                                }
                                Err(e) => {
                                    log::debug!("Cannot open {}: {e}", path.display());
                                    let _ = result_tx
                                        .send(Ok(DocStoreIn::OpenFailed { path }))
                                        .await;
                                }
                            }
                        }
                        DocStoreOut::Save { path, doc } => {
                            handle_save(&path, &doc, &result_tx).await;
                        }
                        DocStoreOut::SaveAs { path, doc, new_path } => {
                            watched_paths.remove(&path);
                            watched_paths.insert(new_path.clone());

                            register_watcher(
                                &new_path, &file_watcher, &watcher_tx,
                                &mut registrations,
                            );

                            handle_save_as(&new_path, &doc, &result_tx).await;
                        }
                    }
                }
                Some(event) = watcher_rx.recv() => {
                    log::trace!("[docstore] select got watcher event: {:?}", event.kind);
                    handle_watcher_event(
                        event, &watched_paths, &result_tx,
                    ).await;
                    log::trace!("[docstore] handle_watcher_event done");
                }
            }
        }
    });

    // Bridge in: channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}

fn register_watcher(
    path: &CanonPath,
    file_watcher: &Arc<FileWatcher>,
    watcher_tx: &mpsc::Sender<WatchEvent>,
    registrations: &mut HashMap<CanonPath, Registration>,
) {
    if let Some(parent) = path.parent() {
        if !registrations.contains_key(&parent) {
            let reg = file_watcher.register(&parent, WatchMode::NonRecursive, watcher_tx.clone());
            registrations.insert(parent, reg);
        }
    }
}

async fn handle_save(
    path: &CanonPath,
    doc: &Arc<dyn Doc>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
) {
    let parent = path.parent();
    let parent_path = parent
        .as_ref()
        .map(|p| p.as_path())
        .unwrap_or(std::path::Path::new("."));
    // Create parent directories for new files that don't exist on disk yet.
    if !parent_path.exists() {
        if let Err(e) = tokio::fs::create_dir_all(parent_path).await {
            let _ = tx
                .send(Err(Alert::Warn(format!(
                    "Failed to create directory {}: {e}",
                    parent_path.display()
                ))))
                .await;
            return;
        }
    }
    let tmp_path = parent_path.join(format!(".led-save-{}", std::process::id()));

    // Serialize to memory, then write async (cleanup already applied by model layer)
    let mut buf = Vec::new();
    if let Err(e) = doc.write_to(&mut buf) {
        let _ = tx
            .send(Err(Alert::Warn(format!(
                "Failed to serialize {}: {e}",
                path.display()
            ))))
            .await;
        return;
    }

    if let Err(e) = tokio::fs::write(&tmp_path, &buf).await {
        let _ = tx
            .send(Err(Alert::Warn(format!(
                "Failed to save {}: {e}",
                path.display()
            ))))
            .await;
        return;
    }

    match tokio::fs::rename(&tmp_path, path.as_path()).await {
        Ok(()) => {
            let _ = tx
                .send(Ok(DocStoreIn::Saved {
                    path: path.clone(),
                    doc: doc.clone(),
                }))
                .await;
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            let _ = tx
                .send(Err(Alert::Warn(format!(
                    "Failed to save {}: {e}",
                    path.display()
                ))))
                .await;
        }
    }
}

async fn handle_save_as(
    path: &CanonPath,
    doc: &Arc<dyn Doc>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
) {
    let parent = path.parent();
    let parent_path = parent
        .as_ref()
        .map(|p| p.as_path())
        .unwrap_or(std::path::Path::new("."));
    if !parent_path.exists() {
        if let Err(e) = tokio::fs::create_dir_all(parent_path).await {
            let _ = tx
                .send(Err(Alert::Warn(format!(
                    "Failed to create directory {}: {e}",
                    parent_path.display()
                ))))
                .await;
            return;
        }
    }
    let tmp_path = parent_path.join(format!(".led-save-{}", std::process::id()));

    let mut buf = Vec::new();
    if let Err(e) = doc.write_to(&mut buf) {
        let _ = tx
            .send(Err(Alert::Warn(format!(
                "Failed to serialize {}: {e}",
                path.display()
            ))))
            .await;
        return;
    }

    if let Err(e) = tokio::fs::write(&tmp_path, &buf).await {
        let _ = tx
            .send(Err(Alert::Warn(format!(
                "Failed to save {}: {e}",
                path.display()
            ))))
            .await;
        return;
    }

    match tokio::fs::rename(&tmp_path, path.as_path()).await {
        Ok(()) => {
            let _ = tx
                .send(Ok(DocStoreIn::SavedAs {
                    path: path.clone(),
                    doc: doc.clone(),
                }))
                .await;
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            let _ = tx
                .send(Err(Alert::Warn(format!(
                    "Failed to save {}: {e}",
                    path.display()
                ))))
                .await;
        }
    }
}

async fn handle_watcher_event(
    event: WatchEvent,
    watched_paths: &HashSet<CanonPath>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
) {
    for path in &event.paths {
        if !watched_paths.contains(path) {
            log::trace!("[docstore] path not watched: {}", path.display());
            continue;
        }

        log::trace!(
            "[docstore] path matched: {} kind={:?}",
            path.display(),
            event.kind
        );

        let msg = match event.kind {
            WatchEventKind::Create | WatchEventKind::Modify => match read_doc(path).await {
                Ok(doc) => {
                    log::trace!("[docstore] read_doc ok, sending ExternalChange");
                    let doc: Arc<dyn Doc> = Arc::new(doc);
                    Some(Ok(DocStoreIn::ExternalChange {
                        path: path.clone(),
                        doc,
                    }))
                }
                Err(e) => {
                    log::warn!("Failed to re-read {}: {e}", path.display());
                    None
                }
            },
            WatchEventKind::Remove => Some(Ok(DocStoreIn::ExternalRemove { path: path.clone() })),
        };

        if let Some(msg) = msg {
            let _ = tx.send(msg).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{FileWatcher, UserPath};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    /// Helper: run a test inside a single-threaded LocalSet, matching
    /// the real app's runtime (spawn_local, current_thread).
    fn run_local<F: std::future::Future<Output = ()> + 'static>(f: F) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            tokio::task::LocalSet::new().run_until(f).await;
        });
    }

    /// Wait until a predicate is satisfied, polling with yield_now.
    /// Panics after timeout.
    async fn wait_for<F: Fn() -> bool>(pred: F, label: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if pred() {
                return;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("wait_for({label}) timed out");
            }
            tokio::task::yield_now().await;
        }
    }

    /// The docstore does not deduplicate — that's the model's job.
    /// Multiple Opens for the same file each produce an Opened event.
    #[test]
    fn duplicate_opens_each_produce_opened() {
        run_local(async {
            let dir = tempfile::TempDir::new().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "hello\n").unwrap();
            let file = UserPath::new(file).canonicalize();

            let watcher = FileWatcher::new();
            let cmd_stream: Stream<DocStoreOut> = Stream::new();
            let result_stream = driver(cmd_stream.clone(), watcher);

            let results: Rc<RefCell<Vec<DocStoreIn>>> = Rc::new(RefCell::new(Vec::new()));
            let r = results.clone();
            result_stream.on(move |opt: Option<&Result<DocStoreIn, Alert>>| {
                if let Some(Ok(ev)) = opt {
                    r.borrow_mut().push(ev.clone());
                }
            });

            // Push 3 Opens synchronously — they all land in the mpsc channel
            // before the driver task gets to run (single-threaded LocalSet).
            cmd_stream.push(DocStoreOut::Open {
                path: file.clone(),
                create_if_missing: false,
            });
            cmd_stream.push(DocStoreOut::Open {
                path: file.clone(),
                create_if_missing: false,
            });
            cmd_stream.push(DocStoreOut::Open {
                path: file.clone(),
                create_if_missing: false,
            });

            let r2 = results.clone();
            wait_for(
                move || {
                    r2.borrow()
                        .iter()
                        .filter(|e| matches!(e, DocStoreIn::Opened { .. }))
                        .count()
                        >= 3
                },
                "3 Opened events",
            )
            .await;

            let events = results.borrow();
            let opened_count = events
                .iter()
                .filter(|e| matches!(e, DocStoreIn::Opened { .. }))
                .count();

            assert_eq!(
                opened_count, 3,
                "docstore does not deduplicate (events: {:?})",
                &*events
            );
        });
    }

    /// Opens for different files in the same batch are all processed.
    #[test]
    fn different_files_in_batch_all_open() {
        run_local(async {
            let dir = tempfile::TempDir::new().unwrap();
            let file_a = dir.path().join("a.txt");
            let file_b = dir.path().join("b.txt");
            std::fs::write(&file_a, "aaa\n").unwrap();
            std::fs::write(&file_b, "bbb\n").unwrap();
            let file_a = UserPath::new(file_a).canonicalize();
            let file_b = UserPath::new(file_b).canonicalize();

            let watcher = FileWatcher::new();
            let cmd_stream: Stream<DocStoreOut> = Stream::new();
            let result_stream = driver(cmd_stream.clone(), watcher);

            let results: Rc<RefCell<Vec<DocStoreIn>>> = Rc::new(RefCell::new(Vec::new()));
            let r = results.clone();
            result_stream.on(move |opt: Option<&Result<DocStoreIn, Alert>>| {
                if let Some(Ok(ev)) = opt {
                    r.borrow_mut().push(ev.clone());
                }
            });

            cmd_stream.push(DocStoreOut::Open {
                path: file_a.clone(),
                create_if_missing: false,
            });
            cmd_stream.push(DocStoreOut::Open {
                path: file_b.clone(),
                create_if_missing: false,
            });

            let r2 = results.clone();
            wait_for(
                move || {
                    r2.borrow()
                        .iter()
                        .filter(|e| matches!(e, DocStoreIn::Opened { .. }))
                        .count()
                        >= 2
                },
                "2 Opened events",
            )
            .await;

            let events = results.borrow();
            let opened_count = events
                .iter()
                .filter(|e| matches!(e, DocStoreIn::Opened { .. }))
                .count();
            assert_eq!(
                opened_count, 2,
                "both files should open (events: {:?})",
                &*events
            );
        });
    }
}
