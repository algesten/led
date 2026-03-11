use std::fs;
use std::path::{Path, PathBuf};

use led_core::AStream;
use tokio_stream::{Stream, StreamExt};

#[derive(Clone, Default)]
pub struct Workspace {
    pub root: PathBuf,
}

pub fn driver(input: impl AStream<PathBuf>) -> impl Stream<Item = Workspace> {
    input.map(|dir| {
        let dir = fs::canonicalize(&dir).unwrap_or(dir);
        let root = find_git_root(&dir);
        Workspace { root }
    })
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
