//! Whole-process lifecycle: the `Phase` state machine and the
//! `force_redraw` repaint counter.
//!
//! The phases:
//!
//! - [`Phase::Starting`] — initial boot. In the M20 rewrite this
//!   lasts until the first successful frame emission; M21 expands
//!   it to cover session restore.
//! - [`Phase::Running`] — fully operational. Default from the
//!   first paint onwards.
//! - [`Phase::Suspended`] — SIGTSTP has parked the process. The
//!   terminal driver has already left the alt-screen; the main
//!   loop is blocked inside the suspend helper until the shell's
//!   `fg` wakes us.
//! - [`Phase::Exiting`] — `Ctrl-X Ctrl-C` requested shutdown. The
//!   main loop breaks on the next iteration; M21 adds a
//!   save-session gate before the break.
//!
//! `force_redraw` is a monotonic counter. Every bump signals "the
//! next paint should repaint every cell" — consumers that cache
//! `last_seen` detect the bump via `current != last_seen`. The
//! main loop uses it to clear `last_frame` after suspend/resume
//! so the cell-diff renderer doesn't assume pre-suspend content
//! is still on screen.

/// The four whole-process lifecycle states.
///
/// Adding a variant requires updating the main-loop match on
/// `Atoms.lifecycle.phase` — the compiler enforces exhaustiveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Starting,
    Running,
    Suspended,
    Exiting,
}

/// Lifecycle atom. Lives on `Atoms.lifecycle` in the runtime.
///
/// Kept small on purpose — every memo that projects this atom
/// invalidates on any field change, so extra fields would force
/// unrelated re-renders. M21 adds a cousin atom (`SessionState`)
/// for session persistence rather than stuffing it here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleState {
    pub phase: Phase,
    /// Monotonic counter. A bump signals "drop any cached frame
    /// and repaint every cell". Never decreases; wrapping is
    /// effectively impossible (one bump per user-visible event).
    pub force_redraw: u64,
}

impl LifecycleState {
    /// Flag a full-repaint request. The main loop compares the
    /// counter against its last-seen value on each tick.
    pub fn bump_redraw(&mut self) {
        self.force_redraw = self.force_redraw.wrapping_add(1);
    }

    /// Is the process currently considered "actively running"?
    /// `Running` only — `Starting`, `Suspended`, and `Exiting`
    /// all return `false`.
    pub fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_starting_with_zero_redraw() {
        let s = LifecycleState::default();
        assert_eq!(s.phase, Phase::Starting);
        assert_eq!(s.force_redraw, 0);
        assert!(!s.is_running());
    }

    #[test]
    fn bump_redraw_advances_counter() {
        let mut s = LifecycleState::default();
        s.bump_redraw();
        s.bump_redraw();
        assert_eq!(s.force_redraw, 2);
    }

    #[test]
    fn is_running_only_when_phase_is_running() {
        let mut s = LifecycleState::default();
        assert!(!s.is_running());
        s.phase = Phase::Running;
        assert!(s.is_running());
        s.phase = Phase::Suspended;
        assert!(!s.is_running());
        s.phase = Phase::Exiting;
        assert!(!s.is_running());
    }
}
