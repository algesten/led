pub mod db;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::DefaultHasher;
use std::sync::Arc;
use std::time::Instant;

use led_core::rx::Stream;
use led_core::{
    CanonPath, Col, FileWatcher, PersistedContentHash, Registration, Row, Startup, SubLine,
    UserPath, WatchEvent, WatchEventKind, WatchMode,
};
use tokio::sync::mpsc;

const GIT_DIR: &str = ".git";
const PRIMARY_DIR: &str = "primary";

// ── Types ──

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Workspace {
    pub root: CanonPath,
    /// The workspace root as the user provided it (preserves symlink paths).
    pub user_root: UserPath,
    pub config: UserPath,
    pub primary: bool,
}

#[derive(Clone, Debug)]
pub enum WorkspaceOut {
    /// Initialize workspace: find git root, acquire primary lock, open DB, load session.
    Init { startup: Arc<Startup> },
    /// Save full session (on quit, primary only).
    SaveSession { data: SessionData },
    /// Flush unpersisted undo entries for a buffer.
    FlushUndo {
        file_path: CanonPath,
        chain_id: String,
        content_hash: PersistedContentHash,
        undo_cursor: usize,
        distance_from_save: i32,
        entries: Vec<led_core::UndoEntry>,
    },
    /// Delete undo state after save.
    ClearUndo { file_path: CanonPath },
    /// Query for cross-instance sync.
    CheckSync {
        file_path: CanonPath,
        last_seen_seq: i64,
        current_chain_id: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub enum WorkspaceIn {
    /// Workspace resolved. Always sent first after Init.
    Workspace { workspace: Workspace },
    /// Session restored (sent once, right after Workspace).
    SessionRestored { session: Option<RestoredSession> },
    /// Session saved to DB.
    SessionSaved,
    /// Undo entries flushed.
    UndoFlushed {
        file_path: CanonPath,
        chain_id: String,
        persisted_undo_len: usize,
        last_seen_seq: i64,
    },
    /// Cross-instance sync result.
    SyncResult { result: SyncResultKind },
    /// Another instance touched the notify dir for a file we have open.
    NotifyEvent { file_path_hash: String },
    /// Workspace tree changed (watcher event — paths that were created/removed).
    WorkspaceChanged { paths: Vec<CanonPath> },
    /// Git internal state changed (external git command detected).
    GitChanged,
    /// Notify watcher is ready (for cross-instance sync tests).
    WatchersReady,
}

#[derive(Clone, Debug)]
pub enum SyncResultKind {
    /// Apply a batch of remote entries. The buffer must validate
    /// `chain_id` and `content_hash` against its own state before
    /// applying — see `BufferState::try_apply_sync`. Covers both
    /// same-chain extension and chain-switch cases.
    SyncEntries {
        file_path: CanonPath,
        chain_id: String,
        content_hash: PersistedContentHash,
        entries: Vec<led_core::UndoEntry>,
        new_last_seen_seq: i64,
    },
    ExternalSave {
        file_path: CanonPath,
    },
    NoChange {
        file_path: CanonPath,
    },
}

// ── Session types ──

#[derive(Clone, Debug)]
pub struct SessionData {
    pub buffers: Vec<SessionBuffer>,
    pub active_tab_order: usize,
    pub show_side_panel: bool,
    pub kv: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct SessionBuffer {
    pub file_path: UserPath,
    pub tab_order: usize,
    pub cursor_row: Row,
    pub cursor_col: Col,
    pub scroll_row: Row,
    pub scroll_sub_line: SubLine,
    /// Undo restore data (loaded from DB during session restore).
    pub undo: Option<UndoRestoreData>,
}

#[derive(Clone, Debug)]
pub struct UndoRestoreData {
    pub chain_id: String,
    pub content_hash: PersistedContentHash,
    pub undo_cursor: Option<usize>,
    pub distance_from_save: i32,
    pub entries: Vec<led_core::UndoEntry>,
    pub last_seen_seq: i64,
}

#[derive(Clone, Debug)]
pub struct RestoredSession {
    pub buffers: Vec<SessionBuffer>,
    pub active_tab_order: usize,
    pub show_side_panel: bool,
    pub kv: HashMap<String, String>,
}

// ── Driver ──

/// Start the workspace driver. Takes a stream of commands,
/// returns a stream of events.
pub fn driver(out: Stream<WorkspaceOut>, file_watcher: Arc<FileWatcher>) -> Stream<WorkspaceIn> {
    let stream: Stream<WorkspaceIn> = Stream::new();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<WorkspaceOut>(64);
    let (result_tx, mut result_rx) = mpsc::channel::<WorkspaceIn>(64);

    // Bridge out: rx::Stream → channel
    out.on(move |opt: Option<&WorkspaceOut>| {
        if let Some(cmd) = opt {
            cmd_tx.try_send(cmd.clone()).ok();
        }
    });

    // Async task: compute workspace, manage DB, dispatch watcher events
    tokio::spawn(async move {
        // Channels for watcher events — senders are moved into registrations
        // on Init.  Before Init the receivers block (sender exists, no events).
        let (root_watch_tx, mut root_watch_rx) = mpsc::channel::<WatchEvent>(128);
        let (notify_watch_tx, mut notify_watch_rx) = mpsc::channel::<WatchEvent>(128);
        let mut root_sender: Option<mpsc::Sender<WatchEvent>> = Some(root_watch_tx);
        let mut notify_sender: Option<mpsc::Sender<WatchEvent>> = Some(notify_watch_tx);

        let mut _root_reg: Option<Registration> = None;
        let mut _notify_reg: Option<Registration> = None;
        let mut pending_notify: HashMap<String, Instant> = HashMap::new();
        let mut current: Option<Workspace> = None;
        let mut root_str = String::new();
        let mut _db: Option<rusqlite::Connection> = None;
        let mut _lock_file: Option<File> = None;

        loop {
            tokio::select! {
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        WorkspaceOut::SaveSession { data } => {
                            if let Some(ref conn) = _db {
                                if let Err(e) = db::save_session(conn, &root_str, &data) {
                                    log::warn!("failed to save session: {e}");
                                }
                            }
                            let _ = result_tx.send(WorkspaceIn::SessionSaved).await;
                        }
                        WorkspaceOut::Init { startup } => {
                            let dir = CanonPath::clone(&startup.start_dir);

                            let root = find_git_root(&dir);
                            let user_root = root.to_user_path(
                                &startup.start_dir,
                                &startup.user_start_dir,
                            );
                            let config = UserPath::clone(&startup.config_dir);

                            let primary = match try_become_primary(&config, &root) {
                                Some(file) => {
                                    _lock_file = Some(file);
                                    true
                                }
                                None => false,
                            };

                            root_str = root.to_string_lossy().into_owned();
                            let workspace = Workspace { root: root.clone(), user_root, config: config.clone(), primary };

                            current = Some(workspace.clone());
                            if result_tx.send(WorkspaceIn::Workspace { workspace }).await.is_err() {
                                break;
                            }

                            // Open DB and load session + undo state
                            let session = match db::open_db(config.as_path()) {
                                Ok(conn) => {
                                    let mut session = if primary {
                                        db::load_session(&conn, &root_str).ok().flatten()
                                    } else {
                                        None
                                    };
                                    // Load undo data per buffer
                                    if let Some(ref mut s) = session {
                                        for buf in &mut s.buffers {
                                            let path_str = buf.file_path.to_string_lossy();
                                            if let Ok(Some(state)) = db::load_undo_all(&conn, &root_str, &path_str) {
                                                buf.undo = Some(UndoRestoreData {
                                                    chain_id: state.chain_id,
                                                    content_hash: state.content_hash,
                                                    undo_cursor: state.undo_cursor,
                                                    distance_from_save: state.distance_from_save,
                                                    entries: state.entries,
                                                    last_seen_seq: state.last_seq,
                                                });
                                            }
                                        }
                                    }
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

                            let notify_dir = config.join("notify");
                            std::fs::create_dir_all(notify_dir.as_path()).ok();

                            // Register watchers with the shared FileWatcher
                            // (inert watcher silently accepts but never delivers)
                            if let Some(tx) = root_sender.take() {
                                _root_reg = Some(file_watcher.register(
                                    &root,
                                    WatchMode::Recursive,
                                    tx,
                                ));
                            }
                            if let Some(tx) = notify_sender.take() {
                                let notify_canon = notify_dir.canonicalize();
                                _notify_reg = Some(file_watcher.register(
                                    &notify_canon,
                                    WatchMode::NonRecursive,
                                    tx,
                                ));
                            }
                            let _ = result_tx.send(WorkspaceIn::WatchersReady).await;
                        }
                        WorkspaceOut::FlushUndo {
                            file_path,
                            chain_id,
                            content_hash,
                            undo_cursor,
                            distance_from_save,
                            entries,
                        } => {
                            if let Some(ref conn) = _db {
                                let path_str = file_path.to_string_lossy();
                                match db::flush_undo(
                                    conn,
                                    &root_str,
                                    &path_str,
                                    &chain_id,
                                    content_hash,
                                    undo_cursor,
                                    distance_from_save,
                                    &entries,
                                ) {
                                    Ok(last_seq) => {
                                        // Touch notify to wake other instances
                                        let hash = path_hash(&file_path);
                                        touch_notify_file(
                                            current.as_ref().map(|w| &w.config),
                                            &hash,
                                        );
                                        let _ = result_tx
                                            .send(WorkspaceIn::UndoFlushed {
                                                file_path,
                                                chain_id,
                                                persisted_undo_len: undo_cursor,
                                                last_seen_seq: last_seq,
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        log::warn!("failed to flush undo: {e}");
                                    }
                                }
                            }
                        }
                        WorkspaceOut::ClearUndo { file_path } => {
                            if let Some(ref conn) = _db {
                                let path_str = file_path.to_string_lossy();
                                if let Err(e) = db::clear_undo(conn, &root_str, &path_str) {
                                    log::warn!("failed to clear undo: {e}");
                                }
                                // Touch notify to wake other instances
                                let hash = path_hash(&file_path);
                                touch_notify_file(
                                    current.as_ref().map(|w| &w.config),
                                    &hash,
                                );
                            }
                        }
                        WorkspaceOut::CheckSync {
                            file_path,
                            last_seen_seq,
                            current_chain_id,
                        } => {
                            if let Some(ref conn) = _db {
                                let path_str = file_path.to_string_lossy();
                                let result = match db::load_undo_after(conn, &root_str, &path_str, last_seen_seq) {
                                    Ok(Some(state)) => {
                                        let same_chain = current_chain_id
                                            .as_ref()
                                            .is_some_and(|c| c == &state.chain_id);
                                        if state.entries.is_empty() && same_chain {
                                            SyncResultKind::NoChange { file_path }
                                        } else {
                                            SyncResultKind::SyncEntries {
                                                file_path,
                                                chain_id: state.chain_id,
                                                content_hash: state.content_hash,
                                                entries: state.entries,
                                                new_last_seen_seq: state.last_seq,
                                            }
                                        }
                                    }
                                    Ok(None) => SyncResultKind::ExternalSave { file_path },
                                    Err(e) => {
                                        log::warn!("failed to check sync: {e}");
                                        SyncResultKind::NoChange { file_path }
                                    }
                                };
                                let _ = result_tx
                                    .send(WorkspaceIn::SyncResult { result })
                                    .await;
                            }
                        }
                    }
                }
                Some(ev) = root_watch_rx.recv() => {
                    let is_git_internal = ev.paths.iter().all(|p| {
                        p.as_path().components().any(|c| c.as_os_str() == ".git")
                    });

                    if is_git_internal {
                        if current.is_some() && ev.paths.iter().any(|p| is_git_sentinel(p)) {
                            let _ = result_tx.send(WorkspaceIn::GitChanged).await;
                        }
                        continue;
                    }

                    match ev.kind {
                        WatchEventKind::Create | WatchEventKind::Remove => {
                            if current.is_some() {
                                if result_tx.send(WorkspaceIn::WorkspaceChanged {
                                    paths: ev.paths,
                                }).await.is_err() {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some(ev) = notify_watch_rx.recv() => {
                    match ev.kind {
                        WatchEventKind::Create | WatchEventKind::Modify => {
                            for path in &ev.paths {
                                if let Some(name) = path.file_name() {
                                    pending_notify.insert(
                                        name.to_string_lossy().into_owned(),
                                        Instant::now(),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    let now = Instant::now();
                    let quiet = std::time::Duration::from_millis(100);
                    let ready: Vec<String> = pending_notify
                        .iter()
                        .filter(|(_, t)| now.duration_since(**t) >= quiet)
                        .map(|(h, _)| h.clone())
                        .collect();
                    for hash in ready {
                        pending_notify.remove(&hash);
                        let _ = result_tx
                            .send(WorkspaceIn::NotifyEvent { file_path_hash: hash })
                            .await;
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

fn find_git_root(start: &CanonPath) -> CanonPath {
    let mut dir = start.as_path().to_path_buf();
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
    let result = root.unwrap_or_else(|| start.as_path().to_path_buf());
    UserPath::new(result).canonicalize()
}

fn try_become_primary(config: &UserPath, root: &CanonPath) -> Option<File> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::io::AsRawFd;

    let lock_dir = config.as_path().join(PRIMARY_DIR);
    std::fs::create_dir_all(&lock_dir).ok()?;

    let mut hasher = DefaultHasher::new();
    root.as_path().hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_dir.join(&hash))
        .ok()?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 { Some(file) } else { None }
}

/// Generate a unique chain_id for undo persistence sessions.
pub fn new_chain_id() -> String {
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = DefaultHasher::new();
    t.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn path_hash(path: &CanonPath) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.as_path().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn is_git_sentinel(path: &CanonPath) -> bool {
    let mut saw_dot_git = false;
    for component in path.as_path().components() {
        let name = component.as_os_str();
        if name == ".git" {
            saw_dot_git = true;
        } else if saw_dot_git {
            // .git/index or .git/HEAD
            if name == "index" || name == "HEAD" {
                return true;
            }
            // .git/refs/...
            if name == "refs" {
                return true;
            }
            return false;
        }
    }
    false
}

fn touch_notify_file(config: Option<&UserPath>, hash: &str) {
    let Some(config) = config else { return };
    let notify_dir = config.as_path().join("notify");
    let path = notify_dir.join(hash);
    std::fs::write(&path, b"").ok();
}
