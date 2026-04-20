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
//!
//! M3 adds editing. Printable chars, `Enter`, `Backspace`, `Delete`
//! each mutate the active tab's buffer in [`BufferEdits`] (the
//! user-decision source that sits alongside `BufferStore`). Cursor
//! movement also reads edits first, store second, so movement works
//! on the edited rope even before save (M4) lands.

use led_driver_buffers_core::{BufferStore, LoadState};
use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers, Terminal};
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_tabs::{Cursor, Scroll, Tab, Tabs};
use ropey::Rope;
use std::sync::Arc;

use crate::keymap::{Command, Keymap};
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
    edits: &mut BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
    keymap: &Keymap,
) -> DispatchOutcome {
    match ev {
        Event::Key(k) => dispatch_key(k, tabs, edits, store, terminal, keymap),
        // `Resize` is applied inside `TerminalInputDriver.process` —
        // pure state, no dispatch work here. M2 does not re-clamp
        // cursor/scroll on resize; next movement re-clamps.
        Event::Resize(_) => DispatchOutcome::Continue,
        Event::Quit => DispatchOutcome::Quit,
    }
}

/// Keymap-first dispatch. The keymap resolves a key to a [`Command`];
/// unbound printable characters fall through to
/// [`Command::InsertChar`] as an implicit "insert the typed char"
/// policy.
pub fn dispatch_key(
    k: KeyEvent,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
    keymap: &Keymap,
) -> DispatchOutcome {
    let cmd = match keymap.lookup(&k) {
        Some(cmd) => cmd,
        None => match implicit_insert(&k) {
            Some(cmd) => cmd,
            None => return DispatchOutcome::Continue,
        },
    };
    run_command(cmd, tabs, edits, store, terminal)
}

/// Printable-char fallback: an unbound `Char(c)` with no Ctrl / Alt
/// is treated as "insert that character". Shift is tolerated because
/// terminals typically fold shift into the char itself (`A` vs `a`).
fn implicit_insert(k: &KeyEvent) -> Option<Command> {
    let KeyCode::Char(c) = k.code else {
        return None;
    };
    if k.modifiers.contains(KeyModifiers::CONTROL) || k.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    if c.is_control() {
        return None;
    }
    Some(Command::InsertChar(c))
}

fn run_command(
    cmd: Command,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    match cmd {
        Command::Quit => DispatchOutcome::Quit,
        Command::Save => {
            request_save_active(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::TabNext => {
            cycle_active(tabs, 1);
            DispatchOutcome::Continue
        }
        Command::TabPrev => {
            cycle_active(tabs, -1);
            DispatchOutcome::Continue
        }
        Command::CursorUp => {
            move_cursor(tabs, edits, store, terminal, Move::Up);
            DispatchOutcome::Continue
        }
        Command::CursorDown => {
            move_cursor(tabs, edits, store, terminal, Move::Down);
            DispatchOutcome::Continue
        }
        Command::CursorLeft => {
            move_cursor(tabs, edits, store, terminal, Move::Left);
            DispatchOutcome::Continue
        }
        Command::CursorRight => {
            move_cursor(tabs, edits, store, terminal, Move::Right);
            DispatchOutcome::Continue
        }
        Command::CursorLineStart => {
            move_cursor(tabs, edits, store, terminal, Move::LineStart);
            DispatchOutcome::Continue
        }
        Command::CursorLineEnd => {
            move_cursor(tabs, edits, store, terminal, Move::LineEnd);
            DispatchOutcome::Continue
        }
        Command::CursorPageUp => {
            move_cursor(tabs, edits, store, terminal, Move::PageUp);
            DispatchOutcome::Continue
        }
        Command::CursorPageDown => {
            move_cursor(tabs, edits, store, terminal, Move::PageDown);
            DispatchOutcome::Continue
        }
        Command::InsertNewline => {
            insert_newline(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::DeleteBack => {
            delete_back(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::DeleteForward => {
            delete_forward(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::InsertChar(c) => {
            insert_char(tabs, edits, c);
            DispatchOutcome::Continue
        }
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
///
/// Rope lookup prefers [`BufferEdits`] (the user's edited view); the
/// store fallback only matters before the runtime has seeded edits
/// for a just-loaded buffer.
fn move_cursor(
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

// ── Save request ───────────────────────────────────────────────────────

/// Insert the active tab's path into `pending_saves` iff the buffer
/// is loaded and dirty. Runtime's query + execute pair turns the
/// entry into an actual write and clears it synchronously before
/// spawning async work.
fn request_save_active(tabs: &Tabs, edits: &mut BufferEdits) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return;
    };
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    if eb.dirty() {
        edits.pending_saves.insert(tab.path.clone());
    }
}

// ── Edit primitives ────────────────────────────────────────────────────

/// Access the active tab and its edited buffer together. Bails if
/// either is missing — buffer not yet loaded means edit keys no-op.
fn with_active<F>(tabs: &mut Tabs, edits: &mut BufferEdits, f: F)
where
    F: FnOnce(&mut Tab, &mut EditedBuffer),
{
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let tab = &mut tabs.open[idx];
    let Some(eb) = edits.buffers.get_mut(&tab.path) else {
        return;
    };
    f(tab, eb);
}

fn bump(eb: &mut EditedBuffer, new_rope: Rope) {
    eb.rope = Arc::new(new_rope);
    eb.version = eb.version.saturating_add(1);
    // saved_version untouched — `dirty()` now derives as version > saved_version.
}

fn insert_char(tabs: &mut Tabs, edits: &mut BufferEdits, ch: char) {
    with_active(tabs, edits, |tab, eb| {
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.insert_char(char_idx, ch);
        bump(eb, rope);
        tab.cursor.col += 1;
        tab.cursor.preferred_col = tab.cursor.col;
    });
}

fn insert_newline(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.insert_char(char_idx, '\n');
        bump(eb, rope);
        tab.cursor.line += 1;
        tab.cursor.col = 0;
        tab.cursor.preferred_col = 0;
    });
}

fn delete_back(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        if tab.cursor.line == 0 && tab.cursor.col == 0 {
            return;
        }
        // Capture the join column *before* the remove. After the
        // newline is gone the previous line grows to include the
        // current one, so post-remove length is too large.
        let join_col = if tab.cursor.col == 0 {
            line_char_len(&eb.rope, tab.cursor.line - 1)
        } else {
            0
        };
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.remove(char_idx - 1..char_idx);
        if tab.cursor.col > 0 {
            tab.cursor.col -= 1;
        } else {
            tab.cursor.line -= 1;
            tab.cursor.col = join_col;
        }
        tab.cursor.preferred_col = tab.cursor.col;
        bump(eb, rope);
    });
}

fn delete_forward(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let line_count = eb.rope.len_lines();
        let on_last_line = tab.cursor.line + 1 >= line_count;
        let at_line_end = tab.cursor.col >= line_char_len(&eb.rope, tab.cursor.line);
        if on_last_line && at_line_end {
            return;
        }
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.remove(char_idx..char_idx + 1);
        bump(eb, rope);
        // Cursor stays put. preferred_col unchanged (col didn't move).
    });
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
    use crate::keymap::default_keymap;
    use led_core::{CanonPath, UserPath};
    use led_driver_buffers_core::LoadState;
    use led_driver_terminal_core::{Dims, KeyCode, KeyEvent, KeyModifiers, Terminal};
    use led_state_tabs::{Cursor, Scroll, Tab, TabId, Tabs};
    use ropey::Rope;
    use std::sync::Arc;

    /// Dispatch a key with the default keymap. Almost every test cares
    /// about state effects rather than the keymap itself, so hiding
    /// the keymap arg makes call sites readable.
    fn dispatch_default(
        k: KeyEvent,
        tabs: &mut Tabs,
        edits: &mut BufferEdits,
        store: &BufferStore,
        terminal: &Terminal,
    ) -> DispatchOutcome {
        dispatch_key(k, tabs, edits, store, terminal, &default_keymap())
    }

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
        // Tests that only care about tab-switch / quit don't need an
        // edits source — empty BufferEdits means every edit-primitive
        // branch no-ops, which is exactly what these tests assume.
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let terminal = Terminal::default();
        let keymap = crate::keymap::default_keymap();
        dispatch_key(k, tabs, &mut edits, &store, &terminal, &keymap)
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
        // Terminals may encode Shift+Tab either as `{Tab, SHIFT}`
        // (modifier) or `{BackTab, NONE}` (special key code). The
        // default keymap binds both forms.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        noop_dispatch(key(KeyModifiers::SHIFT, KeyCode::Tab), &mut tabs);
        assert_eq!(tabs.active, Some(TabId(3)));
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::BackTab), &mut tabs);
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

    /// M3: fixture seeds both `BufferStore` (disk) and `BufferEdits`
    /// (the user-visible rope) with identical content — mirrors the
    /// production path where the runtime copies newly-Ready ropes
    /// into `BufferEdits` via load completions.
    fn fixture_with_content(
        body: &str,
        dims: Dims,
    ) -> (Tabs, BufferEdits, BufferStore, Terminal) {
        let rope = Arc::new(Rope::from_str(body));
        let mut edits = BufferEdits::default();
        edits
            .buffers
            .insert(canon("file.rs"), EditedBuffer::fresh(rope.clone()));
        let mut store = BufferStore::default();
        store
            .loaded
            .insert(canon("file.rs"), LoadState::Ready(rope));
        (
            tabs_with(&[("file.rs", 1)], Some(1)),
            edits,
            store,
            terminal_with(Some(dims)),
        )
    }

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

    // ── M3: edit primitives ─────────────────────────────────────────────

    fn rope_of(edits: &BufferEdits, path: &str) -> Arc<Rope> {
        edits
            .buffers
            .get(&canon(path))
            .expect("seeded")
            .rope
            .clone()
    }

    fn version_of(edits: &BufferEdits, path: &str) -> u64 {
        edits.buffers.get(&canon(path)).expect("seeded").version
    }

    fn dirty_of(edits: &BufferEdits, path: &str) -> bool {
        edits.buffers.get(&canon(path)).expect("seeded").dirty()
    }

    #[test]
    fn insert_char_advances_cursor_and_bumps_version() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc\n", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char('X')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "aXbc\n");
        assert_eq!(tabs.open[0].cursor.col, 2);
        assert_eq!(tabs.open[0].cursor.preferred_col, 2);
        assert_eq!(version_of(&edits, "file.rs"), 1);
        assert!(dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn insert_newline_splits_line_and_drops_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdef\n", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Enter),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc\ndef\n");
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 1,
                col: 0,
                preferred_col: 0,
            }
        );
        assert!(dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn backspace_deletes_char_before_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hello", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 5,
            preferred_col: 5,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Backspace),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hell");
        assert_eq!(tabs.open[0].cursor.col, 4);
    }

    #[test]
    fn backspace_at_column_zero_joins_with_previous_line() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo\nbar\n", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 1,
            col: 0,
            preferred_col: 0,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Backspace),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foobar\n");
        // Cursor landed where the join point is — end of the old "foo".
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 0,
                col: 3,
                preferred_col: 3,
            }
        );
    }

    #[test]
    fn backspace_at_origin_is_a_noop() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc\n", Dims { cols: 10, rows: 5 });
        let v0 = version_of(&edits, "file.rs");

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Backspace),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc\n");
        assert_eq!(version_of(&edits, "file.rs"), v0);
        assert!(!dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn delete_forward_removes_char_at_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hello", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Delete),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hllo");
        // Cursor stays put.
        assert_eq!(tabs.open[0].cursor.col, 1);
    }

    #[test]
    fn delete_forward_at_end_of_line_joins_with_next() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo\nbar", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Delete),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foobar");
        // Cursor position unchanged — still at the join point.
        assert_eq!(tabs.open[0].cursor.col, 3);
    }

    #[test]
    fn delete_forward_at_eof_is_a_noop() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        let v0 = version_of(&edits, "file.rs");

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Delete),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc");
        assert_eq!(version_of(&edits, "file.rs"), v0);
    }

    #[test]
    fn edit_on_unloaded_buffer_is_swallowed() {
        // Tab is open but BufferEdits has no entry (file hasn't
        // loaded yet) — all four primitives no-op and leave the
        // cursor alone.
        let mut tabs = tabs_with(&[("file.rs", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 0,
            preferred_col: 0,
        };

        for code in [
            KeyCode::Char('x'),
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Delete,
        ] {
            dispatch_default(
                key(KeyModifiers::NONE, code),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
        }

        assert!(edits.buffers.is_empty());
        assert_eq!(tabs.open[0].cursor, Cursor::default());
    }

    #[test]
    fn ctrl_c_still_quits_not_inserts() {
        // Regression guard: Ctrl-C must reach the Quit arm before the
        // Char-insert arm, even though 'c' is printable.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 10, rows: 5 });
        let outcome = dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(outcome, DispatchOutcome::Quit);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
        assert!(!dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn edits_survive_tab_switch() {
        // Two tabs, two files; edit each, switch between, confirm the
        // ropes + cursors are preserved per tab.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits
            .buffers
            .insert(canon("a"), EditedBuffer::fresh(Arc::new(Rope::from_str("a"))));
        edits
            .buffers
            .insert(canon("b"), EditedBuffer::fresh(Arc::new(Rope::from_str("b"))));
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };

        // Edit tab a.
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char('!')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        // Switch to tab b.
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Tab),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        tabs.open[1].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char('?')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "a").to_string(), "a!");
        assert_eq!(rope_of(&edits, "b").to_string(), "b?");
        assert!(dirty_of(&edits, "a"));
        assert!(dirty_of(&edits, "b"));
    }

    // ── M4: Ctrl-S save request ─────────────────────────────────────────

    #[test]
    fn ctrl_s_queues_save_for_dirty_active_buffer() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        // Force dirty by bumping version past saved_version.
        let eb = edits
            .buffers
            .get_mut(&canon("file.rs"))
            .expect("seeded");
        eb.version = 1;
        assert!(eb.dirty());

        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert!(edits.pending_saves.contains(&canon("file.rs")));
    }

    #[test]
    fn ctrl_s_on_clean_buffer_is_noop() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        // Buffer is fresh (version == saved_version == 0).
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.is_empty());
    }

    #[test]
    fn ctrl_s_on_unloaded_buffer_is_noop() {
        let mut tabs = tabs_with(&[("file.rs", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.is_empty());
    }

    #[test]
    fn custom_keymap_routes_to_the_bound_command() {
        // Bind Ctrl-Q to Quit on a custom keymap and confirm dispatch
        // honours it. Ctrl-C — unbound in this map — reaches nothing
        // special (falls through as no-op, since it's a control char
        // the implicit-insert fallback also rejects).
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        let mut km = Keymap::empty();
        km.bind("ctrl-q", Command::Quit);

        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('q')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
            &km,
        );
        assert_eq!(outcome, DispatchOutcome::Quit);

        // Ctrl-C not bound here → Continue (not Quit).
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
            &km,
        );
        assert_eq!(outcome, DispatchOutcome::Continue);
    }

    #[test]
    fn unbound_printable_char_falls_through_to_insert() {
        // A printable char with no binding falls through to InsertChar.
        // Only the active tab's edited rope gets the character.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 10, rows: 5 });

        let km = Keymap::empty(); // no bindings at all
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('z')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
            &km,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "z");
    }

    #[test]
    fn ctrl_char_without_binding_is_swallowed_not_inserted() {
        // An unbound Ctrl-combo must NOT fall through to InsertChar,
        // otherwise typing Ctrl-X on an unconfigured keymap would
        // insert 'x' into the buffer.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 10, rows: 5 });

        let km = Keymap::empty();
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
            &km,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
    }

    #[test]
    fn ctrl_s_targets_only_active_tab() {
        // Two dirty buffers; Ctrl-S on tab b should only enqueue b.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(2));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("A")),
                version: 1,
                saved_version: 0,
            },
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("B")),
                version: 1,
                saved_version: 0,
            },
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert!(edits.pending_saves.contains(&canon("b")));
        assert!(!edits.pending_saves.contains(&canon("a")));
    }
}
