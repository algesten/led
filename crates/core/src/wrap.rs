//! Soft-line-wrap geometry — rope-aware, display-cell-based.
//!
//! Rendering wraps each logical line into one or more **sub-lines**
//! (visual rows). A sub-line is a contiguous **char range** within
//! a logical line that occupies at most `wrap_width(content_cols)`
//! **display cells**. Wide chars (CJK, emoji) are 2 cells; combining
//! marks are 0 cells; tabs are next-tab-stop wide.
//!
//! # Wrap policy
//!
//! - Lines that fit within `wrap_width(content_cols)` cells produce
//!   one sub-line covering every grapheme cluster in the line.
//! - Longer lines are split into chunks. Each **non-last** chunk
//!   holds as many graphemes as fit in `wrap_width` cells (greedy);
//!   the painter uses the final cell of the row for a `\`
//!   continuation glyph, so what would have wrapped onto the
//!   reserved cell spills onto the next sub-line.
//! - The **last** chunk holds whatever's left, up to `content_cols`
//!   cells.
//!
//! Leaving the final cell for `\` matches the legacy wrap UX: users
//! see exactly where one logical line wraps. The `wrap_width` /
//! `content_cols <= 1` short-circuits stay verbatim from the
//! pre-M25 implementation.
//!
//! # Coordinates
//!
//! - **Grapheme col** — a 0-based count of grapheme clusters in
//!   the line (`Cursor::col`'s unit).
//! - **Char idx** — a 0-based char offset relative to the line's
//!   start (rope-friendly).
//! - **Display cell** — a 0-based count of terminal cells.
//!
//! [`SubLineRange`] returns char idx + cell count. Cursor placement
//! uses [`col_to_sub_line`] (grapheme col → display cell within sub)
//! and [`sub_line_cells_to_grapheme_col`] (display cell back to
//! grapheme col).

use crate::grapheme::grapheme_display_width;
use ropey::RopeSlice;
use unicode_segmentation::UnicodeSegmentation;

/// 0-based index of a sub-line within its enclosing logical line.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, drv::Input,
    serde::Serialize, serde::Deserialize,
)]
pub struct SubLine(pub usize);

/// One sub-line's footprint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubLineRange {
    /// First char index (in the line) covered by this sub-line.
    pub char_start: usize,
    /// One past the last char index covered.
    pub char_end: usize,
    /// Display cells the sub-line occupies. ≤ `wrap_width(content_cols)`
    /// for non-last subs; ≤ `content_cols` for the last sub.
    pub cells: usize,
}

/// Width of a non-last sub-line, in display cells.
const fn wrap_width(content_cols: usize) -> usize {
    if content_cols <= 1 { content_cols } else { content_cols - 1 }
}

/// Walk a logical line's grapheme clusters and return one
/// [`SubLineRange`] per visual row. The vector is non-empty: empty
/// lines yield a single zero-width sub-line so cursor arithmetic
/// always has a row to land on.
///
/// Hot-loop callers (paint, scroll math, cursor placement) should
/// call this **once** per logical line and reuse the result for
/// every per-sub query (sub count, range, is-continued, cell
/// width). The targeted single-shot helpers
/// [`sub_line_count`] / [`sub_line_range`] / [`is_continued`] all
/// re-walk internally — fine for one-off lookups, wasteful in a
/// per-row loop.
pub fn line_layout(line: RopeSlice<'_>, content_cols: usize) -> Vec<SubLineRange> {
    // Materialise the line content (sans newline). Mirrors
    // grapheme::line_content; kept inline so wrap doesn't pay a
    // double-walk. The allocation is amortised across all
    // per-line consumers via [`line_layout`].
    let mut s: String = line.chars().collect();
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }

    if content_cols <= 1 {
        // Degenerate viewport — render as one sub.
        let chars = s.chars().count();
        let cells = total_cells(&s);
        return vec![SubLineRange {
            char_start: 0,
            char_end: chars,
            cells,
        }];
    }

    let ww = wrap_width(content_cols);
    let mut out = Vec::with_capacity(1);
    let mut sub_start_char = 0usize;
    let mut sub_cells = 0usize;
    let mut chars_consumed = 0usize;

    for g in s.graphemes(true) {
        let g_chars = g.chars().count();
        let g_cells = grapheme_display_width(g, sub_cells);
        if sub_cells + g_cells > ww {
            // Close the current sub before this grapheme.
            out.push(SubLineRange {
                char_start: sub_start_char,
                char_end: chars_consumed,
                cells: sub_cells,
            });
            sub_start_char = chars_consumed;
            // Recompute g_cells for the new sub (matters for tabs:
            // a tab's width depends on the running cell column).
            sub_cells = grapheme_display_width(g, 0);
            chars_consumed += g_chars;
            continue;
        }
        sub_cells += g_cells;
        chars_consumed += g_chars;
    }

    // Always emit the trailing sub, even if empty (so an empty line
    // still produces one row, and end-of-line cursors have somewhere
    // to land).
    out.push(SubLineRange {
        char_start: sub_start_char,
        char_end: chars_consumed,
        cells: sub_cells,
    });

    out
}

fn total_cells(s: &str) -> usize {
    let mut acc = 0usize;
    for g in s.graphemes(true) {
        acc += grapheme_display_width(g, acc);
    }
    acc
}

/// How many sub-lines `line` wraps to under `content_cols`. Always
/// ≥ 1. Single-shot — call [`line_layout`] when you need the count
/// **plus** the per-sub ranges in the same loop.
pub fn sub_line_count(line: RopeSlice<'_>, content_cols: usize) -> usize {
    line_layout(line, content_cols).len()
}

/// The char-index range and cell width of sub-line `sub`. Out-of-
/// range `sub` returns an empty range anchored at the line's char
/// length. Single-shot — call [`line_layout`] when iterating
/// multiple subs of the same logical line.
pub fn sub_line_range(
    sub: SubLine,
    line: RopeSlice<'_>,
    content_cols: usize,
) -> SubLineRange {
    let subs = line_layout(line, content_cols);
    if let Some(r) = subs.get(sub.0).copied() {
        return r;
    }
    let last = subs.last().copied().unwrap_or_default();
    SubLineRange {
        char_start: last.char_end,
        char_end: last.char_end,
        cells: 0,
    }
}

/// `true` when `sub` is **not** the final sub-line (painter draws
/// `\` on this row). Single-shot — when `line_layout` is already
/// in scope, use `sub.0 + 1 < layout.len()` directly.
pub fn is_continued(sub: SubLine, line: RopeSlice<'_>, content_cols: usize) -> bool {
    sub.0 + 1 < sub_line_count(line, content_cols)
}

/// Internal: build the full grapheme→(sub, cells_at_start) mapping.
/// Each entry is `(sub, cells_at_start_within_sub)` for the cursor
/// position immediately before grapheme `i`. Plus a final entry for
/// the past-end cursor.
fn cursor_positions(line: RopeSlice<'_>, content_cols: usize) -> Vec<(usize, usize)> {
    let mut s: String = line.chars().collect();
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }

    let mut out = Vec::with_capacity(8);
    if content_cols <= 1 {
        // Degenerate — every cursor position lives on sub 0; cells
        // equals the grapheme index (the painter clamps anyway).
        for i in 0..=s.graphemes(true).count() {
            out.push((0, i));
        }
        return out;
    }

    let ww = wrap_width(content_cols);
    let mut sub: usize = 0;
    let mut sub_cells: usize = 0;

    // Cursor before grapheme 0.
    out.push((sub, sub_cells));

    for g in s.graphemes(true) {
        let g_cells_at_cur = grapheme_display_width(g, sub_cells);
        if sub_cells + g_cells_at_cur > ww {
            // This grapheme starts a new sub. The cursor "before"
            // this grapheme is at the start of the new sub.
            sub += 1;
            sub_cells = 0;
            // Adjust the just-pushed entry to be on the new sub.
            *out.last_mut().expect("seeded entry") = (sub, sub_cells);
            // Now consume the grapheme on the new sub.
            sub_cells += grapheme_display_width(g, 0);
        } else {
            sub_cells += g_cells_at_cur;
        }
        out.push((sub, sub_cells));
    }
    out
}

/// Where does grapheme col `gcol` fall? Returns the sub-line and the
/// **display cell** offset within that sub-line.
///
/// At a wrap boundary, the cursor lives at the **start** of the next
/// sub-line, not the end of the previous. End-of-line cursors past
/// every wrap boundary live on the final sub-line at its end cell.
pub fn col_to_sub_line(
    gcol: usize,
    line: RopeSlice<'_>,
    content_cols: usize,
) -> (SubLine, usize) {
    let positions = cursor_positions(line, content_cols);
    let last_idx = positions.len().saturating_sub(1);
    let (sub, cells) = positions
        .get(gcol)
        .copied()
        .unwrap_or_else(|| positions[last_idx]);
    (SubLine(sub), cells)
}

/// Inverse of [`col_to_sub_line`]: given a sub-line and a target
/// **display cell** offset within it, return the largest grapheme
/// col whose cell prefix `≤ cells_within`. When the target cell
/// falls in the middle of a wide glyph, snaps to the cluster start.
pub fn sub_line_cells_to_grapheme_col(
    sub: SubLine,
    cells_within: usize,
    line: RopeSlice<'_>,
    content_cols: usize,
) -> usize {
    let target_sub = sub.0;
    let positions = cursor_positions(line, content_cols);

    let mut best: Option<usize> = None;
    for (g, (sub_at, cells_at)) in positions.iter().enumerate() {
        if *sub_at == target_sub && *cells_at <= cells_within {
            best = Some(g);
        }
        if *sub_at > target_sub {
            break;
        }
    }
    // Fallback: last position on or before target_sub.
    best.unwrap_or_else(|| {
        positions
            .iter()
            .enumerate()
            .rfind(|(_, (s, _))| *s <= target_sub)
            .map(|(g, _)| g)
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn r(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn empty_line_is_one_sub_line() {
        let rope = r("");
        let line = rope.line(0);
        assert_eq!(sub_line_count(line, 40), 1);
        assert_eq!(
            sub_line_range(SubLine(0), line, 40),
            SubLineRange { char_start: 0, char_end: 0, cells: 0 }
        );
        assert!(!is_continued(SubLine(0), line, 40));
    }

    #[test]
    fn short_line_fits_on_one_sub_line() {
        let rope = r("hello");
        let line = rope.line(0);
        assert_eq!(sub_line_count(line, 40), 1);
        let r = sub_line_range(SubLine(0), line, 40);
        assert_eq!(r.char_start, 0);
        assert_eq!(r.char_end, 5);
        assert_eq!(r.cells, 5);
        assert!(!is_continued(SubLine(0), line, 40));
    }

    #[test]
    fn line_exactly_wrap_width_wide_fits_one_sub_line() {
        let rope = r("123456789"); // 9 chars, content_cols=10, ww=9.
        let line = rope.line(0);
        assert_eq!(sub_line_count(line, 10), 1);
    }

    #[test]
    fn line_one_past_wrap_width_wraps_to_two() {
        let rope = r("1234567890"); // 10 chars, content_cols=10, ww=9.
        let line = rope.line(0);
        assert_eq!(sub_line_count(line, 10), 2);
        let r0 = sub_line_range(SubLine(0), line, 10);
        let r1 = sub_line_range(SubLine(1), line, 10);
        assert_eq!((r0.char_start, r0.char_end, r0.cells), (0, 9, 9));
        assert_eq!((r1.char_start, r1.char_end, r1.cells), (9, 10, 1));
        assert!(is_continued(SubLine(0), line, 10));
        assert!(!is_continued(SubLine(1), line, 10));
    }

    #[test]
    fn wide_chars_wrap_at_cell_boundary() {
        // 10 ASCII + 1 CJK glyph (2 cells). Total cells = 12.
        // content_cols=12, ww=11. Cells 10 fit; +CJK=12 > 11 → wrap.
        let rope = r("aaaaaaaaaa你");
        let line = rope.line(0);
        assert_eq!(sub_line_count(line, 12), 2);
        let r0 = sub_line_range(SubLine(0), line, 12);
        let r1 = sub_line_range(SubLine(1), line, 12);
        assert_eq!((r0.char_start, r0.char_end, r0.cells), (0, 10, 10));
        assert_eq!((r1.char_start, r1.char_end, r1.cells), (10, 11, 2));
    }

    #[test]
    fn combining_marks_dont_force_wrap() {
        // "cafe\u{0301}" — 5 chars, 4 graphemes, 4 cells.
        let rope = r("cafe\u{0301}");
        let line = rope.line(0);
        assert_eq!(sub_line_count(line, 5), 1);
    }

    #[test]
    fn col_to_sub_line_basic() {
        let rope = r("1234567890abc"); // 13 chars, ww=9
        let line = rope.line(0);
        // Cursor at grapheme 0 → sub 0, cell 0
        assert_eq!(col_to_sub_line(0, line, 10), (SubLine(0), 0));
        // Cursor at grapheme 8 → sub 0, cell 8
        assert_eq!(col_to_sub_line(8, line, 10), (SubLine(0), 8));
        // Cursor at grapheme 9 → wraps; sub 1, cell 0
        assert_eq!(col_to_sub_line(9, line, 10), (SubLine(1), 0));
        // Cursor at grapheme 13 (end) → sub 1, cell 4
        assert_eq!(col_to_sub_line(13, line, 10), (SubLine(1), 4));
    }

    #[test]
    fn col_to_sub_line_with_cjk() {
        // "abc你好def" — 8 graphemes, 10 cells.
        // content_cols=10, ww=9. Cells per grapheme:
        //   a=1, b=1, c=1, 你=2, 好=2, d=1, e=1, f=1
        // Running cells before each: 0,1,2,3,5,7,8,9 (then f wraps).
        let rope = r("abc你好def");
        let line = rope.line(0);
        assert_eq!(col_to_sub_line(0, line, 10), (SubLine(0), 0));
        assert_eq!(col_to_sub_line(3, line, 10), (SubLine(0), 3));
        assert_eq!(col_to_sub_line(4, line, 10), (SubLine(0), 5));
        assert_eq!(col_to_sub_line(5, line, 10), (SubLine(0), 7));
        // Grapheme 7 (`f`) — 9+1>9, wraps to sub 1, cell 0.
        assert_eq!(col_to_sub_line(7, line, 10), (SubLine(1), 0));
        // Past-end cursor lands at end of sub 1.
        assert_eq!(col_to_sub_line(8, line, 10), (SubLine(1), 1));
    }

    fn grapheme_count(line: RopeSlice<'_>) -> usize {
        let mut s: String = line.chars().collect();
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
        s.graphemes(true).count()
    }

    #[test]
    fn round_trip_col_to_sub_to_col() {
        let rope = r("hello world this is a longer test line for wrapping");
        let line = rope.line(0);
        let cc = 20;
        for g in 0..=grapheme_count(line) {
            let (sub, within) = col_to_sub_line(g, line, cc);
            let back = sub_line_cells_to_grapheme_col(sub, within, line, cc);
            assert_eq!(back, g, "round-trip failed for g={g}");
        }
    }

    #[test]
    fn content_cols_one_or_zero_is_a_degenerate_no_op() {
        let rope = r("hello");
        let line = rope.line(0);
        assert_eq!(sub_line_count(line, 0), 1);
        assert_eq!(sub_line_count(line, 1), 1);
        let r = sub_line_range(SubLine(0), line, 0);
        assert_eq!(r.char_end, 5);
        assert_eq!(col_to_sub_line(2, line, 0), (SubLine(0), 2));
    }
}
