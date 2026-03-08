use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

pub struct SessionData {
    pub buffer_paths: Vec<PathBuf>,
    pub active_tab: usize,
    pub focus_is_editor: bool,
    pub show_side_panel: bool,
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
            UNIQUE(root_path, file_path)
        );

        CREATE TABLE IF NOT EXISTS session_kv (
            root_path   TEXT NOT NULL,
            key         TEXT NOT NULL,
            value       TEXT NOT NULL,
            PRIMARY KEY (root_path, key)
        );",
    )
}

/// Migrate existing databases that have the old UNIQUE(root_path, tab_index)
/// constraint to the new UNIQUE(root_path, file_path) constraint, and drop
/// legacy browser_state / browser_expanded_dirs tables.
fn migrate_schema(conn: &Connection) {
    let needs_buffers_migration: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='buffers'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(|sql| sql.contains("root_path, tab_index)"))
        .unwrap_or(false);

    if needs_buffers_migration {
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS buffers_new (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                root_path       TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
                tab_index       INTEGER NOT NULL,
                file_path       TEXT NOT NULL,
                cursor_row      INTEGER NOT NULL DEFAULT 0,
                cursor_col      INTEGER NOT NULL DEFAULT 0,
                scroll_offset   INTEGER NOT NULL DEFAULT 0,
                UNIQUE(root_path, file_path)
            );
            INSERT OR IGNORE INTO buffers_new (root_path, tab_index, file_path, cursor_row, cursor_col, scroll_offset)
                SELECT root_path, tab_index, file_path, cursor_row, cursor_col, scroll_offset FROM buffers;
            DROP TABLE buffers;
            ALTER TABLE buffers_new RENAME TO buffers;",
        );
    }

    // Migrate browser_state and browser_expanded_dirs into session_kv
    let has_browser_state: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='browser_state'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if has_browser_state {
        // Migrate selected_index
        let _ = (|| -> rusqlite::Result<()> {
            let mut stmt = conn.prepare("SELECT root_path, selected_index FROM browser_state")?;
            let rows: Vec<(String, i64)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            for (root, sel) in rows {
                conn.execute(
                    "INSERT OR IGNORE INTO session_kv (root_path, key, value) VALUES (?1, ?2, ?3)",
                    params![root, "browser.selected", sel.to_string()],
                )?;
            }
            Ok(())
        })();

        // Migrate expanded dirs
        let has_expanded = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='browser_expanded_dirs'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) > 0;

        if has_expanded {
            let _ = (|| -> rusqlite::Result<()> {
                let mut stmt =
                    conn.prepare("SELECT root_path, dir_path FROM browser_expanded_dirs")?;
                let rows: Vec<(String, String)> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .filter_map(|r| r.ok())
                    .collect();

                // Group by root_path
                let mut by_root: HashMap<String, Vec<String>> = HashMap::new();
                for (root, dir) in rows {
                    by_root.entry(root).or_default().push(dir);
                }
                for (root, dirs) in by_root {
                    conn.execute(
                        "INSERT OR IGNORE INTO session_kv (root_path, key, value) VALUES (?1, ?2, ?3)",
                        params![root, "browser.expanded_dirs", dirs.join("\n")],
                    )?;
                }
                Ok(())
            })();
            let _ = conn.execute_batch("DROP TABLE IF EXISTS browser_expanded_dirs;");
        }

        let _ = conn.execute_batch("DROP TABLE IF EXISTS browser_state;");
    }
}

pub fn open_db() -> Option<Connection> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let conn = Connection::open(&path).ok()?;
    migrate_schema(&conn);
    run_schema(&conn).ok()?;
    Some(conn)
}

pub fn save_session(conn: &Connection, root_path: &Path, session: &SessionData) {
    let root = root_path.to_string_lossy();
    let focus = if session.focus_is_editor {
        "editor"
    } else {
        "browser"
    };
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
        for (i, path) in session.buffer_paths.iter().enumerate() {
            tx.execute(
                "INSERT INTO buffers (root_path, tab_index, file_path)
                 VALUES (?1, ?2, ?3)",
                params![root, i as i64, path.to_string_lossy()],
            )?;
        }

        tx.commit()?;
        Ok(())
    })();

    if let Err(e) = result {
        log::warn!("failed to save session: {e}");
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
        .prepare("SELECT file_path FROM buffers WHERE root_path = ?1 ORDER BY tab_index")
        .ok()?;
    let buffer_paths: Vec<PathBuf> = stmt
        .query_map(params![root], |row| {
            let fp: String = row.get(0)?;
            Ok(PathBuf::from(fp))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    Some(SessionData {
        buffer_paths,
        active_tab: active_tab as usize,
        focus_is_editor: focus_str == "editor",
        show_side_panel: show_side_panel_int != 0,
    })
}

pub fn load_kv(conn: &Connection, root_path: &Path) -> HashMap<String, String> {
    let root = root_path.to_string_lossy();
    let mut map = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT key, value FROM session_kv WHERE root_path = ?1")
    else {
        return map;
    };
    let Ok(rows) = stmt.query_map(params![root], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        map.insert(row.0, row.1);
    }
    map
}

pub fn save_kv(conn: &Connection, root_path: &Path, kv: &HashMap<String, String>) {
    let root = root_path.to_string_lossy();
    let _ = conn.execute("DELETE FROM session_kv WHERE root_path = ?1", params![root]);
    for (key, value) in kv {
        let _ = conn.execute(
            "INSERT INTO session_kv (root_path, key, value) VALUES (?1, ?2, ?3)",
            params![root, key, value],
        );
    }
}

pub fn reset_db() {
    if let Some(path) = db_path() {
        let _ = std::fs::remove_file(path);
    }
}
