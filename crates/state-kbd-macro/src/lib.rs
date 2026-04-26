//! The `KbdMacroState` source — Emacs-style keyboard macros.
//!
//! State is per-process and is **not persisted across restarts**
//! (matches legacy `docs/spec/macros.md` § "Interaction with
//! other state"). The single `last` slot holds the most recently
//! ended recording; replay clones the `Arc` so an in-flight
//! playback can record itself again into a fresh `current` without
//! borrow-checker conflicts.
//!
//! No driver. Dispatch owns all mutation:
//! - `KbdMacroStart` clears `current` and sets `recording = true`.
//! - The recording hook in `dispatch_key` pushes resolved
//!   `Command`s into `current` while `recording` is true (filtered
//!   by `should_record`).
//! - `KbdMacroEnd` moves `current` into `last`.
//! - `KbdMacroExecute` recursively replays `last` `execute_count`
//!   times, gated by `playback_depth < 100`.

use std::sync::Arc;

use led_core::Command;

/// Hard cap on `KbdMacroExecute` recursion depth. Mirrors legacy
/// `led/src/model/action/mod.rs:278`. Exceeding the cap surfaces
/// the `"Keyboard macro recursion limit"` alert and aborts the
/// playback up the stack.
pub const RECURSION_LIMIT: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KbdMacroState {
    /// `true` between `KbdMacroStart` and `KbdMacroEnd`.
    pub recording: bool,
    /// Commands appended to the in-progress recording. Cleared
    /// on `KbdMacroStart`; moved into `last` on `KbdMacroEnd`.
    pub current: Vec<Command>,
    /// The last successfully ended recording. `None` until the
    /// first `KbdMacroEnd`. Overwritten on every successful end —
    /// a single slot, no register ring.
    ///
    /// `Arc<Vec<_>>` lets playback clone a refcount while the
    /// runtime keeps mutating `current` (a macro that records
    /// itself invoking another execute is legal).
    pub last: Option<Arc<Vec<Command>>>,
    /// Recursion guard. Bumped before each playback iteration,
    /// decremented on return. Hard-capped at [`RECURSION_LIMIT`].
    pub playback_depth: usize,
    /// Pending iteration count from the chord prefix
    /// (`Ctrl-x N e`). Set by the dispatch layer right before
    /// the `KbdMacroExecute` arm runs; consumed (`take()`) on
    /// the next execute. `None` means "play once". `Some(0)`
    /// means "play until inner failure" (clamped to `usize::MAX`
    /// iterations).
    pub execute_count: Option<usize>,
}

impl KbdMacroState {
    /// Convenience: `true` if `last` holds a recording playback
    /// can replay.
    pub fn has_recorded(&self) -> bool {
        self.last.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_clean_slate() {
        let s = KbdMacroState::default();
        assert!(!s.recording);
        assert!(s.current.is_empty());
        assert!(s.last.is_none());
        assert_eq!(s.playback_depth, 0);
        assert_eq!(s.execute_count, None);
        assert!(!s.has_recorded());
    }

    #[test]
    fn populated_state_round_trips_through_clone() {
        let mut s = KbdMacroState::default();
        s.recording = true;
        s.current.push(Command::CursorDown);
        s.current.push(Command::InsertChar('x'));
        s.last = Some(Arc::new(vec![Command::CursorRight]));
        s.playback_depth = 7;
        s.execute_count = Some(42);

        let copy = s.clone();
        assert_eq!(s, copy);
        assert!(copy.has_recorded());
    }

    #[test]
    fn recursion_limit_is_legacy_parity() {
        // Sanity-check the constant — 100 mirrors legacy
        // `led/src/model/action/mod.rs:278`.
        assert_eq!(RECURSION_LIMIT, 100);
    }
}
