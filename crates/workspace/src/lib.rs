use std::path::{Path, PathBuf};

use led_core::FanoutStream;
use tokio::sync;
use tokio_stream::{Stream, StreamExt};

pub fn driver(mut input: impl Stream<Item = PathBuf> + Unpin + Send + 'static) -> impl Stream<Item = PathBuf> {
    let (tx, rx) = sync::broadcast::channel(10);

    tokio::spawn(async move {
        while let Some(dir) = input.next().await {
            let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
            let root = find_git_root(&dir);
            tx.send(root).ok();
        }
    });

    FanoutStream::new(rx)
}

fn find_git_root(start: &Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    let mut root = None;
    loop {
        let git = dir.join(".git");
        if git.exists() && git.is_dir() {
            root = Some(dir.clone());
        }
        if !dir.pop() {
            break;
        }
    }
    root.unwrap_or_else(|| start.to_path_buf())
}
