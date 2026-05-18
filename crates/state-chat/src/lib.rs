//! User-decision source for Claude chats.
//!
//! Three maps keyed by [`SessionUuid`]:
//!
//! - [`ChatPrefs::overrides`]: per-session Effort / PermissionMode
//!   overrides. `None` on either field means "use driver defaults"
//!   (xhigh / auto). Stored here rather than on `ChatStore` because
//!   the source-of-truth for *user choices* is the user-decision
//!   tier; `ChatStore` persists them so they survive restart.
//! - [`ChatPrefs::composer_text`]: the text the user is currently
//!   typing in each chat tab's composer. Cleared on Send.
//! - [`ChatPrefs::pending_sends`]: queue of user messages ready to
//!   ship to the subprocess. The runtime's `subprocess_action`
//!   memo drains this when the corresponding session is Running,
//!   one per turn (the CLI's stream-json input expects one user
//!   message at a time per session).
//!
//! No driver per EXAMPLE-ARCH §User-decision sources have no
//! driver; dispatch in the runtime mutates these maps directly
//! when the user types, presses Send, picks a new chat, etc.

use std::collections::{HashMap, VecDeque};

use led_core::{Effort, PermissionMode, SessionUuid};

// ── Open chat tabs ──────────────────────────────────────────────────

/// Which chat sessions are currently "open as tabs" and which one
/// is focused.
///
/// Sibling to the existing `Tabs` source (file tabs) rather than
/// folded into it — the file-tab struct in `state-tabs` carries
/// ~10 file-specific fields (cursor / scroll / mark / preview /
/// pending_cursor / pending_scroll / last_search / ...) that
/// don't apply to chats, and the dispatch layer accesses
/// `tab.path` in 60+ places. A polymorphic union would force every
/// one of those sites to handle the chat case explicitly —
/// without much benefit, because the runtime's `subprocess_action`
/// memo (task #19) reads chat tabs from *here* anyway.
///
/// The visual "single tab strip with files + chats interleaved"
/// is reconstructed in the render layer (task #23) by reading
/// both sources.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatTabs {
    /// Open chat sessions in tab-strip order (most-recently-
    /// opened on the right is the convention; dispatch enforces).
    pub open: Vec<SessionUuid>,
    /// Currently focused chat tab. `None` ⇒ no chat tab focused
    /// (file tab might be focused instead, or no tabs at all).
    pub focused: Option<SessionUuid>,
}

impl ChatTabs {
    /// Open `uuid` as a new chat tab if not already open;
    /// focus it either way. Idempotent.
    pub fn open_or_focus(&mut self, uuid: SessionUuid) {
        if !self.open.contains(&uuid) {
            self.open.push(uuid.clone());
        }
        self.focused = Some(uuid);
    }

    /// Close `uuid`. If it was focused, focus the next open
    /// chat (preferring the one to the right, falling back to
    /// the left). Returns `true` if `uuid` was open.
    pub fn close(&mut self, uuid: &SessionUuid) -> bool {
        let Some(idx) = self.open.iter().position(|u| u == uuid) else {
            return false;
        };
        self.open.remove(idx);
        if self.focused.as_ref() == Some(uuid) {
            self.focused = self
                .open
                .get(idx)
                .cloned()
                .or_else(|| idx.checked_sub(1).and_then(|i| self.open.get(i).cloned()));
        }
        true
    }

    /// True if `uuid` is currently in the open tab strip.
    pub fn is_open(&self, uuid: &SessionUuid) -> bool {
        self.open.contains(uuid)
    }
}

// ── Per-session prefs / composer / pending sends ────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatPrefs {
    pub overrides: HashMap<SessionUuid, SessionOverrides>,
    pub composer_text: HashMap<SessionUuid, String>,
    pub pending_sends: HashMap<SessionUuid, VecDeque<String>>,
}

/// Per-session override of the driver-wide defaults. `None` on
/// a field means "fall through to default" (`Effort::XHigh` /
/// `PermissionMode::Auto`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionOverrides {
    pub effort: Option<Effort>,
    pub permission_mode: Option<PermissionMode>,
}

impl ChatPrefs {
    /// Resolve the effort to use for a session — override, or
    /// driver default if no override is set.
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

    /// Enqueue a user message for a session and clear the
    /// composer for that session. Called from dispatch on Send.
    pub fn queue_send(&mut self, uuid: SessionUuid, text: String) {
        self.composer_text.remove(&uuid);
        self.pending_sends.entry(uuid).or_default().push_back(text);
    }

    /// Pop the next pending message for a session. Called from
    /// the runtime's subprocess_action memo when the session is
    /// Running and free to receive another turn.
    ///
    /// Returns `None` if the queue is empty (or doesn't exist).
    /// Empty queues are cleaned up so iteration over
    /// `pending_sends` doesn't see ghosts.
    pub fn pop_pending(&mut self, uuid: &SessionUuid) -> Option<String> {
        let q = self.pending_sends.get_mut(uuid)?;
        let item = q.pop_front();
        if q.is_empty() {
            self.pending_sends.remove(uuid);
        }
        item
    }

    /// True if `uuid` has at least one queued message ready to
    /// send. Used by `subprocess_action` to decide whether to
    /// emit a `UserMessage` action this iteration.
    pub fn has_pending(&self, uuid: &SessionUuid) -> bool {
        self.pending_sends
            .get(uuid)
            .map(|q| !q.is_empty())
            .unwrap_or(false)
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
    fn queue_send_clears_composer_and_appends_to_queue() {
        let mut prefs = ChatPrefs::default();
        prefs.composer_text.insert(u(), "draft".into());

        prefs.queue_send(u(), "first".into());
        prefs.queue_send(u(), "second".into());

        assert!(!prefs.composer_text.contains_key(&u()));
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

    // ── ChatTabs ──────────────────────────────────────────────────

    #[test]
    fn open_or_focus_appends_and_focuses_new_uuid() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(SessionUuid::new("a"));
        assert_eq!(tabs.open, vec![SessionUuid::new("a")]);
        assert_eq!(tabs.focused, Some(SessionUuid::new("a")));

        tabs.open_or_focus(SessionUuid::new("b"));
        assert_eq!(
            tabs.open,
            vec![SessionUuid::new("a"), SessionUuid::new("b")]
        );
        assert_eq!(tabs.focused, Some(SessionUuid::new("b")));
    }

    #[test]
    fn open_or_focus_existing_uuid_just_focuses() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(SessionUuid::new("a"));
        tabs.open_or_focus(SessionUuid::new("b"));
        tabs.open_or_focus(SessionUuid::new("a"));
        assert_eq!(tabs.open.len(), 2);
        assert_eq!(tabs.focused, Some(SessionUuid::new("a")));
    }

    #[test]
    fn close_focuses_neighbour_to_the_right_by_default() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(SessionUuid::new("a"));
        tabs.open_or_focus(SessionUuid::new("b"));
        tabs.open_or_focus(SessionUuid::new("c"));
        tabs.focused = Some(SessionUuid::new("b"));
        assert!(tabs.close(&SessionUuid::new("b")));
        // `c` takes b's slot (idx=1), so c is focused.
        assert_eq!(tabs.focused, Some(SessionUuid::new("c")));
        assert_eq!(
            tabs.open,
            vec![SessionUuid::new("a"), SessionUuid::new("c")]
        );
    }

    #[test]
    fn close_rightmost_focuses_left_neighbour() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(SessionUuid::new("a"));
        tabs.open_or_focus(SessionUuid::new("b"));
        // Focused = b (rightmost). Closing it falls back left to a.
        assert!(tabs.close(&SessionUuid::new("b")));
        assert_eq!(tabs.focused, Some(SessionUuid::new("a")));
    }

    #[test]
    fn close_last_tab_clears_focus() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(SessionUuid::new("a"));
        assert!(tabs.close(&SessionUuid::new("a")));
        assert!(tabs.open.is_empty());
        assert!(tabs.focused.is_none());
    }

    #[test]
    fn close_unknown_uuid_is_noop_and_returns_false() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(SessionUuid::new("a"));
        assert!(!tabs.close(&SessionUuid::new("nope")));
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.focused, Some(SessionUuid::new("a")));
    }

    #[test]
    fn is_open_predicate() {
        let mut tabs = ChatTabs::default();
        assert!(!tabs.is_open(&SessionUuid::new("a")));
        tabs.open_or_focus(SessionUuid::new("a"));
        assert!(tabs.is_open(&SessionUuid::new("a")));
        assert!(!tabs.is_open(&SessionUuid::new("b")));
    }
}
