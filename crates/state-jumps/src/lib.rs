//! The `JumpListState` source — a back/forward history of
//! "interesting" cursor positions the user can round-trip to.
//!
//! Matches legacy `led/src/model/jump.rs` semantics:
//! - Recording truncates any forward branch (branching history).
//! - The deque is capped at 100; the oldest entry is evicted.
//! - `index == entries.len()` means "at head, no jump in progress".
//! - `step_back` from head pushes the supplied current position
//!   first — the implicit save-before-back — so the user can
//!   round-trip via `step_forward`.
//!
//! No driver. Dispatch owns all mutation: `match_bracket`,
//! `jump_back`, `jump_forward`, and tab cycling each record or
//! step the list.

use std::collections::VecDeque;

use led_core::CanonPath;

/// Maximum size of the jump deque. Matches legacy
/// `led/src/model/jump.rs:3`. When a record would exceed this,
/// the oldest entry is dropped from the front.
pub const MAX_ENTRIES: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpPosition {
    pub path: CanonPath,
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JumpListState {
    /// History, oldest at the front.
    pub entries: VecDeque<JumpPosition>,
    /// "Where the user currently sits in their history." When
    /// `index == entries.len()` the user is at the head (the
    /// present); otherwise they've navigated into the past via
    /// `step_back`.
    pub index: usize,
}

impl JumpListState {
    /// Push a new entry. Truncates any forward history (the
    /// user's new jump diverges from where they might have
    /// redone to) and caps to [`MAX_ENTRIES`].
    pub fn record(&mut self, pos: JumpPosition) {
        self.entries.truncate(self.index);
        self.entries.push_back(pos);
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.index = self.entries.len();
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    /// Step one entry back. If the user is currently at the
    /// head (`index == entries.len()`), first pushes `current`
    /// so a later `step_forward` can return to where they were.
    ///
    /// Returns `Some(target)` when a step happened, `None` when
    /// the list was empty / the user was already at index 0.
    pub fn step_back(&mut self, current: JumpPosition) -> Option<JumpPosition> {
        if !self.can_back() {
            return None;
        }
        if self.index == self.entries.len() {
            // At head: implicit save-before-back.
            self.entries.push_back(current);
            while self.entries.len() > MAX_ENTRIES {
                self.entries.pop_front();
                // Popping the front shifts indices — keep `index`
                // aligned to "what used to be at self.index".
                if self.index > 0 {
                    self.index -= 1;
                }
            }
            // `index` still points at the pre-save position; the
            // newly-pushed save is at `entries.len() - 1`.
        }
        self.index -= 1;
        self.entries.get(self.index).cloned()
    }

    /// Step one entry forward. Returns `None` if already at (or
    /// past) the head.
    pub fn step_forward(&mut self) -> Option<JumpPosition> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        self.entries.get(self.index).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn pos(path: &str, line: usize, col: usize) -> JumpPosition {
        JumpPosition {
            path: canon(path),
            line,
            col,
        }
    }

    #[test]
    fn default_is_empty() {
        let j = JumpListState::default();
        assert!(j.entries.is_empty());
        assert_eq!(j.index, 0);
        assert!(!j.can_back());
        assert!(!j.can_forward());
    }

    #[test]
    fn record_from_empty_leaves_index_at_head() {
        let mut j = JumpListState::default();
        j.record(pos("a", 10, 0));
        assert_eq!(j.entries.len(), 1);
        assert_eq!(j.index, 1);
        assert!(j.can_back());
        assert!(!j.can_forward());
    }

    #[test]
    fn record_truncates_forward_branch() {
        let mut j = JumpListState::default();
        j.record(pos("a", 1, 0));
        j.record(pos("a", 2, 0));
        j.record(pos("a", 3, 0));
        // Walk back twice — user is at index 1 (of 3 entries).
        j.step_back(pos("a", 99, 0));
        j.step_back(pos("a", 99, 0));
        assert_eq!(j.index, 1);
        // Record diverges; forward entries from index+1 should vanish,
        // and `pos("a", 2, 0)` (entries[1]) gets replaced by our new
        // entry.
        j.record(pos("b", 5, 0));
        assert_eq!(j.entries.len(), 2);
        assert_eq!(j.entries.back().unwrap().path, canon("b"));
    }

    #[test]
    fn record_caps_at_max_entries() {
        let mut j = JumpListState::default();
        for i in 0..MAX_ENTRIES + 5 {
            j.record(pos("a", i, 0));
        }
        assert_eq!(j.entries.len(), MAX_ENTRIES);
        // Oldest evicted; front should be entry #5.
        assert_eq!(j.entries.front().unwrap().line, 5);
        assert_eq!(j.index, MAX_ENTRIES);
    }

    #[test]
    fn step_back_from_head_auto_records_current() {
        let mut j = JumpListState::default();
        j.record(pos("a", 10, 0));
        j.record(pos("a", 20, 0));
        // At head: index = 2, entries.len() = 2.
        let target = j.step_back(pos("b", 99, 5));
        assert_eq!(target, Some(pos("a", 20, 0)));
        // The save-before-back pushed "b/99/5" onto the list,
        // and index stepped back to 1.
        assert_eq!(j.entries.len(), 3);
        assert_eq!(j.entries.back().unwrap(), &pos("b", 99, 5));
        assert_eq!(j.index, 1);
    }

    #[test]
    fn step_back_at_index_zero_returns_none() {
        let mut j = JumpListState::default();
        j.record(pos("a", 1, 0));
        j.step_back(pos("a", 2, 0)); // now at index 1 (pre-save); index decrements to 0.
        // Second step_back: no room.
        assert_eq!(j.step_back(pos("a", 9, 0)), None);
    }

    #[test]
    fn step_forward_returns_next_and_moves_index() {
        let mut j = JumpListState::default();
        j.record(pos("a", 1, 0));
        j.record(pos("a", 2, 0));
        j.step_back(pos("a", 99, 0));
        // Now at index 1 of 3. step_forward advances to index 2.
        let target = j.step_forward();
        assert_eq!(target, Some(pos("a", 99, 0)));
        assert_eq!(j.index, 2);
    }

    #[test]
    fn step_forward_at_head_returns_none() {
        let mut j = JumpListState::default();
        j.record(pos("a", 1, 0));
        assert_eq!(j.step_forward(), None);
    }

    #[test]
    fn back_forward_round_trip() {
        let mut j = JumpListState::default();
        j.record(pos("a", 100, 0));
        j.record(pos("a", 200, 0));
        let back = j.step_back(pos("a", 300, 0)).unwrap();
        assert_eq!(back.line, 200);
        let fwd = j.step_forward().unwrap();
        assert_eq!(fwd.line, 300);
    }
}
