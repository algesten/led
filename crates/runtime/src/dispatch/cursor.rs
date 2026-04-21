//! Cursor movement (M2, M6) — pure geometry over the rope.
//!
//! `apply_move` is the decision function; `move_cursor` is the
//! dispatch-facing wrapper that reads the right rope (edits first,
//! store second) and updates both cursor + scroll on the active tab.

use led_driver_buffers_core::{BufferStore, LoadState};
use led_driver_terminal_core::Terminal;
use led_state_buffer_edits::BufferEdits;
use led_state_tabs::{Cursor, Scroll, Tabs};
use ropey::Rope;
use std::sync::Arc;

use super::shared::{is_word_char, line_char_len, rope_char_at};

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

    let body_rows = terminal
        .dims
        .map(|d| d.rows.saturating_sub(1) as usize)
        .unwrap_or(0);

    let tab = &mut tabs.open[idx];
    tab.cursor = apply_move(tab.cursor, &rope, m, body_rows);
    tab.scroll = adjust_scroll(tab.scroll, tab.cursor, body_rows);
}

/// Pure cursor geometry over a rope. Clamps every output to valid
/// buffer coordinates given the current rope extent.
///
/// Vertical moves (`Up` / `Down` / `PageUp` / `PageDown`) carry
/// `preferred_col` forward and clamp `col` to the destination line —
/// so traversing a short line and landing on a long line later
/// restores the original goal column. Horizontal moves re-anchor
/// `preferred_col` to the new `col`.
pub(super) fn apply_move(c: Cursor, rope: &Rope, m: Move, body_rows: usize) -> Cursor {
    let line_count = rope.len_lines().max(1);
    let last_line = line_count - 1;
    let clamp_col = |line: usize, col: usize| col.min(line_char_len(rope, line));

    // Vertical move: pick `nl`, clamp goal col to it, keep preferred.
    let vertical = |nl: usize| -> Cursor {
        Cursor {
            line: nl,
            col: clamp_col(nl, c.preferred_col),
            preferred_col: c.preferred_col,
        }
    };
    // Horizontal move: anchor preferred_col to the new col.
    let horizontal = |line: usize, col: usize| -> Cursor {
        Cursor {
            line,
            col,
            preferred_col: col,
        }
    };

    match m {
        Move::Up => vertical(c.line.saturating_sub(1)),
        Move::Down => vertical((c.line + 1).min(last_line)),
        Move::PageUp => vertical(c.line.saturating_sub(body_rows.max(1))),
        Move::PageDown => vertical((c.line + body_rows.max(1)).min(last_line)),
        Move::Left => horizontal(c.line, c.col.saturating_sub(1)),
        Move::Right => horizontal(c.line, clamp_col(c.line, c.col.saturating_add(1))),
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

/// Move scroll.top so that the cursor row stays within
/// `[top, top + body_rows)`.
pub(super) fn adjust_scroll(s: Scroll, c: Cursor, body_rows: usize) -> Scroll {
    if body_rows == 0 {
        return s;
    }
    if c.line < s.top {
        Scroll { top: c.line }
    } else if c.line >= s.top.saturating_add(body_rows) {
        Scroll {
            top: c.line + 1 - body_rows,
        }
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use led_state_find_file::FindFileState;
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
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("a\nb\nc\nd\ne\nf", Dims { cols: 10, rows: 5 });
        // body_rows = 4. Cursor starts at (0,0); moving down stays in view.
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
        assert_eq!(tabs.open[0].scroll, Scroll { top: 0 });
    }

    #[test]
    fn down_scrolls_when_cursor_would_leave_viewport() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("a\nb\nc\nd\ne\nf", Dims { cols: 10, rows: 4 });
        // body_rows = 3. Fourth Down leaves viewport → scroll.top becomes 1.
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
        assert_eq!(tabs.open[0].scroll, Scroll { top: 1 });
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
        tabs.open[0].scroll = Scroll { top: 3 };
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
        assert_eq!(tabs.open[0].scroll, Scroll { top: 2 });
    }

    #[test]
    fn right_clamps_to_line_end_then_stops() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi\nworld", Dims { cols: 10, rows: 5 });
        // Line 0 = "hi" (len 2). Right from col 0 → 1 → 2 → 2.
        for expected in [1usize, 2, 2] {
            dispatch_default(
                key(KeyModifiers::NONE, KeyCode::Right),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
            assert_eq!(tabs.open[0].cursor.col, expected);
        }
    }

    #[test]
    fn left_stops_at_line_start() {
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
        let (mut tabs, mut edits, store, term) =
            fixture_with_content(&body, Dims { cols: 40, rows: 11 });
        // body_rows = 10. PageDown from line 0 → line 10, scroll follows.
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
        let c = apply_move(start, &rope, Move::Down, 10);
        assert_eq!(
            c,
            Cursor {
                line: 1,
                col: 2,
                preferred_col: 7,
            }
        );

        // Down again onto the long third line — col returns to 7.
        let c = apply_move(c, &rope, Move::Down, 10);
        assert_eq!(
            c,
            Cursor {
                line: 2,
                col: 7,
                preferred_col: 7,
            }
        );

        // And symmetric Up traversal also restores.
        let c = apply_move(c, &rope, Move::Up, 10);
        assert_eq!(
            c,
            Cursor {
                line: 1,
                col: 2,
                preferred_col: 7,
            }
        );
        let c = apply_move(c, &rope, Move::Up, 10);
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
        let c = apply_move(c, &rope, Move::Left, 10);
        assert_eq!(
            c,
            Cursor {
                line: 0,
                col: 7,
                preferred_col: 7,
            }
        );
        let c = apply_move(c, &rope, Move::Down, 10);
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
        let c = apply_move(start, &rope, Move::PageDown, 10);
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
        let s = adjust_scroll(
            Scroll { top: 0 },
            Cursor {
                line: 8,
                col: 0,
                preferred_col: 0,
            },
            4,
        );
        assert_eq!(s, Scroll { top: 5 });
    }

    #[test]
    fn adjust_scroll_noop_when_cursor_inside_window() {
        let s0 = Scroll { top: 10 };
        let s = adjust_scroll(
            s0,
            Cursor {
                line: 12,
                col: 0,
                preferred_col: 0,
            },
            4,
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
        &mut find_file, &km,
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
}
