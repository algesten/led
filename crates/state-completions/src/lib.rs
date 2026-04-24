//! LSP completion popup state.
//!
//! The driver produces `LspEvent::Completion` batches; the runtime
//! parks them here as a `CompletionSession`. The query layer reads
//! the session to render the popover, and dispatch mutates
//! `selected` / `filtered` / `session` on navigation / commit /
//! dismiss.
//!
//! One session at a time — mirrors legacy's manager state and
//! avoids the UX oddity of two popups fighting over the same
//! cursor. Opening a new session (different tab, new trigger)
//! replaces any prior one.

use std::sync::Arc;

use led_core::CanonPath;
use led_driver_lsp_core::CompletionItem;
use led_state_tabs::TabId;

/// Per-session completion state. `session: None` means no popup
/// is open; every field below is implicitly reset at that point.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompletionsState {
    pub session: Option<CompletionSession>,
    /// Monotonic sequence generator for request / resolve round
    /// trips. Handed to `LspCmd::RequestCompletion.seq` on
    /// dispatch; the runtime gates incoming events by comparing
    /// to the latest issued id so typing races can't display
    /// stale items.
    pub seq_gen: u64,
}

/// One active popup. Created on the first matching
/// `LspEvent::Completion` and cleared on commit / dismiss / tab
/// switch.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionSession {
    /// The tab the popup was triggered on. Used as a guard —
    /// switching tabs dismisses.
    pub tab: TabId,
    /// The buffer. Mostly a sanity check against events arriving
    /// after the user swapped tabs; all indexing into the rope
    /// goes through the tab's current buffer, not a snapshot.
    pub path: CanonPath,
    /// `seq` of the `LspCmd::RequestCompletion` that produced
    /// these items. Responses older than this are dropped.
    pub seq: u64,
    /// Cursor line at request time. The popup dismisses if the
    /// cursor moves to a different line (legacy behaviour —
    /// completions rarely span lines and matching LSP servers
    /// against a multi-line prefix is brittle).
    pub prefix_line: u32,
    /// Char col where the user's in-progress identifier starts.
    /// Client-side refilter extracts the prefix as
    /// `buffer.line(prefix_line)[prefix_start_col..cursor_col]`;
    /// edits that move the cursor LEFT of `prefix_start_col`
    /// dismiss the popup.
    pub prefix_start_col: u32,
    /// All items the server returned, unfiltered. Ref-counted so
    /// refilter + render can share the same Vec cheaply across
    /// frames.
    pub items: Arc<Vec<CompletionItem>>,
    /// Indices into `items`, in display order. Rebuilt on every
    /// keystroke (fuzzy-match + rank by nucleo); empty means
    /// "no items match the current prefix" → popup dismisses.
    pub filtered: Arc<Vec<usize>>,
    /// 0-indexed selection into `filtered`. Clamped on every
    /// refilter; arrow keys bump by ±1.
    pub selected: usize,
    /// Top index into `filtered` for the visible window.
    /// Popover shows at most ~10 rows; scroll moves this up/down
    /// to keep `selected` visible.
    pub scroll: usize,
}

impl CompletionsState {
    /// Allocate the next request / resolve sequence id.
    pub fn next_seq(&mut self) -> u64 {
        self.seq_gen = self.seq_gen.wrapping_add(1);
        self.seq_gen
    }

    /// Drop any active popup. Cheap no-op when already clear.
    pub fn dismiss(&mut self) {
        self.session = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_seq_is_monotonic() {
        let mut s = CompletionsState::default();
        assert_eq!(s.next_seq(), 1);
        assert_eq!(s.next_seq(), 2);
        assert_eq!(s.next_seq(), 3);
    }

    #[test]
    fn dismiss_is_idempotent() {
        let mut s = CompletionsState::default();
        s.dismiss();
        assert!(s.session.is_none());
    }
}
