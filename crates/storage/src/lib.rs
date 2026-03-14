use std::collections::HashSet;
use std::fmt;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Alert, TextDoc, WriteContent, watch};
use tokio::sync::mpsc;

#[derive(Clone)]
pub enum StorageOut {
    Open(PathBuf),
    Close(PathBuf),
    Save(PathBuf, Arc<dyn WriteContent>),
}

#[derive(Clone)]
pub enum StorageIn {
    Opened(PathBuf, TextDoc),
    Saved(PathBuf),
    Changed(PathBuf),
    Removed(PathBuf),
}

impl fmt::Debug for StorageIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageIn::Opened(path, _) => {
                f.debug_tuple("Opened").field(path).field(&"<doc>").finish()
            }
            StorageIn::Saved(path) => f.debug_tuple("Saved").field(path).finish(),
            StorageIn::Changed(path) => f.debug_tuple("Changed").field(path).finish(),
            StorageIn::Removed(path) => f.debug_tuple("Removed").field(path).finish(),
        }
    }
}

/// Start the storage driver. Takes a stream of commands, returns a stream of results.
pub fn driver(out: Stream<StorageOut>) -> Stream<Result<StorageIn, Alert>> {
    let stream: Stream<Result<StorageIn, Alert>> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<StorageOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<Result<StorageIn, Alert>>(64);

    // Bridge out: rx::Stream → channel
    out.on(move |cmd: &StorageOut| {
        cmd_tx.try_send(cmd.clone()).ok();
    });

    // Async driver task
    tokio::spawn(async move {
        let (watcher_tx, mut watcher_rx) = mpsc::channel::<notify::Event>(256);

        let mut open_files: HashSet<PathBuf> = HashSet::new();
        let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
        let mut self_notified: HashSet<PathBuf> = HashSet::new();

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        StorageOut::Open(path) => {
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

                            match std::fs::File::open(&path) {
                                Ok(file) => {
                                    match TextDoc::from_reader(BufReader::new(file)) {
                                        Ok(doc) => {
                                            open_files.insert(path.clone());
                                            let _ = result_tx.send(Ok(StorageIn::Opened(path, doc))).await;
                                        }
                                        Err(e) => {
                                            let _ = result_tx.send(Err(Alert::Warn(format!(
                                                "Failed to read {}: {e}", path.display()
                                            )))).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = result_tx.send(Err(Alert::Warn(format!(
                                        "Cannot open {}: {e}", path.display()
                                    )))).await;
                                }
                            }
                        }
                        StorageOut::Close(path) => {
                            open_files.remove(&path);
                        }
                        StorageOut::Save(path, content) => {
                            handle_save(
                                &path, &content, &mut self_notified, &result_tx,
                            ).await;
                        }
                    }
                }
                Some(event) = watcher_rx.recv() => {
                    handle_watcher_event(
                        event, &open_files, &mut self_notified, &result_tx,
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

async fn handle_save(
    path: &PathBuf,
    content: &Arc<dyn WriteContent>,
    self_notified: &mut HashSet<PathBuf>,
    tx: &mpsc::Sender<Result<StorageIn, Alert>>,
) {
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let tmp_path = parent.join(format!(".led-save-{}", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        {
            let mut file = std::fs::File::create(&tmp_path)?;
            content.write_to(&mut file)?;
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
