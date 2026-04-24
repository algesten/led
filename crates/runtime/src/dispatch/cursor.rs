//! Cursor movement (M2, M6) — pure geometry over the rope.
//!
//! `apply_move` is the decision function; `move_cursor` is the
//! dispatch-facing wrapper that reads the right rope (edits first,
//! store second) and updates both cursor + scroll on the active tab.

use led_core::{SubLine, col_to_sub_line, sub_line_count, sub_line_range};
use led_driver_buffers_core::{BufferStore, LoadState};
use led_driver_terminal_core::{Layout, Terminal};
use led_state_browser::BrowserUi;
use led_state_buffer_edits::BufferEdits;
use led_state_tabs::{Cursor, Scroll, Tabs};
use ropey::Rope;
use std::sync::Arc;

use super::shared::{
    GUTTER_WIDTH, TRAILING_RESERVED_COLS, is_word_char, line_char_len, rope_char_at,
};

/// Logical cursor moves. Built from key events in `dispatch_key` and
/// applied by the pure [`apply_move`] helper so the geometry is unit
/// testable without any keyboard setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Move {
    Up,
    Down,
    Left,
    Right,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
    FileStart,
    FileEnd,
    WordLeft,
    WordRight,
}

/// Apply a move to the active tab: update cursor, then adjust scroll so
/// the cursor stays inside the body viewport. No-op when there is no
/// active tab or its buffer isn't loaded yet — the cursor has nothing
/// to clamp against.
///
/// Rope lookup prefers [`BufferEdits`] (the user's edited view); the
/// store fallback only matters before the runtime has seeded edits
/// for a just-loaded buffer.
pub(super) fn move_cursor(
    tabs: &mut Tabs,
    edits: &BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
    browser: &BrowserUi,
    m: Move,
) {
    let Some(active) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == active) else {
        return;
    };
    let path = &tabs.open[idx].path;
    let rope: Arc<Rope> = match edits.buffers.get(path) {
        Some(eb) => eb.rope.clone(),
        None => match store.loaded.get(path) {
            Some(LoadState::Ready(r)) => r.clone(),
            _ => return,
        },
    };

    // Dispatch must see the SAME editor geometry the painter uses
    // so visual-row navigation lands on the row the user actually
    // sees. Anything else — raw terminal cols, sidebar-blind math
    // — makes the cursor jump to a different visual row than
    // `body_model` is about to render.
    let (body_rows, content_cols) = terminal
        .dims
        .map(|d| {
            let layout = Layout::compute(d, browser.visible);
            (
                layout.editor_area.rows as usize,
                (layout.editor_area.cols as usize)
                    .saturating_sub(GUTTER_WIDTH)
                    .saturating_sub(TRAILING_RESERVED_COLS),
            )
        })
        .unwrap_or((0, 0));

    let tab = &mut tabs.open[idx];
    tab.cursor = apply_move(tab.cursor, &rope, m, body_rows, content_cols);
    tab.scroll = adjust_scroll(tab.scroll, tab.cursor, body_rows, &rope, content_cols);
}

/// Pure cursor geometry over a rope, sub-line-aware.
///
/// Vertical moves (`Up` / `Down` / `PageUp` / `PageDown`) step by
/// visual rows — they cross sub-line boundaries within a wrapped
/// logical line before stepping to the next/previous logical
/// line. `preferred_col` is the column WITHIN the sub-line the
/// user last intentionally set via a horizontal move; vertical
/// moves clamp it to each visited sub-line's width and land there.
///
/// `Home` / `End` land on the start / end of the current sub-line
/// (not the logical line) — matches legacy's soft-wrap UX where a
/// long paragraph's Home / End walk within the displayed row.
pub(super) fn apply_move(
    c: Cursor,
    rope: &Rope,
    m: Move,
    body_rows: usize,
    content_cols: usize,
) -> Cursor {
    let line_count = rope.len_lines().max(1);
    let last_line = line_count - 1;

    // Horizontal move: anchor preferred_col to the new visual
    // column (column within the resulting sub-line).
    let horizontal = |line: usize, col: usize| -> Cursor {
        let len = line_char_len(rope, line);
        let (_, within) = col_to_sub_line(col, len, content_cols);
        Cursor {
            line,
            col,
            preferred_col: within,
        }
    };

    match m {
        Move::Up => visual_step_up(c, rope, content_cols, 1),
        Move::Down => visual_step_down(c, rope, content_cols, 1, last_line),
        Move::PageUp => visual_step_up(c, rope, content_cols, body_rows.max(1)),
        Move::PageDown => {
            visual_step_down(c, rope, content_cols, body_rows.max(1), last_line)
        }
        Move::Left => {
            // Wrap to end of previous line when at col 0 — matches
            // legacy `model::mov::move_left`. No-op at (0, 0). Sub-
            // lines within a logical line transition seamlessly
            // because col simply decrements.
            if c.col > 0 {
                horizontal(c.line, c.col - 1)
            } else if c.line > 0 {
                let prev = c.line - 1;
                horizontal(prev, line_char_len(rope, prev))
            } else {
                horizontal(0, 0)
            }
        }
        Move::Right => {
            // Wrap to start of next line when at line end — matches
            // legacy `model::mov::move_right`. No-op at end-of-file.
            let len = line_char_len(rope, c.line);
            if c.col < len {
                horizontal(c.line, c.col + 1)
            } else if c.line < last_line {
                horizontal(c.line + 1, 0)
            } else {
                horizontal(c.line, len)
            }
        }
        Move::LineStart => horizontal(c.line, 0),
        Move::LineEnd => horizontal(c.line, line_char_len(rope, c.line)),
        Move::FileStart => horizontal(0, 0),
        Move::FileEnd => {
            let line = last_line;
            horizontal(line, line_char_len(rope, line))
        }
        Move::WordLeft => {
            let (line, col) = word_boundary_back(rope, c.line, c.col);
            horizontal(line, col)
        }
        Move::WordRight => {
            let (line, col) = word_boundary_fwd(rope, c.line, c.col);
            horizontal(line, col)
        }
    }
}

/// Step the cursor up by `steps` visual rows. Crosses sub-line
/// boundaries within a wrapped line before advancing to the
/// previous logical line. Preserves `preferred_col`; the landing
/// column is `sub_line_start + min(preferred_col, sub_line_width)`.
fn visual_step_up(c: Cursor, rope: &Rope, content_cols: usize, steps: usize) -> Cursor {
    let mut line = c.line;
    let len = line_char_len(rope, line);
    let (mut sub, _) = col_to_sub_line(c.col, len, content_cols);
    for _ in 0..steps {
        if sub.0 > 0 {
            sub = SubLine(sub.0 - 1);
        } else if line > 0 {
            line -= 1;
            let n = sub_line_count(line_char_len(rope, line), content_cols);
            sub = SubLine(n.saturating_sub(1));
        } else {
            break;
        }
    }
    land_on_sub_line(line, sub, c.preferred_col, rope, content_cols)
}

/// Step the cursor down by `steps` visual rows. Symmetric
/// counterpart of [`visual_step_up`].
fn visual_step_down(
    c: Cursor,
    rope: &Rope,
    content_cols: usize,
    steps: usize,
    last_line: usize,
) -> Cursor {
    let mut line = c.line;
    let cur_len = line_char_len(rope, line);
    let (mut sub, _) = col_to_sub_line(c.col, cur_len, content_cols);
    for _ in 0..steps {
        let n = sub_line_count(line_char_len(rope, line), content_cols);
        if sub.0 + 1 < n {
            sub = SubLine(sub.0 + 1);
        } else if line < last_line {
            line += 1;
            sub = SubLine(0);
        } else {
            break;
        }
    }
    land_on_sub_line(line, sub, c.preferred_col, rope, content_cols)
}

/// Place the cursor at `preferred_col` within `(line, sub)`, clamped
/// to the sub-line's valid cursor range. Keeps `preferred_col`
/// untouched so a subsequent move over a wider sub-line restores
/// the goal column.
///
/// Non-last subs cap at `width - 1`: col `start + width` is the
/// wrap boundary which [`col_to_sub_line`] resolves as the **next**
/// sub's col 0, so landing there would bounce the cursor onto the
/// sub we were trying to leave. Last subs cap at `width` — that's
/// the logical EOL and a valid cursor position.
///
/// The asymmetry matters when `preferred_col` was captured on a
/// last sub (width up to `content_cols`) and we're landing on a
/// non-last sub (width `wrap_width = content_cols - 1`): plain
/// `min(preferred_col, width)` would leave us at the wrap
/// boundary.
fn land_on_sub_line(
    line: usize,
    sub: SubLine,
    preferred_col: usize,
    rope: &Rope,
    content_cols: usize,
) -> Cursor {
    let line_len = line_char_len(rope, line);
    let (start, end) = sub_line_range(sub, line_len, content_cols);
    let width = end.saturating_sub(start);
    let count = sub_line_count(line_len, content_cols);
    let is_last = sub.0 + 1 >= count;
    let max_within = if is_last { width } else { width.saturating_sub(1) };
    Cursor {
        line,
        col: start + preferred_col.min(max_within),
        preferred_col,
    }
}

/// Word = run of alphanumeric-or-underscore chars. `word_boundary_fwd`
/// skips any trailing non-word chars from the current position, then
/// skips word chars to land at the start of the next non-word run.
///
/// Walks the rope directly with `RopeSlice::char_at`-style indexing —
/// no intermediate allocation, matches the allocation-discipline rule
/// for dispatch hot paths.
fn word_boundary_fwd(rope: &Rope, mut line: usize, mut col: usize) -> (usize, usize) {
    let line_count = rope.len_lines().max(1);
    let last_line = line_count - 1;
    loop {
        let len = line_char_len(rope, line);
        // 1. Skip non-word chars on the current line.
        while col < len && !is_word_char(rope_char_at(rope, line, col)) {
            col += 1;
        }
        if col >= len {
            // Ran off the end; advance to the next line's start.
            if line == last_line {
                return (line, len);
            }
            line += 1;
            col = 0;
            continue;
        }
        // 2. Skip word chars; we land at the first non-word after them.
        while col < line_char_len(rope, line) && is_word_char(rope_char_at(rope, line, col)) {
            col += 1;
        }
        return (line, col);
    }
}

fn word_boundary_back(rope: &Rope, mut line: usize, mut col: usize) -> (usize, usize) {
    loop {
        if col == 0 {
            if line == 0 {
                return (0, 0);
            }
            line -= 1;
            col = line_char_len(rope, line);
            continue;
        }
        // 1. Skip non-word chars immediately behind the cursor.
        while col > 0 && !is_word_char(rope_char_at(rope, line, col - 1)) {
            col -= 1;
        }
        if col == 0 {
            // Line ran out to the left; check if we landed on a word
            // boundary or need to cross to the previous line.
            if line == 0 {
                return (0, 0);
            }
            // Previous line hasn't been scanned yet; loop handles it.
            line -= 1;
            col = line_char_len(rope, line);
            continue;
        }
        // 2. Skip the word run itself — we land at its start.
        while col > 0 && is_word_char(rope_char_at(rope, line, col - 1)) {
            col -= 1;
        }
        return (line, col);
    }
}

/// Move scroll's (line, sub-line) anchor so the cursor's visual
/// row sits within `[0, body_rows)` relative to the scroll anchor.
/// Operates in sub-line space so scrolling through a wrapped
/// paragraph advances one visual row at a time.
pub(super) fn adjust_scroll(
    s: Scroll,
    c: Cursor,
    body_rows: usize,
    rope: &Rope,
    content_cols: usize,
) -> Scroll {
    if body_rows == 0 {
        return s;
    }
    let cur_len = line_char_len(rope, c.line);
    let (cur_sub, _) = col_to_sub_line(c.col, cur_len, content_cols);
    let scroll_pos = (s.top, s.top_sub_line);
    let cur_pos = (c.line, cur_sub);
    if cur_pos < scroll_pos {
        // Cursor is above the viewport: align scroll to it.
        return Scroll {
            top: c.line,
            top_sub_line: cur_sub,
        };
    }
    // Count visible rows between scroll anchor and cursor, then
    // scroll forward just enough to keep the cursor visible.
    let rows_to_cursor = rows_between(scroll_pos, cur_pos, rope, content_cols);
    if rows_to_cursor < body_rows {
        return s;
    }
    let advance = rows_to_cursor + 1 - body_rows;
    scroll_forward(s, rope, content_cols, advance)
}

/// Count the visual rows between `(from_line, from_sub)` and
/// `(to_line, to_sub)`. Assumes `from <= to`; caller checks first.
fn rows_between(
    from: (usize, SubLine),
    to: (usize, SubLine),
    rope: &Rope,
    content_cols: usize,
) -> usize {
    let mut row = 0usize;
    let mut ln = from.0;
    let mut sub_start = from.1.0;
    while ln < to.0 {
        let n = sub_line_count(line_char_len(rope, ln), content_cols);
        row = row.saturating_add(n.saturating_sub(sub_start));
        ln += 1;
        sub_start = 0;
    }
    row.saturating_add(to.1.0.saturating_sub(sub_start))
}

/// Advance `s` by `steps` visual rows, clamping to end-of-file.
fn scroll_forward(s: Scroll, rope: &Rope, content_cols: usize, steps: usize) -> Scroll {
    let line_count = rope.len_lines().max(1);
    let last_line = line_count - 1;
    let mut line = s.top;
    let mut sub = s.top_sub_line;
    for _ in 0..steps {
        let n = sub_line_count(line_char_len(rope, line), content_cols);
        if sub.0 + 1 < n {
            sub = SubLine(sub.0 + 1);
        } else if line < last_line {
            line += 1;
            sub = SubLine(0);
        } else {
            break;
        }
    }
    Scroll {
        top: line,
        top_sub_line: sub,
    }
}

#[cfg(test)]
mod tests {
    use led_state_file_search::FileSearchState;
    use led_state_find_file::FindFileState;
    use led_state_isearch::IsearchState;
    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyEvent, KeyModifiers};
    use led_state_alerts::AlertState;
    use led_state_clipboard::ClipboardState;
    use led_state_buffer_edits::BufferEdits;
    use led_state_jumps::JumpListState;
    use led_state_browser::{BrowserUi, FsTree};
    use led_state_kill_ring::KillRing;
    use led_state_tabs::{Cursor, Scroll, Tabs};
    use ropey::Rope;

    use super::super::testutil::*;
    use super::*;
    use crate::keymap::{default_keymap, ChordState, Command};
    
    

    #[test]
    fn down_moves_cursor_and_does_not_scroll_within_viewport() {
        // body_rows = rows − 2 (tab bar + status bar); rows=6 → 4
        // content rows. Cursor starts at (0,0); three Downs land on
        // line 3, still inside the viewport.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("a\nb\nc\nd\ne\nf", Dims { cols: 10, rows: 6 });
        for _ in 0..3 {
            dispatch_default(
                key(KeyModifiers::NONE, KeyCode::Down),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
        }
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 3,
                col: 0,
                preferred_col: 0,
            }
        );
        assert_eq!(tabs.open[0].scroll, Scroll { top: 0, top_sub_line: led_core::SubLine(0) });
    }

    #[test]
    fn down_scrolls_when_cursor_would_leave_viewport() {
        // body_rows = rows − 2; rows=5 → 3. Third Down lands on
        // line 3, one row past the viewport, so scroll advances.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("a\nb\nc\nd\ne\nf", Dims { cols: 10, rows: 5 });
        for _ in 0..3 {
            dispatch_default(
                key(KeyModifiers::NONE, KeyCode::Down),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
        }
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 3,
                col: 0,
                preferred_col: 0,
            }
        );
        assert_eq!(tabs.open[0].scroll, Scroll { top: 1, top_sub_line: led_core::SubLine(0) });
    }

    #[test]
    fn up_scrolls_back_toward_the_top() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("a\nb\nc\nd\ne\nf", Dims { cols: 10, rows: 4 });
        tabs.open[0].cursor = Cursor {
            line: 5,
            col: 0,
            preferred_col: 0,
        };
        tabs.open[0].scroll = Scroll { top: 3, top_sub_line: led_core::SubLine(0) };
        // body_rows = 3. Moving up from line 5 to line 2 should leave view
        // at the top.
        for _ in 0..3 {
            dispatch_default(
                key(KeyModifiers::NONE, KeyCode::Up),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
        }
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 2,
                col: 0,
                preferred_col: 0,
            }
        );
        assert_eq!(tabs.open[0].scroll, Scroll { top: 2, top_sub_line: led_core::SubLine(0) });
    }

    #[test]
    fn right_wraps_from_line_end_to_next_row_start() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi\nworld", Dims { cols: 10, rows: 5 });
        // Line 0 = "hi" (len 2). Right walks 0→1→2 inside line 0,
        // then wraps to (line=1, col=0) on the next press, matching
        // legacy `model::mov::move_right`.
        for (expected_line, expected_col) in [(0, 1), (0, 2), (1, 0)] {
            dispatch_default(
                key(KeyModifiers::NONE, KeyCode::Right),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
            assert_eq!(tabs.open[0].cursor.line, expected_line);
            assert_eq!(tabs.open[0].cursor.col, expected_col);
        }
    }

    #[test]
    fn right_at_eof_does_not_advance() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 2,
            preferred_col: 2,
        };
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Right),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 2);
    }

    #[test]
    fn left_wraps_from_line_start_to_previous_row_end() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi\nworld", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 1,
            col: 0,
            preferred_col: 0,
        };
        // From (line=1, col=0), Left wraps to end of line 0 (col=2).
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Left),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 2);
    }

    #[test]
    fn left_at_file_start_does_not_move() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi\nworld", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Left),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.col, 0);
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Left),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn home_end_jump_within_current_line() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdef\nghij", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::End),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 0,
                col: 6,
                preferred_col: 6,
            }
        );
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Home),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 0,
                col: 0,
                preferred_col: 0,
            }
        );
    }

    #[test]
    fn page_down_advances_by_one_viewport() {
        let body = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        // body_rows = rows − 2 = 10 with rows=12. PageDown moves
        // the cursor down by 10 visual rows.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content(&body, Dims { cols: 40, rows: 12 });
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::PageDown),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.line, 10);
        assert_eq!(tabs.open[0].scroll.top, 1);
    }

    #[test]
    fn movement_is_noop_when_buffer_not_loaded() {
        let mut tabs = tabs_with(&[("file.rs", 1)], Some(1));
        let mut edits = BufferEdits::default(); // not seeded
        let store = BufferStore::default(); // no content loaded
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Down),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor, Cursor::default());
        assert_eq!(tabs.open[0].scroll, Scroll::default());
    }

    // ── pure helper tests ───────────────────────────────────────────────

    #[test]
    fn apply_move_clamps_col_when_moving_to_shorter_line() {
        let rope = Rope::from_str("abcdef\nghi");
        let c = apply_move(
            Cursor {
                line: 0,
                col: 5,
                preferred_col: 5,
            },
            &rope,
            Move::Down,
            10,
            80,
        );
        // "ghi".len() == 3 → col clamps; preferred_col carries forward
        // so a later Down onto a longer line can restore column 5.
        assert_eq!(
            c,
            Cursor {
                line: 1,
                col: 3,
                preferred_col: 5,
            }
        );
    }

    #[test]
    fn vertical_traversal_restores_preferred_col_on_longer_line() {
        // The regression this guards against: moving Down past a line
        // that's shorter than the cursor's column must not anchor the
        // column to the shorter line. Continuing Down onto a longer
        // line should return the cursor to the original column.
        let rope = Rope::from_str("abcdefghij\nxy\n0123456789");
        let start = Cursor {
            line: 0,
            col: 7,
            preferred_col: 7,
        };

        // Down onto the short middle line ("xy") clamps col to 2.
        let c = apply_move(start, &rope, Move::Down, 10, 80);
        assert_eq!(
            c,
            Cursor {
                line: 1,
                col: 2,
                preferred_col: 7,
            }
        );

        // Down again onto the long third line — col returns to 7.
        let c = apply_move(c, &rope, Move::Down, 10, 80);
        assert_eq!(
            c,
            Cursor {
                line: 2,
                col: 7,
                preferred_col: 7,
            }
        );

        // And symmetric Up traversal also restores.
        let c = apply_move(c, &rope, Move::Up, 10, 80);
        assert_eq!(
            c,
            Cursor {
                line: 1,
                col: 2,
                preferred_col: 7,
            }
        );
        let c = apply_move(c, &rope, Move::Up, 10, 80);
        assert_eq!(
            c,
            Cursor {
                line: 0,
                col: 7,
                preferred_col: 7,
            }
        );
    }

    #[test]
    fn horizontal_move_resets_preferred_col() {
        // After Right, the preferred column anchors to the new col, so
        // a subsequent Down follows the new (smaller) goal, not the
        // old one.
        let rope = Rope::from_str("abcdefghij\n0123456789");
        let c = Cursor {
            line: 0,
            col: 8,
            preferred_col: 8,
        };
        let c = apply_move(c, &rope, Move::Left, 10, 80);
        assert_eq!(
            c,
            Cursor {
                line: 0,
                col: 7,
                preferred_col: 7,
            }
        );
        let c = apply_move(c, &rope, Move::Down, 10, 80);
        assert_eq!(
            c,
            Cursor {
                line: 1,
                col: 7,
                preferred_col: 7,
            }
        );
    }

    #[test]
    fn page_down_also_preserves_preferred_col() {
        let body = (0..30)
            .map(|i| {
                if i == 5 {
                    "xy".into()
                } else {
                    format!("line {i:03}")
                }
            })
            .collect::<Vec<String>>()
            .join("\n");
        let rope = Rope::from_str(&body);
        let start = Cursor {
            line: 0,
            col: 6,
            preferred_col: 6,
        };
        // PageDown by 10 lands at line 10 ("line 010", len 8) — col 6 restored.
        let c = apply_move(start, &rope, Move::PageDown, 10, 80);
        assert_eq!(
            c,
            Cursor {
                line: 10,
                col: 6,
                preferred_col: 6,
            }
        );
    }

    #[test]
    fn adjust_scroll_pulls_cursor_back_into_view() {
        // A rope with at least 9 lines so rows_between can walk
        // from (0, 0) to (8, 0) through real sub-line counts.
        let rope = Rope::from_str("\n\n\n\n\n\n\n\n\n");
        let s = adjust_scroll(
            Scroll { top: 0, top_sub_line: led_core::SubLine(0) },
            Cursor {
                line: 8,
                col: 0,
                preferred_col: 0,
            },
            4,
            &rope,
            80,
        );
        assert_eq!(s, Scroll { top: 5, top_sub_line: led_core::SubLine(0) });
    }

    #[test]
    fn adjust_scroll_noop_when_cursor_inside_window() {
        let rope = Rope::from_str("\n\n\n\n\n\n\n\n\n\n\n\n\n");
        let s0 = Scroll { top: 10, top_sub_line: led_core::SubLine(0) };
        let s = adjust_scroll(
            s0,
            Cursor {
                line: 12,
                col: 0,
                preferred_col: 0,
            },
            4,
            &rope,
            80,
        );
        assert_eq!(s, s0);
    }

    #[test]
    fn file_start_and_file_end_jump_to_extremes() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc\ndef\nghij", Dims { cols: 40, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 1,
            col: 2,
            preferred_col: 2,
        };

        // ctrl+end → last line, last col.
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::End),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 2,
                col: 4,
                preferred_col: 4,
            }
        );

        // ctrl+home → line 0, col 0.
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Home),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 0,
                col: 0,
                preferred_col: 0,
            }
        );
    }

    #[test]
    fn word_right_and_word_left_move_by_word() {
        // M10 unbinds alt+b/f from word motion (legacy reserves
        // them for jump-back/forward). Use an explicit keymap so
        // this test still exercises the word-move primitives.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo bar  baz", Dims { cols: 40, rows: 5 });
        let mut km = default_keymap();
        km.bind("alt+f", Command::CursorWordRight);
        km.bind("alt+b", Command::CursorWordLeft);
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();

        let mut press = |k: KeyEvent,
                     tabs: &mut Tabs,
                     edits: &mut BufferEdits,
                     chord: &mut ChordState,
                     kill_ring: &mut KillRing,
                     clip: &mut ClipboardState,
                     alerts: &mut AlertState,
                     jumps: &mut JumpListState,
                     browser: &mut BrowserUi,
                     fs: &FsTree| {
            super::super::dispatch_key(
                k, tabs, edits, kill_ring, clip, alerts, jumps, browser, fs, &store, &term,
        &mut find_file, &mut isearch, &mut file_search, &mut path_chains, &km,
                chord,);
        };

        press(
            key(KeyModifiers::ALT, KeyCode::Char('f')),
            &mut tabs,
            &mut edits,
            &mut chord,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
        );
        assert_eq!(tabs.open[0].cursor.col, 3);
        press(
            key(KeyModifiers::ALT, KeyCode::Char('f')),
            &mut tabs,
            &mut edits,
            &mut chord,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
        );
        assert_eq!(tabs.open[0].cursor.col, 7);
        press(
            key(KeyModifiers::ALT, KeyCode::Char('b')),
            &mut tabs,
            &mut edits,
            &mut chord,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
        );
        assert_eq!(tabs.open[0].cursor.col, 4);
        press(
            key(KeyModifiers::ALT, KeyCode::Char('b')),
            &mut tabs,
            &mut edits,
            &mut chord,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
        );
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn end_of_line_then_type_does_not_panic() {
        // Crash repro: C-e (Move::LineEnd) + any letter. Happens
        // on short lines too — must not depend on soft-wrap.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hello\nworld", Dims { cols: 40, rows: 10 });
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('e')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        let eb = edits.buffers.get(&tabs.open[0].path).expect("eb");
        assert_eq!(eb.rope.to_string(), "hellox\nworld");
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 6);

        // Also exercise the render path — crashes here are where
        // the user's actual session dies.
        use crate::query::{
            self, DiagnosticsStatesInput, EditedBuffersInput,
            OverlaysInput, StoreLoadedInput, SyntaxStatesInput, TabsActiveInput,
        };
        use led_state_browser::BrowserUi;
        use led_state_diagnostics::DiagnosticsStates;
        use led_state_syntax::SyntaxStates;
        use led_driver_terminal_core::{Layout, Rect};
        let syntax = SyntaxStates::default();
        let diags = DiagnosticsStates::default();
        let browser = BrowserUi::default();
        let dims = term.dims.expect("dims");
        let layout = Layout::compute(dims, browser.visible);
        let _ = query::body_model(
            EditedBuffersInput::new(&edits),
            StoreLoadedInput::new(&store),
            TabsActiveInput::new(&tabs),
            OverlaysInput::new(&None, &None, &None),
            SyntaxStatesInput::new(&syntax),
            DiagnosticsStatesInput::new(&diags),
            layout.editor_area,
        );
        // Reaching here == body_model didn't panic.
        let _: Rect = layout.editor_area;
    }

    // ── soft-wrap (sub-line) navigation ─────────────────────────────────

    #[test]
    fn down_moves_within_wrapped_logical_line() {
        // content_cols=10 reserves the last column for a `\`
        // continuation glyph, so the effective wrap width is 9.
        // A 16-char line splits into sub 0 [0,9) and sub 1 [9,16).
        // content_cols=10, wrap_width=9. A 16-char line splits
        // into sub 0 [0, 9) and sub 1 [9, 16). From (line=0,
        // col=3), Down must land on the same logical line,
        // sub-line 1, at col 9+3=12.
        let rope = Rope::from_str("0123456789abcdef\nnext");
        let c = apply_move(
            Cursor { line: 0, col: 3, preferred_col: 3 },
            &rope,
            Move::Down,
            10,
            10,
        );
        assert_eq!(c.line, 0);
        assert_eq!(c.col, 12);
        assert_eq!(c.preferred_col, 3);
    }

    #[test]
    fn up_onto_non_last_sub_caps_preferred_col_below_wrap_boundary() {
        // Regression: when `preferred_col` carries a value >= the
        // target non-last sub's width, a naïve `min(pref, width)`
        // would land the cursor at `start + width` — the wrap
        // boundary — and `col_to_sub_line` resolves that as the
        // NEXT sub, bouncing arrow-up between two adjacent subs.
        // Cap at `width - 1` for non-last subs keeps the cursor
        // inside the target.
        //
        // content_cols=10, wrap_width=9. Line of 19 chars wraps
        // into 3 subs: [0,9), [9,18), [18,19) (last, 1 char).
        // Parking preferred_col=10 (artificially high) simulates
        // state arriving from a wider context.
        let rope = Rope::from_str("0123456789abcdefghi\nnext");
        let c = apply_move(
            Cursor { line: 0, col: 19, preferred_col: 10 },
            &rope,
            Move::Up,
            10,
            10,
        );
        assert_eq!(c.line, 0);
        // Up from sub 2 lands on sub 1 (non-last). Cap at width-1
        // = 8, so col = 9 + 8 = 17 (not 18, which is the wrap
        // boundary to sub 2).
        assert_eq!(c.col, 17);
        assert_eq!(c.preferred_col, 10);
    }

    #[test]
    fn up_moves_within_wrapped_logical_line() {
        // content_cols=10, wrap_width=9. Sub 0 [0,9), sub 1 [9,16).
        // From (line=0, col=12) on sub 1 with preferred_col=3,
        // Up lands on sub 0 at col 3.
        let rope = Rope::from_str("0123456789abcdef\nnext");
        let c = apply_move(
            Cursor { line: 0, col: 12, preferred_col: 3 },
            &rope,
            Move::Up,
            10,
            10,
        );
        assert_eq!(c.line, 0);
        assert_eq!(c.col, 3);
        assert_eq!(c.preferred_col, 3);
    }

    #[test]
    fn down_crosses_into_next_logical_line_after_last_sub() {
        // From sub-line 1 of line 0, Down should advance to line 1.
        let rope = Rope::from_str("0123456789abcdef\nnext");
        let c = apply_move(
            Cursor { line: 0, col: 12, preferred_col: 3 },
            &rope,
            Move::Down,
            10,
            10,
        );
        assert_eq!(c.line, 1);
        assert_eq!(c.col, 3);
    }

    #[test]
    fn line_end_goes_to_logical_end_even_on_wrapped_line() {
        // Legacy `model::mov::line_end` ignores sub-line boundaries:
        // End walks to `line_len` regardless of how many visual
        // rows the line spans. A 16-char line at content_cols=10
        // wraps into 2 sub-lines, but End from sub 0 still jumps
        // straight to col 16.
        let rope = Rope::from_str("0123456789abcdef");
        let c = apply_move(
            Cursor { line: 0, col: 3, preferred_col: 3 },
            &rope,
            Move::LineEnd,
            10,
            10,
        );
        assert_eq!(c.col, 16);
    }

    #[test]
    fn line_start_goes_to_logical_col_zero_even_on_wrapped_line() {
        // Legacy `model::mov::line_start` returns col 0. Home from
        // sub-line 1 walks all the way back to the start of the
        // logical line, not just the start of the visible row.
        let rope = Rope::from_str("0123456789abcdef");
        let c = apply_move(
            Cursor { line: 0, col: 12, preferred_col: 3 },
            &rope,
            Move::LineStart,
            10,
            10,
        );
        assert_eq!(c.col, 0);
    }

    #[test]
    fn adjust_scroll_advances_by_sub_line_when_wrapped_line_fills_viewport() {
        // A single wrapped line with 60 chars at content_cols=10
        // (wrap_width=9) produces 7 sub-lines. With body_rows=3
        // and scroll top sitting at (0, sub 0), a cursor at (0,
        // col 40) is sub-line 4 of that same logical line — the
        // viewport needs to scroll forward two sub-lines so the
        // cursor ends up at the bottom row.
        let rope = Rope::from_str("abcdefghijABCDEFGHIJ0123456789!@#$%^&*()qwertyuiopQWERTYUIOP");
        let s = adjust_scroll(
            Scroll { top: 0, top_sub_line: led_core::SubLine(0) },
            Cursor { line: 0, col: 40, preferred_col: 0 },
            3,
            &rope,
            10,
        );
        assert_eq!(s, Scroll { top: 0, top_sub_line: led_core::SubLine(2) });
    }
}
