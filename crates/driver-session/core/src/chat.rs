//! Persistent storage for Claude chats.
//!
//! The `driver-session` driver is led's SQLite layer; chats join
//! the same persistence flow as workspaces/buffers/undo. Cross-
//! driver composition (driver-claude → SQLite) happens entirely
//! through the runtime — driver-claude never imports
//! driver-session per EXAMPLE-ARCH §Cross-driver composition.
//!
//! - [`ChatStore`] is the in-memory mirror, loaded once per
//!   workspace on `SessionCmd::Init` and updated incrementally
//!   as the runtime's `pending_persist_writes` memo emits Insert
//!   / Append / Update cmds.
//! - [`ChatRow`] and [`ChatMessageRow`] are the row shapes the
//!   worker maps to/from SQLite.
//!
//! Times here are unix seconds, sourced from the runtime's
//! `ClockInput` per EXAMPLE-ARCH §Time is a source field — the
//! worker just writes what it's given so tests can drive virtual
//! time without sleeping.

use std::collections::HashMap;

use led_core::{Effort, PermissionMode, SessionUuid};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatStore {
    pub rows: HashMap<SessionUuid, ChatRow>,
    pub messages: HashMap<SessionUuid, Vec<ChatMessageRow>>,
    /// `false` until the first `ChatsLoaded` event fires after
    /// `Init`. Memos that read this should treat `false` as
    /// "not ready" (return empty / Noop) rather than "no chats".
    pub loaded: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatRow {
    pub id: SessionUuid,
    pub workspace_root: String,
    pub short_label: Option<String>,
    pub long_summary: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub permission_mode: Option<PermissionMode>,
    pub created_at: i64,
    pub last_active_at: i64,
    /// Raw JSON from the most recent assistant/result `usage`
    /// block — kept opaque on the persistence side so forward-
    /// compatible CLI shapes don't need a schema migration.
    pub last_usage_json: Option<String>,
    pub status: ChatStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatStatus {
    Active,
    /// `--resume` returned "No conversation found" — the CLI's
    /// own transcript for this id is gone. led keeps the row
    /// so the user can see/delete the orphan, but won't try to
    /// respawn it. Per [[feedback_driver_failure_state]].
    Orphaned,
}

impl ChatStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatStatus::Active => "active",
            ChatStatus::Orphaned => "orphaned",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "orphaned" => ChatStatus::Orphaned,
            _ => ChatStatus::Active,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessageRow {
    pub session: SessionUuid,
    pub seq: u64,
    pub role: ChatRole,
    pub kind: ChatMessageKind,
    /// JSON-encoded body (text for AssistantText/UserSent;
    /// {tool_use_id,name,input} for ToolUse; {tool_use_id,content}
    /// for ToolResult; etc). Kept as opaque JSON so the schema
    /// doesn't need a column per shape.
    pub body_json: String,
    /// Optional per-message usage snapshot — `usage` from the
    /// assistant event that produced this message.
    pub usage_json: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
    System,
}

impl ChatRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::Tool => "tool",
            ChatRole::System => "system",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "user" => ChatRole::User,
            "tool" => ChatRole::Tool,
            "system" => ChatRole::System,
            _ => ChatRole::Assistant,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatMessageKind {
    Text,
    ToolUse,
    ToolResult,
    TurnComplete,
    TurnError,
}

impl ChatMessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatMessageKind::Text => "text",
            ChatMessageKind::ToolUse => "tool_use",
            ChatMessageKind::ToolResult => "tool_result",
            ChatMessageKind::TurnComplete => "turn_complete",
            ChatMessageKind::TurnError => "turn_error",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "tool_use" => ChatMessageKind::ToolUse,
            "tool_result" => ChatMessageKind::ToolResult,
            "turn_complete" => ChatMessageKind::TurnComplete,
            "turn_error" => ChatMessageKind::TurnError,
            _ => ChatMessageKind::Text,
        }
    }
}

impl ChatStore {
    /// Apply a `ChatsLoaded` payload. Replaces all rows + messages
    /// for the workspaces represented (not just appends). Called
    /// once per `SessionCmd::Init` cycle.
    pub fn apply_loaded(
        &mut self,
        rows: Vec<ChatRow>,
        messages: Vec<ChatMessageRow>,
    ) {
        self.rows.clear();
        self.messages.clear();
        for row in rows {
            self.rows.insert(row.id.clone(), row);
        }
        for m in messages {
            self.messages.entry(m.session.clone()).or_default().push(m);
        }
        // Per-session ordering by seq for predictable display.
        for v in self.messages.values_mut() {
            v.sort_by_key(|m| m.seq);
        }
        self.loaded = true;
    }

    /// Mirror an InsertChatRow cmd into the in-memory store
    /// (called by the sync driver's execute step so the next
    /// iteration's memos see the row immediately, not after the
    /// SQLite write round-trips).
    pub fn apply_insert(&mut self, row: ChatRow) {
        self.rows.insert(row.id.clone(), row);
    }

    pub fn apply_append(&mut self, message: ChatMessageRow) {
        self.messages
            .entry(message.session.clone())
            .or_default()
            .push(message);
    }

    pub fn apply_labels(
        &mut self,
        id: &SessionUuid,
        short_label: Option<String>,
        long_summary: Option<String>,
    ) {
        if let Some(row) = self.rows.get_mut(id) {
            row.short_label = short_label;
            row.long_summary = long_summary;
        }
    }

    pub fn apply_last_active(
        &mut self,
        id: &SessionUuid,
        at: i64,
        usage_json: Option<String>,
    ) {
        if let Some(row) = self.rows.get_mut(id) {
            row.last_active_at = at;
            if usage_json.is_some() {
                row.last_usage_json = usage_json;
            }
        }
    }

    pub fn apply_status(&mut self, id: &SessionUuid, status: ChatStatus) {
        if let Some(row) = self.rows.get_mut(id) {
            row.status = status;
        }
    }

    /// Highest `seq` already stored for `id`, or 0 if no messages
    /// yet. Callers use this to compute the next seq for an
    /// Append cmd (kept on the runtime side, not the driver, so
    /// the seq is consistent with the optimistic local mirror).
    pub fn max_seq(&self, id: &SessionUuid) -> u64 {
        self.messages
            .get(id)
            .and_then(|v| v.last().map(|m| m.seq))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> SessionUuid {
        SessionUuid::new(s)
    }

    fn row(id: &str, ws: &str, last_active: i64) -> ChatRow {
        ChatRow {
            id: u(id),
            workspace_root: ws.into(),
            short_label: None,
            long_summary: None,
            model: None,
            effort: None,
            permission_mode: None,
            created_at: 0,
            last_active_at: last_active,
            last_usage_json: None,
            status: ChatStatus::Active,
        }
    }

    fn msg(session: &str, seq: u64, role: ChatRole, body: &str) -> ChatMessageRow {
        ChatMessageRow {
            session: u(session),
            seq,
            role,
            kind: ChatMessageKind::Text,
            body_json: body.into(),
            usage_json: None,
            created_at: 0,
        }
    }

    #[test]
    fn apply_loaded_resets_and_sorts_messages_by_seq() {
        let mut store = ChatStore::default();
        store.apply_loaded(
            vec![row("a", "/ws", 100), row("b", "/ws", 50)],
            // Out of order on purpose.
            vec![
                msg("a", 3, ChatRole::Assistant, "third"),
                msg("a", 1, ChatRole::User, "first"),
                msg("a", 2, ChatRole::Assistant, "second"),
            ],
        );

        assert!(store.loaded);
        assert_eq!(store.rows.len(), 2);
        let a = store.messages.get(&u("a")).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].seq, 1);
        assert_eq!(a[1].seq, 2);
        assert_eq!(a[2].seq, 3);
    }

    #[test]
    fn apply_insert_and_append_update_store_optimistically() {
        let mut store = ChatStore::default();
        store.apply_insert(row("a", "/ws", 10));
        store.apply_append(msg("a", 1, ChatRole::User, "hi"));
        store.apply_append(msg("a", 2, ChatRole::Assistant, "hi back"));

        assert_eq!(store.rows.len(), 1);
        assert_eq!(store.messages[&u("a")].len(), 2);
        assert_eq!(store.max_seq(&u("a")), 2);
        assert_eq!(store.max_seq(&u("nonexistent")), 0);
    }

    #[test]
    fn apply_labels_updates_row_fields_in_place() {
        let mut store = ChatStore::default();
        store.apply_insert(row("a", "/ws", 10));
        store.apply_labels(
            &u("a"),
            Some("short-label".into()),
            Some("the long summary".into()),
        );
        let r = &store.rows[&u("a")];
        assert_eq!(r.short_label.as_deref(), Some("short-label"));
        assert_eq!(r.long_summary.as_deref(), Some("the long summary"));
    }

    #[test]
    fn apply_last_active_updates_timestamp_and_optional_usage() {
        let mut store = ChatStore::default();
        store.apply_insert(row("a", "/ws", 10));
        store.apply_last_active(&u("a"), 20, Some(r#"{"input_tokens":5}"#.into()));
        let r = &store.rows[&u("a")];
        assert_eq!(r.last_active_at, 20);
        assert_eq!(
            r.last_usage_json.as_deref(),
            Some(r#"{"input_tokens":5}"#)
        );
        // Update without usage doesn't clobber the existing value.
        store.apply_last_active(&u("a"), 30, None);
        let r = &store.rows[&u("a")];
        assert_eq!(r.last_active_at, 30);
        assert_eq!(
            r.last_usage_json.as_deref(),
            Some(r#"{"input_tokens":5}"#)
        );
    }

    #[test]
    fn apply_status_marks_orphaned() {
        let mut store = ChatStore::default();
        store.apply_insert(row("a", "/ws", 10));
        store.apply_status(&u("a"), ChatStatus::Orphaned);
        assert_eq!(store.rows[&u("a")].status, ChatStatus::Orphaned);
    }

    #[test]
    fn enum_strings_round_trip() {
        for s in [ChatStatus::Active, ChatStatus::Orphaned] {
            assert_eq!(ChatStatus::parse(s.as_str()), s);
        }
        for r in [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::System,
        ] {
            assert_eq!(ChatRole::parse(r.as_str()), r);
        }
        for k in [
            ChatMessageKind::Text,
            ChatMessageKind::ToolUse,
            ChatMessageKind::ToolResult,
            ChatMessageKind::TurnComplete,
            ChatMessageKind::TurnError,
        ] {
            assert_eq!(ChatMessageKind::parse(k.as_str()), k);
        }
    }
}
