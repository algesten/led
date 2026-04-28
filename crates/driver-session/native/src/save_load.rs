use std::collections::HashMap;

use led_core::{SubLine, UserPath};
use led_state_session::{SessionBuffer, SessionData};
use led_state_tabs::{Cursor, Scroll};
use rusqlite::{Connection, params};

use crate::undo::load_undo_all;

// ── Save / load (legacy-exact) ───────────────────────────────

pub(crate) fn save_session(
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

pub(crate) fn load_session(
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
