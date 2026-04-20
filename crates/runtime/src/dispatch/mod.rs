//! Dispatch: applies `Event`s to atoms.
//!
//! Kept deliberately small per QUERY-ARCH § "The event handler". Each
//! function mutates atoms directly; no memos, no queries. Returns a
//! [`DispatchOutcome`] so the main loop can learn that a quit was
//! requested without looking for a sentinel in state.
//!
//! This module is split per concern:
//!
//! - [`shared`] — helpers used by most submodules (with_active, bump,
//!   cursor↔char conversion, line_char_len).
//! - [`cursor`] — Move enum, apply_move, move_cursor, adjust_scroll,
//!   word-boundary walking.
//! - [`edit`] — insert_char, insert_newline, delete_back,
//!   delete_forward.
//! - [`mark`] — set_mark_active, clear_mark, region_range.
//! - [`kill`] — kill_region, kill_line, request_yank, `apply_yank`
//!   (the ingest-side paste callback, re-exported).
//! - [`undo`] — undo_active / redo_active.
//! - [`tabs`] — cycle_active, kill_active.
//! - [`save`] — request_save_active, request_save_all.
//!
//! Tests live in `mod tests` below; they cover all the submodules.

mod cursor;
mod edit;
mod kill;
mod mark;
mod save;
mod shared;
mod tabs;
mod undo;

// Public surface — kept tight so the runtime only reaches in for
// the five externally-relevant names.
pub use kill::apply_yank;

// Aliases used by `run_command` + the tests module. Non-test items
// stay used unconditionally; test-only helpers live behind
// `#[cfg(test)]`.
use cursor::{Move, move_cursor};
use edit::{delete_back, delete_forward, insert_char, insert_newline};
use kill::{kill_line, kill_region, request_yank};
use mark::{clear_mark, set_mark_active};
use save::{request_save_active, request_save_all};
use tabs::{cycle_active, kill_active};
use undo::{redo_active, undo_active};

// Test-only imports: the cursor-geometry unit tests exercise these
// pure helpers directly.
#[cfg(test)]
use cursor::{adjust_scroll, apply_move};

use led_driver_buffers_core::BufferStore;
use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers, Terminal};
use led_state_buffer_edits::BufferEdits;
use led_state_kill_ring::KillRing;
use led_state_tabs::Tabs;

// Test-only re-export so the tests module's `use super::*` can
// construct `EditedBuffer` literals directly.
#[cfg(test)]
#[allow(unused_imports)]
use led_state_buffer_edits::EditedBuffer;

use crate::Event;
use crate::keymap::{ChordState, Command, Keymap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    Continue,
    Quit,
}

/// Top-level entry point used by the main loop.
#[allow(clippy::too_many_arguments)]
pub fn dispatch(
    ev: Event,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    kill_ring: &mut KillRing,
    store: &BufferStore,
    terminal: &Terminal,
    keymap: &Keymap,
    chord: &mut ChordState,
) -> DispatchOutcome {
    match ev {
        Event::Key(k) => dispatch_key(k, tabs, edits, kill_ring, store, terminal, keymap, chord),
        // `Resize` is applied inside `TerminalInputDriver.process` —
        // pure state, no dispatch work here. M2 does not re-clamp
        // cursor/scroll on resize; next movement re-clamps.
        Event::Resize(_) => DispatchOutcome::Continue,
        Event::Quit => DispatchOutcome::Quit,
    }
}

/// Keymap-first dispatch with chord support. Algorithm:
///
/// 1. If a chord prefix is pending, consume it and look up the
///    second key in that prefix's table. Unknown second key silently
///    cancels. Matches legacy `keymap.md` § "Chord prefix with no
///    second chord".
/// 2. Otherwise try the direct table.
/// 3. Otherwise, if the key is itself a prefix, store it as pending
///    and return.
/// 4. Otherwise fall through to [`implicit_insert`] — printable chars
///    with no Ctrl/Alt insert themselves.
///
/// The pending prefix is always cleared before resolving the second
/// key so a failed chord never leaks state into the next press.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_key(
    k: KeyEvent,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    kill_ring: &mut KillRing,
    store: &BufferStore,
    terminal: &Terminal,
    keymap: &Keymap,
    chord: &mut ChordState,
) -> DispatchOutcome {
    let resolved = resolve_command(k, keymap, chord);
    match resolved {
        Resolved::Command(cmd) => {
            let outcome = run_command(cmd, tabs, edits, kill_ring, store, terminal);
            // Kill-ring coalescing: any non-KillLine command breaks
            // the flag, so the next KillLine starts a fresh entry.
            if !matches!(cmd, Command::KillLine) {
                kill_ring.last_was_kill_line = false;
            }
            // Undo coalescing: any command other than a coalescable
            // InsertChar closes the open group. Non-edit commands
            // finalise via the blanket path; edit commands that
            // opened their own (non-coalescable) group already
            // finalised inside their primitive.
            if !is_coalescable_insert(&cmd) {
                finalise_history(edits);
            }
            outcome
        }
        Resolved::PrefixStored | Resolved::Continue => DispatchOutcome::Continue,
    }
}

fn is_coalescable_insert(cmd: &Command) -> bool {
    matches!(cmd, Command::InsertChar(c) if c.is_alphanumeric() || *c == '_')
}

fn finalise_history(edits: &mut BufferEdits) {
    for (_, eb) in edits.buffers.iter_mut() {
        eb.history.finalise();
    }
}

/// What `dispatch_key` did with the keystroke. Split out so the
/// coalescing bookkeeping stays in one place.
enum Resolved {
    Command(Command),
    /// A chord prefix was stored; next key resolves against it.
    PrefixStored,
    /// No binding and no implicit-insert match — silent no-op.
    Continue,
}

fn resolve_command(k: KeyEvent, keymap: &Keymap, chord: &mut ChordState) -> Resolved {
    if let Some(prefix) = chord.pending.take() {
        if let Some(cmd) = keymap.lookup_chord(&prefix, &k) {
            return Resolved::Command(cmd);
        }
        // Silent cancel — matches legacy behaviour.
        return Resolved::Continue;
    }
    if let Some(cmd) = keymap.lookup_direct(&k) {
        return Resolved::Command(cmd);
    }
    if keymap.is_prefix(&k) {
        chord.pending = Some(k);
        return Resolved::PrefixStored;
    }
    if let Some(cmd) = implicit_insert(&k) {
        return Resolved::Command(cmd);
    }
    Resolved::Continue
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
    kill_ring: &mut KillRing,
    store: &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome {
    match cmd {
        Command::Quit => DispatchOutcome::Quit,
        Command::Abort => {
            // Clear any set mark as part of the abort gesture.
            // Future milestones (M9 confirm-kill, M13 isearch,
            // M17/18 LSP overlays) short-circuit the dispatch
            // stream before this point when their modal is active.
            clear_mark(tabs);
            DispatchOutcome::Continue
        }
        Command::Save => {
            request_save_active(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::SaveAll => {
            request_save_all(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::SaveNoFormat => {
            // Alias of Save in M6. M18 (LSP format) will differentiate:
            // Save runs format first, SaveNoFormat skips it.
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
        Command::KillBuffer => {
            kill_active(tabs, edits);
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
        Command::CursorFileStart => {
            move_cursor(tabs, edits, store, terminal, Move::FileStart);
            DispatchOutcome::Continue
        }
        Command::CursorFileEnd => {
            move_cursor(tabs, edits, store, terminal, Move::FileEnd);
            DispatchOutcome::Continue
        }
        Command::CursorWordLeft => {
            move_cursor(tabs, edits, store, terminal, Move::WordLeft);
            DispatchOutcome::Continue
        }
        Command::CursorWordRight => {
            move_cursor(tabs, edits, store, terminal, Move::WordRight);
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
        Command::SetMark => {
            set_mark_active(tabs);
            DispatchOutcome::Continue
        }
        Command::KillRegion => {
            kill_region(tabs, edits, kill_ring);
            DispatchOutcome::Continue
        }
        Command::KillLine => {
            kill_line(tabs, edits, kill_ring);
            DispatchOutcome::Continue
        }
        Command::Yank => {
            request_yank(tabs, kill_ring);
            DispatchOutcome::Continue
        }
        Command::Undo => {
            undo_active(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::Redo => {
            redo_active(tabs, edits);
            DispatchOutcome::Continue
        }
    }
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

    /// Dispatch a key with the default keymap and a fresh chord
    /// slot. Tests that care about chord state pass their own
    /// `ChordState` via `dispatch_key` directly.
    fn dispatch_default(
        k: KeyEvent,
        tabs: &mut Tabs,
        edits: &mut BufferEdits,
        store: &BufferStore,
        terminal: &Terminal,
    ) -> DispatchOutcome {
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        dispatch_key(
            k,
            tabs,
            edits,
            &mut kill_ring,
            store,
            terminal,
            &default_keymap(),
            &mut chord,
        )
    }

    /// Press a chord sequence (prefix then second) with a fresh
    /// `ChordState`. Used by tests that want to exercise legacy-style
    /// chord-bound commands without duplicating the state setup.
    fn dispatch_chord_default(
        prefix: KeyEvent,
        second: KeyEvent,
        tabs: &mut Tabs,
        edits: &mut BufferEdits,
        store: &BufferStore,
        terminal: &Terminal,
    ) -> DispatchOutcome {
        let keymap = default_keymap();
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        dispatch_key(
            prefix,
            tabs,
            edits,
            &mut kill_ring,
            store,
            terminal,
            &keymap,
            &mut chord,
        );
        dispatch_key(
            second,
            tabs,
            edits,
            &mut kill_ring,
            store,
            terminal,
            &keymap,
            &mut chord,
        )
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
        let mut kill_ring = KillRing::default();
        let store = BufferStore::default();
        let terminal = Terminal::default();
        let keymap = crate::keymap::default_keymap();
        let mut chord = ChordState::default();
        dispatch_key(
            k,
            tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &terminal,
            &keymap,
            &mut chord,
        )
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
    fn ctrl_x_ctrl_c_signals_quit_as_chord() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let mut kill_ring = KillRing::default();
        let store = BufferStore::default();
        let term = Terminal::default();
        let keymap = default_keymap();
        let mut chord = ChordState::default();

        // First half of the chord: ctrl+x → pending, Continue.
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert_eq!(outcome, DispatchOutcome::Continue);
        assert!(chord.pending.is_some());

        // Second half: ctrl+c → chord fires Quit.
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert_eq!(outcome, DispatchOutcome::Quit);
        assert!(chord.pending.is_none());
    }

    #[test]
    fn plain_ctrl_c_no_longer_quits() {
        // Legacy parity: plain ctrl+c is unbound by default. It falls
        // through implicit_insert (control char — rejected there too)
        // and is a silent no-op.
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let outcome = noop_dispatch(key(KeyModifiers::CONTROL, KeyCode::Char('c')), &mut tabs);
        assert_eq!(outcome, DispatchOutcome::Continue);
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
    fn fixture_with_content(body: &str, dims: Dims) -> (Tabs, BufferEdits, BufferStore, Terminal) {
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
    fn ctrl_c_does_not_insert_c() {
        // Regression guard: plain ctrl+c is unbound in the M6 default
        // keymap (quit moved to ctrl+x ctrl+c), but we must still not
        // let implicit_insert turn it into `InsertChar('c')`.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 10, rows: 5 });
        let outcome = dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(outcome, DispatchOutcome::Continue);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
        assert!(!dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn edits_survive_tab_switch() {
        // Two tabs, two files; edit each, switch between, confirm the
        // ropes + cursors are preserved per tab.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("a"))),
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("b"))),
        );
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

    // ── Save via legacy chord (ctrl+x ctrl+s) ───────────────────────────

    #[test]
    fn ctrl_x_ctrl_s_queues_save_for_dirty_active_buffer() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        // Force dirty by bumping version past saved_version.
        let eb = edits.buffers.get_mut(&canon("file.rs")).expect("seeded");
        eb.version = 1;
        assert!(eb.dirty());

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert!(edits.pending_saves.contains(&canon("file.rs")));
    }

    #[test]
    fn ctrl_x_ctrl_s_on_clean_buffer_is_noop() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        // Buffer is fresh (version == saved_version == 0).
        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.is_empty());
    }

    #[test]
    fn ctrl_x_ctrl_s_on_unloaded_buffer_is_noop() {
        let mut tabs = tabs_with(&[("file.rs", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
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
        km.bind("ctrl+q", Command::Quit);
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();

        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('q')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &km,
            &mut chord,
        );
        assert_eq!(outcome, DispatchOutcome::Quit);

        // Ctrl-C not bound here → Continue (not Quit).
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &km,
            &mut chord,
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
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('z')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &km,
            &mut chord,
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
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &km,
            &mut chord,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
    }

    #[test]
    fn ctrl_x_ctrl_s_targets_only_active_tab() {
        // Two dirty buffers; Ctrl-X Ctrl-S on tab b should only enqueue b.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(2));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("A")),
                version: 1,
                saved_version: 0,
                history: Default::default(),
            },
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("B")),
                version: 1,
                saved_version: 0,
                history: Default::default(),
            },
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert!(edits.pending_saves.contains(&canon("b")));
        assert!(!edits.pending_saves.contains(&canon("a")));
    }

    // ── M6: chord dispatch + new commands ───────────────────────────────

    #[test]
    fn unknown_second_key_in_chord_cancels_silently() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        let keymap = default_keymap();
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        // ctrl+x → pending.
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert!(chord.pending.is_some());
        // Second key `z` isn't bound under ctrl+x → silent cancel.
        let outcome = dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('z')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &store,
            &term,
            &keymap,
            &mut chord,
        );
        assert_eq!(outcome, DispatchOutcome::Continue);
        assert!(chord.pending.is_none());
        // `z` was NOT inserted — the printable fallback only fires
        // at the root, not inside a prefix.
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hi");
    }

    #[test]
    fn kill_buffer_closes_clean_active_tab() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("A"))),
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("B"))),
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::NONE, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].id, TabId(2));
        assert!(!edits.buffers.contains_key(&canon("a")));
    }

    #[test]
    fn kill_buffer_on_dirty_is_noop_until_m9() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("A")),
                version: 1,
                saved_version: 0,
                history: Default::default(),
            },
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("B"))),
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::NONE, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        // Tab a still open — M6 doesn't have confirm-kill.
        assert_eq!(tabs.open.len(), 2);
    }

    #[test]
    fn save_all_enqueues_every_dirty_buffer() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("A")),
                version: 1,
                saved_version: 0,
                history: Default::default(),
            },
        );
        // b is clean.
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("B"))),
        );
        edits.buffers.insert(
            canon("c"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("C")),
                version: 2,
                saved_version: 0,
                history: Default::default(),
            },
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('a')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.contains(&canon("a")));
        assert!(edits.pending_saves.contains(&canon("c")));
        assert!(!edits.pending_saves.contains(&canon("b")));
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
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo bar  baz", Dims { cols: 40, rows: 5 });
        // Cursor starts at (0, 0). alt+f → end of "foo" (col 3).
        dispatch_default(
            key(KeyModifiers::ALT, KeyCode::Char('f')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.col, 3);
        // alt+f again → skip " ", skip "bar", land at col 7.
        dispatch_default(
            key(KeyModifiers::ALT, KeyCode::Char('f')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.col, 7);
        // alt+b → back to start of "bar" (col 4).
        dispatch_default(
            key(KeyModifiers::ALT, KeyCode::Char('b')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.col, 4);
        // alt+b → start of "foo" (col 0).
        dispatch_default(
            key(KeyModifiers::ALT, KeyCode::Char('b')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn abort_is_a_noop_at_the_plain_editor_level() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let outcome = noop_dispatch(key(KeyModifiers::NONE, KeyCode::Esc), &mut tabs);
        assert_eq!(outcome, DispatchOutcome::Continue);
    }

    // ── M7: mark / region / kill ring / yank ────────────────────────────

    fn dispatch_with_ring(
        k: KeyEvent,
        tabs: &mut Tabs,
        edits: &mut BufferEdits,
        kill_ring: &mut KillRing,
        store: &BufferStore,
        terminal: &Terminal,
    ) -> DispatchOutcome {
        let mut chord = ChordState::default();
        dispatch_key(
            k,
            tabs,
            edits,
            kill_ring,
            store,
            terminal,
            &default_keymap(),
            &mut chord,
        )
    }

    #[test]
    fn set_mark_captures_current_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc\ndef", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 1,
            col: 2,
            preferred_col: 2,
        };
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char(' ')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(
            tabs.open[0].mark,
            Some(Cursor {
                line: 1,
                col: 2,
                preferred_col: 2,
            })
        );
    }

    #[test]
    fn abort_clears_mark() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        });
        noop_dispatch(key(KeyModifiers::NONE, KeyCode::Esc), &mut tabs);
        assert!(tabs.open[0].mark.is_none());
    }

    #[test]
    fn kill_region_removes_marked_range_into_ring() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdefgh", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 2,
            preferred_col: 2,
        };
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 6,
            preferred_col: 6,
        });
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abgh");
        assert_eq!(kr.latest.as_deref(), Some("cdef"));
        assert_eq!(tabs.open[0].cursor.col, 2);
        assert!(tabs.open[0].mark.is_none());
    }

    #[test]
    fn kill_region_handles_mark_after_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdefgh", Dims { cols: 20, rows: 5 });
        // Cursor at 6, mark at 2 — reverse of the previous test.
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 6,
            preferred_col: 6,
        };
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 2,
            preferred_col: 2,
        });
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abgh");
        assert_eq!(kr.latest.as_deref(), Some("cdef"));
        // Cursor lands at region start (col 2), not where it started.
        assert_eq!(tabs.open[0].cursor.col, 2);
    }

    #[test]
    fn kill_line_kills_to_eol() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo bar\nbaz", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 4,
            preferred_col: 4,
        };
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foo \nbaz");
        assert_eq!(kr.latest.as_deref(), Some("bar"));
        assert!(kr.last_was_kill_line);
    }

    #[test]
    fn kill_line_at_eol_joins_with_next() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo\nbar", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foobar");
        assert_eq!(kr.latest.as_deref(), Some("\n"));
    }

    #[test]
    fn consecutive_kill_lines_coalesce() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("aaa\nbbb\nccc", Dims { cols: 20, rows: 5 });
        let mut kr = KillRing::default();
        // First kill: kill "aaa" on line 0.
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        // Second kill: kill the newline that now precedes "bbb".
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        // Coalesced: "aaa" + "\n".
        assert_eq!(kr.latest.as_deref(), Some("aaa\n"));
    }

    #[test]
    fn non_kill_command_breaks_coalescing() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("aaa\nbbb", Dims { cols: 20, rows: 5 });
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert!(kr.last_was_kill_line);
        // Any other command resets the flag.
        dispatch_with_ring(
            key(KeyModifiers::NONE, KeyCode::Right),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert!(!kr.last_was_kill_line);
    }

    #[test]
    fn yank_sets_pending_on_active_tab() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("x", Dims { cols: 20, rows: 5 });
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(kr.pending_yank, Some(TabId(1)));
    }

    #[test]
    fn apply_yank_inserts_text_at_cursor() {
        let (mut tabs, mut edits, _store, _term) =
            fixture_with_content("hello", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        apply_yank(&mut tabs, &mut edits, TabId(1), "XYZ");
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "helXYZlo");
        assert_eq!(tabs.open[0].cursor.col, 6);
    }

    // ── M8: undo / redo ─────────────────────────────────────────────────

    fn type_chars(
        chars: &str,
        tabs: &mut Tabs,
        edits: &mut BufferEdits,
        store: &BufferStore,
        term: &Terminal,
    ) {
        // Each char goes through the full keymap → implicit_insert
        // path so coalescing fires exactly as at runtime.
        for c in chars.chars() {
            dispatch_default(
                key(KeyModifiers::NONE, KeyCode::Char(c)),
                tabs,
                edits,
                store,
                term,
            );
        }
    }

    #[test]
    fn undo_removes_coalesced_word_inserts_in_one_shot() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });

        type_chars("hello", &mut tabs, &mut edits, &store, &term);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hello");

        // Ctrl-/ → one group, five chars gone.
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
    }

    #[test]
    fn undo_with_space_boundary_pops_only_last_word() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });

        type_chars("hello ", &mut tabs, &mut edits, &store, &term);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hello ");

        // Space broke coalescing → two groups: "hello" then " ".
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hello");
    }

    #[test]
    fn redo_applies_the_undone_group() {
        // Plain undo is bound; redo isn't — use a custom keymap.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });

        type_chars("hi", &mut tabs, &mut edits, &store, &term);
        let mut km = default_keymap();
        km.bind("ctrl+y", Command::Redo); // override Yank for test
        let mut chord = ChordState::default();
        let mut kr = KillRing::default();

        // Undo: ""
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
            &km,
            &mut chord,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");

        // Redo: "hi"
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
            &km,
            &mut chord,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hi");
    }

    #[test]
    fn undo_restores_killed_region() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdefgh", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 2,
            preferred_col: 2,
        };
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 6,
            preferred_col: 6,
        });
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abgh");

        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abcdefgh");
    }

    #[test]
    fn edit_after_undo_drops_future() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });
        type_chars("hi", &mut tabs, &mut edits, &store, &term);
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
        // Redo is bound in this test via a custom map; before that,
        // a new edit should drop the future branch.
        type_chars("x", &mut tabs, &mut edits, &store, &term);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "x");

        let mut km = default_keymap();
        km.bind("ctrl+y", Command::Redo);
        let mut chord = ChordState::default();
        let mut kr = KillRing::default();
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
            &km,
            &mut chord,
        );
        // Still "x" — nothing to redo because the new edit dropped
        // the future branch.
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "x");
    }

    #[test]
    fn undo_restores_cursor_before() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 20, rows: 5 });
        type_chars("hi", &mut tabs, &mut edits, &store, &term);
        // Cursor is at (0, 2). Move it elsewhere to verify that undo
        // restores to cursor_before.
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 0,
            preferred_col: 0,
        };
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
        // Undo restored cursor_before, which was (0, 0) for the
        // first char of the coalesced "hi" group.
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn kill_region_noop_when_no_mark() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc", Dims { cols: 20, rows: 5 });
        assert!(tabs.open[0].mark.is_none());
        let mut kr = KillRing::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc");
        assert!(kr.latest.is_none());
    }
}
