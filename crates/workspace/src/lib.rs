use std::fs::{self, File, OpenOptions};
use std::hash::DefaultHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use led_core::Startup;
use led_core::rx::Stream;
use notify::{EventKind, RecursiveMode, Watcher};
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

    // Async task: compute workspace + start watcher
    tokio::spawn(async move {
        let (watch_tx, mut watch_rx) = mpsc::channel::<()>(16);
        let mut _watcher: Option<notify::RecommendedWatcher> = None;
        let mut current: Option<Workspace> = None;

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(startup) = maybe_cmd else { break };
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

                    let workspace = Workspace { root: root.clone(), config, primary };

                    // Start recursive watcher on workspace root (skip in headless/test mode)
                    if !startup.headless {
                        _watcher = start_watcher(&root, watch_tx.clone());
                    }

                    current = Some(workspace.clone());
                    if result_tx.send(workspace).await.is_err() {
                        break;
                    }
                }
                Some(()) = watch_rx.recv() => {
                    // Workspace tree changed — re-emit to trigger browser rebuild
                    if let Some(ref ws) = current {
                        if result_tx.send(ws.clone()).await.is_err() {
                            break;
                        }
                    }
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

fn start_watcher(root: &Path, tx: mpsc::Sender<()>) -> Option<notify::RecommendedWatcher> {
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let Ok(ev) = res else { return };
            match ev.kind {
                EventKind::Create(_) | EventKind::Remove(_) => {}
                _ => return,
            }
            // Skip .git internal changes
            if ev
                .paths
                .iter()
                .all(|p| p.components().any(|c| c.as_os_str() == ".git"))
            {
                return;
            }
            tx.try_send(()).ok();
        })
        .ok()?;

    watcher.watch(root, RecursiveMode::Recursive).ok()?;
    Some(watcher)
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
