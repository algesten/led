//! Soft-line-wrap geometry — pure text math, no rope / driver deps.
//!
//! Rendering wraps each logical line into one or more **sub-lines**
//! (visual rows). A sub-line is a contiguous character range within
//! a logical line; rendering emits one `BodyLine` per sub-line so a
//! 200-char logical line displays across several visible rows when
//! the editor viewport is narrower.
//!
//! # Wrap policy (direct port of legacy `led_core::wrap`)
//!
//! - Lines that fit within `content_cols` chars produce one
//!   sub-line holding the whole line.
//! - Longer lines are split into chunks. Each **non-last** chunk
//!   holds `content_cols - 1` chars — the painter uses the final
//!   column for a `\` continuation glyph, so content that would
//!   have been there spills onto the next sub-line.
//! - The **last** chunk holds whatever's left, up to `content_cols`
//!   chars (no continuation glyph — the line ends here).
//!
//! Leaving the final column for `\` matches legacy's visible
//! behaviour: users see exactly where one logical line wraps.

/// 0-based index of a sub-line within its enclosing logical line.
/// `SubLine(0)` is the first sub-line, which always starts at col 0.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, drv::Input,
    serde::Serialize, serde::Deserialize,
)]
pub struct SubLine(pub usize);

/// Width of a non-last sub-line, in chars. Equals
/// `content_cols - 1` — exactly one trailing col is reserved for
/// the `\` continuation glyph, so content fills every column up
/// to (but not including) the last. Matches emacs's display
/// behaviour and the legacy led painter: no blank interior col
/// before the glyph, no blank col after, `\` flush against the
/// terminal's right edge. `content_cols <= 1` disables wrapping
/// (degenerate narrow viewports render as a single row).
const fn wrap_width(content_cols: usize) -> usize {
    if content_cols <= 1 { content_cols } else { content_cols - 1 }
}

/// How many sub-lines a logical line of `line_char_len` chars
/// wraps to under `content_cols`. Always at least 1 — an empty
/// line still shows up as a single blank visual row.
///
/// Every sub — last included — caps at `wrap_width` chars. A
/// line that fits exactly in `wrap_width` chars renders as one
/// sub with no glyph; one more char forces a second sub so the
/// wrap marker (`\` at the rightmost col) and the overflow char
/// can both be shown. This keeps the cursor always in a valid
/// visible column: at EOL of a line whose length happens to equal
/// `content_cols` (one past `wrap_width`), the previous version
/// let the last sub fill the row and pushed the cursor off the
/// right edge — the user then sees content ending without a `\`
/// and no way to tell the line actually continues conceptually.
pub fn sub_line_count(line_char_len: usize, content_cols: usize) -> usize {
    if content_cols <= 1 {
        return 1;
    }
    let ww = wrap_width(content_cols);
    if line_char_len <= ww {
        return 1;
    }
    // Repeatedly shave `ww` off the remaining width, until what's
    // left fits in one `wrap_width`-wide row. Always 1 more than
    // the count of `ww`-sized chunks.
    let mut count = 0;
    let mut remaining = line_char_len;
    while remaining > ww {
        count += 1;
        remaining -= ww;
    }
    count + 1
}

/// The `[col_start, col_end)` char range of sub-line `sub` on a
/// logical line of `line_char_len` chars, wrapped at
/// `content_cols`. Non-last sub-lines span `content_cols - 1`
/// chars; the last sub-line holds whatever's left.
///
/// Callers are responsible for clamping `sub` to the valid range
/// reported by [`sub_line_count`]; out-of-range `sub` returns an
/// empty range anchored at `line_char_len`.
pub fn sub_line_range(
    sub: SubLine,
    line_char_len: usize,
    content_cols: usize,
) -> (usize, usize) {
    if content_cols <= 1 {
        return (0, line_char_len);
    }
    let ww = wrap_width(content_cols);
    if line_char_len <= ww {
        return (0, line_char_len);
    }
    let start = sub.0.saturating_mul(ww);
    if start >= line_char_len {
        return (line_char_len, line_char_len);
    }
    let remaining = line_char_len - start;
    let end = if remaining <= ww {
        // Last chunk — takes the rest, up to `wrap_width` chars.
        line_char_len
    } else {
        // Non-last chunk — exactly `wrap_width` chars of content.
        start + ww
    };
    (start, end)
}

/// `true` when `sub` is **not** the final sub-line of a logical
/// line of `line_char_len` chars — i.e. the painter should draw
/// a `\` continuation glyph on this visual row.
pub fn is_continued(
    sub: SubLine,
    line_char_len: usize,
    content_cols: usize,
) -> bool {
    sub.0 + 1 < sub_line_count(line_char_len, content_cols)
}

/// Which sub-line a given column falls on, along with the column
/// **within** that sub-line (0-based). Inverse of
/// [`sub_line_range`].
///
/// Cursor at the exact wrap boundary (`col == k * wrap_width` for
/// `k >= 1`) lives at the START of sub-line `k`, not at the
/// visual end of sub-line `k-1`. Cursor past the end of the
/// penultimate chunk but still within the logical line lives on
/// the last sub-line.
pub fn col_to_sub_line(
    col: usize,
    line_char_len: usize,
    content_cols: usize,
) -> (SubLine, usize) {
    if content_cols <= 1 {
        return (SubLine(0), col);
    }
    let ww = wrap_width(content_cols);
    if line_char_len <= ww {
        return (SubLine(0), col);
    }
    let count = sub_line_count(line_char_len, content_cols);
    let last_start = (count - 1).saturating_mul(ww);
    if col >= last_start {
        // Last sub-line absorbs every col past the penultimate
        // wrap boundary, including an end-of-line cursor.
        return (SubLine(count - 1), col - last_start);
    }
    (SubLine(col / ww), col % ww)
}

/// Inverse of [`col_to_sub_line`]: given a sub-line and a column
/// within it, return the logical-line column. Clamps
/// `col_within` to the sub-line's width so invalid inputs still
/// land on a valid boundary.
pub fn sub_line_col_to_line_col(
    sub: SubLine,
    col_within: usize,
    line_char_len: usize,
    content_cols: usize,
) -> usize {
    if content_cols <= 1 {
        return col_within.min(line_char_len);
    }
    let ww = wrap_width(content_cols);
    if line_char_len <= ww {
        return col_within.min(line_char_len);
    }
    let start = sub.0.saturating_mul(ww);
    // Every sub — last included — caps at `ww` chars; the last
    // one may be shorter when the line doesn't divide evenly.
    let count = sub_line_count(line_char_len, content_cols);
    let width = if sub.0 + 1 == count {
        line_char_len.saturating_sub(start)
    } else {
        ww
    };
    start + col_within.min(width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line_is_one_sub_line() {
        assert_eq!(sub_line_count(0, 40), 1);
        assert_eq!(sub_line_range(SubLine(0), 0, 40), (0, 0));
        assert!(!is_continued(SubLine(0), 0, 40));
    }

    #[test]
    fn short_line_fits_on_one_sub_line() {
        assert_eq!(sub_line_count(10, 40), 1);
        assert_eq!(sub_line_range(SubLine(0), 10, 40), (0, 10));
        assert!(!is_continued(SubLine(0), 10, 40));
    }

    #[test]
    fn line_exactly_wrap_width_wide_fits_in_one_sub_line() {
        // A 9-char line at content_cols=10 (wrap_width=9) holds
        // the whole line on one row — no continuation needed.
        assert_eq!(sub_line_count(9, 10), 1);
        assert_eq!(sub_line_range(SubLine(0), 9, 10), (0, 9));
        assert!(!is_continued(SubLine(0), 9, 10));
    }

    #[test]
    fn line_longer_than_wrap_width_wraps_even_when_short_of_content_cols() {
        // A 10-char line at content_cols=10 (wrap_width=9) overflows
        // `wrap_width` by one — wraps into a 9+1 split so the last
        // char and the cursor-past-EOL position both stay visible.
        assert_eq!(sub_line_count(10, 10), 2);
        assert_eq!(sub_line_range(SubLine(0), 10, 10), (0, 9));
        assert_eq!(sub_line_range(SubLine(1), 10, 10), (9, 10));
        assert!(is_continued(SubLine(0), 10, 10));
        assert!(!is_continued(SubLine(1), 10, 10));
    }

    #[test]
    fn wrapped_line_leaves_last_col_for_continuation_glyph() {
        // 25 chars, content_cols=10, wrap_width=9. One trailing
        // col reserved per non-last sub (the `\`).
        // Sub 0 = [0, 9), sub 1 = [9, 18), sub 2 = [18, 25).
        assert_eq!(sub_line_count(25, 10), 3);
        assert_eq!(sub_line_range(SubLine(0), 25, 10), (0, 9));
        assert_eq!(sub_line_range(SubLine(1), 25, 10), (9, 18));
        assert_eq!(sub_line_range(SubLine(2), 25, 10), (18, 25));
        assert!(is_continued(SubLine(0), 25, 10));
        assert!(is_continued(SubLine(1), 25, 10));
        assert!(!is_continued(SubLine(2), 25, 10));
    }

    #[test]
    fn col_to_sub_line_uses_wrap_width_not_content_cols() {
        // content_cols=10, wrap_width=9.
        assert_eq!(col_to_sub_line(0, 25, 10), (SubLine(0), 0));
        assert_eq!(col_to_sub_line(8, 25, 10), (SubLine(0), 8));
        // Col 9 is the start of sub-line 1 (boundary → next sub).
        assert_eq!(col_to_sub_line(9, 25, 10), (SubLine(1), 0));
        assert_eq!(col_to_sub_line(17, 25, 10), (SubLine(1), 8));
        // Col 18 is the start of the LAST sub-line, which holds
        // the rest of the line (up to 10 chars wide).
        assert_eq!(col_to_sub_line(18, 25, 10), (SubLine(2), 0));
        // End-of-line cursor at col 25 lives on the last sub-line.
        assert_eq!(col_to_sub_line(25, 25, 10), (SubLine(2), 7));
    }

    #[test]
    fn sub_line_col_round_trips() {
        for col in [0, 5, 8, 9, 10, 17, 18, 24, 25] {
            let (sub, within) = col_to_sub_line(col, 25, 10);
            assert_eq!(
                sub_line_col_to_line_col(sub, within, 25, 10),
                col,
                "round-trip failed for col {col}"
            );
        }
    }

    #[test]
    fn content_cols_one_or_zero_is_a_degenerate_no_op() {
        assert_eq!(sub_line_count(50, 0), 1);
        assert_eq!(sub_line_range(SubLine(0), 50, 0), (0, 50));
        assert_eq!(col_to_sub_line(20, 50, 0), (SubLine(0), 20));
        assert_eq!(sub_line_count(50, 1), 1);
        assert_eq!(col_to_sub_line(20, 50, 1), (SubLine(0), 20));
    }
}
