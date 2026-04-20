//! Dispatch: applies `Event`s to atoms.
//!
//! Kept deliberately small per QUERY-ARCH § "The event handler". Each
//! function mutates atoms directly; no memos, no queries. Returns a
//! [`DispatchOutcome`] so the main loop can learn that a quit was
//! requested without looking for a sentinel in state.
//!
//! M2 extends dispatch with cursor movement. Arrow (and Home/End/Page)
//! keys mutate the active tab's `cursor` and then re-evaluate `scroll`
//! so the cursor stays inside the viewport. Clamping against the rope
//! requires read-only access to [`BufferStore`]; scroll needs the
//! current `Terminal.dims` for the body-row count. Hence the widened
//! signature.

use led_driver_buffers_core::{BufferStore, LoadState};
use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers, Terminal};
use led_state_tabs::{Cursor, Scroll, Tab, Tabs};
use ropey::Rope;

use crate::Event;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    Continue,
    Quit,
}

/// Logical cursor moves. Built from key events in [`dispatch_key`] and
/// applied by the pure [`apply_move`] helper so the geometry is unit
/// testable without any keyboard setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Move {
    Up,
    Down,
    Left,
    Right,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
}

/// Top-level entry point used by the main loop.
pub fn dispatch(
    ev: Event,
    tabs: &mut Tabs,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    match ev {
        Event::Key(k) => dispatch_key(k, tabs, store, terminal),
        // `Resize` is applied inside `TerminalInputDriver.process` —
        // pure state, no dispatch work here. M2 does not re-clamp
        // cursor/scroll on resize; next movement re-clamps.
        Event::Resize(_) => DispatchOutcome::Continue,
        Event::Quit => DispatchOutcome::Quit,
    }
}

pub fn dispatch_key(
    k: KeyEvent,
    tabs: &mut Tabs,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    match (k.modifiers, k.code) {
        (m, KeyCode::Char('c')) if m.contains(KeyModifiers::CONTROL) => DispatchOutcome::Quit,
        (m, KeyCode::Tab) if m.is_empty() => {
            cycle_active(tabs, 1);
            DispatchOutcome::Continue
        }
        (m, KeyCode::BackTab) if m.contains(KeyModifiers::SHIFT) || m.is_empty() => {
            // Many terminals emit BackTab without an explicit SHIFT flag;
            // accept both for robustness.
            cycle_active(tabs, -1);
            DispatchOutcome::Continue
        }
        (m, KeyCode::Up) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::Up);
            DispatchOutcome::Continue
        }
        (m, KeyCode::Down) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::Down);
            DispatchOutcome::Continue
        }
        (m, KeyCode::Left) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::Left);
            DispatchOutcome::Continue
        }
        (m, KeyCode::Right) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::Right);
            DispatchOutcome::Continue
        }
        (m, KeyCode::Home) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::LineStart);
            DispatchOutcome::Continue
        }
        (m, KeyCode::End) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::LineEnd);
            DispatchOutcome::Continue
        }
        (m, KeyCode::PageUp) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::PageUp);
            DispatchOutcome::Continue
        }
        (m, KeyCode::PageDown) if m.is_empty() => {
            move_cursor(tabs, store, terminal, Move::PageDown);
            DispatchOutcome::Continue
        }
        _ => DispatchOutcome::Continue,
    }
}

fn cycle_active(tabs: &mut Tabs, delta: isize) {
    if tabs.open.is_empty() {
        return;
    }
    let n = tabs.open.len() as isize;
    let cur_idx = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t: &Tab| t.id == id))
        .unwrap_or(0) as isize;
    let next_idx = (cur_idx + delta).rem_euclid(n) as usize;
    tabs.active = Some(tabs.open[next_idx].id);
}

/// Apply a move to the active tab: update cursor, then adjust scroll so
/// the cursor stays inside the body viewport. No-op when there is no
/// active tab or its buffer isn't loaded yet — the cursor has nothing
/// to clamp against.
fn move_cursor(tabs: &mut Tabs, store: &BufferStore, terminal: &Terminal, m: Move) {
    let Some(active) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == active) else {
        return;
    };
    let rope = match store.loaded.get(&tabs.open[idx].path) {
        Some(LoadState::Ready(r)) => r.clone(),
        _ => return,
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
fn apply_move(c: Cursor, rope: &Rope, m: Move, body_rows: usize) -> Cursor {
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
    }
}

/// Move scroll.top so that the cursor row stays within [top, top+body_rows).
fn adjust_scroll(s: Scroll, c: Cursor, body_rows: usize) -> Scroll {
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

/// Character count of a buffer line, stripped of trailing `\n` / `\r\n`.
/// Out-of-range lines yield 0.
///
/// Walks the rope directly — no intermediate `String` allocation.
/// Called on every cursor keystroke, so this needs to stay cheap.
fn line_char_len(rope: &Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        return 0;
    }
    let slice = rope.line(line);
    let mut end = slice.len_chars();
    if end == 0 {
        return 0;
    }
    if slice.char(end - 1) == '\n' {
        end -= 1;
        if end > 0 && slice.char(end - 1) == '\r' {
            end -= 1;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{CanonPath, UserPath};
    use led_driver_buffers_core::LoadState;
    use led_driver_terminal_core::{Dims, KeyCode, KeyEvent, KeyModifiers, Terminal};
    use led_state_tabs::{Cursor, Scroll, Tab, TabId, Tabs};
    use ropey::Rope;
    use std::sync::Arc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn tabs_with(paths: &[(&str, u64)], active: Option<u64>) -> Tabs {
        let mut t = Tabs::default();
        for (p, id) in paths {
            t.open.push_back(Tab {
                id: TabId(*id),
                path: canon(p),
                ..Default::default()
            });
        }
        t.active = active.map(TabId);
        t
    }

    fn store_with(entries: &[(&str, &str)]) -> BufferStore {
        let mut s = BufferStore::default();
        for (p, body) in entries {
            s.loaded.insert(
                canon(p),
                LoadState::Ready(Arc::new(Rope::from_str(body))),
            );
        }
        s
    }

    fn terminal_with(dims: Option<Dims>) -> Terminal {
        Terminal {
            dims,
            ..Default::default()
        }
    }

    fn key(mods: KeyModifiers, code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
        }
    }

    fn noop_dispatch(k: KeyEvent, tabs: &mut Tabs) -> DispatchOutcome {
        let store = BufferStore::default();
        let terminal = Terminal::default();
        dispatch_key(k, tabs, &store, &terminal)
    }

    // ── Tab switching + quit (M1 behaviour, unchanged) ──────────────────

    #[test]
    fn tab_cycles_active_forward() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(2)));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(1)));
    }

    #[test]
    fn shift_tab_cycles_backward() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        noop_dispatch(key(KeyModifiers::SHIFT, KeyCode::BackTab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        noop_dispatch(key(KeyModifiers::SHIFT, KeyCode::BackTab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(2)));
    }

    #[test]
    fn ctrl_c_signals_quit() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let outcome = noop_dispatch(key(KeyModifiers::CONTROL, KeyCode::Char('c')), &mut tabs);
        assert_eq!(outcome, DispatchOutcome::Quit);
    }

    #[test]
    fn tab_on_empty_does_nothing() {
        let mut tabs = Tabs::default();
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, None);
    }

    // ── M2: cursor movement ─────────────────────────────────────────────

    fn fixture_with_content(body: &str, dims: Dims) -> (Tabs, BufferStore, Terminal) {
        (
            tabs_with(&[("file.rs", 1)], Some(1)),
            store_with(&[("file.rs", body)]),
            terminal_with(Some(dims)),
        )
    }

    #[test]
    fn down_moves_cursor_and_does_not_scroll_within_viewport() {
        let (mut tabs, store, term) =
            fixture_with_content("a\nb\nc\nd\ne\nf", Dims { cols: 10, rows: 5 });
        // body_rows = 4. Cursor starts at (0,0); moving down stays in view.
        for _ in 0..3 {
            dispatch_key(
                key(KeyModifiers::NONE, KeyCode::Down),
                &mut tabs,
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
        let (mut tabs, store, term) =
            fixture_with_content("a\nb\nc\nd\ne\nf", Dims { cols: 10, rows: 4 });
        // body_rows = 3. Fourth Down leaves viewport → scroll.top becomes 1.
        for _ in 0..3 {
            dispatch_key(
                key(KeyModifiers::NONE, KeyCode::Down),
                &mut tabs,
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
        let (mut tabs, store, term) =
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
            dispatch_key(
                key(KeyModifiers::NONE, KeyCode::Up),
                &mut tabs,
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
        let (mut tabs, store, term) =
            fixture_with_content("hi\nworld", Dims { cols: 10, rows: 5 });
        // Line 0 = "hi" (len 2). Right from col 0 → 1 → 2 → 2.
        for expected in [1usize, 2, 2] {
            dispatch_key(
                key(KeyModifiers::NONE, KeyCode::Right),
                &mut tabs,
                &store,
                &term,
            );
            assert_eq!(tabs.open[0].cursor.col, expected);
        }
    }

    #[test]
    fn left_stops_at_line_start() {
        let (mut tabs, store, term) =
            fixture_with_content("hi\nworld", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Left),
            &mut tabs,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.col, 0);
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Left),
            &mut tabs,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn home_end_jump_within_current_line() {
        let (mut tabs, store, term) =
            fixture_with_content("abcdef\nghij", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::End),
            &mut tabs,
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
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Home),
            &mut tabs,
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
        let (mut tabs, store, term) =
            fixture_with_content(&body, Dims { cols: 40, rows: 11 });
        // body_rows = 10. PageDown from line 0 → line 10, scroll follows.
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::PageDown),
            &mut tabs,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.line, 10);
        assert_eq!(tabs.open[0].scroll.top, 1);
    }

    #[test]
    fn movement_is_noop_when_buffer_not_loaded() {
        let mut tabs = tabs_with(&[("file.rs", 1)], Some(1));
        let store = BufferStore::default(); // no content loaded
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Down),
            &mut tabs,
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
            .map(|i| if i == 5 { "xy".into() } else { format!("line {i:03}") })
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
}
