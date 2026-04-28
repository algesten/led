use rusqlite::Connection;

use crate::SCHEMA_VERSION;

// ── Schema (legacy-exact) ────────────────────────────────────

pub(crate) fn run_schema(conn: &Connection) -> rusqlite::Result<()> {
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
