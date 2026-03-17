use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Alert, Doc, DocId, TextDoc, watch};
use tokio::sync::mpsc;

#[derive(Clone)]
pub enum DocStoreOut {
    Open { path: PathBuf },
    Save { id: DocId, doc: Arc<dyn Doc> },
    Close { id: DocId, doc: Arc<dyn Doc> },
}

#[derive(Clone)]
pub enum DocStoreIn {
    Opened {
        id: DocId,
        path: PathBuf,
        doc: Arc<dyn Doc>,
    },
    Saved {
        id: DocId,
        doc: Arc<dyn Doc>,
    },
    ExternalChange {
        id: DocId,
        doc: Arc<dyn Doc>,
    },
    ExternalRemove {
        id: DocId,
    },
}

impl fmt::Debug for DocStoreIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocStoreIn::Opened { id, path, .. } => f
                .debug_struct("Opened")
                .field("id", id)
                .field("path", path)
                .finish(),
            DocStoreIn::Saved { id, .. } => f.debug_struct("Saved").field("id", id).finish(),
            DocStoreIn::ExternalChange { id, .. } => {
                f.debug_struct("ExternalChange").field("id", id).finish()
            }
            DocStoreIn::ExternalRemove { id } => {
                f.debug_struct("ExternalRemove").field("id", id).finish()
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
pub fn driver(out: Stream<DocStoreOut>) -> Stream<Result<DocStoreIn, Alert>> {
    let stream: Stream<Result<DocStoreIn, Alert>> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<DocStoreOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<Result<DocStoreIn, Alert>>(64);

    // Bridge out: rx::Stream → channel
    out.on(move |opt: Option<&DocStoreOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async driver task
    tokio::spawn(async move {
        let (watcher_tx, mut watcher_rx) = mpsc::channel::<notify::Event>(256);

        let mut next_doc_id: u64 = 0;
        let mut open_docs: HashMap<DocId, OpenDoc> = HashMap::new();
        let mut path_to_id: HashMap<PathBuf, DocId> = HashMap::new();
        let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
        let mut self_notified: HashSet<PathBuf> = HashSet::new();

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        DocStoreOut::Open { path } => {
                            if let Some(parent) = path.parent() {
                                if watched_dirs.insert(parent.to_path_buf()) {
                                    let mut watcher = watch(parent);
                                    let fwd = watcher_tx.clone();
                                    tokio::spawn(async move {
                                        while let Some(event) = watcher.recv().await {
                                            let _ = fwd.send(event).await;
                                        }
                                    });
                                }
                            }

                            match read_doc(&path).await {
                                Ok(doc) => {
                                    let id = DocId(next_doc_id);
                                    next_doc_id += 1;
                                    open_docs.insert(id, OpenDoc { path: path.clone() });
                                    path_to_id.insert(path.clone(), id);
                                    let doc: Arc<dyn Doc> = Arc::new(doc);
                                    let _ = result_tx.send(Ok(DocStoreIn::Opened { id, path, doc })).await;
                                }
                                Err(e) => {
                                    let _ = result_tx.send(Err(Alert::Warn(format!(
                                        "Cannot open {}: {e}", path.display()
                                    )))).await;
                                }
                            }
                        }
                        DocStoreOut::Save { id, doc } => {
                            if let Some(open) = open_docs.get(&id) {
                                handle_save(
                                    &open.path, &doc, &mut self_notified, &result_tx, id,
                                ).await;
                            }
                        }
                        DocStoreOut::Close { id, .. } => {
                            if let Some(open) = open_docs.remove(&id) {
                                path_to_id.remove(&open.path);
                            }
                        }
                    }
                }
                Some(event) = watcher_rx.recv() => {
                    handle_watcher_event(
                        event, &path_to_id, &mut self_notified, &result_tx,
                    ).await;
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

/// Apply format-on-save: strip trailing whitespace per line, ensure final newline.
/// Returns the formatted doc with edits recorded as an undo group.
fn format_on_save(doc: &Arc<dyn Doc>) -> Arc<dyn Doc> {
    let mut doc = doc.close_undo_group();
    let line_count = doc.line_count();

    // Strip trailing whitespace (iterate in reverse so offsets stay valid)
    for line_idx in (0..line_count).rev() {
        let line = doc.line(line_idx); // already stripped of \n
        let trimmed = line.trim_end();
        if trimmed.len() < line.len() {
            let line_start = doc.line_to_char(line_idx);
            let start = line_start + trimmed.len();
            let end = line_start + line.len();
            doc = doc.remove(start, end);
        }
    }

    // Ensure final newline
    let len_chars = doc.line_to_char(doc.line_count().saturating_sub(1))
        + doc.line_len(doc.line_count().saturating_sub(1));
    let last_line = doc.line(doc.line_count().saturating_sub(1));
    if !last_line.is_empty() {
        doc = doc.insert(len_chars, "\n");
    }

    doc.close_undo_group()
}

async fn handle_save(
    path: &PathBuf,
    doc: &Arc<dyn Doc>,
    self_notified: &mut HashSet<PathBuf>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
    id: DocId,
) {
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp_path = parent.join(format!(".led-save-{}", std::process::id()));

    // Format on save: strip trailing whitespace, ensure final newline
    let doc = format_on_save(doc);

    // Serialize to memory, then write async
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
            self_notified.insert(path.clone());
            let doc = doc.mark_saved();
            let _ = tx.send(Ok(DocStoreIn::Saved { id, doc })).await;
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
    event: notify::Event,
    path_to_id: &HashMap<PathBuf, DocId>,
    self_notified: &mut HashSet<PathBuf>,
    tx: &mpsc::Sender<Result<DocStoreIn, Alert>>,
) {
    use notify::EventKind;

    for path in &event.paths {
        let Some(&id) = path_to_id.get(path) else {
            continue;
        };

        if self_notified.remove(path) {
            continue;
        }

        let msg = match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => match read_doc(path).await {
                Ok(doc) => {
                    let doc: Arc<dyn Doc> = Arc::new(doc);
                    Some(Ok(DocStoreIn::ExternalChange { id, doc }))
                }
                Err(e) => {
                    log::warn!("Failed to re-read {}: {e}", path.display());
                    None
                }
            },
            EventKind::Remove(_) => Some(Ok(DocStoreIn::ExternalRemove { id })),
            _ => None,
        };

        if let Some(msg) = msg {
            let _ = tx.send(msg).await;
        }
    }
}
