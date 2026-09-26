//! User-decision sources for Claude chats.
//!
//! - [`ChatPrefs`]: per-session Effort / PermissionMode overrides and
//!   the queue of user messages ready to ship to the subprocess. The
//!   runtime's `subprocess_action` memo drains the queue when the
//!   corresponding session is Running, one per turn (the CLI's
//!   stream-json input expects one user message at a time per
//!   session).
//! - [`ChatSessions`]: the binding between a chat-buffer path (the
//!   synthetic `<uuid>.chat` file backing the editor view of a chat)
//!   and its session UUID + per-path offsets that track where the
//!   user is typing vs. where assistant replies splice in.
//!
//! No driver — per EXAMPLE-ARCH §User-decision sources have no
//! driver; dispatch in the runtime mutates these maps directly when
//! the user types, presses Send, picks a new chat, etc.

use std::collections::HashMap;

use imbl::{HashMap as IHashMap, Vector};

use led_core::{CanonPath, Effort, PermissionMode, SessionUuid};

// ── Per-session prefs / pending sends ────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatPrefs {
    pub overrides: IHashMap<SessionUuid, SessionOverrides>,
    pub pending_sends: IHashMap<SessionUuid, Vector<String>>,
}

/// Per-session override of the driver-wide defaults. `None` on a
/// field means "fall through to default" (`Effort::XHigh` /
/// `PermissionMode::Auto`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionOverrides {
    pub effort: Option<Effort>,
    pub permission_mode: Option<PermissionMode>,
}

impl ChatPrefs {
    /// Resolve the effort to use for a session — override, or driver
    /// default if no override is set.
    pub fn effort_for(&self, uuid: &SessionUuid) -> Effort {
        self.overrides
            .get(uuid)
            .and_then(|o| o.effort)
            .unwrap_or_default()
    }

    /// Resolve the permission mode to use for a session.
    pub fn permission_mode_for(&self, uuid: &SessionUuid) -> PermissionMode {
        self.overrides
            .get(uuid)
            .and_then(|o| o.permission_mode)
            .unwrap_or_default()
    }

    /// Enqueue a user message for a session. Called from dispatch on
    /// Submit.
    pub fn queue_send(&mut self, uuid: SessionUuid, text: String) {
        self.pending_sends.entry(uuid).or_default().push_back(text);
    }

    /// Pop the next pending message for a session. Called from the
    /// runtime's subprocess_action memo when the session is Running
    /// and free to receive another turn.
    ///
    /// Returns `None` if the queue is empty (or doesn't exist). Empty
    /// queues are cleaned up so iteration over `pending_sends`
    /// doesn't see ghosts.
    pub fn pop_pending(&mut self, uuid: &SessionUuid) -> Option<String> {
        let q = self.pending_sends.get_mut(uuid)?;
        let item = q.pop_front();
        if q.is_empty() {
            self.pending_sends.remove(uuid);
        }
        item
    }

    /// True if `uuid` has at least one queued message ready to send.
    /// Used by `subprocess_action` to decide whether to emit a
    /// `UserMessage` action this iteration.
    pub fn has_pending(&self, uuid: &SessionUuid) -> bool {
        self.pending_sends
            .get(uuid)
            .map(|q| !q.is_empty())
            .unwrap_or(false)
    }
}

// ── Chat sessions (path → session metadata) ──────────────────────────

/// Per chat-buffer path metadata. Tracks the session UUID, the
/// rope-character offsets that separate "already submitted" from
/// "user is still typing", the splice anchor for in-flight responses,
/// and the watermark on the timeline event log so each event is
/// rendered exactly once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatSessionState {
    pub session: SessionUuid,
    /// Character offset where the next user submission begins.
    /// Everything before it is already-submitted history.
    pub submit_offset: usize,
    /// Character offset where the next assistant-driven splice
    /// inserts. Parked at end-of-rope after a submit; advanced past
    /// each spliced chunk so subsequent events stack in order.
    pub response_anchor: usize,
    /// Each `(start, end)` pair is the rope-char range of a single
    /// user submission, recorded BEFORE the trailing pad/separator.
    /// Used by future colouring logic; mutated as splices shift
    /// later ranges.
    pub user_ranges: Vec<(usize, usize)>,
    /// Watermark into `ChatTranscripts.per_session[uuid].events`.
    /// `pending_chat_splices` skips events `[0, last_synced_event)`
    /// when computing the next splice batch.
    pub last_synced_event: usize,
}

/// Map from chat-buffer path → session metadata. Process-local —
/// the session DB only restores `Tabs` + `EditedBuffers`; the chat
/// path → uuid reseed happens in the chat phase's `execute` pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatSessions {
    pub by_path: HashMap<CanonPath, ChatSessionState>,
}

impl ChatSessions {
    /// Register `path` as a chat buffer bound to `session`. Offsets
    /// start at 0; the caller (e.g. the reseed pass) may park them
    /// at end-of-rope afterwards.
    pub fn insert(&mut self, path: CanonPath, session: SessionUuid) {
        self.by_path.insert(
            path,
            ChatSessionState {
                session,
                ..Default::default()
            },
        );
    }

    pub fn get(&self, path: &CanonPath) -> Option<&ChatSessionState> {
        self.by_path.get(path)
    }

    pub fn get_mut(&mut self, path: &CanonPath) -> Option<&mut ChatSessionState> {
        self.by_path.get_mut(path)
    }

    pub fn remove(&mut self, path: &CanonPath) -> Option<ChatSessionState> {
        self.by_path.remove(path)
    }

    /// True if `path` is registered as a chat-buffer path.
    pub fn is_chat(&self, path: &CanonPath) -> bool {
        self.by_path.contains_key(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u() -> SessionUuid {
        SessionUuid::new("u1")
    }

    #[test]
    fn effort_resolves_to_default_without_override() {
        let prefs = ChatPrefs::default();
        assert_eq!(prefs.effort_for(&u()), Effort::default());
        assert_eq!(prefs.permission_mode_for(&u()), PermissionMode::default());
    }

    #[test]
    fn override_wins_over_default() {
        let mut prefs = ChatPrefs::default();
        prefs.overrides.insert(
            u(),
            SessionOverrides {
                effort: Some(Effort::Max),
                permission_mode: Some(PermissionMode::Plan),
            },
        );
        assert_eq!(prefs.effort_for(&u()), Effort::Max);
        assert_eq!(prefs.permission_mode_for(&u()), PermissionMode::Plan);
    }

    #[test]
    fn partial_override_falls_through_for_unset_fields() {
        let mut prefs = ChatPrefs::default();
        prefs.overrides.insert(
            u(),
            SessionOverrides {
                effort: Some(Effort::Low),
                permission_mode: None,
            },
        );
        assert_eq!(prefs.effort_for(&u()), Effort::Low);
        assert_eq!(prefs.permission_mode_for(&u()), PermissionMode::default());
    }

    #[test]
    fn queue_send_appends_to_queue() {
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u(), "first".into());
        prefs.queue_send(u(), "second".into());

        assert_eq!(prefs.pending_sends[&u()].len(), 2);
        assert_eq!(prefs.pending_sends[&u()][0], "first");
        assert_eq!(prefs.pending_sends[&u()][1], "second");
    }

    #[test]
    fn pop_pending_drains_in_fifo_order_and_removes_empty_queue() {
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u(), "a".into());
        prefs.queue_send(u(), "b".into());

        assert_eq!(prefs.pop_pending(&u()), Some("a".into()));
        assert!(prefs.has_pending(&u()));
        assert_eq!(prefs.pop_pending(&u()), Some("b".into()));
        assert!(!prefs.has_pending(&u()));
        // Empty queue cleaned up.
        assert!(!prefs.pending_sends.contains_key(&u()));

        assert_eq!(prefs.pop_pending(&u()), None);
    }

    #[test]
    fn has_pending_false_for_unknown_session() {
        let prefs = ChatPrefs::default();
        assert!(!prefs.has_pending(&u()));
    }

    // ── ChatSessions ─────────────────────────────────────────────

    fn p(s: &str) -> CanonPath {
        led_core::UserPath::new(s).canonicalize()
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut s = ChatSessions::default();
        s.insert(p("/tmp/x.chat"), u());
        assert!(s.is_chat(&p("/tmp/x.chat")));
        let state = s.get(&p("/tmp/x.chat")).unwrap();
        assert_eq!(state.session, u());
        assert_eq!(state.submit_offset, 0);
        assert_eq!(state.response_anchor, 0);
        assert!(state.user_ranges.is_empty());
        assert_eq!(state.last_synced_event, 0);
    }

    #[test]
    fn remove_drops_entry() {
        let mut s = ChatSessions::default();
        s.insert(p("/tmp/x.chat"), u());
        let removed = s.remove(&p("/tmp/x.chat"));
        assert!(removed.is_some());
        assert!(!s.is_chat(&p("/tmp/x.chat")));
    }
}
