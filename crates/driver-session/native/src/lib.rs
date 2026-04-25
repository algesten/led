//! Desktop SQLite worker for the session driver (M21).
//!
//! Single `std::thread` consuming `SessionCmd` off an mpsc inbox
//! and posting `SessionEvent`s back. SQLite via `rusqlite`
//! (bundled). Per-workspace primary flock via `libc::flock` on
//! `<config_dir>/primary/<hash(root)>`.
//!
//! Schema (M21 baseline — schema_version = 1):
//!
//! ```sql
//! CREATE TABLE workspaces (
//!     root_path       TEXT PRIMARY KEY,
//!     active_tab      INTEGER NOT NULL DEFAULT 0,
//!     show_side_panel INTEGER NOT NULL DEFAULT 1
//! );
//! CREATE TABLE buffers (
//!     root_path       TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
//!     file_path       TEXT NOT NULL,
//!     tab_order       INTEGER NOT NULL,
//!     cursor_row      INTEGER NOT NULL DEFAULT 0,
//!     cursor_col      INTEGER NOT NULL DEFAULT 0,
//!     scroll_row      INTEGER NOT NULL DEFAULT 0,
//!     scroll_sub_line INTEGER NOT NULL DEFAULT 0,
//!     PRIMARY KEY (root_path, file_path)
//! );
//! ```
//!
//! Migrations: `user_version` mismatch wipes both tables and
//! recreates. Same drop-on-mismatch policy legacy follows.

use std::collections::hash_map::DefaultHasher;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use led_core::{CanonPath, Notifier, SubLine, UserPath};
use led_driver_session_core::{
    SessionCmd, SessionDriver, SessionEvent, Trace,
};
use led_state_session::{SessionData, SessionTab};
use led_state_tabs::{Cursor, Scroll};
use rusqlite::{Connection, params};

const SCHEMA_VERSION: i64 = 1;

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

/// Long-lived per-workspace state held by the worker between
/// `Init` and `Shutdown`. `Connection` is the SQLite handle;
/// `_flock` keeps the OS lock alive (drop releases it).
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
                let result = init_workspace(&root, &config_dir);
                match result {
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
            SessionCmd::Save { data } => {
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
                    let _ = tx.send(SessionEvent::Saved);
                    notify.notify();
                    continue;
                }
                match save_session(&ws.conn, &ws.root_path, &data) {
                    Ok(()) => {
                        let _ = tx.send(SessionEvent::Saved);
                    }
                    Err(e) => {
                        let _ = tx.send(SessionEvent::Failed {
                            message: e.to_string(),
                        });
                    }
                }
                notify.notify();
            }
            SessionCmd::Shutdown => {
                drop(workspace.take()); // drops conn + flock
                return;
            }
        }
    }
}

fn init_workspace(
    root: &CanonPath,
    config_dir: &CanonPath,
) -> Result<(Workspace, Option<SessionData>), String> {
    let root_path = root.as_path().to_string_lossy().into_owned();
    let cfg = config_dir.as_path();
    std::fs::create_dir_all(cfg).map_err(|e| e.to_string())?;

    // Acquire (or fail to acquire) the primary flock.
    let flock = try_acquire_primary_flock(cfg, root)?;
    let primary = flock.is_some();

    // Open the DB regardless — secondaries still read for
    // possible future cross-instance sync (M26).
    let db_path = cfg.join("db.sqlite");
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .map_err(|e| e.to_string())?;
    run_schema(&conn).map_err(|e| e.to_string())?;

    // Only primaries restore — secondaries get a clean session
    // so two windows don't race on the same row.
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
    // SAFETY: `flock` is a thin POSIX syscall; we pass a valid
    // fd from the freshly-opened file. The `LOCK_NB` flag means
    // we never block the worker thread.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(file))
    } else {
        // Another led owns the workspace. Drop the file —
        // releasing it won't release the other process's lock.
        Ok(None)
    }
}

fn run_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 =
        conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS buffers;
            DROP TABLE IF EXISTS workspaces;
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
        ",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
}

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
        data.active_tab_idx.unwrap_or(0) as i64,
        data.show_side_panel as i64,
    ])?;

    tx.prepare_cached("DELETE FROM buffers WHERE root_path = ?1")?
        .execute(params![root_path])?;

    let mut stmt = tx.prepare_cached(
        "INSERT INTO buffers
            (root_path, file_path, tab_order, cursor_row, cursor_col,
             scroll_row, scroll_sub_line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for (idx, tab) in data.tabs.iter().enumerate() {
        stmt.execute(params![
            root_path,
            tab.path.as_path().to_string_lossy(),
            idx as i64,
            tab.cursor.line as i64,
            tab.cursor.col as i64,
            tab.scroll.top as i64,
            tab.scroll.top_sub_line.0 as i64,
        ])?;
    }
    drop(stmt);

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
            Ok((active_tab, show_side_panel))
        });
    let (active_tab, show_side_panel) = match workspace {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut stmt = conn.prepare_cached(
        "SELECT file_path, cursor_row, cursor_col, scroll_row, scroll_sub_line
         FROM buffers
         WHERE root_path = ?1
         ORDER BY tab_order ASC",
    )?;
    let rows = stmt.query_map(params![root_path], |row| {
        let path_str: String = row.get(0)?;
        let cursor_row: i64 = row.get(1)?;
        let cursor_col: i64 = row.get(2)?;
        let scroll_row: i64 = row.get(3)?;
        let scroll_sub: i64 = row.get(4)?;
        let canon = UserPath::new(std::path::Path::new(&path_str)).canonicalize();
        Ok(SessionTab {
            path: canon,
            cursor: Cursor {
                line: cursor_row.max(0) as usize,
                col: cursor_col.max(0) as usize,
                preferred_col: cursor_col.max(0) as usize,
            },
            scroll: Scroll {
                top: scroll_row.max(0) as usize,
                top_sub_line: SubLine(scroll_sub.max(0) as usize),
            },
        })
    })?;

    let mut tabs: Vec<SessionTab> = Vec::new();
    for row in rows {
        tabs.push(row?);
    }
    drop(stmt);

    let active_tab_idx = if tabs.is_empty() {
        None
    } else {
        Some((active_tab.max(0) as usize).min(tabs.len().saturating_sub(1)))
    };
    Ok(Some(SessionData {
        active_tab_idx,
        show_side_panel: show_side_panel != 0,
        tabs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct NoopTrace;
    impl Trace for NoopTrace {
        fn session_init_start(&self, _: &CanonPath) {}
        fn session_save_start(&self) {}
        fn session_save_done(&self, _: bool) {}
    }

    fn canon_of(p: &std::path::Path) -> CanonPath {
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

    #[test]
    fn fresh_workspace_returns_no_restored_data() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");
        let (drv, _native) = spawn(Arc::new(NoopTrace), Notifier::noop());
        drv.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev = drain_one(&drv, Duration::from_secs(5)).expect("Init replied");
        match ev {
            SessionEvent::Restored { primary, restored } => {
                assert!(primary, "first instance should be primary");
                assert!(restored.is_none(), "fresh DB has no session");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn save_then_init_round_trips_session() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");

        let target = SessionData {
            active_tab_idx: Some(1),
            show_side_panel: true,
            tabs: vec![
                SessionTab {
                    path: canon_of(&root.join("a.rs")),
                    cursor: Cursor {
                        line: 10,
                        col: 5,
                        preferred_col: 5,
                    },
                    scroll: Scroll {
                        top: 4,
                        top_sub_line: SubLine(0),
                    },
                },
                SessionTab {
                    path: canon_of(&root.join("b.rs")),
                    cursor: Cursor::default(),
                    scroll: Scroll::default(),
                },
            ],
        };

        // First spawn: Init, Save, Shutdown.
        {
            let (drv, _native) = spawn(Arc::new(NoopTrace), Notifier::noop());
            drv.execute([&SessionCmd::Init {
                root: canon_of(&root),
                config_dir: canon_of(&cfg),
            }]);
            drain_one(&drv, Duration::from_secs(5)).expect("init");
            drv.execute([&SessionCmd::Save {
                data: target.clone(),
            }]);
            let saved =
                drain_one(&drv, Duration::from_secs(5)).expect("save");
            matches!(saved, SessionEvent::Saved);
            drv.execute([&SessionCmd::Shutdown]);
        }

        // Give the worker a moment to drop the flock.
        std::thread::sleep(Duration::from_millis(50));

        // Second spawn: Init should restore exactly.
        let (drv, _native) = spawn(Arc::new(NoopTrace), Notifier::noop());
        drv.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev = drain_one(&drv, Duration::from_secs(5)).expect("re-init");
        match ev {
            SessionEvent::Restored { primary, restored } => {
                assert!(primary);
                let r = restored.expect("session restored");
                assert_eq!(r.active_tab_idx, target.active_tab_idx);
                assert_eq!(r.show_side_panel, target.show_side_panel);
                assert_eq!(r.tabs.len(), target.tabs.len());
                assert_eq!(r.tabs[0].cursor, target.tabs[0].cursor);
                assert_eq!(r.tabs[0].scroll, target.tabs[0].scroll);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn second_instance_runs_as_secondary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = tmp.path().join("config");

        // First instance: keep alive (don't Shutdown) so the
        // flock stays held while we open a second.
        let (drv1, _n1) = spawn(Arc::new(NoopTrace), Notifier::noop());
        drv1.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev1 = drain_one(&drv1, Duration::from_secs(5)).expect("first init");
        match ev1 {
            SessionEvent::Restored { primary, .. } => assert!(primary),
            other => panic!("unexpected: {other:?}"),
        }

        // Second instance.
        let (drv2, _n2) = spawn(Arc::new(NoopTrace), Notifier::noop());
        drv2.execute([&SessionCmd::Init {
            root: canon_of(&root),
            config_dir: canon_of(&cfg),
        }]);
        let ev2 = drain_one(&drv2, Duration::from_secs(5)).expect("second init");
        match ev2 {
            SessionEvent::Restored { primary, restored } => {
                assert!(!primary, "second instance must not be primary");
                assert!(
                    restored.is_none(),
                    "secondary should not restore session",
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
