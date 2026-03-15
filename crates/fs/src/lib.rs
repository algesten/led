use std::path::PathBuf;

use led_core::rx::Stream;
use tokio::sync::mpsc;

/// Commands sent to the filesystem driver.
#[derive(Clone, Debug)]
pub enum FsOut {
    ListDir { path: PathBuf },
}

/// Results returned from the filesystem driver.
#[derive(Clone, Debug)]
pub enum FsIn {
    DirListed {
        path: PathBuf,
        entries: Vec<DirEntry>,
    },
}

/// A single directory entry (file or subdirectory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

pub fn driver(out: Stream<FsOut>) -> Stream<FsIn> {
    let stream: Stream<FsIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<FsOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<FsIn>(64);

    // Bridge: rx::Stream → channel
    out.on(move |opt: Option<&FsOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async task: handle filesystem operations
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                FsOut::ListDir { path } => {
                    let entries = list_dir(&path);
                    result_tx.send(FsIn::DirListed { path, entries }).await.ok();
                }
            }
        }
    });

    // Bridge: channel → rx::Stream
    let s = stream.clone();
    tokio::task::spawn_local(async move {
        while let Some(v) = result_rx.recv().await {
            s.push(v);
        }
    });

    stream
}

fn list_dir(path: &std::path::Path) -> Vec<DirEntry> {
    let read = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("failed to read dir {}: {}", path.display(), e);
            return Vec::new();
        }
    };

    let mut entries: Vec<DirEntry> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let is_dir = e.file_type().ok()?.is_dir();
            Some(DirEntry { name, is_dir })
        })
        .collect();

    // Sort: directories first, then files, alphabetical within each group
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));

    entries
}
