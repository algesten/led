//! Desktop SQLite worker for the session driver (M21).
//!
//! Schema, save / load, undo flush, undo clear all match legacy
//! `led/crates/workspace/src/db.rs` (832 LOC) verbatim — same
//! `SCHEMA_VERSION = 3`, same five tables + index, same SQL.
//! The per-row `entry_data` BLOB is the one place we diverge:
//! legacy stores rmp-serde of its single-op `UndoEntry`, we
//! store rmp-serde of our multi-op `EditGroup`. Storage shape
//! identical, payload shape ours; cross-binary DB compat is
//! intentionally out of scope.

use std::collections::hash_map::DefaultHasher;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::{CanonPath, Notifier, UndoDbSeq};
use led_driver_session_core::{
    SessionCmd, SessionDriver, SessionEvent, SyncResultKind, Trace,
};
use led_state_session::SessionData;
use rusqlite::Connection;

mod save_load;
mod schema;
mod sync;
mod undo;

use save_load::{load_session, save_session};
use schema::run_schema;
use sync::check_sync;
use undo::{FlushUndoArgs, clear_undo, flush_undo};

pub(crate) const SCHEMA_VERSION: i64 = 3;

pub struct SessionNative {
    _marker: (),
}

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (SessionDriver, SessionNative) {
    let (tx_cmd, rx_cmd) = mpsc::channel::<SessionCmd>();
    let (tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
    let native = spawn_worker(rx_cmd, tx_ev, notify);
    let driver = SessionDriver::new(tx_cmd, rx_ev, trace);
    (driver, native)
}

pub fn spawn_worker(
    rx_cmd: Receiver<SessionCmd>,
    tx_ev: Sender<SessionEvent>,
    notify: Notifier,
) -> SessionNative {
    thread::Builder::new()
        .name("led-session".into())
        .spawn(move || worker_loop(rx_cmd, tx_ev, notify))
        .expect("spawning session worker should succeed");
    SessionNative { _marker: () }
}

struct Workspace {
    conn: Connection,
    root_path: String,
    primary: bool,
    /// Cached `<config_dir>/notify/` directory. Created on init
    /// so the FlushUndo / ClearUndo arms can `std::fs::write` an
    /// empty touch file at `<notify_dir>/<path_hash>` without
    /// re-checking dir existence on every call.
    notify_dir: std::path::PathBuf,
    _flock: Option<File>,
}

fn worker_loop(rx: Receiver<SessionCmd>, tx: Sender<SessionEvent>, notify: Notifier) {
    let mut workspace: Option<Workspace> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            SessionCmd::Init { root, config_dir } => {
                match init_workspace(&root, &config_dir) {
                    Ok((ws, restored)) => {
                        let primary = ws.primary;
                        workspace = Some(ws);
                        let _ = tx.send(SessionEvent::Restored {
                            primary,
                            restored,
                        });
                    }
                    Err(msg) => {
                        let _ = tx.send(SessionEvent::Failed { message: msg });
                    }
                }
                notify.notify();
            }
            SessionCmd::SaveSession { data } => {
                let Some(ws) = workspace.as_ref() else {
                    let _ = tx.send(SessionEvent::Failed {
                        message: "session not initialised".into(),
                    });
                    notify.notify();
                    continue;
                };
                if !ws.primary {
                    // Secondaries don't write — but report success
                    // so the Quit gate can still clear.
                    let _ = tx.send(SessionEvent::SessionSaved);
                    notify.notify();
                    continue;
                }
                match save_session(&ws.conn, &ws.root_path, &data) {
                    Ok(()) => {
                        let _ = tx.send(SessionEvent::SessionSaved);
                    }
                    Err(e) => {
                        let _ = tx.send(SessionEvent::Failed {
                            message: e.to_string(),
                        });
                    }
                }
                notify.notify();
            }
            SessionCmd::FlushUndo {
                path,
                chain_id,
                content_hash,
                undo_cursor,
                distance_from_save,
                entries,
            } => {
                let Some(ws) = workspace.as_ref() else {
                    continue;
                };
                if !ws.primary {
                    continue;
                }
                let path_str = path.as_path().to_string_lossy().into_owned();
                match flush_undo(
                    &ws.conn,
                    &FlushUndoArgs {
                        root_path: &ws.root_path,
                        file_path: &path_str,
                        chain_id: chain_id.as_str(),
                        content_hash,
                        undo_cursor,
                        distance_from_save,
                        entries: &entries,
                    },
                ) {
                    Ok(last_seq) => {
                        // M26: notify peers about the new undo
                        // entries before sending the success
                        // event. Touch happens unconditionally
                        // for primaries — secondaries already
                        // skipped above.
                        touch_notify_file(&ws.notify_dir, &path);
                        let _ = tx.send(SessionEvent::UndoFlushed {
                            path,
                            chain_id,
                            persisted_undo_len: undo_cursor,
                            last_seq: UndoDbSeq(last_seq),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(SessionEvent::Failed {
                            message: format!("flush_undo: {e}"),
                        });
                    }
                }
                notify.notify();
            }
            SessionCmd::ClearUndo { path } => {
                let Some(ws) = workspace.as_ref() else {
                    continue;
                };
                if !ws.primary {
                    continue;
                }
                let path_str = path.as_path().to_string_lossy().into_owned();
                let _ = clear_undo(&ws.conn, &ws.root_path, &path_str);
                // M26: peers see this clear via the notify-dir
                // watch; without the touch a peer with the same
                // file open would never reload after this
                // primary's save.
                touch_notify_file(&ws.notify_dir, &path);
            }
            SessionCmd::CheckSync {
                path,
                last_seen_seq,
                current_chain_id,
            } => {
                let Some(ws) = workspace.as_ref() else {
                    continue;
                };
                let path_str = path.as_path().to_string_lossy().into_owned();
                let kind = match check_sync(
                    &ws.conn,
                    &ws.root_path,
                    &path_str,
                    last_seen_seq.0,
                    current_chain_id.as_str(),
                ) {
                    Ok(kind) => kind,
                    Err(e) => {
                        let _ = tx.send(SessionEvent::Failed {
                            message: format!("check_sync: {e}"),
                        });
                        notify.notify();
                        continue;
                    }
                };
                // Reattach the path to the result variant so the
                // runtime can route by buffer.
                let kind = attach_sync_path(kind, path);
                let _ = tx.send(SessionEvent::SyncResult { kind });
                notify.notify();
            }
            SessionCmd::Shutdown => {
                drop(workspace.take());
                return;
            }
        }
    }
}

/// Touch `<notify_dir>/<path_hash>` so a peer's notify-dir
/// watcher fires `Modify`. Empty file is enough; the watcher
/// only cares about the inode mtime change.
fn touch_notify_file(notify_dir: &std::path::Path, path: &CanonPath) {
    let hash = path.path_hash();
    let _ = std::fs::write(notify_dir.join(hash), []);
}

/// Helper that re-stamps `path` onto whichever
/// [`SyncResultKind`] the SQL helper produced. The helper
/// returns `Path*` variants without the path so the SQL layer
/// stays cheap; this reattaches.
fn attach_sync_path(kind: SyncResultKind, path: CanonPath) -> SyncResultKind {
    match kind {
        SyncResultKind::SyncEntries {
            chain_id,
            content_hash,
            entries,
            new_last_seen_seq,
            ..
        } => SyncResultKind::SyncEntries {
            path,
            chain_id,
            content_hash,
            entries,
            new_last_seen_seq,
        },
        SyncResultKind::ExternalSave { .. } => SyncResultKind::ExternalSave { path },
        SyncResultKind::NoChange { .. } => SyncResultKind::NoChange { path },
    }
}

// ── Init / flock ─────────────────────────────────────────────

fn init_workspace(
    root: &CanonPath,
    config_dir: &CanonPath,
) -> Result<(Workspace, Option<SessionData>), String> {
    let root_path = root.as_path().to_string_lossy().into_owned();
    let cfg = config_dir.as_path();
    std::fs::create_dir_all(cfg).map_err(|e| e.to_string())?;
    let flock = try_acquire_primary_flock(cfg, root)?;
    let primary = flock.is_some();
    let db_path = cfg.join("db.sqlite");
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .map_err(|e| e.to_string())?;
    run_schema(&conn).map_err(|e| e.to_string())?;
    // M26: precreate the notify dir so FlushUndo / ClearUndo
    // touches cost a single fs::write.
    let notify_dir = cfg.join("notify");
    std::fs::create_dir_all(&notify_dir).map_err(|e| e.to_string())?;
    let restored = if primary {
        load_session(&conn, &root_path).map_err(|e| e.to_string())?
    } else {
        None
    };
    Ok((
        Workspace {
            conn,
            root_path,
            primary,
            notify_dir,
            _flock: flock,
        },
        restored,
    ))
}

fn try_acquire_primary_flock(
    config_dir: &std::path::Path,
    root: &CanonPath,
) -> Result<Option<File>, String> {
    let primary_dir = config_dir.join("primary");
    std::fs::create_dir_all(&primary_dir).map_err(|e| e.to_string())?;
    let mut hasher = DefaultHasher::new();
    root.as_path().hash(&mut hasher);
    let lock_name = format!("{:x}", hasher.finish());
    let lock_path = primary_dir.join(lock_name);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    // SAFETY: `flock` is async-signal-safe and takes a valid fd.
    // LOCK_NB ensures we never block the worker thread.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{ChainId, PersistedContentHash, SubLine, UserPath};
    use led_state_buffer_edits::{EditGroup, EditOp};
    use led_state_session::SessionBuffer;
    use led_state_tabs::{Cursor, Cursor as TabCursor, Scroll};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc as StdArc;
    use std::time::{Duration, Instant};

    struct NoopTrace;
    impl Trace for NoopTrace {
        fn session_init_start(&self, _: &CanonPath) {}
        fn session_save_start(&self) {}
        fn session_save_done(&self, _: bool) {}
        fn session_drop_undo(&self, _: &CanonPath) {}
        fn session_flush_undo(&self, _: &CanonPath, _: &ChainId) {}
        fn session_check_sync(&self, _: &CanonPath) {}
    }

    fn canon_of(p: &Path) -> CanonPath {
        UserPath::new(p).canonicalize()
    }

    fn drain_one(drv: &SessionDriver, deadline: Duration) -> Option<SessionEvent> {
        let start = Instant::now();
        while start.elapsed() < deadline {
            let mut batch = drv.process();
            if let Some(ev) = batch.pop() {
                return Some(ev);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn group(at: usize, text: &str) -> EditGroup {
        EditGroup {
            ops: vec![
                EditOp::Delete {
                    at,
                    text: StdArc::from(""),
                },
                EditOp::Insert {
                    at,
                    text: StdArc::from(text),
                },
            ],
            cursor_before: TabCursor::default(),
            cursor_after: TabCursor::default(),
            seq: led_core::EditSeq(1),
            file_search_mark: None,
            save_point_hash: None,
        }
    }

    #[test]
    fn fresh_workspace_returns_no_restored_data() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");
        let (drv, _native) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev = drain_one(&drv, Duration::from_secs(5)).expect("Init replied");
        match ev {
            SessionEvent::Restored { primary, restored } => {
                assert!(primary);
                assert!(restored.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn save_with_no_buffers_still_restores_kv() {
        // Repro: user opens led, navigates the file browser without
        // promoting any preview to a real tab, then quits. The save
        // writes a workspaces row + kv but no buffers row. The next
        // Init must still surface the kv (browser.expanded_dirs,
        // browser.selected_path, …) so the sidebar state survives.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");
        let mut kv = HashMap::new();
        kv.insert(
            "browser.selected_path".to_string(),
            "/some/path".to_string(),
        );
        let target = SessionData {
            active_tab_order: 0,
            show_side_panel: false,
            buffers: vec![],
            kv: kv.clone(),
        };
        {
            let (drv, _n) = spawn(StdArc::new(NoopTrace), Notifier::noop());
            drv.execute([&SessionCmd::Init {
                root: canon_of(&root),
                config_dir: canon_of(&cfg),
            }]);
            drain_one(&drv, Duration::from_secs(5)).expect("init");
            drv.execute([&SessionCmd::SaveSession {
                data: target.clone(),
            }]);
            drain_one(&drv, Duration::from_secs(5)).expect("save");
            drv.execute([&SessionCmd::Shutdown]);
        }
        std::thread::sleep(Duration::from_millis(50));

        let (drv, _n) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev = drain_one(&drv, Duration::from_secs(5)).expect("re-init");
        let SessionEvent::Restored { restored, .. } = ev else {
            panic!("unexpected event: {ev:?}");
        };
        let r = restored.expect("session restored even with empty buffers");
        assert!(r.buffers.is_empty());
        assert!(!r.show_side_panel);
        assert_eq!(r.kv, kv);
    }

    #[test]
    fn save_then_init_round_trips_session_with_kv() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");
        let mut kv = HashMap::new();
        kv.insert("browser.expanded_dirs".to_string(), "/a\n/b".to_string());
        kv.insert("jump_list.index".to_string(), "3".to_string());
        let target = SessionData {
            active_tab_order: 1,
            show_side_panel: true,
            buffers: vec![
                SessionBuffer {
                    path: canon_of(&root.join("a.rs")),
                    tab_order: 0,
                    cursor: Cursor {
                        line: 10,
                        col: 5,
                        preferred_col: 5,
                    },
                    scroll: Scroll {
                        top: 4,
                        top_sub_line: SubLine(0),
                    },
                    undo: None,
                },
                SessionBuffer {
                    path: canon_of(&root.join("b.rs")),
                    tab_order: 1,
                    cursor: Cursor::default(),
                    scroll: Scroll::default(),
                    undo: None,
                },
            ],
            kv: kv.clone(),
        };

        // First spawn: Init, SaveSession, Shutdown.
        {
            let (drv, _n) = spawn(StdArc::new(NoopTrace), Notifier::noop());
            drv.execute([&SessionCmd::Init {
                root: canon_of(&root),
                config_dir: canon_of(&cfg),
            }]);
            drain_one(&drv, Duration::from_secs(5)).expect("init");
            drv.execute([&SessionCmd::SaveSession {
                data: target.clone(),
            }]);
            drain_one(&drv, Duration::from_secs(5)).expect("save");
            drv.execute([&SessionCmd::Shutdown]);
        }
        std::thread::sleep(Duration::from_millis(50));

        // Second spawn: Init restores exactly.
        let (drv, _n) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev = drain_one(&drv, Duration::from_secs(5)).expect("re-init");
        let SessionEvent::Restored { primary, restored } = ev else {
            panic!("unexpected event: {ev:?}");
        };
        assert!(primary);
        let r = restored.expect("session restored");
        assert_eq!(r.active_tab_order, 1);
        assert!(r.show_side_panel);
        assert_eq!(r.buffers.len(), 2);
        assert_eq!(r.buffers[0].cursor, target.buffers[0].cursor);
        assert_eq!(r.buffers[0].scroll, target.buffers[0].scroll);
        assert_eq!(r.kv, kv);
    }

    #[test]
    fn flush_load_clear_undo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");
        let path = canon_of(&root.join("a.rs"));

        let (drv, _n) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        drain_one(&drv, Duration::from_secs(5)).expect("init");

        // Need a workspace+buffers row before FK-protected undo
        // rows can land. Save a session first.
        drv.execute([&SessionCmd::SaveSession {
            data: SessionData {
                active_tab_order: 0,
                show_side_panel: true,
                buffers: vec![SessionBuffer {
                    path: path.clone(),
                    tab_order: 0,
                    cursor: Cursor::default(),
                    scroll: Scroll::default(),
                    undo: None,
                }],
                kv: HashMap::new(),
            },
        }]);
        drain_one(&drv, Duration::from_secs(5)).expect("save");

        drv.execute([&SessionCmd::FlushUndo {
            path: path.clone(),
            chain_id: ChainId::new("chain-1"),
            content_hash: PersistedContentHash(0xDEADBEEF),
            undo_cursor: 2,
            distance_from_save: 1,
            entries: vec![group(0, "hello"), group(5, " world")],
        }]);
        let ev = drain_one(&drv, Duration::from_secs(5)).expect("flushed");
        let SessionEvent::UndoFlushed { last_seq, .. } = ev else {
            panic!("unexpected: {ev:?}");
        };
        assert!(last_seq.0 > 0);

        // Drop + re-init in a new spawn — the second instance
        // should restore the entries via load_session.
        drv.execute([&SessionCmd::Shutdown]);
        std::thread::sleep(Duration::from_millis(50));

        let (drv2, _n2) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv2.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev = drain_one(&drv2, Duration::from_secs(5)).expect("re-init");
        let SessionEvent::Restored { restored, .. } = ev else {
            panic!("unexpected: {ev:?}");
        };
        let r = restored.expect("session restored");
        let undo = r.buffers[0].undo.as_ref().expect("undo restored");
        assert_eq!(undo.chain_id.as_str(), "chain-1");
        assert_eq!(undo.content_hash, PersistedContentHash(0xDEADBEEF));
        assert_eq!(undo.undo_cursor, Some(2));
        assert_eq!(undo.distance_from_save, 1);
        assert_eq!(undo.entries.len(), 2);

        // ClearUndo wipes them.
        drv2.execute([&SessionCmd::ClearUndo { path: path.clone() }]);
        std::thread::sleep(Duration::from_millis(50));
        drv2.execute([&SessionCmd::Shutdown]);
        std::thread::sleep(Duration::from_millis(50));

        let (drv3, _n3) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv3.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev = drain_one(&drv3, Duration::from_secs(5)).expect("re-init");
        let SessionEvent::Restored { restored, .. } = ev else {
            panic!("unexpected: {ev:?}");
        };
        let r = restored.expect("session restored");
        assert!(
            r.buffers[0].undo.is_none(),
            "undo cleared after ClearUndo",
        );
    }

    #[test]
    fn second_instance_runs_as_secondary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");
        let (drv1, _n1) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv1.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev1 = drain_one(&drv1, Duration::from_secs(5)).expect("first");
        match ev1 {
            SessionEvent::Restored { primary, .. } => assert!(primary),
            other => panic!("unexpected: {other:?}"),
        }
        let (drv2, _n2) = spawn(StdArc::new(NoopTrace), Notifier::noop());
        drv2.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev2 = drain_one(&drv2, Duration::from_secs(5)).expect("second");
        match ev2 {
            SessionEvent::Restored { primary, restored } => {
                assert!(!primary);
                assert!(restored.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
