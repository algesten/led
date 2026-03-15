mod db;

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

// ── Types ──

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Workspace {
    pub root: PathBuf,
    pub config: PathBuf,
    pub primary: bool,
}

#[derive(Clone, Debug)]
pub enum WorkspaceOut {
    /// Initialize workspace: find git root, acquire primary lock, open DB, load session.
    Init { startup: Arc<Startup> },
    /// Save full session (on quit, primary only).
    SaveSession { data: SessionData },
}

#[derive(Clone, Debug)]
pub enum WorkspaceIn {
    /// Workspace resolved. Always sent first after Init.
    Workspace { workspace: Workspace },
    /// Session restored (sent once, right after Workspace).
    SessionRestored { session: Option<RestoredSession> },
    /// Session saved to DB.
    SessionSaved,
    /// Workspace tree changed (watcher event — re-emits the workspace).
    WorkspaceChanged { workspace: Workspace },
}

// ── Session types ──

#[derive(Clone, Debug)]
pub struct SessionData {
    pub buffers: Vec<SessionBuffer>,
    pub active_tab_order: usize,
    pub show_side_panel: bool,
}

#[derive(Clone, Debug)]
pub struct SessionBuffer {
    pub file_path: PathBuf,
    pub tab_order: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_row: usize,
    pub scroll_sub_line: usize,
}

#[derive(Clone, Debug)]
pub struct RestoredSession {
    pub buffers: Vec<SessionBuffer>,
    pub active_tab_order: usize,
    pub show_side_panel: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SessionRestorePhase {
    #[default]
    Pending,
    Restoring,
    Done,
}

// ── Driver ──

/// Start the workspace driver. Takes a stream of commands,
/// returns a stream of events.
pub fn driver(out: Stream<WorkspaceOut>) -> Stream<WorkspaceIn> {
    let stream: Stream<WorkspaceIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<WorkspaceOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<WorkspaceIn>(64);

    // Bridge out: rx::Stream → channel
    out.on(move |opt: Option<&WorkspaceOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async task: compute workspace, manage DB, start watchers
    tokio::spawn(async move {
        let (watch_tx, mut watch_rx) = mpsc::channel::<()>(16);
        let mut _watcher: Option<notify::RecommendedWatcher> = None;
        let (watcher_ready_tx, mut watcher_ready_rx) =
            mpsc::channel::<notify::RecommendedWatcher>(1);
        let mut current: Option<Workspace> = None;
        let mut _db: Option<rusqlite::Connection> = None;
        let mut _lock_file: Option<File> = None;

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        WorkspaceOut::SaveSession { data } => {
                            if let Some(ref conn) = _db {
                                if let Err(e) = db::save_session(conn, &data) {
                                    log::warn!("failed to save session: {e}");
                                }
                            }
                            let _ = result_tx.send(WorkspaceIn::SessionSaved).await;
                        }
                        WorkspaceOut::Init { startup } => {
                            let dir = fs::canonicalize(&*startup.start_dir)
                                .unwrap_or_else(|_| startup.start_dir.as_ref().clone());

                            let root = find_git_root(&dir);
                            let config = PathBuf::clone(&startup.config_dir);

                            let primary = match try_become_primary(&config, &root) {
                                Some(file) => {
                                    _lock_file = Some(file);
                                    true
                                }
                                None => false,
                            };

                            let workspace = Workspace { root: root.clone(), config: config.clone(), primary };

                            current = Some(workspace.clone());
                            if result_tx.send(WorkspaceIn::Workspace { workspace }).await.is_err() {
                                break;
                            }

                            // Open DB and load session
                            let session = match db::open_db(&config) {
                                Ok(conn) => {
                                    let session = if primary {
                                        db::load_session(&conn).ok().flatten()
                                    } else {
                                        None
                                    };
                                    _db = Some(conn);
                                    session
                                }
                                Err(e) => {
                                    log::warn!("failed to open session db: {e}");
                                    None
                                }
                            };

                            if result_tx.send(WorkspaceIn::SessionRestored { session }).await.is_err() {
                                break;
                            }

                            // Start recursive watcher on workspace root.
                            // spawn_blocking so the (potentially slow) OS watcher
                            // setup doesn't block the driver task. The watcher
                            // is delivered via watcher_ready_rx in the select loop.
                            let watch_tx2 = watch_tx.clone();
                            let root2 = root.clone();
                            let watcher_tx = watcher_ready_tx.clone();
                            tokio::task::spawn_blocking(move || {
                                if let Some(w) = start_watcher(&root2, watch_tx2) {
                                    watcher_tx.blocking_send(w).ok();
                                }
                            });
                        }
                    }
                }
                Some(w) = watcher_ready_rx.recv() => {
                    _watcher = Some(w);
                }
                Some(()) = watch_rx.recv() => {
                    // Workspace tree changed — re-emit to trigger browser rebuild
                    if let Some(ref ws) = current {
                        if result_tx.send(WorkspaceIn::WorkspaceChanged { workspace: ws.clone() }).await.is_err() {
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

// ── Internals ──

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
