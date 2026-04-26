//! `ClipboardState` — the driver-adjacent slice of the yank / kill
//! flow.
//!
//! Course-correct #5: `read_in_flight` used to live on `KillRing`, a
//! user-decision source. That was a category error — it's async-work
//! tracking, not a user choice. This crate pulls it out alongside
//! the small set of signals that bridge between dispatch
//! (kill-produced text, yank request) and the clipboard driver.
//!
//! Contents:
//! - `pending_yank`: **user decision** — dispatch sets this on
//!   `Yank`; the clipboard driver's Read completion consumes it.
//! - `read_in_flight`: **driver state** — true between Read
//!   execute and Read done. Prevents double-issue.
//! - `pending_write`: **bridge** — kill-produced text queued for
//!   async Write. Execute drains + clears.

use std::sync::Arc;

use led_state_tabs::TabId;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardState {
    pub pending_yank: Option<TabId>,
    pub read_in_flight: bool,
    pub pending_write: Option<Arc<str>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_idle() {
        let c = ClipboardState::default();
        assert!(c.pending_yank.is_none());
        assert!(!c.read_in_flight);
        assert!(c.pending_write.is_none());
    }
}
