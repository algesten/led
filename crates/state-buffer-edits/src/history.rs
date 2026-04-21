//! Undo / redo history for a single buffer.
//!
//! Three-stack model:
//! - `past` holds groups already applied (undoable). Most-recent at
//!   the end.
//! - `future` holds groups that were undone but could still be
//!   redone. Most-recent at the end.
//! - `current` is an open group being accumulated by coalescing.
//!   `finalise()` closes it into `past`.
//!
//! A fresh edit after an undo chain clears `future` — editing after
//! undo breaks the redo branch (matches Emacs / Zed / most editors).
//!
//! The op log structure is also what future rebase queries (LSP,
//! git, PR) walk to translate position-stamped data through
//! subsequent edits. See [`rebase_char_index`].

use led_state_tabs::Cursor;
use std::sync::Arc;

/// A single edit to the rope. `Insert` and `Delete` are exact
/// inverses of each other: `Delete { at, text }` is the inverse of
/// `Insert { at, text }` and vice versa. Undo applies the inverse;
/// redo applies the forward op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOp {
    Insert { at: usize, text: Arc<str> },
    Delete { at: usize, text: Arc<str> },
}

/// A logically contiguous run of ops sharing a single undo step.
/// Cursor bookends let undo/redo restore the cursor where the user
/// would expect it.
///
/// `seq` is a session-monotonic sequence number stamped at
/// `finalise()` — higher seq means "more recent across all
/// buffers." Used by the global (cross-buffer) undo path (file-
/// search overlay's Ctrl+_) to pick the newest group anywhere.
/// Regular per-buffer undo/redo ignores it; it's just an
/// ordering tag for the multi-buffer case.
///
/// `file_search_mark` carries overlay metadata so per-buffer
/// undo/redo can resync `FileSearchState.hit_replacements` when
/// the group represents a per-hit replace or its inverse. `None`
/// on every group that wasn't issued through the file-search path.
#[derive(Debug, Clone, PartialEq)]
pub struct EditGroup {
    pub ops: Vec<EditOp>,
    pub cursor_before: Cursor,
    pub cursor_after: Cursor,
    pub seq: u64,
    pub file_search_mark: Option<FileSearchMark>,
}

/// Payload attached to per-hit search-replace undo groups so the
/// overlay's hit_replacements Vec stays consistent across undo /
/// redo. `hit_idx` is the position in FileSearchState.flat_hits;
/// `forward_marks_replaced` tells which mark state the
/// forward-apply of this group produces. Undo sets the opposite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchMark {
    pub hit_idx: usize,
    pub forward_marks_replaced: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    past: Vec<EditGroup>,
    future: Vec<EditGroup>,
    current: Option<EditGroup>,
    /// Monotonically-increasing count of record_* invocations that
    /// landed on this history. Does NOT equal `applied_ops().count()`
    /// when inserts coalesce — a `record_insert_char` call that
    /// merged into an adjacent op still bumps this counter. Used as
    /// a cheap "has anything changed since version V" test.
    applied: usize,
    /// Shared session-wide seq generator. Cloned from the owning
    /// `BufferEdits` on construction so every history stamps
    /// groups from the same counter. Defaulted for tests /
    /// standalone use.
    seq_gen: crate::SeqGen,
}

impl History {
    pub fn with_seq_gen(seq_gen: crate::SeqGen) -> Self {
        Self {
            seq_gen,
            ..Default::default()
        }
    }
}

impl History {
    pub fn can_undo(&self) -> bool {
        !self.past.is_empty() || self.current.is_some()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// Drop every recorded group (past, future, current). Called by
    /// the runtime after a successful save — saved state becomes
    /// the new baseline and the prior undo chain no longer matches
    /// it. Matches legacy's `WorkspaceClearUndo` semantic.
    pub fn clear(&mut self) {
        self.past.clear();
        self.future.clear();
        self.current = None;
    }

    /// Iterate every applied op in order (past + current). Used by
    /// [`rebase_char_index`] and by tests that inspect the log.
    pub fn applied_ops(&self) -> impl Iterator<Item = &EditOp> {
        self.past
            .iter()
            .flat_map(|g| g.ops.iter())
            .chain(self.current.iter().flat_map(|g| g.ops.iter()))
    }

    pub fn past_len(&self) -> usize {
        self.past.len()
    }

    pub fn future_len(&self) -> usize {
        self.future.len()
    }

    /// Total number of successful record_* calls since creation.
    /// Matches the number of times `version` has been bumped. Not
    /// the same as `applied_ops().count()` — coalescing can merge
    /// two recorded inserts into one op.
    pub fn applied_count(&self) -> usize {
        self.applied
    }

    /// Record a single-char word-insert. If the open group's last
    /// op is an adjacent word-char insert, append the char to its
    /// text instead of opening a new op. Any new edit clears
    /// `future`.
    pub fn record_insert_char(
        &mut self,
        at: usize,
        ch: char,
        cursor_before: Cursor,
        cursor_after: Cursor,
    ) {
        self.future.clear();
        self.applied += 1;
        if is_word_char(ch)
            && let Some(group) = self.current.as_mut()
            && let Some(EditOp::Insert {
                at: last_at,
                text: last_text,
            }) = group.ops.last_mut()
        {
            let last_len = last_text.chars().count();
            let is_adjacent = at == *last_at + last_len;
            let last_ends_word =
                last_text.chars().last().is_some_and(is_word_char);
            if is_adjacent && last_ends_word {
                let mut merged = String::with_capacity(last_text.len() + 1);
                merged.push_str(last_text);
                merged.push(ch);
                *last_text = Arc::from(merged);
                group.cursor_after = cursor_after;
                return;
            }
        }
        // New group.
        let op = EditOp::Insert {
            at,
            text: Arc::from(ch.to_string()),
        };
        self.open_new_group(op, cursor_before, cursor_after);
    }

    /// Record a non-coalescing insert (newline, yank, multi-char
    /// paste). Opens a new group.
    pub fn record_insert(
        &mut self,
        at: usize,
        text: Arc<str>,
        cursor_before: Cursor,
        cursor_after: Cursor,
    ) {
        self.future.clear();
        self.applied += 1;
        let op = EditOp::Insert { at, text };
        self.open_new_group(op, cursor_before, cursor_after);
    }

    /// Record a deletion. Deletes are always their own group.
    pub fn record_delete(
        &mut self,
        at: usize,
        text: Arc<str>,
        cursor_before: Cursor,
        cursor_after: Cursor,
    ) {
        self.future.clear();
        self.applied += 1;
        let op = EditOp::Delete { at, text };
        self.open_new_group(op, cursor_before, cursor_after);
    }

    /// Record a compound replace (delete + insert at the same
    /// position) as a single undo group, optionally tagged with a
    /// `FileSearchMark`. Closes any currently-open group first so
    /// the replace stands alone — same discipline as a kill or a
    /// delete. Stamps the new group's seq immediately because it
    /// can't coalesce with anything.
    pub fn record_replace(
        &mut self,
        at: usize,
        removed: Arc<str>,
        inserted: Arc<str>,
        cursor_before: Cursor,
        cursor_after: Cursor,
        mark: Option<FileSearchMark>,
    ) {
        self.future.clear();
        self.finalise();
        self.applied += 2;
        let group = EditGroup {
            ops: vec![
                EditOp::Delete { at, text: removed },
                EditOp::Insert { at, text: inserted },
            ],
            cursor_before,
            cursor_after,
            seq: self.seq_gen.next(),
            file_search_mark: mark,
        };
        self.past.push(group);
    }

    /// Close the open group (if any) into `past`. Called by
    /// dispatch after every non-edit command so the next edit
    /// starts fresh. Stamps the closing group with the next
    /// session seq so cross-buffer ordering is preserved.
    pub fn finalise(&mut self) {
        if let Some(mut group) = self.current.take() {
            group.seq = self.seq_gen.next();
            self.past.push(group);
        }
    }

    /// Pop the most recent applied group for undo. Caller applies
    /// the inverse ops and pushes the group onto `future` via
    /// [`push_future`].
    pub fn take_undo(&mut self) -> Option<EditGroup> {
        // Close any open group before popping so the user undoes
        // what they just typed (not a stale past entry).
        self.finalise();
        self.past.pop()
    }

    /// Pop the most recent undone group for redo.
    pub fn take_redo(&mut self) -> Option<EditGroup> {
        // An open current during a redo chain can happen only if
        // the dispatcher forgot to finalise. Defensive close.
        self.finalise();
        self.future.pop()
    }

    /// Push a group back onto `future` (used by undo to stash what
    /// was just undone).
    pub fn push_future(&mut self, group: EditGroup) {
        self.future.push(group);
    }

    /// Push a group onto `past` (used by redo to stash what was
    /// just redone).
    pub fn push_past(&mut self, group: EditGroup) {
        self.past.push(group);
    }

    fn open_new_group(&mut self, op: EditOp, cursor_before: Cursor, cursor_after: Cursor) {
        // Close any existing group so the new one stands alone.
        self.finalise();
        self.current = Some(EditGroup {
            ops: vec![op],
            cursor_before,
            cursor_after,
            // seq is stamped at finalise(); 0 is a placeholder
            // for the open-group state.
            seq: 0,
            file_search_mark: None,
        });
    }

    /// Attach a `FileSearchMark` to the currently-open group. Must
    /// be called before `finalise()` on that group. Used by the
    /// file-search dispatch to tag per-hit replace + inverse
    /// groups so undo/redo can resync the overlay's marks.
    pub fn mark_current_as_file_search(&mut self, mark: FileSearchMark) {
        if let Some(g) = self.current.as_mut() {
            g.file_search_mark = Some(mark);
        }
    }

    /// Seq of the top `past` group, if any. Used by the global
    /// undo path to pick the max-seq buffer across the workspace.
    /// Returns the in-flight `current` group's (still 0) seq when
    /// no past is available — the caller treats 0 as "no meaningful
    /// seq, don't pick this one."
    pub fn past_top_seq(&self) -> Option<u64> {
        self.past.last().map(|g| g.seq)
    }

    /// Seq of the top `future` group (the one that'd get redone
    /// next), if any.
    pub fn future_top_seq(&self) -> Option<u64> {
        self.future.last().map(|g| g.seq)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Walk applied ops in order and transform `idx` into its current
/// char-index equivalent.
///
/// Conventions:
/// - `Insert { at, text }` where `at <= idx` shifts `idx` right by
///   `text.chars().count()`. `at > idx` is a no-op.
/// - `Delete { at, text }`:
///   - Removed range `[at, at + len)` entirely before `idx` shifts
///     left by `len`.
///   - Removed range entirely at/after `idx` is a no-op.
///   - Removed range straddling `idx` clamps `idx` to `at`.
///
/// `from_version` refers to the version we're rebasing from: the
/// function skips the first `from_version` ops (which were already
/// applied before the caller's coord was recorded).
pub fn rebase_char_index(idx: usize, from_version: u64, history: &History) -> usize {
    let mut cur = idx;
    let from = from_version as usize;
    for (i, op) in history.applied_ops().enumerate() {
        if i < from {
            continue;
        }
        cur = rebase_one(cur, op);
    }
    cur
}

fn rebase_one(idx: usize, op: &EditOp) -> usize {
    match op {
        EditOp::Insert { at, text } => {
            if *at <= idx {
                idx + text.chars().count()
            } else {
                idx
            }
        }
        EditOp::Delete { at, text } => {
            let len = text.chars().count();
            let end = *at + len;
            if end <= idx {
                idx - len
            } else if *at >= idx {
                idx
            } else {
                // Overlap: range contains idx, clamp to start.
                *at
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(line: usize, col: usize) -> Cursor {
        Cursor {
            line,
            col,
            preferred_col: col,
        }
    }

    #[test]
    fn record_insert_char_coalesces_word_chars() {
        let mut h = History::default();
        h.record_insert_char(0, 'h', cur(0, 0), cur(0, 1));
        h.record_insert_char(1, 'i', cur(0, 1), cur(0, 2));
        assert_eq!(h.past_len(), 0);
        assert_eq!(h.applied_count(), 2);
        // Single group with merged "hi" text.
        let ops: Vec<_> = h.applied_ops().collect();
        assert_eq!(ops.len(), 1);
        match ops[0] {
            EditOp::Insert { at, text } => {
                assert_eq!(*at, 0);
                assert_eq!(&**text, "hi");
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn record_insert_char_breaks_on_non_word() {
        let mut h = History::default();
        h.record_insert_char(0, 'a', cur(0, 0), cur(0, 1));
        h.record_insert_char(1, ' ', cur(0, 1), cur(0, 2));
        // Space starts a new group.
        h.finalise();
        assert_eq!(h.past_len(), 2);
    }

    #[test]
    fn record_insert_is_always_its_own_group() {
        let mut h = History::default();
        h.record_insert(0, Arc::from("hi"), cur(0, 0), cur(0, 2));
        h.record_insert(2, Arc::from("there"), cur(0, 2), cur(0, 7));
        h.finalise();
        assert_eq!(h.past_len(), 2);
    }

    #[test]
    fn record_delete_is_always_its_own_group() {
        let mut h = History::default();
        h.record_delete(0, Arc::from("h"), cur(0, 1), cur(0, 0));
        h.record_delete(0, Arc::from("e"), cur(0, 1), cur(0, 0));
        h.finalise();
        assert_eq!(h.past_len(), 2);
    }

    #[test]
    fn take_undo_redo_round_trip() {
        let mut h = History::default();
        h.record_insert(0, Arc::from("hi"), cur(0, 0), cur(0, 2));
        let undone = h.take_undo().expect("past has one");
        h.push_future(undone);
        assert!(!h.can_undo());
        let redone = h.take_redo().expect("future has one");
        h.push_past(redone);
        assert!(h.can_undo());
        assert!(!h.can_redo());
    }

    #[test]
    fn editing_after_undo_clears_future() {
        let mut h = History::default();
        h.record_insert(0, Arc::from("hi"), cur(0, 0), cur(0, 2));
        let undone = h.take_undo().expect("past has one");
        h.push_future(undone);
        assert!(h.can_redo());

        h.record_insert(0, Arc::from("xx"), cur(0, 0), cur(0, 2));
        assert!(!h.can_redo());
    }

    // ── rebase_char_index ──────────────────────────────────────────────

    #[test]
    fn rebase_insert_before_idx_shifts_right() {
        let mut h = History::default();
        h.record_insert(0, Arc::from("abc"), cur(0, 0), cur(0, 3));
        let new = rebase_char_index(10, 0, &h);
        assert_eq!(new, 13);
    }

    #[test]
    fn rebase_insert_after_idx_no_op() {
        let mut h = History::default();
        h.record_insert(20, Arc::from("abc"), cur(0, 0), cur(0, 3));
        assert_eq!(rebase_char_index(10, 0, &h), 10);
    }

    #[test]
    fn rebase_delete_before_idx_shifts_left() {
        let mut h = History::default();
        h.record_delete(0, Arc::from("abc"), cur(0, 3), cur(0, 0));
        assert_eq!(rebase_char_index(10, 0, &h), 7);
    }

    #[test]
    fn rebase_delete_overlapping_idx_clamps() {
        let mut h = History::default();
        h.record_delete(5, Arc::from("abcde"), cur(0, 10), cur(0, 5));
        // idx=8 is inside the deleted range → clamp to 5.
        assert_eq!(rebase_char_index(8, 0, &h), 5);
    }

    #[test]
    fn rebase_from_version_skips_already_applied() {
        let mut h = History::default();
        h.record_insert(0, Arc::from("abc"), cur(0, 0), cur(0, 3));
        h.record_insert(3, Arc::from("de"), cur(0, 3), cur(0, 5));
        // Rebase from version 1 — skip the first op, apply second only.
        // idx=10, op2 inserts "de" at 3 → 3 <= 10, shift by 2 → 12.
        assert_eq!(rebase_char_index(10, 1, &h), 12);
    }
}
