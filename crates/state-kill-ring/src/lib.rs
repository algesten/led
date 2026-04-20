//! The `KillRing` source — session-global state for kill/yank.
//!
//! User-decision state: mutated by dispatch in response to kill and
//! yank commands. No driver (the kill ring itself has no async side;
//! the clipboard driver is a separate crate that dispatch also
//! touches via its own source).
//!
//! M7 is single-slot: `latest: Option<Arc<str>>` holds the most
//! recently killed text. The field name and shape anticipate a real
//! ring (yank-pop cycling) in a later milestone.

use led_state_tabs::TabId;
use std::sync::Arc;

/// Session-global kill/yank state.
///
/// Invariants (maintained by dispatch):
/// - `last_was_kill_line` is true iff the most recent dispatched
///   command was `KillLine`. Any other command — edit, movement,
///   save, etc. — resets it to false before returning.
/// - `pending_yank` is `Some(tab_id)` between a `Yank` keypress and
///   the clipboard driver's response. `read_in_flight` tracks the
///   driver side of the same lifecycle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KillRing {
    /// Latest killed text. `Arc<str>` keeps the yank path O(1)
    /// clone — dispatch holds the `Arc` while inserting into the
    /// rope. Future milestones promote this to an ordered ring.
    pub latest: Option<Arc<str>>,
    /// True iff the most recent command was `KillLine`. Controls
    /// whether the next `KillLine` appends to `latest` or replaces
    /// it.
    pub last_was_kill_line: bool,
    /// When `Some(id)`, a yank has been requested and the runtime
    /// is waiting for the clipboard driver to respond. The id
    /// captures which tab the text should land in — if the user
    /// switches tabs between the keypress and the paste, the paste
    /// still goes to the original buffer.
    pub pending_yank: Option<TabId>,
    /// True while a clipboard read is in flight. Query uses this
    /// to avoid spawning a second read on top of the first.
    pub read_in_flight: bool,
    /// Text queued for an async clipboard write after a kill
    /// command. Runtime drains this in the execute phase, sync-
    /// clears it, and hands the text to the clipboard driver.
    pub pending_clipboard_write: Option<Arc<str>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let k = KillRing::default();
        assert!(k.latest.is_none());
        assert!(!k.last_was_kill_line);
        assert!(k.pending_yank.is_none());
        assert!(!k.read_in_flight);
    }

    #[test]
    fn fields_clone_round_trip() {
        let k = KillRing {
            latest: Some(Arc::from("hi")),
            last_was_kill_line: true,
            pending_yank: Some(TabId(7)),
            read_in_flight: true,
            pending_clipboard_write: Some(Arc::from("hi")),
        };
        let c = k.clone();
        assert_eq!(c, k);
    }
}
