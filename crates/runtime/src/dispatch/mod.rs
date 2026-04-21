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
//! Each submodule owns its own `#[cfg(test)] mod tests`. Shared
//! fixtures (atom builders, dispatch wrappers) live in [`testutil`];
//! `mod tests` in this file covers only the dispatch-level concerns
//! (chord resolution, implicit-insert gating, quit chord, abort).

mod browser;
mod cursor;
mod edit;
mod file_search;
mod find_file;
mod isearch;
mod kill;
mod mark;
mod nav;
mod save;
mod shared;
mod tabs;
mod undo;

#[cfg(test)]
mod testutil;

// Public surface — kept tight so the runtime only reaches in for
// the five externally-relevant names.
pub use kill::apply_yank;
pub use shared::open_or_focus_tab;

// Aliases used by `run_command`.
use browser::{
    collapse_all, collapse_dir, expand_dir, move_selection, open_selected, open_selected_bg,
    page_selection, select_first, select_last, toggle_focus, toggle_side_panel,
};
use cursor::{Move, move_cursor};
use edit::{delete_back, delete_forward, insert_char, insert_newline};
use kill::{kill_line, kill_region, request_yank};
use mark::{clear_mark, set_mark_active};
use nav::{jump_back, jump_forward, match_bracket};
use save::{request_save_active, request_save_all};
use tabs::{cycle_active, force_kill, kill_active};
use undo::{redo_active, undo_active};

use led_driver_buffers_core::BufferStore;
use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers, Terminal};
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, Focus, FsTree};
use led_state_buffer_edits::BufferEdits;
use led_state_clipboard::ClipboardState;
use led_state_file_search::FileSearchState;
use led_state_find_file::FindFileState;
use led_state_isearch::IsearchState;
use led_state_jumps::JumpListState;
use led_state_kill_ring::KillRing;
use led_state_tabs::Tabs;

use crate::Event;
use crate::keymap::{ChordState, Command, Keymap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    Continue,
    Quit,
}

/// Bundle of mutable + shared references the dispatch loop needs.
///
/// The main loop constructs one of these per event and calls
/// [`Dispatcher::dispatch`]. Fields are public so test code can
/// build one with ad-hoc borrows too.
///
/// The struct holds references, not owned state — it's cheap to
/// build one per tick (or even per call) from the runtime's stack
/// bindings. No lifetime extension or pinning required.
pub struct Dispatcher<'a> {
    pub tabs: &'a mut Tabs,
    pub edits: &'a mut BufferEdits,
    pub kill_ring: &'a mut KillRing,
    pub clip: &'a mut ClipboardState,
    pub alerts: &'a mut AlertState,
    pub jumps: &'a mut JumpListState,
    pub browser: &'a mut BrowserUi,
    pub fs: &'a FsTree,
    pub store: &'a BufferStore,
    pub terminal: &'a Terminal,
    pub find_file: &'a mut Option<FindFileState>,
    pub isearch: &'a mut Option<IsearchState>,
    pub file_search: &'a mut Option<FileSearchState>,
    pub keymap: &'a Keymap,
    pub chord: &'a mut ChordState,
}

impl<'a> Dispatcher<'a> {
    /// Top-level entry point: dispatch one [`Event`] through to
    /// either a command execution or a silent state-change.
    pub fn dispatch(&mut self, ev: Event) -> DispatchOutcome {
        match ev {
            Event::Key(k) => self.dispatch_key(k),
            // `Resize` is applied inside `TerminalInputDriver.process` —
            // pure state, no dispatch work here. M2 does not re-clamp
            // cursor/scroll on resize; next movement re-clamps.
            Event::Resize(_) => DispatchOutcome::Continue,
            Event::Quit => DispatchOutcome::Quit,
        }
    }

    /// Resolve + run a single keystroke. Delegates to the free
    /// [`dispatch_key`] so the submodule functions and tests that
    /// already take individual args keep working unchanged.
    pub fn dispatch_key(&mut self, k: KeyEvent) -> DispatchOutcome {
        dispatch_key(
            k,
            self.tabs,
            self.edits,
            self.kill_ring,
            self.clip,
            self.alerts,
            self.jumps,
            self.browser,
            self.fs,
            self.store,
            self.terminal,
            self.find_file,
            self.isearch,
            self.file_search,
            self.keymap,
            self.chord,
        )
    }
}

/// Keymap-first dispatch with chord support. Algorithm:
///
/// 0. If a confirm-kill prompt is live, intercept this keystroke:
///    `y`/`Y` (no modifiers) confirms and force-kills the targeted
///    tab; any other key clears the prompt and falls through to the
///    normal resolution so e.g. `Esc` still clears the mark or an
///    arrow key still moves.
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
    clip: &mut ClipboardState,
    alerts: &mut AlertState,
    jumps: &mut JumpListState,
    browser: &mut BrowserUi,
    fs: &FsTree,
    store: &BufferStore,
    terminal: &Terminal,
    find_file: &mut Option<FindFileState>,
    isearch: &mut Option<IsearchState>,
    file_search: &mut Option<FileSearchState>,
    keymap: &Keymap,
    chord: &mut ChordState,
) -> DispatchOutcome {
    // Step 0 — confirm-kill gate.
    if let Some(target) = alerts.confirm_kill {
        alerts.confirm_kill = None;
        if k.modifiers.is_empty() && matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            force_kill(tabs, edits, target);
            return DispatchOutcome::Continue;
        }
        // Any other key: prompt dismissed; fall through so the key
        // runs its normal binding / implicit-insert behaviour.
    }

    let resolved = resolve_command(
        k,
        keymap,
        chord,
        browser.focus == Focus::Side,
        find_file.is_some(),
        file_search.is_some(),
    );
    match resolved {
        Resolved::Command(cmd) => {
            let outcome = run_command(
                cmd, tabs, edits, kill_ring, clip, alerts, jumps, browser, fs, store, terminal,
                find_file, isearch, file_search,
            );
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

fn resolve_command(
    k: KeyEvent,
    keymap: &Keymap,
    chord: &mut ChordState,
    browser_focused: bool,
    find_file_active: bool,
    file_search_active: bool,
) -> Resolved {
    if let Some(prefix) = chord.pending.take() {
        if let Some(cmd) = keymap.lookup_chord(&prefix, &k) {
            return Resolved::Command(cmd);
        }
        // Silent cancel — matches legacy behaviour.
        return Resolved::Continue;
    }
    // Find-file context overlay wins over the global direct table
    // and over browser context (M12). Browser and find-file are
    // mutually exclusive — the overlay runs with main-pane focus.
    if find_file_active
        && let Some(cmd) = keymap.lookup_find_file(&k)
    {
        return Resolved::Command(cmd);
    }
    // File-search overlay context (M14). Takes precedence over the
    // global direct table so e.g. `tab` maps to the overlay's
    // field-cycling command instead of falling through to nothing.
    if file_search_active
        && let Some(cmd) = keymap.lookup_file_search(&k)
    {
        return Resolved::Command(cmd);
    }
    // Browser-context overlay wins over the global direct table when
    // focus is on the sidebar (M11). The file-search overlay also
    // lives in the sidebar but wants global keys (`enter` →
    // `InsertNewline`, etc.) to reach its own `run_overlay_command`,
    // so browser_direct is suppressed while file_search is active.
    if browser_focused
        && !file_search_active
        && let Some(cmd) = keymap.lookup_browser(&k)
    {
        return Resolved::Command(cmd);
    }
    if let Some(cmd) = keymap.lookup_direct(&k) {
        return Resolved::Command(cmd);
    }
    if keymap.is_prefix(&k) {
        chord.pending = Some(k);
        return Resolved::PrefixStored;
    }
    // Implicit insert. Fires when the editor is focused, and also
    // when the file-search overlay is active (its "focus = Side"
    // suppresses normal sidebar-focused implicit-insert, but the
    // overlay still wants typed chars as query input). Browser
    // focus without the overlay → suppress.
    if (!browser_focused || file_search_active)
        && let Some(cmd) = implicit_insert(&k)
    {
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

#[allow(clippy::too_many_arguments)]
fn run_command(
    cmd: Command,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    kill_ring: &mut KillRing,
    clip: &mut ClipboardState,
    alerts: &mut AlertState,
    jumps: &mut JumpListState,
    browser: &mut BrowserUi,
    fs: &FsTree,
    store: &BufferStore,
    terminal: &Terminal,
    find_file: &mut Option<FindFileState>,
    isearch: &mut Option<IsearchState>,
    file_search: &mut Option<FileSearchState>,
) -> DispatchOutcome {
    // Find-file overlay intercept. When active, the overlay owns
    // input editing + its own command set; most commands route into
    // `state.input` instead of the buffer. `Quit` passes through
    // so `ctrl+x ctrl+c` still exits.
    if let Some(outcome) = find_file::run_overlay_command(cmd, find_file, tabs, edits) {
        return outcome;
    }

    // In-buffer isearch overlay intercept. Typing / backspace /
    // Enter / Esc / another Ctrl-s are fully consumed; every other
    // command triggers "accept on passthrough" — the current match
    // becomes the cursor's home, then the command runs normally.
    if let Some(outcome) = isearch::run_overlay_command(cmd, isearch, tabs, edits, jumps) {
        return outcome;
    }

    // File-search overlay intercept (M14). Typing / toggles /
    // Abort are fully consumed; other commands fall through.
    if let Some(outcome) = file_search::run_overlay_command(
        cmd,
        file_search,
        browser,
        tabs,
        edits,
        terminal,
        fs.root.as_ref(),
    ) {
        return outcome;
    }

    let browser_focused = browser.focus == Focus::Side;
    match cmd {
        Command::Quit => DispatchOutcome::Quit,
        Command::Abort => {
            // Isearch takes priority: Abort closes the overlay
            // without clearing the mark. Find-file Abort is already
            // consumed upstream in `find_file::run_overlay_command`.
            // M17 / M18 will short-circuit their own modals before
            // reaching here.
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
            cycle_active(tabs, jumps, 1);
            DispatchOutcome::Continue
        }
        Command::TabPrev => {
            cycle_active(tabs, jumps, -1);
            DispatchOutcome::Continue
        }
        Command::KillBuffer => {
            kill_active(tabs, edits, alerts);
            DispatchOutcome::Continue
        }
        Command::CursorUp => {
            if browser_focused {
                move_selection(browser, -1);
            } else {
                move_cursor(tabs, edits, store, terminal, Move::Up);
            }
            DispatchOutcome::Continue
        }
        Command::CursorDown => {
            if browser_focused {
                move_selection(browser, 1);
            } else {
                move_cursor(tabs, edits, store, terminal, Move::Down);
            }
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
            let page = terminal
                .dims
                .map(|d| d.rows.saturating_sub(2) as usize)
                .unwrap_or(1);
            if browser_focused {
                page_selection(browser, page, /* down= */ false);
            } else {
                move_cursor(tabs, edits, store, terminal, Move::PageUp);
            }
            DispatchOutcome::Continue
        }
        Command::CursorPageDown => {
            let page = terminal
                .dims
                .map(|d| d.rows.saturating_sub(2) as usize)
                .unwrap_or(1);
            if browser_focused {
                page_selection(browser, page, /* down= */ true);
            } else {
                move_cursor(tabs, edits, store, terminal, Move::PageDown);
            }
            DispatchOutcome::Continue
        }
        Command::CursorFileStart => {
            if browser_focused {
                select_first(browser);
            } else {
                move_cursor(tabs, edits, store, terminal, Move::FileStart);
            }
            DispatchOutcome::Continue
        }
        Command::CursorFileEnd => {
            if browser_focused {
                select_last(browser);
            } else {
                move_cursor(tabs, edits, store, terminal, Move::FileEnd);
            }
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
            kill_region(tabs, edits, kill_ring, clip);
            DispatchOutcome::Continue
        }
        Command::KillLine => {
            kill_line(tabs, edits, kill_ring, clip);
            DispatchOutcome::Continue
        }
        Command::Yank => {
            request_yank(tabs, clip);
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
        Command::JumpBack => {
            jump_back(tabs, edits, jumps);
            DispatchOutcome::Continue
        }
        Command::JumpForward => {
            jump_forward(tabs, edits, jumps);
            DispatchOutcome::Continue
        }
        Command::MatchBracket => {
            match_bracket(tabs, edits, jumps);
            DispatchOutcome::Continue
        }
        Command::ExpandDir => {
            expand_dir(browser, fs);
            DispatchOutcome::Continue
        }
        Command::CollapseDir => {
            collapse_dir(browser, fs);
            DispatchOutcome::Continue
        }
        Command::CollapseAll => {
            collapse_all(browser, fs);
            DispatchOutcome::Continue
        }
        Command::OpenSelected => {
            open_selected(browser, fs, tabs);
            DispatchOutcome::Continue
        }
        Command::OpenSelectedBg => {
            open_selected_bg(browser, tabs);
            DispatchOutcome::Continue
        }
        Command::ToggleSidePanel => {
            toggle_side_panel(browser);
            DispatchOutcome::Continue
        }
        Command::ToggleFocus => {
            toggle_focus(browser);
            DispatchOutcome::Continue
        }
        Command::FindFile => {
            find_file::activate_open(find_file, tabs, fs);
            DispatchOutcome::Continue
        }
        Command::SaveAs => {
            find_file::activate_save_as(find_file, tabs, fs);
            DispatchOutcome::Continue
        }
        Command::FindFileTabComplete => {
            // Stage 1: tab-complete is a no-op until M12 phase 4 lands.
            // The keymap reserves the binding so `Tab` in the overlay
            // doesn't fall through to `insert_tab` (M23).
            DispatchOutcome::Continue
        }
        Command::InBufferSearch => {
            isearch::in_buffer_search(isearch, tabs, edits);
            DispatchOutcome::Continue
        }
        Command::OpenFileSearch => {
            file_search::activate(file_search, browser, tabs);
            DispatchOutcome::Continue
        }
        Command::CloseFileSearch => {
            file_search::deactivate(file_search, browser, tabs);
            DispatchOutcome::Continue
        }
        // Toggles + ReplaceAll are only meaningful inside the
        // overlay — `file_search::run_overlay_command` consumes
        // them when active. If we get here, the overlay isn't
        // open, and these are no-ops.
        Command::ToggleSearchCase
        | Command::ToggleSearchRegex
        | Command::ToggleSearchReplace
        | Command::ReplaceAll => DispatchOutcome::Continue,
    }
}


#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers, Terminal};
    use led_state_alerts::AlertState;
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    use led_state_kill_ring::KillRing;
    use ropey::Rope;

    use super::*;
    use super::testutil::*;
    use crate::keymap::{default_keymap, Command};

    #[test]
    fn ctrl_x_ctrl_c_signals_quit_as_chord() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let store = BufferStore::default();
        let term = Terminal::default();
        let keymap = default_keymap();
        let mut chord = ChordState::default();

        // First half of the chord: ctrl+x → pending, Continue.
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &keymap,
            &mut chord,);
        assert_eq!(outcome, DispatchOutcome::Continue);
        assert!(chord.pending.is_some());

        // Second half: ctrl+c → chord fires Quit.
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &keymap,
            &mut chord,);
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

    // ── M6: chord dispatch + new commands ───────────────────────────────

    #[test]
    fn unknown_second_key_in_chord_cancels_silently() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        let keymap = default_keymap();
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        // ctrl+x → pending.
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &keymap,
            &mut chord,);
        assert!(chord.pending.is_some());
        // Second key `z` isn't bound under ctrl+x → silent cancel.
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let outcome = dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('z')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &keymap,
            &mut chord,);
        assert_eq!(outcome, DispatchOutcome::Continue);
        assert!(chord.pending.is_none());
        // `z` was NOT inserted — the printable fallback only fires
        // at the root, not inside a prefix.
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hi");
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
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();

        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('q')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &km,
            &mut chord,);
        assert_eq!(outcome, DispatchOutcome::Quit);

        // Ctrl-C not bound here → Continue (not Quit).
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &km,
            &mut chord,);
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
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        dispatch_key(
            key(KeyModifiers::NONE, KeyCode::Char('z')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &km,
            &mut chord,);
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
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            &mut tabs,
            &mut edits,
            &mut kill_ring,
            &mut clip,
            &mut alerts,
            &mut jumps,
            &mut browser,
            &fs,
            &store,
            &term,
        &mut find_file,
            &mut isearch,
            &mut file_search,
            &km,
            &mut chord,);
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

    #[test]
    fn abort_is_a_noop_at_the_plain_editor_level() {
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let outcome = noop_dispatch(key(KeyModifiers::NONE, KeyCode::Esc), &mut tabs);
        assert_eq!(outcome, DispatchOutcome::Continue);
    }
}
