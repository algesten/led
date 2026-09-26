//! SQLite helpers for the `claude_sessions` + `claude_messages`
//! tables. Pure side-effecting functions; the worker_loop arms
//! call them and translate `rusqlite::Result` into
//! `SessionEvent::Failed` on the unhappy path.

use led_core::{Effort, PermissionMode, SessionUuid};
use led_driver_session_core::{
    ChatMessageKind, ChatMessageRow, ChatRole, ChatRow, ChatStatus,
};
use rusqlite::{Connection, params};

/// Bulk-load every chat row + message for `workspace_root`,
/// ordered for display.
pub fn load_chats(
    conn: &Connection,
    workspace_root: &str,
) -> rusqlite::Result<(Vec<ChatRow>, Vec<ChatMessageRow>)> {
    let mut rows_stmt = conn.prepare(
        "SELECT id, short_label, long_summary, model, effort, permission_mode,
                created_at, last_active_at, last_usage_json, status
         FROM claude_sessions
         WHERE workspace_root = ?1
         ORDER BY last_active_at DESC",
    )?;
    let mut rows = Vec::new();
    let mut ids = Vec::new();
    let mut iter = rows_stmt.query(params![workspace_root])?;
    while let Some(r) = iter.next()? {
        let id: String = r.get(0)?;
        let id = SessionUuid::new(&id);
        ids.push(id.as_str().to_string());
        rows.push(ChatRow {
            id: id.clone(),
            workspace_root: workspace_root.to_string(),
            short_label: r.get(1)?,
            long_summary: r.get(2)?,
            model: r.get(3)?,
            effort: r
                .get::<_, Option<String>>(4)?
                .and_then(|s| Effort::from_flag(&s)),
            permission_mode: r
                .get::<_, Option<String>>(5)?
                .and_then(|s| PermissionMode::from_flag(&s)),
            created_at: r.get(6)?,
            last_active_at: r.get(7)?,
            last_usage_json: r.get(8)?,
            status: r
                .get::<_, String>(9)
                .map(|s| ChatStatus::parse(&s))
                .unwrap_or(ChatStatus::Active),
        });
    }
    drop(iter);
    drop(rows_stmt);

    let mut messages = Vec::new();
    if !ids.is_empty() {
        // Build a single SELECT with IN(?,?,…) — small N (one
        // workspace's sessions) so dynamic param building is
        // fine.
        let placeholders: Vec<&str> = (0..ids.len()).map(|_| "?").collect();
        let sql = format!(
            "SELECT session_id, seq, role, kind, body_json, usage_json, created_at
             FROM claude_messages
             WHERE session_id IN ({})
             ORDER BY session_id, seq",
            placeholders.join(",")
        );
        let mut msg_stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut iter = msg_stmt.query(params.as_slice())?;
        while let Some(r) = iter.next()? {
            let session_id: String = r.get(0)?;
            messages.push(ChatMessageRow {
                session: SessionUuid::new(&session_id),
                seq: r.get::<_, i64>(1)? as u64,
                role: ChatRole::parse(&r.get::<_, String>(2)?),
                kind: ChatMessageKind::parse(&r.get::<_, String>(3)?),
                body_json: r.get(4)?,
                usage_json: r.get(5)?,
                created_at: r.get(6)?,
            });
        }
    }
    Ok((rows, messages))
}

pub fn insert_chat_row(conn: &Connection, row: &ChatRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO claude_sessions
            (id, workspace_root, short_label, long_summary, model,
             effort, permission_mode,
             created_at, last_active_at, last_usage_json, status)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(id) DO NOTHING",
        params![
            row.id.as_str(),
            &row.workspace_root,
            row.short_label.as_deref(),
            row.long_summary.as_deref(),
            row.model.as_deref(),
            row.effort.map(|e| e.as_flag()),
            row.permission_mode.map(|p| p.as_flag()),
            row.created_at,
            row.last_active_at,
            row.last_usage_json.as_deref(),
            row.status.as_str(),
        ],
    )?;
    Ok(())
}

pub fn append_chat_message(conn: &Connection, m: &ChatMessageRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO claude_messages
            (session_id, seq, role, kind, body_json, usage_json, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(session_id, seq) DO NOTHING",
        params![
            m.session.as_str(),
            m.seq as i64,
            m.role.as_str(),
            m.kind.as_str(),
            &m.body_json,
            m.usage_json.as_deref(),
            m.created_at,
        ],
    )?;
    Ok(())
}

pub fn update_chat_labels(
    conn: &Connection,
    id: &SessionUuid,
    short_label: Option<&str>,
    long_summary: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE claude_sessions
            SET short_label = ?2, long_summary = ?3
            WHERE id = ?1",
        params![id.as_str(), short_label, long_summary],
    )?;
    Ok(())
}

pub fn update_chat_last_active(
    conn: &Connection,
    id: &SessionUuid,
    at: i64,
    usage_json: Option<&str>,
) -> rusqlite::Result<()> {
    if let Some(j) = usage_json {
        conn.execute(
            "UPDATE claude_sessions
                SET last_active_at = ?2, last_usage_json = ?3
                WHERE id = ?1",
            params![id.as_str(), at, j],
        )?;
    } else {
        conn.execute(
            "UPDATE claude_sessions
                SET last_active_at = ?2
                WHERE id = ?1",
            params![id.as_str(), at],
        )?;
    }
    Ok(())
}

pub fn update_chat_status(
    conn: &Connection,
    id: &SessionUuid,
    status: ChatStatus,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE claude_sessions
            SET status = ?2
            WHERE id = ?1",
        params![id.as_str(), status.as_str()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        // Minimal workspaces row so FK doesn't block our inserts.
        crate::schema::run_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (root_path) VALUES ('/ws') ON CONFLICT DO NOTHING",
            [],
        )
        .unwrap();
        conn
    }

    fn sample_row(id: &str, last_active: i64) -> ChatRow {
        ChatRow {
            id: SessionUuid::new(id),
            workspace_root: "/ws".into(),
            short_label: Some("hello".into()),
            long_summary: Some("the long version".into()),
            model: Some("claude-opus-4-7[1m]".into()),
            effort: Some(Effort::XHigh),
            permission_mode: Some(PermissionMode::Auto),
            created_at: 100,
            last_active_at: last_active,
            last_usage_json: Some(r#"{"input_tokens":5}"#.into()),
            status: ChatStatus::Active,
        }
    }

    #[test]
    fn insert_then_load_round_trips_all_fields() {
        let conn = open_memory_db();
        insert_chat_row(&conn, &sample_row("a", 200)).unwrap();
        insert_chat_row(&conn, &sample_row("b", 100)).unwrap();

        let (rows, messages) = load_chats(&conn, "/ws").unwrap();
        // Sorted by last_active DESC.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id.as_str(), "a");
        assert_eq!(rows[1].id.as_str(), "b");
        assert_eq!(rows[0].short_label.as_deref(), Some("hello"));
        assert_eq!(rows[0].effort, Some(Effort::XHigh));
        assert_eq!(rows[0].permission_mode, Some(PermissionMode::Auto));
        assert_eq!(rows[0].status, ChatStatus::Active);
        assert!(messages.is_empty());
    }

    #[test]
    fn insert_is_idempotent_via_on_conflict_do_nothing() {
        let conn = open_memory_db();
        insert_chat_row(&conn, &sample_row("a", 200)).unwrap();
        // Second insert with different short_label is silently
        // ignored; the first write wins. Updates go through the
        // dedicated UPDATE cmds.
        let mut second = sample_row("a", 200);
        second.short_label = Some("changed".into());
        insert_chat_row(&conn, &second).unwrap();
        let (rows, _) = load_chats(&conn, "/ws").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].short_label.as_deref(), Some("hello"));
    }

    #[test]
    fn append_message_and_load_orders_by_seq() {
        let conn = open_memory_db();
        insert_chat_row(&conn, &sample_row("a", 200)).unwrap();
        append_chat_message(
            &conn,
            &ChatMessageRow {
                session: SessionUuid::new("a"),
                seq: 2,
                role: ChatRole::Assistant,
                kind: ChatMessageKind::Text,
                body_json: r#""second""#.into(),
                usage_json: None,
                created_at: 201,
            },
        )
        .unwrap();
        append_chat_message(
            &conn,
            &ChatMessageRow {
                session: SessionUuid::new("a"),
                seq: 1,
                role: ChatRole::User,
                kind: ChatMessageKind::Text,
                body_json: r#""first""#.into(),
                usage_json: None,
                created_at: 200,
            },
        )
        .unwrap();
        let (_, messages) = load_chats(&conn, "/ws").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].seq, 1);
        assert_eq!(messages[1].seq, 2);
    }

    #[test]
    fn update_labels_overwrites_existing_values() {
        let conn = open_memory_db();
        insert_chat_row(&conn, &sample_row("a", 200)).unwrap();
        update_chat_labels(
            &conn,
            &SessionUuid::new("a"),
            Some("renamed"),
            Some("new summary"),
        )
        .unwrap();
        let (rows, _) = load_chats(&conn, "/ws").unwrap();
        assert_eq!(rows[0].short_label.as_deref(), Some("renamed"));
        assert_eq!(rows[0].long_summary.as_deref(), Some("new summary"));
    }

    #[test]
    fn update_last_active_preserves_usage_when_none_provided() {
        let conn = open_memory_db();
        insert_chat_row(&conn, &sample_row("a", 200)).unwrap();
        update_chat_last_active(&conn, &SessionUuid::new("a"), 300, None).unwrap();
        let (rows, _) = load_chats(&conn, "/ws").unwrap();
        assert_eq!(rows[0].last_active_at, 300);
        assert_eq!(
            rows[0].last_usage_json.as_deref(),
            Some(r#"{"input_tokens":5}"#)
        );
        update_chat_last_active(
            &conn,
            &SessionUuid::new("a"),
            400,
            Some(r#"{"input_tokens":10}"#),
        )
        .unwrap();
        let (rows, _) = load_chats(&conn, "/ws").unwrap();
        assert_eq!(rows[0].last_active_at, 400);
        assert_eq!(
            rows[0].last_usage_json.as_deref(),
            Some(r#"{"input_tokens":10}"#)
        );
    }

    #[test]
    fn update_status_to_orphaned_persists() {
        let conn = open_memory_db();
        insert_chat_row(&conn, &sample_row("a", 200)).unwrap();
        update_chat_status(&conn, &SessionUuid::new("a"), ChatStatus::Orphaned).unwrap();
        let (rows, _) = load_chats(&conn, "/ws").unwrap();
        assert_eq!(rows[0].status, ChatStatus::Orphaned);
    }

    #[test]
    fn load_returns_empty_for_unknown_workspace() {
        let conn = open_memory_db();
        let (rows, messages) = load_chats(&conn, "/nowhere").unwrap();
        assert!(rows.is_empty());
        assert!(messages.is_empty());
    }
}
