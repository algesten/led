use std::path::PathBuf;

use led_core::rx::Stream;
use tokio::sync::mpsc;

/// Commands sent to the filesystem driver.
#[derive(Clone, Debug)]
pub enum FsOut {
    ListDir {
        path: PathBuf,
    },
    FindFileList {
        dir: PathBuf,
        prefix: String,
        show_hidden: bool,
    },
}

/// Results returned from the filesystem driver.
#[derive(Clone, Debug)]
pub enum FsIn {
    DirListed {
        path: PathBuf,
        entries: Vec<DirEntry>,
    },
    FindFileListed {
        dir: PathBuf,
        entries: Vec<FindFileEntry>,
    },
}

/// A single directory entry (file or subdirectory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// A find-file completion entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindFileEntry {
    pub name: String,  // display name: dirs get trailing "/"
    pub full: PathBuf, // full path
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
                FsOut::FindFileList {
                    dir,
                    prefix,
                    show_hidden,
                } => {
                    let entries = find_file_list(&dir, &prefix, show_hidden);
                    result_tx
                        .send(FsIn::FindFileListed { dir, entries })
                        .await
                        .ok();
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

fn find_file_list(dir: &std::path::Path, prefix: &str, show_hidden: bool) -> Vec<FindFileEntry> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            log::warn!(
                "find_file_list: failed to read dir {}: {}",
                dir.display(),
                e
            );
            return Vec::new();
        }
    };

    let prefix_lower = prefix.to_lowercase();

    let mut entries: Vec<FindFileEntry> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let raw_name = e.file_name().to_string_lossy().into_owned();
            if !show_hidden && raw_name.starts_with('.') {
                return None;
            }
            if !raw_name.to_lowercase().starts_with(&prefix_lower) {
                return None;
            }
            let is_dir = e.file_type().ok()?.is_dir();
            let display_name = if is_dir {
                format!("{raw_name}/")
            } else {
                raw_name
            };
            let full = dir.join(e.file_name());
            Some(FindFileEntry {
                name: display_name,
                full,
                is_dir,
            })
        })
        .collect();

    // Sort: dirs first, then case-insensitive alphabetical
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    entries
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
