use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

pub struct BufferState {
    pub file_path: PathBuf,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_offset: usize,
}

pub struct SessionData {
    pub buffers: Vec<BufferState>,
    pub active_tab: usize,
    pub focus_is_editor: bool,
    pub show_side_panel: bool,
    pub browser_selected: usize,
    pub browser_expanded_dirs: HashSet<PathBuf>,
}

fn db_path() -> Option<PathBuf> {
    let config_dir = dirs::config_dir()?.join("led");
    Some(config_dir.join("db.sqlite"))
}

fn run_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS workspaces (
            root_path       TEXT PRIMARY KEY,
            active_tab      INTEGER NOT NULL DEFAULT 0,
            focus           TEXT NOT NULL DEFAULT 'editor',
            show_side_panel INTEGER NOT NULL DEFAULT 1
        );

        CREATE TABLE IF NOT EXISTS buffers (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            root_path       TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
            tab_index       INTEGER NOT NULL,
            file_path       TEXT NOT NULL,
            cursor_row      INTEGER NOT NULL DEFAULT 0,
            cursor_col      INTEGER NOT NULL DEFAULT 0,
            scroll_offset   INTEGER NOT NULL DEFAULT 0,
            UNIQUE(root_path, tab_index)
        );

        CREATE TABLE IF NOT EXISTS browser_state (
            root_path       TEXT PRIMARY KEY REFERENCES workspaces(root_path) ON DELETE CASCADE,
            selected_index  INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS browser_expanded_dirs (
            root_path   TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
            dir_path    TEXT NOT NULL,
            PRIMARY KEY (root_path, dir_path)
        );",
    )
}

pub fn open_db() -> Option<Connection> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let conn = Connection::open(&path).ok()?;
    run_schema(&conn).ok()?;
    Some(conn)
}

pub fn save_session(conn: &Connection, root_path: &Path, session: &SessionData) {
    let root = root_path.to_string_lossy();
    let focus = if session.focus_is_editor { "editor" } else { "browser" };
    let side = if session.show_side_panel { 1 } else { 0 };

    let result: rusqlite::Result<()> = (|| {
        let tx = conn.unchecked_transaction()?;

        tx.execute(
            "INSERT INTO workspaces (root_path, active_tab, focus, show_side_panel)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(root_path) DO UPDATE SET
                active_tab = excluded.active_tab,
                focus = excluded.focus,
                show_side_panel = excluded.show_side_panel",
            params![root, session.active_tab as i64, focus, side],
        )?;

        tx.execute("DELETE FROM buffers WHERE root_path = ?1", params![root])?;
        for (i, buf) in session.buffers.iter().enumerate() {
            tx.execute(
                "INSERT INTO buffers (root_path, tab_index, file_path, cursor_row, cursor_col, scroll_offset)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    root,
                    i as i64,
                    buf.file_path.to_string_lossy(),
                    buf.cursor_row as i64,
                    buf.cursor_col as i64,
                    buf.scroll_offset as i64,
                ],
            )?;
        }

        tx.execute(
            "INSERT INTO browser_state (root_path, selected_index)
             VALUES (?1, ?2)
             ON CONFLICT(root_path) DO UPDATE SET selected_index = excluded.selected_index",
            params![root, session.browser_selected as i64],
        )?;

        tx.execute(
            "DELETE FROM browser_expanded_dirs WHERE root_path = ?1",
            params![root],
        )?;
        for dir in &session.browser_expanded_dirs {
            tx.execute(
                "INSERT INTO browser_expanded_dirs (root_path, dir_path) VALUES (?1, ?2)",
                params![root, dir.to_string_lossy()],
            )?;
        }

        tx.commit()?;
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("warning: failed to save session: {e}");
    }
}

pub fn load_session(conn: &Connection, root_path: &Path) -> Option<SessionData> {
    let root = root_path.to_string_lossy();

    let (active_tab, focus_str, show_side_panel_int): (i64, String, i64) = conn
        .query_row(
            "SELECT active_tab, focus, show_side_panel FROM workspaces WHERE root_path = ?1",
            params![root],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()?;

    let mut stmt = conn
        .prepare(
            "SELECT file_path, cursor_row, cursor_col, scroll_offset
             FROM buffers WHERE root_path = ?1 ORDER BY tab_index",
        )
        .ok()?;
    let buffers: Vec<BufferState> = stmt
        .query_map(params![root], |row| {
            let fp: String = row.get(0)?;
            Ok(BufferState {
                file_path: PathBuf::from(fp),
                cursor_row: row.get::<_, i64>(1)? as usize,
                cursor_col: row.get::<_, i64>(2)? as usize,
                scroll_offset: row.get::<_, i64>(3)? as usize,
            })
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    let browser_selected: usize = conn
        .query_row(
            "SELECT selected_index FROM browser_state WHERE root_path = ?1",
            params![root],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize;

    let mut dir_stmt = conn
        .prepare("SELECT dir_path FROM browser_expanded_dirs WHERE root_path = ?1")
        .ok()?;
    let browser_expanded_dirs: HashSet<PathBuf> = dir_stmt
        .query_map(params![root], |row| {
            let p: String = row.get(0)?;
            Ok(PathBuf::from(p))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    Some(SessionData {
        buffers,
        active_tab: active_tab as usize,
        focus_is_editor: focus_str == "editor",
        show_side_panel: show_side_panel_int != 0,
        browser_selected,
        browser_expanded_dirs,
    })
}

pub fn reset_db() {
    if let Some(path) = db_path() {
        let _ = std::fs::remove_file(path);
    }
}
