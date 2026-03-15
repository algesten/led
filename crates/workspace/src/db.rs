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

fn run_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS session_buffers (
            file_path       TEXT PRIMARY KEY,
            tab_order       INTEGER NOT NULL,
            cursor_row      INTEGER NOT NULL DEFAULT 0,
            cursor_col      INTEGER NOT NULL DEFAULT 0,
            scroll_row      INTEGER NOT NULL DEFAULT 0,
            scroll_sub_line INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS session_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS buffer_undo_state (
            file_path          TEXT PRIMARY KEY
                REFERENCES session_buffers(file_path) ON DELETE CASCADE,
            chain_id           TEXT NOT NULL,
            content_hash       INTEGER NOT NULL,
            undo_cursor        INTEGER NOT NULL,
            distance_from_save INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS undo_entries (
            seq         INTEGER PRIMARY KEY AUTOINCREMENT,
            file_path   TEXT NOT NULL
                REFERENCES buffer_undo_state(file_path) ON DELETE CASCADE,
            entry_data  BLOB NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_undo_entries_file
            ON undo_entries(file_path, seq);
        ",
    )
}

// ── Session ──

pub fn save_session(conn: &Connection, data: &SessionData) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;

    tx.execute("DELETE FROM session_buffers", [])?;

    let mut stmt = tx.prepare(
        "INSERT INTO session_buffers (file_path, tab_order, cursor_row, cursor_col, scroll_row, scroll_sub_line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for buf in &data.buffers {
        stmt.execute(params![
            buf.file_path.to_string_lossy(),
            buf.tab_order as i64,
            buf.cursor_row as i64,
            buf.cursor_col as i64,
            buf.scroll_row as i64,
            buf.scroll_sub_line as i64,
        ])?;
    }
    drop(stmt);

    tx.execute(
        "INSERT OR REPLACE INTO session_meta (key, value) VALUES ('active_tab_order', ?1)",
        params![data.active_tab_order.to_string()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO session_meta (key, value) VALUES ('show_side_panel', ?1)",
        params![if data.show_side_panel { "1" } else { "0" }],
    )?;

    tx.commit()
}

pub fn load_session(conn: &Connection) -> rusqlite::Result<Option<RestoredSession>> {
    let mut stmt = conn.prepare(
        "SELECT file_path, tab_order, cursor_row, cursor_col, scroll_row, scroll_sub_line
         FROM session_buffers ORDER BY tab_order",
    )?;

    let buffers: Vec<SessionBuffer> = stmt
        .query_map([], |row| {
            let path: String = row.get(0)?;
            Ok(SessionBuffer {
                file_path: path.into(),
                tab_order: row.get::<_, i64>(1)? as usize,
                cursor_row: row.get::<_, i64>(2)? as usize,
                cursor_col: row.get::<_, i64>(3)? as usize,
                scroll_row: row.get::<_, i64>(4)? as usize,
                scroll_sub_line: row.get::<_, i64>(5)? as usize,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if buffers.is_empty() {
        return Ok(None);
    }

    let active_tab_order: usize = conn
        .query_row(
            "SELECT value FROM session_meta WHERE key = 'active_tab_order'",
            [],
            |row| {
                let v: String = row.get(0)?;
                Ok(v.parse().unwrap_or(0))
            },
        )
        .unwrap_or(0);

    let show_side_panel: bool = conn
        .query_row(
            "SELECT value FROM session_meta WHERE key = 'show_side_panel'",
            [],
            |row| {
                let v: String = row.get(0)?;
                Ok(v != "0")
            },
        )
        .unwrap_or(true);

    Ok(Some(RestoredSession {
        buffers,
        active_tab_order,
        show_side_panel,
    }))
}

// ── Undo persistence ──

pub fn flush_undo(
    conn: &Connection,
    file_path: &str,
    chain_id: &str,
    content_hash: u64,
    undo_cursor: usize,
    distance_from_save: i32,
    entries: &[Vec<u8>],
) -> rusqlite::Result<i64> {
    let tx = conn.unchecked_transaction()?;

    tx.execute(
        "INSERT OR REPLACE INTO buffer_undo_state
         (file_path, chain_id, content_hash, undo_cursor, distance_from_save)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            file_path,
            chain_id,
            content_hash as i64,
            undo_cursor as i64,
            distance_from_save
        ],
    )?;

    let mut stmt =
        tx.prepare("INSERT INTO undo_entries (file_path, entry_data) VALUES (?1, ?2)")?;
    for entry in entries {
        stmt.execute(params![file_path, entry])?;
    }
    drop(stmt);

    let last_seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM undo_entries WHERE file_path = ?1",
        params![file_path],
        |row| row.get(0),
    )?;

    tx.commit()?;
    Ok(last_seq)
}

pub fn clear_undo(conn: &Connection, file_path: &str) -> rusqlite::Result<()> {
    // FK cascade deletes undo_entries
    conn.execute(
        "DELETE FROM buffer_undo_state WHERE file_path = ?1",
        params![file_path],
    )?;
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
    file_path: &str,
    after_seq: i64,
) -> rusqlite::Result<Option<UndoSyncState>> {
    let state = conn.query_row(
        "SELECT chain_id, content_hash FROM buffer_undo_state WHERE file_path = ?1",
        params![file_path],
        |row| {
            let chain_id: String = row.get(0)?;
            let content_hash: i64 = row.get(1)?;
            Ok((chain_id, content_hash as u64))
        },
    );

    let (chain_id, content_hash) = match state {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut stmt = conn.prepare(
        "SELECT seq, entry_data FROM undo_entries
         WHERE file_path = ?1 AND seq > ?2
         ORDER BY seq",
    )?;

    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map(params![file_path, after_seq], |row| {
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
                },
                SessionBuffer {
                    file_path: "/tmp/b.rs".into(),
                    tab_order: 1,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                },
            ],
            active_tab_order: 1,
            show_side_panel: false,
        };

        save_session(&conn, &data).unwrap();
        let restored = load_session(&conn).unwrap().expect("session should exist");

        assert_eq!(restored.buffers.len(), 2);
        assert_eq!(restored.buffers[0].file_path.to_str().unwrap(), "/tmp/a.rs");
        assert_eq!(restored.buffers[0].cursor_row, 10);
        assert_eq!(restored.buffers[0].cursor_col, 5);
        assert_eq!(restored.buffers[1].file_path.to_str().unwrap(), "/tmp/b.rs");
        assert_eq!(restored.active_tab_order, 1);
        assert!(!restored.show_side_panel);
    }

    #[test]
    fn load_empty_session_returns_none() {
        let conn = mem_db();
        let restored = load_session(&conn).unwrap();
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
            }],
            active_tab_order: 0,
            show_side_panel: true,
        };
        save_session(&conn, &data1).unwrap();

        let data2 = SessionData {
            buffers: vec![SessionBuffer {
                file_path: "/tmp/new.rs".into(),
                tab_order: 0,
                cursor_row: 5,
                cursor_col: 3,
                scroll_row: 0,
                scroll_sub_line: 0,
            }],
            active_tab_order: 0,
            show_side_panel: false,
        };
        save_session(&conn, &data2).unwrap();

        let restored = load_session(&conn).unwrap().unwrap();
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

        // Must have a session buffer first (FK)
        save_session(
            &conn,
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                }],
                active_tab_order: 0,
                show_side_panel: true,
            },
        )
        .unwrap();

        let entries = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let last_seq = flush_undo(&conn, "/tmp/a.rs", "chain-1", 12345, 2, 0, &entries).unwrap();
        assert!(last_seq > 0);

        let state = load_undo_after(&conn, "/tmp/a.rs", 0).unwrap().unwrap();
        assert_eq!(state.chain_id, "chain-1");
        assert_eq!(state.content_hash, 12345);
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0], vec![1, 2, 3]);
        assert_eq!(state.entries[1], vec![4, 5, 6]);

        // Load after last_seq returns no new entries
        let state2 = load_undo_after(&conn, "/tmp/a.rs", last_seq)
            .unwrap()
            .unwrap();
        assert!(state2.entries.is_empty());
    }

    #[test]
    fn clear_undo_deletes_entries() {
        let conn = mem_db();

        save_session(
            &conn,
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                }],
                active_tab_order: 0,
                show_side_panel: true,
            },
        )
        .unwrap();

        flush_undo(&conn, "/tmp/a.rs", "chain-1", 12345, 2, 0, &[vec![1, 2, 3]]).unwrap();

        clear_undo(&conn, "/tmp/a.rs").unwrap();

        let state = load_undo_after(&conn, "/tmp/a.rs", 0).unwrap();
        assert!(state.is_none());
    }

    #[test]
    fn session_delete_cascades_to_undo() {
        let conn = mem_db();

        save_session(
            &conn,
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                }],
                active_tab_order: 0,
                show_side_panel: true,
            },
        )
        .unwrap();

        flush_undo(&conn, "/tmp/a.rs", "chain-1", 12345, 2, 0, &[vec![1, 2, 3]]).unwrap();

        // Save a new session without /tmp/a.rs — should cascade delete undo
        save_session(
            &conn,
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/b.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                }],
                active_tab_order: 0,
                show_side_panel: true,
            },
        )
        .unwrap();

        let state = load_undo_after(&conn, "/tmp/a.rs", 0).unwrap();
        assert!(state.is_none());
    }

    #[test]
    fn load_undo_nonexistent_file_returns_none() {
        let conn = mem_db();
        let state = load_undo_after(&conn, "/tmp/no.rs", 0).unwrap();
        assert!(state.is_none());
    }

    #[test]
    fn flush_undo_incremental() {
        let conn = mem_db();

        save_session(
            &conn,
            &SessionData {
                buffers: vec![SessionBuffer {
                    file_path: "/tmp/a.rs".into(),
                    tab_order: 0,
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                }],
                active_tab_order: 0,
                show_side_panel: true,
            },
        )
        .unwrap();

        // First flush
        let seq1 = flush_undo(&conn, "/tmp/a.rs", "chain-1", 100, 1, 0, &[vec![10]]).unwrap();

        // Second flush — new entries only
        let seq2 = flush_undo(&conn, "/tmp/a.rs", "chain-1", 200, 2, 1, &[vec![20]]).unwrap();
        assert!(seq2 > seq1);

        // Load after seq1 — should get only the second entry
        let state = load_undo_after(&conn, "/tmp/a.rs", seq1).unwrap().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0], vec![20]);
        assert_eq!(state.last_seq, seq2);
    }
}
