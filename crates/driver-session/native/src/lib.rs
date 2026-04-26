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

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::{CanonPath, Notifier, PersistedContentHash, SubLine, UserPath};
use led_driver_session_core::{
    SessionCmd, SessionDriver, SessionEvent, Trace,
};
use led_state_buffer_edits::EditGroup;
use led_state_session::{SessionBuffer, SessionData, UndoRestoreData};
use led_state_tabs::{Cursor, Scroll};
use rusqlite::{Connection, params};

const SCHEMA_VERSION: i64 = 3;

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
                        chain_id: &chain_id,
                        content_hash,
                        undo_cursor,
                        distance_from_save,
                        entries: &entries,
                    },
                ) {
                    Ok(last_seq) => {
                        let _ = tx.send(SessionEvent::UndoFlushed {
                            path,
                            chain_id,
                            persisted_undo_len: undo_cursor,
                            last_seq,
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
            }
            SessionCmd::Shutdown => {
                drop(workspace.take());
                return;
            }
        }
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

// ── Schema (legacy-exact) ────────────────────────────────────

fn run_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 =
        conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        // Legacy's drop list, including pre-v3 tables.
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS undo_entries;
            DROP TABLE IF EXISTS buffer_undo_state;
            DROP TABLE IF EXISTS session_kv;
            DROP TABLE IF EXISTS buffers;
            DROP TABLE IF EXISTS workspaces;
            DROP TABLE IF EXISTS session_buffers;
            DROP TABLE IF EXISTS session_meta;
            DROP TABLE IF EXISTS undo_state;
            DROP TABLE IF EXISTS buffer_state;
            ",
        )?;
    }
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS workspaces (
            root_path       TEXT PRIMARY KEY,
            active_tab      INTEGER NOT NULL DEFAULT 0,
            show_side_panel INTEGER NOT NULL DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS buffers (
            root_path       TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
            file_path       TEXT NOT NULL,
            tab_order       INTEGER NOT NULL,
            cursor_row      INTEGER NOT NULL DEFAULT 0,
            cursor_col      INTEGER NOT NULL DEFAULT 0,
            scroll_row      INTEGER NOT NULL DEFAULT 0,
            scroll_sub_line INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (root_path, file_path)
        );

        CREATE TABLE IF NOT EXISTS session_kv (
            root_path   TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
            key         TEXT NOT NULL,
            value       TEXT NOT NULL,
            PRIMARY KEY (root_path, key)
        );

        CREATE TABLE IF NOT EXISTS buffer_undo_state (
            root_path          TEXT NOT NULL,
            file_path          TEXT NOT NULL,
            chain_id           TEXT NOT NULL,
            content_hash       INTEGER NOT NULL,
            undo_cursor        INTEGER,
            distance_from_save INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (root_path, file_path)
        );

        CREATE TABLE IF NOT EXISTS undo_entries (
            seq         INTEGER PRIMARY KEY AUTOINCREMENT,
            root_path   TEXT NOT NULL,
            file_path   TEXT NOT NULL,
            entry_data  BLOB NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_undo_entries_file
            ON undo_entries(root_path, file_path, seq);
        ",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
}

// ── Save / load (legacy-exact) ───────────────────────────────

fn save_session(
    conn: &Connection,
    root_path: &str,
    data: &SessionData,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;

    tx.prepare_cached(
        "INSERT INTO workspaces (root_path, active_tab, show_side_panel)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(root_path) DO UPDATE SET
            active_tab = excluded.active_tab,
            show_side_panel = excluded.show_side_panel",
    )?
    .execute(params![
        root_path,
        data.active_tab_order as i64,
        data.show_side_panel as i64
    ])?;

    tx.prepare_cached("DELETE FROM buffers WHERE root_path = ?1")?
        .execute(params![root_path])?;

    let mut stmt = tx.prepare_cached(
        "INSERT INTO buffers (root_path, file_path, tab_order, cursor_row, cursor_col, scroll_row, scroll_sub_line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for buf in &data.buffers {
        stmt.execute(params![
            root_path,
            buf.path.as_path().to_string_lossy(),
            buf.tab_order as i64,
            buf.cursor.line as i64,
            buf.cursor.col as i64,
            buf.scroll.top as i64,
            buf.scroll.top_sub_line.0 as i64,
        ])?;
    }
    drop(stmt);

    // Save KV pairs
    tx.prepare_cached("DELETE FROM session_kv WHERE root_path = ?1")?
        .execute(params![root_path])?;
    let mut kv_stmt = tx.prepare_cached(
        "INSERT INTO session_kv (root_path, key, value) VALUES (?1, ?2, ?3)",
    )?;
    for (k, v) in &data.kv {
        kv_stmt.execute(params![root_path, k, v])?;
    }
    drop(kv_stmt);

    // Clean up undo state for files no longer in the session.
    tx.prepare_cached(
        "DELETE FROM undo_entries WHERE root_path = ?1
           AND file_path NOT IN (SELECT file_path FROM buffers WHERE root_path = ?1)",
    )?
    .execute(params![root_path])?;
    tx.prepare_cached(
        "DELETE FROM buffer_undo_state WHERE root_path = ?1
           AND file_path NOT IN (SELECT file_path FROM buffers WHERE root_path = ?1)",
    )?
    .execute(params![root_path])?;

    tx.commit()
}

fn load_session(
    conn: &Connection,
    root_path: &str,
) -> rusqlite::Result<Option<SessionData>> {
    let workspace = conn
        .prepare_cached(
            "SELECT active_tab, show_side_panel FROM workspaces WHERE root_path = ?1",
        )?
        .query_row(params![root_path], |row| {
            let active_tab: i64 = row.get(0)?;
            let show_side_panel: i64 = row.get(1)?;
            Ok((active_tab as usize, show_side_panel != 0))
        });
    let (active_tab_order, show_side_panel) = match workspace {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut stmt = conn.prepare_cached(
        "SELECT file_path, tab_order, cursor_row, cursor_col, scroll_row, scroll_sub_line
         FROM buffers WHERE root_path = ?1 ORDER BY tab_order",
    )?;
    let mut buffers: Vec<SessionBuffer> = stmt
        .query_map(params![root_path], |row| {
            let path_str: String = row.get(0)?;
            Ok(SessionBuffer {
                path: UserPath::new(std::path::Path::new(&path_str)).canonicalize(),
                tab_order: row.get::<_, i64>(1)? as usize,
                cursor: Cursor {
                    line: row.get::<_, i64>(2)? as usize,
                    col: row.get::<_, i64>(3)? as usize,
                    preferred_col: row.get::<_, i64>(3)? as usize,
                },
                scroll: Scroll {
                    top: row.get::<_, i64>(4)? as usize,
                    top_sub_line: SubLine(row.get::<_, i64>(5)? as usize),
                },
                undo: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    if buffers.is_empty() {
        return Ok(None);
    }

    // Per-buffer undo restore: walk the buffer list and look up
    // their persisted state. Mirrors the legacy lib.rs flow that
    // populates `SessionBuffer::undo` in the workspace driver
    // (legacy lib.rs:246-260).
    for buf in &mut buffers {
        let path_str = buf.path.as_path().to_string_lossy().into_owned();
        if let Some(state) = load_undo_all(conn, root_path, &path_str)? {
            buf.undo = Some(state);
        }
    }

    let mut kv_stmt =
        conn.prepare_cached("SELECT key, value FROM session_kv WHERE root_path = ?1")?;
    let kv: HashMap<String, String> = kv_stmt
        .query_map(params![root_path], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Some(SessionData {
        buffers,
        active_tab_order,
        show_side_panel,
        kv,
    }))
}

// ── Undo flush / clear / load (legacy-exact structure) ───────

/// Bundle of SQL params + entries slice for [`flush_undo`].
/// Carved out so the helper takes a small `&FlushUndoArgs<'_>`
/// instead of an 8-positional-arg list.
struct FlushUndoArgs<'a> {
    root_path: &'a str,
    file_path: &'a str,
    chain_id: &'a str,
    content_hash: PersistedContentHash,
    undo_cursor: usize,
    distance_from_save: i32,
    entries: &'a [EditGroup],
}

fn flush_undo(conn: &Connection, args: &FlushUndoArgs<'_>) -> rusqlite::Result<i64> {
    let tx = conn.unchecked_transaction()?;

    tx.prepare_cached(
        "INSERT OR REPLACE INTO buffer_undo_state
         (root_path, file_path, chain_id, content_hash, undo_cursor, distance_from_save)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        args.root_path,
        args.file_path,
        args.chain_id,
        args.content_hash.0 as i64,
        args.undo_cursor as i64,
        args.distance_from_save,
    ])?;

    let mut stmt = tx.prepare_cached(
        "INSERT INTO undo_entries (root_path, file_path, entry_data) VALUES (?1, ?2, ?3)",
    )?;
    for entry in args.entries {
        let bytes = rmp_serde::to_vec(entry).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })?;
        stmt.execute(params![args.root_path, args.file_path, bytes])?;
    }
    drop(stmt);

    let last_seq: i64 = tx
        .prepare_cached(
            "SELECT COALESCE(MAX(seq), 0) FROM undo_entries
             WHERE root_path = ?1 AND file_path = ?2",
        )?
        .query_row(params![args.root_path, args.file_path], |row| row.get(0))?;

    tx.commit()?;
    Ok(last_seq)
}

fn clear_undo(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "DELETE FROM undo_entries WHERE root_path = ?1 AND file_path = ?2",
    )?
    .execute(params![root_path, file_path])?;
    conn.prepare_cached(
        "DELETE FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
    )?
    .execute(params![root_path, file_path])?;
    Ok(())
}

fn load_undo_all(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
) -> rusqlite::Result<Option<UndoRestoreData>> {
    let state = conn
        .prepare_cached(
            "SELECT chain_id, content_hash, undo_cursor, distance_from_save
             FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
        )?
        .query_row(params![root_path, file_path], |row| {
            Ok(UndoRestoreData {
                chain_id: row.get(0)?,
                content_hash: PersistedContentHash(row.get::<_, i64>(1)? as u64),
                undo_cursor: row.get::<_, Option<i64>>(2)?.map(|v| v as usize),
                distance_from_save: row.get(3)?,
                entries: Vec::new(),
                last_seq: 0,
            })
        });
    let mut restore = match state {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut stmt = conn.prepare_cached(
        "SELECT seq, entry_data FROM undo_entries
         WHERE root_path = ?1 AND file_path = ?2
         ORDER BY seq",
    )?;
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map(params![root_path, file_path], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    restore.last_seq = rows.last().map(|(seq, _)| *seq).unwrap_or(0);
    restore.entries = rows
        .into_iter()
        .filter_map(|(_, data)| rmp_serde::from_slice::<EditGroup>(&data).ok())
        .collect();
    Ok(Some(restore))
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_state_buffer_edits::EditOp;
    use led_state_tabs::Cursor as TabCursor;
    use std::path::Path;
    use std::sync::Arc as StdArc;
    use std::time::{Duration, Instant};

    struct NoopTrace;
    impl Trace for NoopTrace {
        fn session_init_start(&self, _: &CanonPath) {}
        fn session_save_start(&self) {}
        fn session_save_done(&self, _: bool) {}
        fn session_drop_undo(&self, _: &CanonPath) {}
        fn session_flush_undo(&self, _: &CanonPath, _: &str) {}
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
            seq: 1,
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
            chain_id: "chain-1".into(),
            content_hash: PersistedContentHash(0xDEADBEEF),
            undo_cursor: 2,
            distance_from_save: 1,
            entries: vec![group(0, "hello"), group(5, " world")],
        }]);
        let ev = drain_one(&drv, Duration::from_secs(5)).expect("flushed");
        let SessionEvent::UndoFlushed { last_seq, .. } = ev else {
            panic!("unexpected: {ev:?}");
        };
        assert!(last_seq > 0);

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
        assert_eq!(undo.chain_id, "chain-1");
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
