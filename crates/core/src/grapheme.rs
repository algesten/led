//! Grapheme-cluster + display-cell helpers for `Cursor::col` math.
//!
//! `Cursor::col` indexes **grapheme clusters** on a line. The rope,
//! `unicode-segmentation`, and `unicode-width` together provide:
//!
//! - cluster count per line ([`line_grapheme_len`])
//! - grapheme col ↔ char index conversion ([`grapheme_col_to_char`],
//!   [`char_to_grapheme_col`])
//! - cluster width in terminal cells ([`grapheme_display_width`])
//! - prefix display widths ([`prefix_display_width`])
//! - inverse for vertical-move column preservation
//!   ([`display_col_to_grapheme`])
//!
//! Tabs (`\t`) are special: they always carry their cell-stop
//! semantics (next multiple of [`TAB_STOP`]). The grapheme-aware
//! width math threads `prior_cells` through every helper so a tab's
//! width depends on where it sits on the line.
//!
//! Trailing newlines (`\n`, `\r\n`) are not counted as graphemes —
//! `line_grapheme_len` mirrors the existing `line_char_len`'s
//! "exclude line terminator" rule.

use ropey::RopeSlice;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// Cells per tab-stop. Hardcoded to match the rest of the editor
/// (body painter, `insert_tab` fallback, expand-tabs).
pub const TAB_STOP: usize = 4;

/// Materialise the line's content (sans trailing newline) into a
/// `String`. Internal helper — every public function in this module
/// goes through it once per call. `unicode-segmentation` requires a
/// contiguous `&str`, so the rope-line walk has to materialise; the
/// allocation discipline (idle-tick zero-alloc) is preserved by the
/// memoised gating one layer up.
fn line_content(line: RopeSlice<'_>) -> String {
    let mut s: String = line.chars().collect();
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

/// Number of grapheme clusters on `line`, excluding any trailing
/// `\n` / `\r\n`. Empty line → 0.
pub fn line_grapheme_len(line: RopeSlice<'_>) -> usize {
    let s = line_content(line);
    s.graphemes(true).count()
}

/// Convert a grapheme col into the char index inside the line
/// (0-based, relative to the line's start). Out-of-range cols
/// saturate to the line's char length excluding any trailing
/// newline.
pub fn grapheme_col_to_char(line: RopeSlice<'_>, grapheme_col: usize) -> usize {
    let s = line_content(line);
    let mut chars_consumed = 0usize;
    for (i, g) in s.graphemes(true).enumerate() {
        if i == grapheme_col {
            return chars_consumed;
        }
        chars_consumed += g.chars().count();
    }
    chars_consumed
}

/// Inverse of [`grapheme_col_to_char`]. A char index that lands
/// strictly inside a multi-codepoint cluster snaps to the cluster
/// **start** (the cursor lives between graphemes, not inside them).
/// A char index exactly at a cluster boundary returns the index of
/// the cluster starting there.
pub fn char_to_grapheme_col(line: RopeSlice<'_>, char_idx: usize) -> usize {
    let s = line_content(line);
    let mut chars_before = 0usize;
    let mut total = 0usize;
    for (i, g) in s.graphemes(true).enumerate() {
        let chars_after = chars_before + g.chars().count();
        if char_idx < chars_after {
            return i;
        }
        if char_idx == chars_after {
            return i + 1;
        }
        chars_before = chars_after;
        total = i + 1;
    }
    total
}

/// Display width in terminal cells of one grapheme cluster.
///
/// `prior_cells` is the running cell column on the line so far —
/// needed because `\t` expands to "the next [`TAB_STOP`] boundary",
/// not a fixed width.
///
/// - `\t` → `TAB_STOP - (prior_cells % TAB_STOP)`
/// - control / unprintable scalars → 0 (matches the painter, which
///   skips them rather than drawing replacement glyphs)
/// - everything else → sum of [`UnicodeWidthChar::width`] over the
///   cluster's scalars (combining marks contribute 0; ZWJ joiners
///   contribute 0; the base contributes 1 or 2)
pub fn grapheme_display_width(cluster: &str, prior_cells: usize) -> usize {
    if cluster == "\t" {
        return TAB_STOP - (prior_cells % TAB_STOP);
    }
    cluster.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Sum of cell widths of the first `grapheme_count` clusters on
/// `line`. Used by the painter to place the cursor at the correct
/// terminal column. Out-of-range counts saturate to the line's full
/// display width.
pub fn prefix_display_width(line: RopeSlice<'_>, grapheme_count: usize) -> usize {
    let s = line_content(line);
    let mut cells = 0usize;
    for (i, g) in s.graphemes(true).enumerate() {
        if i >= grapheme_count {
            break;
        }
        cells += grapheme_display_width(g, cells);
    }
    cells
}

/// Inverse of [`prefix_display_width`]: the largest grapheme col `g`
/// such that `prefix_display_width(line, g) <= cells`.
///
/// When `cells` falls in the middle of a wide glyph, the result is
/// the glyph's start col (cursor snaps to cluster boundary).
pub fn display_col_to_grapheme(line: RopeSlice<'_>, cells: usize) -> usize {
    let s = line_content(line);
    let mut acc = 0usize;
    for (i, g) in s.graphemes(true).enumerate() {
        let w = grapheme_display_width(g, acc);
        if acc + w > cells {
            return i;
        }
        acc += w;
        if acc == cells {
            return i + 1;
        }
    }
    s.graphemes(true).count()
}

/// Convert a grapheme col into a count of UTF-16 code units before
/// that position on the line. This is the unit LSP `Position::character`
/// uses by default (per the LSP `PositionEncodingKind::UTF16` spec —
/// the universal default; UTF-8 / UTF-32 encodings are opt-in via
/// `clientCapabilities.general.positionEncodings` and we don't
/// negotiate them).
///
/// Each scalar `c < 0x10000` contributes 1 unit; supplementary
/// codepoints (`c >= 0x10000`) contribute 2 (surrogate pair). Tabs,
/// combining marks, etc. follow the same rule — they're ordinary
/// scalars in this domain.
pub fn grapheme_col_to_utf16_units(line: RopeSlice<'_>, grapheme_col: usize) -> u32 {
    let s = line_content(line);
    let mut units = 0u32;
    let mut chars_taken = 0usize;
    let target_chars = grapheme_col_to_char(line, grapheme_col);
    for c in s.chars() {
        if chars_taken >= target_chars {
            break;
        }
        units += if (c as u32) < 0x10000 { 1 } else { 2 };
        chars_taken += 1;
    }
    units
}

/// Inverse of [`grapheme_col_to_utf16_units`]: convert an LSP
/// `Position::character` (UTF-16 code units) into a grapheme col.
/// A unit count that lands in the middle of a surrogate pair or a
/// multi-codepoint cluster snaps to the cluster start (the cursor
/// lives between graphemes, not inside them).
pub fn utf16_units_to_grapheme_col(line: RopeSlice<'_>, utf16_units: u32) -> usize {
    let s = line_content(line);
    let mut units = 0u32;
    let mut chars_taken = 0usize;
    for c in s.chars() {
        let w = if (c as u32) < 0x10000 { 1 } else { 2 };
        if units + w > utf16_units {
            break;
        }
        units += w;
        chars_taken += 1;
    }
    char_to_grapheme_col(line, chars_taken)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn rope_line(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn empty_line_grapheme_len_zero() {
        let r = rope_line("");
        assert_eq!(line_grapheme_len(r.line(0)), 0);
    }

    #[test]
    fn ascii_line_grapheme_len_matches_chars() {
        let r = rope_line("hello\n");
        assert_eq!(line_grapheme_len(r.line(0)), 5);
    }

    #[test]
    fn cjk_line_grapheme_len_is_glyph_count() {
        // 4 CJK glyphs, 4 chars, 8 display cells.
        let r = rope_line("你好世界");
        assert_eq!(line_grapheme_len(r.line(0)), 4);
    }

    #[test]
    fn combining_mark_collapses_into_one_cluster() {
        // "café" written as `cafe\u{0301}` — 5 chars, 4 clusters.
        let r = rope_line("cafe\u{0301}");
        assert_eq!(line_grapheme_len(r.line(0)), 4);
    }

    #[test]
    fn zwj_family_emoji_is_one_cluster() {
        // 👨‍👩‍👧‍👦: 7 codepoints, 1 grapheme.
        let r = rope_line("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}");
        assert_eq!(line_grapheme_len(r.line(0)), 1);
    }

    #[test]
    fn trailing_newline_not_counted() {
        let r = rope_line("ab\n");
        assert_eq!(line_grapheme_len(r.line(0)), 2);
        let r = rope_line("ab\r\n");
        assert_eq!(line_grapheme_len(r.line(0)), 2);
    }

    #[test]
    fn grapheme_col_to_char_round_trips() {
        // "café" with combining acute: 5 chars, 4 clusters.
        let r = rope_line("cafe\u{0301}");
        let line = r.line(0);
        // Cluster 0 -> char 0, 1 -> 1, 2 -> 2, 3 -> 3 (e starts here),
        // past-end (4) -> 5 (e + combining = 2 chars).
        assert_eq!(grapheme_col_to_char(line, 0), 0);
        assert_eq!(grapheme_col_to_char(line, 1), 1);
        assert_eq!(grapheme_col_to_char(line, 2), 2);
        assert_eq!(grapheme_col_to_char(line, 3), 3);
        assert_eq!(grapheme_col_to_char(line, 4), 5);
        // Out-of-range saturates.
        assert_eq!(grapheme_col_to_char(line, 99), 5);
    }

    #[test]
    fn char_to_grapheme_col_snaps_inside_cluster() {
        let r = rope_line("cafe\u{0301}");
        let line = r.line(0);
        // Char 0..=3 → cluster 0..=3. Char 4 lands inside the
        // `e + combining` cluster → snaps to cluster start (3).
        // Wait — actually char 3 is start of `e`, char 4 is start
        // of combining acute, char 5 is past-end.
        assert_eq!(char_to_grapheme_col(line, 0), 0);
        assert_eq!(char_to_grapheme_col(line, 3), 3);
        assert_eq!(char_to_grapheme_col(line, 4), 3); // inside cluster
        assert_eq!(char_to_grapheme_col(line, 5), 4); // past end
    }

    #[test]
    fn round_trip_grapheme_col_char_idx() {
        let r = rope_line("a\u{0301}b\u{0301}c\u{0301}");
        let line = r.line(0);
        for g in 0..=line_grapheme_len(line) {
            let c = grapheme_col_to_char(line, g);
            assert_eq!(char_to_grapheme_col(line, c), g, "round-trip g={g}");
        }
    }

    #[test]
    fn ascii_display_width_matches_grapheme_count() {
        let r = rope_line("hello");
        let line = r.line(0);
        assert_eq!(prefix_display_width(line, 0), 0);
        assert_eq!(prefix_display_width(line, 5), 5);
    }

    #[test]
    fn cjk_display_width_doubles_grapheme_count() {
        let r = rope_line("你好");
        let line = r.line(0);
        assert_eq!(prefix_display_width(line, 0), 0);
        assert_eq!(prefix_display_width(line, 1), 2);
        assert_eq!(prefix_display_width(line, 2), 4);
    }

    #[test]
    fn combining_mark_does_not_add_width() {
        let r = rope_line("cafe\u{0301}");
        let line = r.line(0);
        assert_eq!(prefix_display_width(line, 4), 4);
    }

    #[test]
    fn tab_expands_to_next_stop() {
        assert_eq!(grapheme_display_width("\t", 0), 4);
        assert_eq!(grapheme_display_width("\t", 1), 3);
        assert_eq!(grapheme_display_width("\t", 3), 1);
        assert_eq!(grapheme_display_width("\t", 4), 4);
        assert_eq!(grapheme_display_width("\t", 7), 1);
    }

    #[test]
    fn prefix_display_width_with_tabs() {
        let r = rope_line("\thello");
        let line = r.line(0);
        // Tab → 4 cells, then 5 ASCII chars.
        assert_eq!(prefix_display_width(line, 0), 0);
        assert_eq!(prefix_display_width(line, 1), 4);
        assert_eq!(prefix_display_width(line, 2), 5);
        assert_eq!(prefix_display_width(line, 6), 9);
    }

    #[test]
    fn display_col_to_grapheme_basic() {
        let r = rope_line("你好世界");
        let line = r.line(0);
        // 4 CJK glyphs × 2 cells = 8 cells total.
        assert_eq!(display_col_to_grapheme(line, 0), 0);
        assert_eq!(display_col_to_grapheme(line, 1), 0); // mid-glyph snaps to start
        assert_eq!(display_col_to_grapheme(line, 2), 1);
        assert_eq!(display_col_to_grapheme(line, 3), 1);
        assert_eq!(display_col_to_grapheme(line, 4), 2);
        assert_eq!(display_col_to_grapheme(line, 8), 4);
        assert_eq!(display_col_to_grapheme(line, 99), 4); // past end saturates
    }

    #[test]
    fn display_col_to_grapheme_round_trip() {
        let r = rope_line("aé你\tb");
        let line = r.line(0);
        // a=1 cell, é=1 cell, 你=2 cells, tab from cell 4 → 4 cells, b=1
        for g in 0..=line_grapheme_len(line) {
            let cells = prefix_display_width(line, g);
            assert_eq!(display_col_to_grapheme(line, cells), g, "g={g}");
        }
    }

    #[test]
    fn utf16_units_round_trip_ascii() {
        let r = rope_line("hello world");
        let line = r.line(0);
        for g in 0..=line_grapheme_len(line) {
            let units = grapheme_col_to_utf16_units(line, g);
            assert_eq!(units as usize, g, "ASCII: 1 unit per char");
            assert_eq!(utf16_units_to_grapheme_col(line, units), g);
        }
    }

    #[test]
    fn utf16_units_supplementary_codepoint_counts_two() {
        // 🎉 = U+1F389 (supplementary plane) — 2 UTF-16 units.
        let r = rope_line("a🎉b");
        let line = r.line(0);
        // Graphemes: 'a' (g0), '🎉' (g1), 'b' (g2).
        assert_eq!(grapheme_col_to_utf16_units(line, 0), 0);
        assert_eq!(grapheme_col_to_utf16_units(line, 1), 1); // after 'a'
        assert_eq!(grapheme_col_to_utf16_units(line, 2), 3); // after '🎉' = 1 + 2
        assert_eq!(grapheme_col_to_utf16_units(line, 3), 4); // after 'b'
    }

    #[test]
    fn utf16_units_to_grapheme_snaps_inside_surrogate() {
        // Past 'a' (1 unit) into the middle of '🎉' (2 units) — snaps
        // to the surrogate-pair start, i.e. grapheme col 1 (cursor
        // between 'a' and '🎉').
        let r = rope_line("a🎉b");
        let line = r.line(0);
        assert_eq!(utf16_units_to_grapheme_col(line, 0), 0);
        assert_eq!(utf16_units_to_grapheme_col(line, 1), 1);
        assert_eq!(utf16_units_to_grapheme_col(line, 2), 1); // mid-surrogate snaps back
        assert_eq!(utf16_units_to_grapheme_col(line, 3), 2);
        assert_eq!(utf16_units_to_grapheme_col(line, 4), 3);
    }

    #[test]
    fn utf16_units_combining_mark_counts_each_scalar() {
        // 'cafe\u{0301}' — chars c-a-f-e-acute. All BMP, 1 unit each.
        // Graphemes: c, a, f, é (=e+acute). Past-end grapheme col
        // 4 = 5 UTF-16 units (one per scalar).
        let r = rope_line("cafe\u{0301}");
        let line = r.line(0);
        assert_eq!(grapheme_col_to_utf16_units(line, 4), 5);
        assert_eq!(utf16_units_to_grapheme_col(line, 5), 4);
        // Mid-cluster (between 'e' and acute) snaps back to grapheme 3.
        assert_eq!(utf16_units_to_grapheme_col(line, 4), 3);
    }

    #[test]
    fn emoji_zwj_family_one_grapheme_wide() {
        // 👨‍👩‍👧‍👦 — single cluster. unicode-width's per-char widths
        // sum to 8 (4 wide-emoji × 2), but it's one grapheme.
        let s = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let r = rope_line(s);
        let line = r.line(0);
        assert_eq!(line_grapheme_len(line), 1);
        // The four emoji each width 2; ZWJs width 0. Sum is 8 cells.
        // (Real terminals typically render this as a single 2-cell
        // glyph, but unicode-width sees only the codepoints; led is
        // honest about what the payload claims rather than
        // second-guessing the terminal. Wrong-renders are a terminal
        // limitation, not a led correctness bug.)
        assert_eq!(prefix_display_width(line, 1), 8);
    }
}
