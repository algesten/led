use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, params};

use crate::{RestoredSession, SessionBuffer, SessionData};

/// Open (or create) the database at `config_dir/db.sqlite`.
pub fn open_db(config_dir: &Path) -> rusqlite::Result<Connection> {
    std::fs::create_dir_all(config_dir).ok();
    let path = config_dir.join("db.sqlite");
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
    run_schema(&conn)?;
    Ok(conn)
}

const SCHEMA_VERSION: i64 = 3;

fn run_schema(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if version != SCHEMA_VERSION {
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS undo_entries;
            DROP TABLE IF EXISTS buffer_undo_state;
            DROP TABLE IF EXISTS session_kv;
            DROP TABLE IF EXISTS buffers;
            DROP TABLE IF EXISTS workspaces;
            DROP TABLE IF EXISTS session_buffers;
            DROP TABLE IF EXISTS session_meta;
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

// ── Session ──

pub fn save_session(
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
            buf.file_path.to_string_lossy(),
            buf.tab_order as i64,
            buf.cursor_row as i64,
            buf.cursor_col as i64,
            buf.scroll_row as i64,
            buf.scroll_sub_line as i64,
        ])?;
    }
    drop(stmt);

    // Save KV pairs
    tx.prepare_cached("DELETE FROM session_kv WHERE root_path = ?1")?
        .execute(params![root_path])?;
    let mut kv_stmt =
        tx.prepare_cached("INSERT INTO session_kv (root_path, key, value) VALUES (?1, ?2, ?3)")?;
    for (k, v) in &data.kv {
        kv_stmt.execute(params![root_path, k, v])?;
    }
    drop(kv_stmt);

    // Clean up undo state for files no longer in the session
    tx.prepare_cached(
        "DELETE FROM undo_entries WHERE root_path = ?1 AND file_path NOT IN (SELECT file_path FROM buffers WHERE root_path = ?1)",
    )?.execute(params![root_path])?;
    tx.prepare_cached(
        "DELETE FROM buffer_undo_state WHERE root_path = ?1 AND file_path NOT IN (SELECT file_path FROM buffers WHERE root_path = ?1)",
    )?.execute(params![root_path])?;

    tx.commit()
}

pub fn load_session(
    conn: &Connection,
    root_path: &str,
) -> rusqlite::Result<Option<RestoredSession>> {
    let workspace = conn
        .prepare_cached("SELECT active_tab, show_side_panel FROM workspaces WHERE root_path = ?1")?
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

    let buffers: Vec<SessionBuffer> = stmt
        .query_map(params![root_path], |row| {
            let path: String = row.get(0)?;
            Ok(SessionBuffer {
                file_path: path.into(),
                tab_order: row.get::<_, i64>(1)? as usize,
                cursor_row: row.get::<_, i64>(2)? as usize,
                cursor_col: row.get::<_, i64>(3)? as usize,
                scroll_row: row.get::<_, i64>(4)? as usize,
                scroll_sub_line: row.get::<_, i64>(5)? as usize,
                undo: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if buffers.is_empty() {
        return Ok(None);
    }

    // Load KV pairs
    let mut kv_stmt =
        conn.prepare_cached("SELECT key, value FROM session_kv WHERE root_path = ?1")?;
    let kv: HashMap<String, String> = kv_stmt
        .query_map(params![root_path], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Some(RestoredSession {
        buffers,
        active_tab_order,
        show_side_panel,
        kv,
    }))
}

// ── Undo persistence ──

pub fn flush_undo(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
    chain_id: &str,
    content_hash: u64,
    undo_cursor: usize,
    distance_from_save: i32,
    entries: &[Vec<u8>],
) -> rusqlite::Result<i64> {
    let tx = conn.unchecked_transaction()?;

    tx.prepare_cached(
        "INSERT OR REPLACE INTO buffer_undo_state
         (root_path, file_path, chain_id, content_hash, undo_cursor, distance_from_save)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        root_path,
        file_path,
        chain_id,
        content_hash as i64,
        undo_cursor as i64,
        distance_from_save
    ])?;

    let mut stmt = tx.prepare_cached(
        "INSERT INTO undo_entries (root_path, file_path, entry_data) VALUES (?1, ?2, ?3)",
    )?;
    for entry in entries {
        stmt.execute(params![root_path, file_path, entry])?;
    }
    drop(stmt);

    let last_seq: i64 = tx.prepare_cached(
        "SELECT COALESCE(MAX(seq), 0) FROM undo_entries WHERE root_path = ?1 AND file_path = ?2",
    )?.query_row(params![root_path, file_path], |row| row.get(0))?;

    tx.commit()?;
    Ok(last_seq)
}

pub fn clear_undo(conn: &Connection, root_path: &str, file_path: &str) -> rusqlite::Result<()> {
    conn.prepare_cached("DELETE FROM undo_entries WHERE root_path = ?1 AND file_path = ?2")?
        .execute(params![root_path, file_path])?;
    conn.prepare_cached("DELETE FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2")?
        .execute(params![root_path, file_path])?;
    Ok(())
}

pub struct UndoSyncState {
    pub chain_id: String,
    pub content_hash: u64,
    pub entries: Vec<Vec<u8>>,
    pub last_seq: i64,
}

pub fn load_undo_after(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
    after_seq: i64,
) -> rusqlite::Result<Option<UndoSyncState>> {
    let state = conn.prepare_cached(
        "SELECT chain_id, content_hash FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
    )?.query_row(params![root_path, file_path], |row| {
        let chain_id: String = row.get(0)?;
        let content_hash: i64 = row.get(1)?;
        Ok((chain_id, content_hash as u64))
    });

    let (chain_id, content_hash) = match state {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut stmt = conn.prepare_cached(
        "SELECT seq, entry_data FROM undo_entries
         WHERE root_path = ?1 AND file_path = ?2 AND seq > ?3
         ORDER BY seq",
    )?;

    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map(params![root_path, file_path, after_seq], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let last_seq = rows.last().map(|(seq, _)| *seq).unwrap_or(after_seq);
    let entries = rows.into_iter().map(|(_, data)| data).collect();

    Ok(Some(UndoSyncState {
        chain_id,
        content_hash,
        entries,
        last_seq,
    }))
}

/// Load ALL undo entries for a buffer (for session restore).
pub fn load_undo_all(
    conn: &Connection,
    root_path: &str,
    file_path: &str,
) -> rusqlite::Result<Option<UndoRestoreState>> {
    let state = conn
        .prepare_cached(
            "SELECT chain_id, content_hash, undo_cursor, distance_from_save
         FROM buffer_undo_state WHERE root_path = ?1 AND file_path = ?2",
        )?
        .query_row(params![root_path, file_path], |row| {
            Ok(UndoRestoreState {
                chain_id: row.get(0)?,
                content_hash: row.get::<_, i64>(1)? as u64,
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
    restore.entries = rows.into_iter().map(|(_, data)| data).collect();

    Ok(Some(restore))
}

pub struct UndoRestoreState {
    pub chain_id: String,
    pub content_hash: u64,
    pub undo_cursor: Option<usize>,
    pub distance_from_save: i32,
    pub entries: Vec<Vec<u8>>,
    pub last_seq: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        run_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn save_and_load_session() {
        let conn = mem_db();

        let data = SessionData {
            buffers: vec![
                SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 10,
                    cursor_col: 5,
                    scroll_row: 3,
                    scroll_sub_line: 0,
                    undo: None,
                },
                SessionBuffer {
                    file_path: "/tmp/b.rs".into(),
                    tab_order: 1,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                },
            ],
            active_tab_order: 1,
            show_side_panel: false,
            kv: HashMap::from([
                ("browser.selected".into(), "2".into()),
                ("browser.expanded_dirs".into(), "/tmp/foo\n/tmp/bar".into()),
            ]),
        };

        save_session(&conn, "/project", &data).unwrap();
        let restored = load_session(&conn, "/project")
            .unwrap()
            .expect("session should exist");

        assert_eq!(restored.buffers.len(), 2);
        assert_eq!(restored.buffers[0].file_path.to_str().unwrap(), "/tmp/a.rs");
        assert_eq!(restored.buffers[0].cursor_row, 10);
        assert_eq!(restored.buffers[0].cursor_col, 5);
        assert_eq!(restored.buffers[1].file_path.to_str().unwrap(), "/tmp/b.rs");
        assert_eq!(restored.active_tab_order, 1);
        assert!(!restored.show_side_panel);
        assert_eq!(restored.kv.get("browser.selected").unwrap(), "2");
        assert_eq!(
            restored.kv.get("browser.expanded_dirs").unwrap(),
            "/tmp/foo\n/tmp/bar"
        );
    }

    #[test]
    fn load_empty_session_returns_none() {
        let conn = mem_db();
        let restored = load_session(&conn, "/project").unwrap();
        assert!(restored.is_none());
    }

    #[test]
    fn save_session_replaces_previous() {
        let conn = mem_db();

        let data1 = SessionData {
            buffers: vec![SessionBuffer {
                file_path: "/tmp/old.rs".into(),
                tab_order: 0,
                cursor_row: 0,
                cursor_col: 0,
                scroll_row: 0,
                scroll_sub_line: 0,
                undo: None,
            }],
            active_tab_order: 0,
            show_side_panel: true,
            kv: HashMap::new(),
        };
        save_session(&conn, "/project", &data1).unwrap();

        let data2 = SessionData {
            buffers: vec![SessionBuffer {
                file_path: "/tmp/new.rs".into(),
                tab_order: 0,
                cursor_row: 5,
                cursor_col: 3,
                scroll_row: 0,
                scroll_sub_line: 0,
                undo: None,
            }],
            active_tab_order: 0,
            show_side_panel: false,
            kv: HashMap::new(),
        };
        save_session(&conn, "/project", &data2).unwrap();

        let restored = load_session(&conn, "/project").unwrap().unwrap();
        assert_eq!(restored.buffers.len(), 1);
        assert_eq!(
            restored.buffers[0].file_path.to_str().unwrap(),
            "/tmp/new.rs"
        );
        assert_eq!(restored.buffers[0].cursor_row, 5);
    }

    #[test]
    fn flush_and_load_undo() {
        let conn = mem_db();

        // Must have a workspace + buffer first (FK)
        save_session(
            &conn,
            "/project",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: true,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        let entries = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let last_seq = flush_undo(
            &conn,
            "/project",
            "/tmp/a.rs",
            "chain-1",
            12345,
            2,
            1,
            &entries,
        )
        .unwrap();
        assert!(last_seq > 0);

        let state = load_undo_after(&conn, "/project", "/tmp/a.rs", 0)
            .unwrap()
            .unwrap();
        assert_eq!(state.chain_id, "chain-1");
        assert_eq!(state.content_hash, 12345);
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0], vec![1, 2, 3]);
        assert_eq!(state.entries[1], vec![4, 5, 6]);

        // Load after last_seq returns no new entries
        let state2 = load_undo_after(&conn, "/project", "/tmp/a.rs", last_seq)
            .unwrap()
            .unwrap();
        assert!(state2.entries.is_empty());
    }

    #[test]
    fn load_undo_all_for_restore() {
        let conn = mem_db();

        save_session(
            &conn,
            "/project",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: true,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        flush_undo(
            &conn,
            "/project",
            "/tmp/a.rs",
            "chain-1",
            12345,
            2,
            3,
            &[vec![10], vec![20]],
        )
        .unwrap();

        let state = load_undo_all(&conn, "/project", "/tmp/a.rs")
            .unwrap()
            .unwrap();
        assert_eq!(state.chain_id, "chain-1");
        assert_eq!(state.content_hash, 12345);
        assert_eq!(state.undo_cursor, Some(2));
        assert_eq!(state.distance_from_save, 3);
        assert_eq!(state.entries.len(), 2);
    }

    #[test]
    fn clear_undo_deletes_entries() {
        let conn = mem_db();

        save_session(
            &conn,
            "/project",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: true,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        flush_undo(
            &conn,
            "/project",
            "/tmp/a.rs",
            "chain-1",
            12345,
            2,
            0,
            &[vec![1, 2, 3]],
        )
        .unwrap();

        clear_undo(&conn, "/project", "/tmp/a.rs").unwrap();

        let state = load_undo_after(&conn, "/project", "/tmp/a.rs", 0).unwrap();
        assert!(state.is_none());
    }

    #[test]
    fn session_delete_cascades_to_undo() {
        let conn = mem_db();

        save_session(
            &conn,
            "/project",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: true,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        flush_undo(
            &conn,
            "/project",
            "/tmp/a.rs",
            "chain-1",
            12345,
            2,
            0,
            &[vec![1, 2, 3]],
        )
        .unwrap();

        // Save a new session without /tmp/a.rs — should cascade delete undo
        save_session(
            &conn,
            "/project",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/b.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: true,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        let state = load_undo_after(&conn, "/project", "/tmp/a.rs", 0).unwrap();
        assert!(state.is_none());
    }

    #[test]
    fn load_undo_nonexistent_file_returns_none() {
        let conn = mem_db();
        let state = load_undo_after(&conn, "/project", "/tmp/no.rs", 0).unwrap();
        assert!(state.is_none());
    }

    #[test]
    fn flush_undo_incremental() {
        let conn = mem_db();

        save_session(
            &conn,
            "/project",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: true,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        // First flush
        let seq1 = flush_undo(
            &conn,
            "/project",
            "/tmp/a.rs",
            "chain-1",
            100,
            1,
            0,
            &[vec![10]],
        )
        .unwrap();

        // Second flush — new entries only
        let seq2 = flush_undo(
            &conn,
            "/project",
            "/tmp/a.rs",
            "chain-1",
            200,
            2,
            1,
            &[vec![20]],
        )
        .unwrap();
        assert!(seq2 > seq1);

        // Load after seq1 — should get only the second entry
        let state = load_undo_after(&conn, "/project", "/tmp/a.rs", seq1)
            .unwrap()
            .unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0], vec![20]);
        assert_eq!(state.last_seq, seq2);
    }

    #[test]
    fn multi_workspace_isolation() {
        let conn = mem_db();

        save_session(
            &conn,
            "/project-a",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: true,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        save_session(
            &conn,
            "/project-b",
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/b.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    undo: None,
                }],
                active_tab_order: 0,
                show_side_panel: false,
                kv: HashMap::new(),
            },
        )
        .unwrap();

        let a = load_session(&conn, "/project-a").unwrap().unwrap();
        let b = load_session(&conn, "/project-b").unwrap().unwrap();
        assert_eq!(a.buffers[0].file_path.to_str().unwrap(), "/tmp/a.rs");
        assert_eq!(b.buffers[0].file_path.to_str().unwrap(), "/tmp/b.rs");
        assert!(a.show_side_panel);
        assert!(!b.show_side_panel);
    }
}
