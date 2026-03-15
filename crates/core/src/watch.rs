use std::path::Path;

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// Watch a directory for changes. The watcher is created on a dedicated
/// thread so that platform-specific event loops (e.g. FSEvents' CFRunLoop
/// on macOS) don't interfere with the tokio runtime's I/O driver.
pub fn watch(path: &Path) -> mpsc::Receiver<notify::Event> {
    let (tx, rx) = mpsc::channel(3);
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

        // Keep the watcher alive by parking this thread forever.
        // The watcher (and its internal event loop) lives here.
        std::mem::forget(watcher);
        loop {
            std::thread::park();
        }
    });

    rx
}
