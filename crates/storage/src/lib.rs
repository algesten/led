use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use led_core::{AStream, Alert, WriteContent, watch};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

pub enum StorageOut {
    Open(PathBuf),
    Close(PathBuf),
    Save(PathBuf, Arc<dyn WriteContent>),
}

#[derive(Debug, Clone)]
pub enum StorageIn {
    Opened(PathBuf),
    Saved(PathBuf),
    Changed(PathBuf),
    Removed(PathBuf),
}

pub fn driver(out: impl AStream<StorageOut>) -> impl AStream<Result<StorageIn, Alert>> {
    let (tx, rx) = mpsc::channel::<Result<StorageIn, Alert>>(64);

    tokio::spawn(async move {
        // All watcher events from all watched directories are forwarded here.
        let (watcher_tx, mut watcher_rx) = mpsc::channel::<notify::Event>(256);

        let mut open_files: HashSet<PathBuf> = HashSet::new();
        let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
        // Paths we saved ourselves — suppress the next change event for these.
        let mut self_notified: HashSet<PathBuf> = HashSet::new();

        let mut out = std::pin::pin!(out);

        loop {
            tokio::select! {
                maybe_cmd = out.next() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        StorageOut::Open(path) => {
                            open_files.insert(path.clone());

                            if let Some(parent) = path.parent() {
                                if watched_dirs.insert(parent.to_path_buf()) {
                                    let mut watch_rx = watch(parent);
                                    let fwd = watcher_tx.clone();
                                    tokio::spawn(async move {
                                        while let Some(event) = watch_rx.recv().await {
                                            let _ = fwd.send(event).await;
                                        }
                                    });
                                }
                            }

                            let _ = tx.send(Ok(StorageIn::Opened(path))).await;
                        }
                        StorageOut::Close(path) => {
                            open_files.remove(&path);
                        }
                        StorageOut::Save(path, content) => {
                            handle_save(
                                &path, &content, &mut self_notified, &tx,
                            ).await;
                        }
                    }
                }
                Some(event) = watcher_rx.recv() => {
                    handle_watcher_event(
                        event, &open_files, &mut self_notified, &tx,
                    ).await;
                }
            }
        }
    });

    ReceiverStream::new(rx)
}

async fn handle_save(
    path: &PathBuf,
    content: &Arc<dyn WriteContent>,
    self_notified: &mut HashSet<PathBuf>,
    tx: &mpsc::Sender<Result<StorageIn, Alert>>,
) {
    // Write to a temp file in the same directory, then atomically rename.
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp_path = parent.join(format!(".led-save-{}", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        {
            let mut file = std::fs::File::create(&tmp_path)?;
            content.write_to(&mut file)?;
            // Ensure data is flushed before rename.
            std::io::Write::flush(&mut file)?;
        }
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            self_notified.insert(path.clone());
            let _ = tx.send(Ok(StorageIn::Saved(path.clone()))).await;
        }
        Err(e) => {
            // Clean up temp file on failure.
            let _ = std::fs::remove_file(&tmp_path);
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
    open_files: &HashSet<PathBuf>,
    self_notified: &mut HashSet<PathBuf>,
    tx: &mpsc::Sender<Result<StorageIn, Alert>>,
) {
    use notify::EventKind;

    for path in &event.paths {
        if !open_files.contains(path) {
            continue;
        }

        // If this is our own save, consume the flag and skip.
        if self_notified.remove(path) {
            continue;
        }

        let msg = match event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                Some(Ok(StorageIn::Changed(path.clone())))
            }
            EventKind::Remove(_) => Some(Ok(StorageIn::Removed(path.clone()))),
            _ => None,
        };

        if let Some(msg) = msg {
            let _ = tx.send(msg).await;
        }
    }
}
