use std::fs::{self, File, OpenOptions};
use std::hash::DefaultHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use led_core::Startup;
use led_core::rx::Stream;
use tokio::sync::mpsc;

const GIT_DIR: &str = ".git";
const PRIMARY_DIR: &str = "primary";

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Workspace {
    pub root: PathBuf,
    pub config: PathBuf,
    pub primary: bool,
}

/// Start the workspace driver. Takes a stream of Startup commands,
/// returns a stream of computed Workspaces.
pub fn driver(out: Stream<Arc<Startup>>) -> Stream<Workspace> {
    let stream: Stream<Workspace> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Arc<Startup>>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<Workspace>(64);

    // Bridge out: rx::Stream → channel
    out.on(move |cmd: &Arc<Startup>| {
        cmd_tx.try_send(cmd.clone()).ok();
    });

    // Async task: compute workspace
    tokio::spawn(async move {
        while let Some(startup) = cmd_rx.recv().await {
            let dir = fs::canonicalize(&*startup.start_dir)
                .unwrap_or_else(|_| startup.start_dir.as_ref().clone());

            let root = find_git_root(&dir);

            let config = startup.config_dir.clone();

            let primary = match try_become_primary(&config, &root) {
                Some(lock_file) => {
                    std::mem::forget(lock_file);
                    true
                }
                None => false,
            };

            let workspace = Workspace {
                root,
                config,
                primary,
            };

            if result_tx.send(workspace).await.is_err() {
                break;
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

fn find_git_root(start: &Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    let mut root = None;
    loop {
        let git = dir.join(GIT_DIR);
        if git.exists() && git.is_dir() {
            root = Some(dir.clone());
        }
        if !dir.pop() {
            break;
        }
    }
    root.unwrap_or_else(|| start.to_path_buf())
}

fn try_become_primary(config: &Path, root: &Path) -> Option<File> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::io::AsRawFd;

    let lock_dir = config.join(PRIMARY_DIR);
    std::fs::create_dir_all(&lock_dir).ok()?;

    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_dir.join(&hash))
        .ok()?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 { Some(file) } else { None }
}
