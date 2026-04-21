//! The `KillRing` source — session-global kill-text state.
//!
//! Pure user-decision state: dispatch writes `latest` on kill
//! commands and reads it on `Yank`. Course-correct #5 pulled the
//! async-adjacent fields (`pending_yank`, `read_in_flight`,
//! `pending_clipboard_write`) out into [`led_state_clipboard`] —
//! they belong to the driver, not the kill ring.
//!
//! M7 is single-slot: `latest: Option<Arc<str>>` holds the most
//! recently killed text. The field name and shape anticipate a real
//! ring (yank-pop cycling) in a later milestone.

use std::sync::Arc;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let k = KillRing::default();
        assert!(k.latest.is_none());
        assert!(!k.last_was_kill_line);
    }

    #[test]
    fn fields_clone_round_trip() {
        let k = KillRing {
            latest: Some(Arc::from("hi")),
            last_was_kill_line: true,
        };
        let c = k.clone();
        assert_eq!(c, k);
    }
}
