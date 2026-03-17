use std::path::Path;

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// A file-watcher guard. Dropping it shuts down the watcher thread and
/// releases all OS file descriptors associated with the watcher.
pub struct FileWatcher {
    rx: mpsc::Receiver<notify::Event>,
    /// Dropping this sender signals the watcher thread to exit.
    _stop: std::sync::mpsc::Sender<()>,
}

impl FileWatcher {
    pub async fn recv(&mut self) -> Option<notify::Event> {
        self.rx.recv().await
    }
}

/// Watch a directory for changes. The watcher is created on a dedicated
/// thread so that platform-specific event loops (e.g. FSEvents' CFRunLoop
/// on macOS) don't interfere with the tokio runtime's I/O driver.
pub fn watch(path: &Path) -> FileWatcher {
    let (tx, rx) = mpsc::channel(3);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let path = path.to_path_buf();

    std::thread::spawn(move || {
        let notify_cb = move |event| match event {
            Ok(v) => {
                let _ = tx.blocking_send(v);
            }
            Err(e) => log::warn!("watch failed: {:?}", e),
        };

        let mut watcher = notify::recommended_watcher(notify_cb).expect("start a watcher");

        watcher
            .watch(&path, RecursiveMode::Recursive)
            .expect("watch path");

        // Block until the stop channel closes (FileWatcher dropped).
        // The watcher stays alive as long as we're blocked here.
        let _ = stop_rx.recv();
    });

    FileWatcher { rx, _stop: stop_tx }
}
