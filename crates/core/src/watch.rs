use std::path::Path;

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub fn watch(path: &Path) -> mpsc::Receiver<notify::Event> {
    let (tx, rx) = mpsc::channel(3);

    let notify = move |event| match event {
        Ok(v) => {
            let _ = tx.blocking_send(v);
        }
        Err(e) => log::warn!("watch failed: {:?}", e),
    };

    let mut watcher = notify::recommended_watcher(notify).expect("start a watcher for");

    watcher
        .watch(path, RecursiveMode::Recursive)
        .expect("watch path");

    // Keep the watcher alive for the process lifetime. It will stop
    // producing events once all receivers are dropped.
    std::mem::forget(watcher);

    rx
}
