//! `ClipboardIntent` — user-decision side of the yank / kill flow.
//!
//! Per EXAMPLE-ARCH § "Stateless drivers still need an in-flight
//! source", the in-flight bookkeeping moved off this crate and onto
//! the driver-owned `driver_clipboard_core::ClipboardState`. This
//! crate retains only the user-decided intents:
//!
//! - `pending_yank`: dispatch sets it on `Yank`; ingest consumes it
//!   when the clipboard driver's Read completes.
//! - `pending_write`: dispatch sets it on kill; the execute phase
//!   takes it when the driver's write slot is idle.

use std::sync::Arc;

use led_state_tabs::TabId;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardIntent {
    pub pending_yank: Option<TabId>,
    pub pending_write: Option<Arc<str>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_idle() {
        let c = ClipboardIntent::default();
        assert!(c.pending_yank.is_none());
        assert!(c.pending_write.is_none());
    }
}
