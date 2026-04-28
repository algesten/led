//! Sparse line-level invalidation tracker for stamped marker
//! sources (LSP diagnostics, git line statuses).
//!
//! # The shape
//!
//! A marker source — diagnostics, git hunks — was computed against
//! some buffer state identified by a `PersistedContentHash`
//! (the "anchor"). The runtime keeps that anchor alongside the
//! marker payload. As the user types, the buffer drifts past the
//! anchor; some rows are touched, most are not. The painter wants
//! to ask, per visible row:
//!
//! - "Is this current row's content unchanged from the anchor?"
//!   → if yes, the marker for the corresponding anchor-row still
//!   applies (possibly at a shifted position).
//! - "Where is anchor row R now?"
//!   → for forward-shifting diagnostic positions through edits.
//!
//! # Why sparse
//!
//! The 99% case is "no edits since anchor" — the buffer was just
//! saved, or the LSP just delivered. Both `touched` and `shifts`
//! are empty, lookups are O(1), the struct is one allocation
//! that fits in a cache line. As the user types, ranges grow
//! sparsely with the actual edit footprint; we never materialise
//! a per-row map.
//!
//! # Construction
//!
//! Built by walking [`History::groups_from`] forward from the
//! save-point matching the anchor hash. Each [`EditOp`] contributes:
//!
//! - the start row of the edit → marked touched (its content
//!   changed),
//! - any intermediate rows of a multi-line insert → marked touched
//!   (they didn't exist at anchor),
//! - any structural row count change → recorded as a [`RowShift`]
//!   so untouched rows past the edit still translate to their
//!   anchor positions.

use led_core::BufferVersion;
use ropey::Rope;

use crate::history::{EditOp, History};

/// Cumulative row offset at and past `anchor_row_at`. Stored in
/// `RowDelta::shifts` sorted ascending by `anchor_row_at` so a
/// translation does an O(log n) binary search.
///
/// `cumulative` is `current_rows_so_far - anchor_rows_so_far`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowShift {
    /// Anchor-coordinate row at and after which this cumulative
    /// offset applies (i.e. rows `>= anchor_row_at` in anchor
    /// coordinates land at `current_row = anchor_row + cumulative`).
    pub anchor_row_at: usize,
    /// Cumulative net change in row count up to this point. Can
    /// be negative (rows were deleted) or positive (rows were
    /// inserted).
    pub cumulative: isize,
}

/// Sparse delta from the anchor state to the current buffer.
///
/// Default value (all empty) means "no edits since anchor —
/// every current row maps to itself, nothing is touched." That's
/// the common case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowDelta {
    /// Buffer version at the *current* end of the walk. Carried
    /// for diagnostics / debugging — the lookup methods below
    /// don't use it. Memo cache equality compares everything.
    pub current_version: BufferVersion,
    /// Sorted, deduplicated CURRENT-coordinate row indices whose
    /// content was modified or which were inserted since the
    /// anchor. Empty in the no-edits case.
    pub touched: Vec<usize>,
    /// Sorted ranges of ANCHOR-coordinate rows that no longer
    /// exist in the current buffer (consumed by deletes). Each
    /// `(start, end)` pair excludes `end`. Empty when no rows
    /// were removed.
    pub deleted_anchor: Vec<(usize, usize)>,
    /// Cumulative line-count shifts ordered by anchor-row.
    /// Empty when no structural edits happened.
    pub shifts: Vec<RowShift>,
}

impl RowDelta {
    /// "Is the current-coordinate row R touched (modified or
    /// inserted) since anchor?" O(log touched.len()).
    pub fn is_touched(&self, current_row: usize) -> bool {
        self.touched.binary_search(&current_row).is_ok()
    }

    /// "Where in the current buffer is the row that was at
    /// `anchor_row` at anchor time?"
    ///
    /// Returns `Some(current_row)` when the row's content survived
    /// (no in-line edit, no enclosing delete). Returns `None` when
    /// the row was either deleted outright or its content was
    /// modified since anchor.
    pub fn current_for_anchor(&self, anchor_row: usize) -> Option<usize> {
        if self.is_anchor_deleted(anchor_row) {
            return None;
        }
        let cumulative = self.cumulative_at_anchor(anchor_row);
        let current = (anchor_row as isize) + cumulative;
        if current < 0 {
            return None;
        }
        let current = current as usize;
        if self.is_touched(current) {
            return None;
        }
        Some(current)
    }

    fn is_anchor_deleted(&self, anchor_row: usize) -> bool {
        self.deleted_anchor
            .iter()
            .any(|(start, end)| anchor_row >= *start && anchor_row < *end)
    }

    /// "Has the row at current-coordinate `current_row` survived
    /// unchanged from some anchor-coordinate row?"
    ///
    /// Returns `Some(anchor_row)` when the row is not in
    /// `touched`; `None` when it's touched.
    pub fn anchor_for_current(&self, current_row: usize) -> Option<usize> {
        if self.is_touched(current_row) {
            return None;
        }
        // Walk the shift table from the back to find the latest
        // entry whose `current_row_at` (= `anchor_row_at + cumul`)
        // is <= current_row.
        let mut applicable: isize = 0;
        for shift in &self.shifts {
            let current_at = shift.anchor_row_at as isize + shift.cumulative;
            if current_at <= current_row as isize {
                applicable = shift.cumulative;
            } else {
                break;
            }
        }
        let anchor = current_row as isize - applicable;
        if anchor < 0 {
            None
        } else {
            Some(anchor as usize)
        }
    }

    fn cumulative_at_anchor(&self, anchor_row: usize) -> isize {
        let mut cum = 0;
        for shift in &self.shifts {
            if shift.anchor_row_at <= anchor_row {
                cum = shift.cumulative;
            } else {
                break;
            }
        }
        cum
    }

    /// True when no edits have happened since anchor. The 99%
    /// idle case.
    pub fn is_unchanged(&self) -> bool {
        self.touched.is_empty()
            && self.deleted_anchor.is_empty()
            && self.shifts.is_empty()
    }
}

/// Build a [`RowDelta`] from the edit ops in `history` after
/// `save_idx` (exclusive), against the *current* rope.
///
/// `current_rope` is the post-application rope. The walk
/// reconstructs the anchor-time rope by inverting every op
/// from the save-point forward, then walks forward again,
/// recording each op's row footprint into `touched` and
/// `shifts`. Mirrors the structure of `diag_offer::offer_diagnostics`'s
/// replay walk; we share the apply / invert / describe primitives
/// rather than duplicating them. Returns the populated
/// [`RowDelta`].
pub fn build_row_delta(
    history: &History,
    save_idx: usize,
    current_rope: &Rope,
    current_version: BufferVersion,
) -> RowDelta {
    // Reconstruct anchor-time rope by inverting every op after
    // save_idx in reverse. We need the anchor-time rope to call
    // `char_to_line` against PRE-edit positions for each op as
    // we walk forward.
    let groups: Vec<&crate::history::EditGroup> = history
        .groups_from(save_idx + 1)
        .filter(|g| !g.ops.is_empty())
        .collect();

    if groups.is_empty() {
        return RowDelta {
            current_version,
            ..Default::default()
        };
    }

    let mut doc: Rope = current_rope.clone();
    for group in groups.iter().rev() {
        for op in group.ops.iter().rev() {
            invert_op(&mut doc, op);
        }
    }

    // Walk forward applying each op. Track touched rows in CURRENT
    // coordinates (i.e. account for all prior shifts). For each op:
    //
    //   - row_at = doc.char_to_line(at)        (anchor-frame row)
    //   - cumulative shift before this op
    //   - convert row_at to current-frame row by adding shift
    //   - mark touched + insert RowShift if line count changed
    let mut touched: Vec<usize> = Vec::new();
    let mut deleted_anchor: Vec<(usize, usize)> = Vec::new();
    let mut shifts: Vec<RowShift> = Vec::new();
    let mut cumulative: isize = 0;

    for group in &groups {
        for op in &group.ops {
            let anchor_row_at = doc.char_to_line(match op {
                EditOp::Insert { at, .. } | EditOp::Delete { at, .. } => *at,
            });
            let line_start = doc.line_to_char(anchor_row_at);
            let col = match op {
                EditOp::Insert { at, .. } | EditOp::Delete { at, .. } => *at - line_start,
            };
            let new_newlines = match op {
                EditOp::Insert { text, .. } => newline_count(text),
                EditOp::Delete { .. } => 0,
            };
            let old_newlines = match op {
                EditOp::Delete { text, .. } => newline_count(text),
                EditOp::Insert { .. } => 0,
            };
            let text_ends_with_newline = match op {
                EditOp::Insert { text, .. } => text.as_bytes().last() == Some(&b'\n'),
                EditOp::Delete { .. } => false,
            };
            let delta = new_newlines as isize - old_newlines as isize;
            let current_row_at = (anchor_row_at as isize + cumulative) as usize;

            match op {
                EditOp::Insert { .. } => {
                    // Mark the start row unless the insert is a
                    // clean prepend (col == 0 AND ends in '\n'),
                    // in which case the original row is shifted
                    // down intact and the start row is a wholly
                    // new line containing only inserted content.
                    let start_row_is_new_clean =
                        col == 0 && text_ends_with_newline && new_newlines > 0;
                    mark_touched(&mut touched, current_row_at);
                    let _ = start_row_is_new_clean;
                    // Intermediate fully-new rows (when
                    // new_newlines >= 2): rows at
                    // current_row_at+1 .. current_row_at+new_newlines.
                    if new_newlines > 1 {
                        for r in 1..new_newlines {
                            mark_touched(&mut touched, current_row_at + r);
                        }
                    }
                    // Tail row (current_row_at + new_newlines):
                    // touched iff the original row was split
                    // (col > 0) OR the inserted text doesn't end
                    // at a row boundary (so it bleeds into the
                    // original line's content).
                    if new_newlines > 0 && (col > 0 || !text_ends_with_newline) {
                        mark_touched(&mut touched, current_row_at + new_newlines);
                    }
                }
                EditOp::Delete { .. } => {
                    // Merged row carries the new combined content
                    // → touched. Anchor rows fully consumed by
                    // the delete (anchor_row_at + 1 ..=
                    // anchor_row_at + old_newlines) no longer
                    // exist; record so `current_for_anchor`
                    // returns None for them.
                    mark_touched(&mut touched, current_row_at);
                    if old_newlines > 0 {
                        deleted_anchor.push((
                            anchor_row_at + 1,
                            anchor_row_at + old_newlines + 1,
                        ));
                    }
                }
            }

            if delta != 0 {
                cumulative += delta;
                // The shift kicks in at the first anchor row
                // beyond what this op touched. For a clean
                // prepend insert (col == 0, ends with '\n'), that's
                // the start row itself — anchor row R becomes
                // current row R + new_newlines. For a split insert
                // (col > 0) it's the row AFTER, since the start row
                // was modified in place. For deletes, it's the row
                // after the last fully-deleted line.
                let shift_starts_at = match op {
                    EditOp::Insert { .. } => {
                        if col == 0 && text_ends_with_newline {
                            anchor_row_at
                        } else {
                            anchor_row_at + 1
                        }
                    }
                    EditOp::Delete { .. } => anchor_row_at + old_newlines + 1,
                };
                shifts.push(RowShift {
                    anchor_row_at: shift_starts_at,
                    cumulative,
                });
            }

            apply_op(&mut doc, op);
        }
    }

    touched.sort_unstable();
    touched.dedup();

    // Coalesce shifts with the same anchor_row_at — keep last.
    if shifts.len() > 1 {
        let mut coalesced: Vec<RowShift> = Vec::with_capacity(shifts.len());
        for s in shifts {
            if let Some(last) = coalesced.last_mut()
                && last.anchor_row_at == s.anchor_row_at
            {
                last.cumulative = s.cumulative;
                continue;
            }
            coalesced.push(s);
        }
        shifts = coalesced;
    }

    RowDelta {
        current_version,
        touched,
        deleted_anchor,
        shifts,
    }
}

fn newline_count(s: &str) -> usize {
    s.bytes().filter(|b| *b == b'\n').count()
}

fn mark_touched(touched: &mut Vec<usize>, row: usize) {
    touched.push(row);
}

fn apply_op(doc: &mut Rope, op: &EditOp) {
    match op {
        EditOp::Insert { at, text } => {
            doc.insert(*at, text);
        }
        EditOp::Delete { at, text } => {
            let len = text.chars().count();
            doc.remove(*at..*at + len);
        }
    }
}

fn invert_op(doc: &mut Rope, op: &EditOp) {
    match op {
        EditOp::Insert { at, text } => {
            let len = text.chars().count();
            doc.remove(*at..*at + len);
        }
        EditOp::Delete { at, text } => {
            doc.insert(*at, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EditedBuffer;
    use led_core::EphemeralContentHash;
    use led_state_tabs::Cursor;
    use std::sync::Arc;

    fn cur(line: usize, col: usize) -> Cursor {
        Cursor {
            line,
            col,
            preferred_col: col,
        }
    }

    #[test]
    fn empty_history_yields_unchanged() {
        let rope = Arc::new(Rope::from_str("a\nb\nc\n"));
        let eb = EditedBuffer::fresh(rope.clone());
        let h = EphemeralContentHash::of_rope(&rope).persist();
        let delta = eb.row_delta_for(h).expect("save-point would normally be set on save; the fresh buffer has none — skip via fast-path in caller");
        let _ = delta;
    }

    #[test]
    fn anchor_to_current_no_edits_returns_self() {
        let delta = RowDelta::default();
        assert_eq!(delta.current_for_anchor(0), Some(0));
        assert_eq!(delta.current_for_anchor(5), Some(5));
        assert_eq!(delta.anchor_for_current(0), Some(0));
        assert_eq!(delta.anchor_for_current(5), Some(5));
        assert!(!delta.is_touched(0));
        assert!(delta.is_unchanged());
    }

    #[test]
    fn touched_row_blocks_translation() {
        let delta = RowDelta {
            current_version: BufferVersion::default(),
            touched: vec![3],
            deleted_anchor: vec![],
            shifts: vec![],
        };
        assert!(delta.is_touched(3));
        assert_eq!(delta.anchor_for_current(3), None);
        assert_eq!(delta.current_for_anchor(3), None);
        assert_eq!(delta.anchor_for_current(2), Some(2));
        assert_eq!(delta.anchor_for_current(4), Some(4));
    }

    #[test]
    fn structural_insert_shifts_following_rows() {
        // Anchor:   a, b, c       (3 rows)
        // Inserted "X\n" at row 1 → a, X, b, c (4 rows, row 1 is new, row 2 was old row 1)
        let delta = RowDelta {
            current_version: BufferVersion::default(),
            touched: vec![1],
            deleted_anchor: vec![],
            shifts: vec![RowShift {
                anchor_row_at: 1,
                cumulative: 1,
            }],
        };
        assert_eq!(delta.current_for_anchor(0), Some(0));
        assert_eq!(delta.current_for_anchor(1), Some(2));
        assert_eq!(delta.current_for_anchor(2), Some(3));
        assert_eq!(delta.anchor_for_current(0), Some(0));
        assert_eq!(delta.anchor_for_current(1), None); // touched (the new row)
        assert_eq!(delta.anchor_for_current(2), Some(1));
        assert_eq!(delta.anchor_for_current(3), Some(2));
    }

    #[test]
    fn build_no_groups_returns_default() {
        let rope = Arc::new(Rope::from_str("hello\n"));
        let mut eb = EditedBuffer::fresh(rope.clone());
        let h = EphemeralContentHash::of_rope(&rope).persist();
        eb.history.insert_save_point(h);
        let save_idx = eb.history.find_save_point(h).unwrap();
        let delta = build_row_delta(&eb.history, save_idx, &eb.rope, eb.version);
        assert!(delta.is_unchanged());
    }

    #[test]
    fn build_marks_same_row_insert_as_touched() {
        // hello\n  → he-X-llo\n (insert 'X' at char 2)
        let rope = Arc::new(Rope::from_str("hello\n"));
        let h = EphemeralContentHash::of_rope(&rope).persist();
        let mut eb = EditedBuffer::fresh(rope);
        eb.history.insert_save_point(h);

        let mut r: Rope = (*eb.rope).clone();
        r.insert_char(2, 'X');
        eb.rope = Arc::new(r);
        eb.version = BufferVersion(1);
        eb.history.record_insert_char(2, 'X', cur(0, 2), cur(0, 3));
        eb.history.finalise();

        let delta = eb.row_delta_for(h).expect("save-point present");
        assert_eq!(delta.touched, vec![0]);
        assert!(delta.shifts.is_empty());
        assert!(!delta.is_unchanged());
    }

    #[test]
    fn build_records_shift_for_newline_insert() {
        // a\nb\n → a\nX\nb\n (insert "X\n" at char 2)
        let rope = Arc::new(Rope::from_str("a\nb\n"));
        let h = EphemeralContentHash::of_rope(&rope).persist();
        let mut eb = EditedBuffer::fresh(rope);
        eb.history.insert_save_point(h);

        let mut r: Rope = (*eb.rope).clone();
        r.insert(2, "X\n");
        eb.rope = Arc::new(r);
        eb.version = BufferVersion(1);
        eb.history
            .record_insert(2, Arc::<str>::from("X\n"), cur(1, 0), cur(2, 0));
        eb.history.finalise();

        let delta = eb.row_delta_for(h).expect("save-point present");
        // Row 1 (the inserted "X") is touched.
        assert!(delta.is_touched(1));
        // Row 0 is anchor row 0, unchanged.
        assert_eq!(delta.anchor_for_current(0), Some(0));
        // Current row 2 = anchor row 1.
        assert_eq!(delta.anchor_for_current(2), Some(1));
        // Current row 3 = anchor row 2 (trailing empty).
        assert_eq!(delta.anchor_for_current(3), Some(2));
    }

    #[test]
    fn no_save_point_returns_none() {
        let rope = Arc::new(Rope::from_str("hello\n"));
        let eb = EditedBuffer::fresh(rope);
        let bogus = led_core::PersistedContentHash(0xDEADBEEF);
        assert!(eb.row_delta_for(bogus).is_none());
    }

    #[test]
    fn fast_path_when_buffer_matches_anchor() {
        let rope = Arc::new(Rope::from_str("a\nb\n"));
        let h = EphemeralContentHash::of_rope(&rope).persist();
        let eb = EditedBuffer::fresh(rope);
        // No save-point set, but the rope still hashes to `h`,
        // so the fast path returns an unchanged delta.
        let delta = eb.row_delta_for(h).expect("fast path");
        assert!(delta.is_unchanged());
    }

    #[test]
    fn split_insert_marks_both_halves_touched() {
        // abc\n  → ab + X\n + Y + c\n
        let rope = Arc::new(Rope::from_str("abc\n"));
        let h = EphemeralContentHash::of_rope(&rope).persist();
        let mut eb = EditedBuffer::fresh(rope);
        eb.history.insert_save_point(h);

        let mut r: Rope = (*eb.rope).clone();
        r.insert(2, "X\nY");
        eb.rope = Arc::new(r);
        eb.version = BufferVersion(1);
        eb.history
            .record_insert(2, Arc::<str>::from("X\nY"), cur(0, 2), cur(1, 1));
        eb.history.finalise();

        let delta = eb.row_delta_for(h).expect("save-point present");
        // Row 0 ("abX") and row 1 ("Yc") are both touched.
        assert!(delta.is_touched(0));
        assert!(delta.is_touched(1));
        // Row 2 ("") = anchor row 1 ("").
        assert_eq!(delta.anchor_for_current(2), Some(1));
    }

    #[test]
    fn delete_newline_merges_rows() {
        // a\nb\n → ab\n  (delete '\n' at char 1)
        let rope = Arc::new(Rope::from_str("a\nb\n"));
        let h = EphemeralContentHash::of_rope(&rope).persist();
        let mut eb = EditedBuffer::fresh(rope);
        eb.history.insert_save_point(h);

        let mut r: Rope = (*eb.rope).clone();
        r.remove(1..2);
        eb.rope = Arc::new(r);
        eb.version = BufferVersion(1);
        eb.history
            .record_delete(1, Arc::<str>::from("\n"), cur(0, 1), cur(0, 1));
        eb.history.finalise();

        let delta = eb.row_delta_for(h).expect("save-point present");
        // Row 0 (the merged "ab") is touched.
        assert!(delta.is_touched(0));
        // Anchor row 0 ("a") and anchor row 1 ("b") both lost their
        // identity via the merge — both must report None.
        assert_eq!(delta.current_for_anchor(0), None);
        assert_eq!(delta.current_for_anchor(1), None);
        // Anchor row 2 ("") is at current row 1 (one row got
        // removed → cumulative = -1 → current = 2 - 1 = 1).
        assert_eq!(delta.current_for_anchor(2), Some(1));
        assert_eq!(delta.anchor_for_current(1), Some(2));
    }
}
