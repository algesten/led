use std::collections::HashMap;
use std::fmt;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{
    Alert, Doc, DocId, FileWatcher, Registration, TextDoc, WatchEvent, WatchEventKind, WatchMode,
};
use tokio::sync::mpsc;

#[derive(Clone)]
pub enum DocStoreOut {
    Open {
        path: PathBuf,
        tab_order: usize,
        /// When true, opening a non-existent path creates an empty buffer
        /// instead of reporting OpenFailed.  Used for user-initiated opens
        /// (CLI arg, find-file); session restore passes false.
        create_if_missing: bool,
    },
    Save {
        id: DocId,
        doc: Arc<dyn Doc>,
    },
    SaveAs {
        id: DocId,
        doc: Arc<dyn Doc>,
        path: PathBuf,
    },
    Close {
        id: DocId,
        doc: Arc<dyn Doc>,
    },
}

#[derive(Clone)]
pub enum DocStoreIn {
    Opened {
        id: DocId,
        path: PathBuf,
        doc: Arc<dyn Doc>,
        tab_order: usize,
    },
    Saved {
        id: DocId,
        doc: Arc<dyn Doc>,
    },
    SavedAs {
        id: DocId,
        path: PathBuf,
        doc: Arc<dyn Doc>,
    },
    ExternalChange {
        id: DocId,
        path: PathBuf,
        doc: Arc<dyn Doc>,
    },
    ExternalRemove {
        id: DocId,
    },
    OpenFailed {
        path: PathBuf,
    },
}

impl fmt::Debug for DocStoreIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocStoreIn::Opened {
                id,
                path,
                tab_order,
                ..
            } => f
                .debug_struct("Opened")
                .field("id", id)
                .field("path", path)
                .field("tab_order", tab_order)
                .finish(),
            DocStoreIn::Saved { id, .. } => f.debug_struct("Saved").field("id", id).finish(),
            DocStoreIn::SavedAs { id, path, .. } => f
                .debug_struct("SavedAs")
                .field("id", id)
                .field("path", path)
                .finish(),
            DocStoreIn::ExternalChange { id, path, .. } => f
                .debug_struct("ExternalChange")
                .field("id", id)
                .field("path", path)
                .finish(),
            DocStoreIn::ExternalRemove { id } => {
                f.debug_struct("ExternalRemove").field("id", id).finish()
            }
            DocStoreIn::OpenFailed { path } => {
                f.debug_struct("OpenFailed").field("path", path).finish()
            }
        }
    }
}

struct OpenDoc {
    path: PathBuf,
}

/// Read a file and construct a TextDoc. Async read, sync Rope construction from memory.
async fn read_doc(path: &PathBuf) -> std::io::Result<TextDoc> {
    let bytes = tokio::fs::read(path).await?;
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

        let mut next_doc_id: u64 = 0;
        let mut open_docs: HashMap<DocId, OpenDoc> = HashMap::new();
        // Keyed by canonical path so lookups match what the notify crate
        // reports (e.g. /var → /private/var on macOS).
        let mut path_to_id: HashMap<PathBuf, DocId> = HashMap::new();
        let mut registrations: HashMap<PathBuf, Registration> = HashMap::new();

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        DocStoreOut::Open { path, tab_order, create_if_missing } => {
                            if let Some(parent) = path.parent() {
                                let canonical = std::fs::canonicalize(parent)
                                    .unwrap_or_else(|_| parent.to_path_buf());
                                if !registrations.contains_key(&canonical) {
                                    let reg = file_watcher.register(
                                        parent,
                                        WatchMode::NonRecursive,
                                        watcher_tx.clone(),
                                    );
                                    registrations.insert(canonical, reg);
                                }
                            }

                            let canonical = canonicalize(&path);
                            let doc_result = match read_doc(&path).await {
                                Ok(doc) => Ok(doc),
                                Err(e) if create_if_missing
                                    && e.kind() == std::io::ErrorKind::NotFound =>
                                {
                                    // New file: create an empty document.
                                    // The file will be created on disk when the user saves.
                                    Ok(TextDoc::from_reader(Cursor::new(b"" as &[u8])).unwrap())
                                }
                                Err(e) => Err(e),
                            };
                            match doc_result {
                                Ok(doc) => {
                                    // Reuse existing DocId if already tracked,
                                    // otherwise allocate a new one.
                                    let id = match path_to_id.get(&canonical) {
                                        Some(&existing) => existing,
                                        None => {
                                            let id = DocId(next_doc_id);
                                            next_doc_id += 1;
                                            open_docs.insert(id, OpenDoc { path: path.clone() });
                                            path_to_id.insert(canonical, id);
                                            id
                                        }
                                    };
                                    let doc: Arc<dyn Doc> = Arc::new(doc);
                                    let _ = result_tx.send(Ok(DocStoreIn::Opened { id, path, doc, tab_order })).await;
                                }
                                Err(e) => {
                                    log::debug!("Cannot open {}: {e}", path.display());
                                    let _ = result_tx
                                        .send(Ok(DocStoreIn::OpenFailed { path }))
                                        .await;
                                }
                            }
                        }
                        DocStoreOut::Save { id, doc } => {
                            if let Some(open) = open_docs.get(&id) {
                                handle_save(
                                    &open.path, &doc, &result_tx, id,
                                ).await;
                            }
                        }
                        DocStoreOut::SaveAs { id, doc, path } => {
                            // Update internal path tracking
                            let old_canonical = open_docs.get(&id).map(|o| canonicalize(&o.path));
                            if let Some(old_canonical) = old_canonical {
                                path_to_id.remove(&old_canonical);
                            }
                            let new_canonical = canonicalize(&path);
                            open_docs.insert(id, OpenDoc { path: path.clone() });
                            path_to_id.insert(new_canonical, id);

                            // Register watcher for new parent directory
                            if let Some(parent) = path.parent() {
                                let canonical_parent = std::fs::canonicalize(parent)
                                    .unwrap_or_else(|_| parent.to_path_buf());
                                if !registrations.contains_key(&canonical_parent) {
                                    let reg = file_watcher.register(
                                        parent,
                                        WatchMode::NonRecursive,
                                        watcher_tx.clone(),
                                    );
                                    registrations.insert(canonical_parent, reg);
                                }
                            }

                            handle_save_as(
                                &path, &doc, &result_tx, id,
                            ).await;
                        }
                        DocStoreOut::Close { id, .. } => {
                            if let Some(open) = open_docs.remove(&id) {
                                let canonical = canonicalize(&open.path);
                                path_to_id.remove(&canonical);
                            }
                        }
                    }
                }
                Some(event) = watcher_rx.recv() => {
                    log::trace!("[docstore] select got watcher event: {:?}", event.kind);
                    handle_watcher_event(
                        event, &path_to_id, &result_tx,
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

async fn handle_save(
    path: &PathBuf,
    doc: &Arc<dyn Doc>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
    id: DocId,
) {
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    // Create parent directories for new files that don't exist on disk yet.
    if !parent.exists() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            let _ = tx
                .send(Err(Alert::Warn(format!(
                    "Failed to create directory {}: {e}",
                    parent.display()
                ))))
                .await;
            return;
        }
    }
    let tmp_path = parent.join(format!(".led-save-{}", std::process::id()));

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

    match tokio::fs::rename(&tmp_path, path).await {
        Ok(()) => {
            let _ = tx.send(Ok(DocStoreIn::Saved { id, doc: doc.clone() })).await;
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
    path: &PathBuf,
    doc: &Arc<dyn Doc>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
    id: DocId,
) {
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    if !parent.exists() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            let _ = tx
                .send(Err(Alert::Warn(format!(
                    "Failed to create directory {}: {e}",
                    parent.display()
                ))))
                .await;
            return;
        }
    }
    let tmp_path = parent.join(format!(".led-save-{}", std::process::id()));

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

    match tokio::fs::rename(&tmp_path, path).await {
        Ok(()) => {
            let _ = tx
                .send(Ok(DocStoreIn::SavedAs {
                    id,
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
    path_to_id: &HashMap<PathBuf, DocId>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
) {
    for path in &event.paths {
        let Some(&id) = path_to_id.get(path) else {
            log::trace!("[docstore] path not in path_to_id: {}", path.display());
            continue;
        };

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
                        id,
                        path: path.clone(),
                        doc,
                    }))
                }
                Err(e) => {
                    log::warn!("Failed to re-read {}: {e}", path.display());
                    None
                }
            },
            WatchEventKind::Remove => Some(Ok(DocStoreIn::ExternalRemove { id })),
        };

        if let Some(msg) = msg {
            let _ = tx.send(msg).await;
        }
    }
}

fn canonicalize(path: &PathBuf) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())
}
