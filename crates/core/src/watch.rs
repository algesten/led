use std::path::Path;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub fn watch(path: &Path) -> mpsc::Receiver<notify::Event> {
    let (tx, rx) = mpsc::channel(3);

    let notify_cb = move |event| match event {
        Ok(v) => {
            let _ = tx.blocking_send(v);
        }
        Err(e) => log::warn!("watch failed: {:?}", e),
    };

    let config = notify::Config::default()
        .with_poll_interval(Duration::from_secs(1));
    let mut watcher =
        notify::PollWatcher::new(notify_cb, config).expect("start a watcher");

    watcher
        .watch(path, RecursiveMode::Recursive)
        .expect("watch path");

    // Keep the watcher alive for the process lifetime.
    std::mem::forget(watcher);

    rx
}
