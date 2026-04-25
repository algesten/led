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
mod code_actions;
#[path = "completions.rs"]
mod completions_overlay;
mod cursor;
mod edit;
mod file_search;
mod find_file;
mod isearch;
mod kill;
mod mark;
mod nav;
mod rename;
mod save;
mod shared;
mod tabs;
mod undo;

#[cfg(test)]
mod testutil;

// Public surface — kept tight so the runtime only reaches in for
// the five externally-relevant names.
pub use code_actions::install_picker as install_code_action_picker;
pub(crate) use cursor::center_on_cursor;
pub use kill::apply_yank;
pub use shared::editor_content_cols;
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
use nav::{jump_back, jump_forward, match_bracket, next_issue_active, prev_issue_active};
use save::{request_save_active, request_save_all};
use tabs::{cycle_active, force_kill, kill_active};
use undo::{redo_active, undo_active};

use led_driver_buffers_core::BufferStore;
use led_driver_terminal_core::{KeyCode, KeyEvent, KeyModifiers, Terminal};
use led_state_alerts::AlertState;
use led_state_browser::{BrowserUi, Focus, FsTree};
use led_state_buffer_edits::BufferEdits;
use led_state_clipboard::ClipboardState;
use led_state_completions::CompletionsState;
use led_state_file_search::FileSearchState;
use led_state_find_file::FindFileState;
use led_state_isearch::IsearchState;
use led_state_jumps::JumpListState;
use led_state_kill_ring::KillRing;
use led_state_lsp::LspExtrasState;
use led_state_tabs::Tabs;

use crate::Event;
use crate::keymap::{ChordState, Command, Keymap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    Continue,
    /// User hit the quit chord (`Ctrl-X Ctrl-C` by default). The
    /// main loop sets `lifecycle.phase = Exiting` and breaks. M21
    /// gates the break on `session.saved`.
    Quit,
    /// User hit the suspend binding (`Ctrl-Z` by default). The
    /// main loop invokes `suspend_and_resume`, bumps
    /// `force_redraw`, and continues from `Running` again on
    /// SIGCONT return.
    Suspend,
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
    pub completions: &'a mut CompletionsState,
    pub lsp_extras: &'a mut LspExtrasState,
    /// LSP diagnostics, read-only here — issue navigation
    /// (Alt-./Alt-,) reads them to build the nav cycle.
    pub diagnostics: &'a led_state_diagnostics::DiagnosticsStates,
    /// Git state (branch + file/line statuses). Same consumer
    /// as `diagnostics` — tiered issue nav walks both.
    pub git: &'a led_state_git::GitState,
    /// Symlink-resolution chains keyed by canonical path. Dispatch
    /// populates this whenever a tab opens from a user-typed path
    /// (find-file commit, browser entry). Load-completion
    /// language detection consults it so symlinked dotfiles
    /// (`~/.profile` → `dotfiles/profile`) still detect via the
    /// user-typed basename.
    pub path_chains: &'a mut std::collections::HashMap<
        led_core::CanonPath,
        led_core::PathChain,
    >,
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
            self.path_chains,
            self.completions,
            self.lsp_extras,
            self.diagnostics,
            self.git,
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
    path_chains: &mut std::collections::HashMap<led_core::CanonPath, led_core::PathChain>,
    completions: &mut CompletionsState,
    lsp_extras: &mut LspExtrasState,
    diagnostics: &led_state_diagnostics::DiagnosticsStates,
    git: &led_state_git::GitState,
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
                find_file, isearch, file_search, path_chains, completions, lsp_extras,
                diagnostics, git,
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
            // Edit-like commands leave `preferred_col` as the raw
            // logical col — refresh to the within-sub-line col so
            // subsequent vertical moves land on the right visual
            // column. Pure cursor moves already set it correctly
            // (horizontal moves) or deliberately preserve it across
            // clamping (vertical moves), so we skip them.
            if is_edit_like(&cmd) {
                refresh_active_preferred_col(tabs, edits, terminal, browser);
            }
            // Auto-trigger LSP completion after an identifier-ish
            // InsertChar. Matches legacy led editing_of.rs:69-75 —
            // alphanumeric or `_` fires a fresh request, other
            // commands either dismiss the live popup or pass
            // through. When a session is already active, typing
            // just queues another request (server seq-gating
            // drops the older one); stage 6 will add client-side
            // refilter so the popup updates without a round-trip
            // for every keystroke.
            handle_completion_trigger(&cmd, tabs, edits, completions);
            outcome
        }
        Resolved::PrefixStored | Resolved::Continue => DispatchOutcome::Continue,
    }
}

/// Queue a `RequestCompletion` for the active tab when the user
/// just typed an identifier char, OR dismiss the live session
/// on a non-trigger key. When a session is already active, also
/// refilters the visible items against the new typed prefix —
/// matches legacy `refilter_completion` at
/// `/Users/martin/dev/led/crates/lsp/src/manager.rs:1735-1830`.
///
/// Called from the dispatch boundary so every command path
/// (direct, chord, implicit-insert) flows through the same logic.
fn handle_completion_trigger(
    cmd: &Command,
    tabs: &Tabs,
    edits: &BufferEdits,
    completions: &mut CompletionsState,
) {
    match cmd {
        Command::InsertChar(c) if c.is_alphanumeric() || *c == '_' => {
            // Auto-trigger on identifier chars. Needs an active
            // tab with a loaded buffer; otherwise silently drop.
            let Some(id) = tabs.active else {
                return;
            };
            let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
                return;
            };
            if tab.preview {
                return;
            }
            if edits.buffers.get(&tab.path).is_none() {
                return;
            }
            let line = tab.cursor.line as u32;
            let col = tab.cursor.col as u32;
            // Client-side refilter first — updates the popup
            // instantly against the newly-typed prefix without
            // waiting for a server round-trip. Also queues a
            // fresh server request so delayed items can still
            // arrive (server response replaces the session when
            // its seq matches).
            refilter_active_session(tabs, edits, completions);
            completions.queue_request(tab.path.clone(), line, col, Some(*c));
        }
        Command::DeleteBack | Command::DeleteForward => {
            // Keep the session alive across backspace / delete
            // within the prefix range. Refilter; if the cursor
            // moved left of prefix_start_col (user deleted past
            // the start of the identifier), dismiss.
            refilter_active_session(tabs, edits, completions);
        }
        // Any other command dismisses an active popup. The
        // dispatch-action set (MoveUp, MoveDown, InsertNewline,
        // etc.) is handled in stage 5 before the command even
        // runs; reaching here means the command didn't get
        // intercepted, so the popup is stale.
        _ => {
            if completions.session.is_some() {
                completions.dismiss();
            }
        }
    }
}

/// Rebuild `session.filtered` from the current typed prefix.
/// Dismisses the session when the cursor has moved left of the
/// prefix anchor or when no items match — both mean the popup
/// has lost its context.
fn refresh_completion_filter(
    tabs: &Tabs,
    edits: &BufferEdits,
    completions: &mut CompletionsState,
) {
    let Some(session) = completions.session.as_ref() else {
        return;
    };
    // Resolve the active tab + its buffer. If either is gone or
    // the tab switched, dismiss.
    let Some(tab) = tabs.open.iter().find(|t| t.id == session.tab) else {
        completions.dismiss();
        return;
    };
    let Some(eb) = edits.buffers.get(&tab.path) else {
        completions.dismiss();
        return;
    };
    if tab.cursor.line as u32 != session.prefix_line {
        completions.dismiss();
        return;
    }
    if (tab.cursor.col as u32) < session.prefix_start_col {
        completions.dismiss();
        return;
    }
    // Extract the typed prefix from the rope.
    let line_idx = session.prefix_line as usize;
    if line_idx >= eb.rope.len_lines() {
        completions.dismiss();
        return;
    }
    let line_start = eb.rope.line_to_char(line_idx);
    let from = line_start + session.prefix_start_col as usize;
    let to = line_start + tab.cursor.col;
    if to < from || to > eb.rope.len_chars() {
        completions.dismiss();
        return;
    }
    let prefix: String = eb.rope.slice(from..to).to_string();
    let filtered = led_state_completions::refilter(&session.items, &prefix);
    if filtered.is_empty() {
        completions.dismiss();
        return;
    }
    // Dismiss when the sole remaining candidate equals what the
    // user has already typed — committing would be a no-op, so
    // the popup is pure noise. Matches the same check on the
    // ingest path in runtime/lib.rs.
    if filtered.len() == 1
        && led_state_completions::is_identity_match(
            &session.items[filtered[0]],
            &prefix,
        )
    {
        completions.dismiss();
        return;
    }
    // Preserve the highlighted label across the refilter when
    // possible — matches the UX users expect (the item they were
    // aiming at shouldn't jump around as the list shrinks).
    let prev_selected_item = session
        .filtered
        .get(session.selected)
        .copied();
    let new_selected = prev_selected_item
        .and_then(|item_ix| filtered.iter().position(|&i| i == item_ix))
        .unwrap_or(0);
    if let Some(session) = completions.session.as_mut() {
        session.filtered = std::sync::Arc::new(filtered);
        session.selected = new_selected;
        // Scroll reset — ensure_visible semantics (in the
        // overlay module) would re-clamp anyway, but starting
        // at 0 keeps the popup predictable after a refilter.
        if new_selected < session.scroll {
            session.scroll = new_selected;
        }
    }
}

fn refilter_active_session(
    tabs: &Tabs,
    edits: &BufferEdits,
    completions: &mut CompletionsState,
) {
    if completions.session.is_some() {
        refresh_completion_filter(tabs, edits, completions);
    }
}

fn is_coalescable_insert(cmd: &Command) -> bool {
    // Every printable insert is coalescable — the actual word-
    // boundary decision (close on whitespace-after-non-whitespace)
    // lives in `History::record_insert_char` so a single group can
    // span " appended" the way legacy does.
    matches!(cmd, Command::InsertChar(_))
}

/// Commands whose primitive mutates the cursor via an edit (not a
/// pure move). These leave `preferred_col` as the raw logical col;
/// the dispatch boundary refreshes it to the within-sub-line col
/// so vertical moves after the edit land on the right visual column.
fn is_edit_like(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::InsertChar(_)
            | Command::InsertNewline
            | Command::DeleteBack
            | Command::DeleteForward
            | Command::KillRegion
            | Command::KillLine
            | Command::Undo
            | Command::Redo
            | Command::JumpBack
            | Command::JumpForward
    )
}

/// Refresh the active tab's `preferred_col` after an edit-like
/// command. Uses the painter's content-col geometry so the
/// within-sub-line col agrees with what `body_model` will render.
fn refresh_active_preferred_col(
    tabs: &mut Tabs,
    edits: &BufferEdits,
    terminal: &Terminal,
    browser: &BrowserUi,
) {
    use shared::{editor_content_cols, refresh_preferred_col};
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let tab = &mut tabs.open[idx];
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    let content_cols = editor_content_cols(terminal, browser);
    refresh_preferred_col(&mut tab.cursor, &eb.rope, content_cols);
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
    path_chains: &mut std::collections::HashMap<led_core::CanonPath, led_core::PathChain>,
    completions: &mut CompletionsState,
    lsp_extras: &mut LspExtrasState,
    diagnostics: &led_state_diagnostics::DiagnosticsStates,
    git: &led_state_git::GitState,
) -> DispatchOutcome {
    // Find-file overlay intercept. When active, the overlay owns
    // input editing + its own command set; most commands route into
    // `state.input` instead of the buffer. `Quit` passes through
    // so `ctrl+x ctrl+c` still exits.
    if let Some(outcome) =
        find_file::run_overlay_command(cmd, find_file, tabs, edits, path_chains)
    {
        return outcome;
    }

    // LSP completion popup intercept (M17). Fires before buffer
    // editing so Up / Down navigate the list, Enter commits, Esc
    // dismisses. InsertChar / DeleteBack fall through to the
    // normal edit path; `handle_completion_trigger` at the
    // dispatch boundary then refilters / queues the next request.
    //
    // (The submodule is aliased as `completions_overlay` to
    // avoid shadowing the `completions` parameter.)
    if let Some(outcome) = completions_overlay::run_overlay_command(
        cmd, completions, tabs, edits,
    ) {
        return outcome;
    }

    // LSP rename overlay intercept (M18). Modal: every key
    // lands in the input until Enter (commit) or Esc (abort).
    // Quit passes through so the user can still ctrl+x ctrl+c
    // out of the editor mid-rename.
    if let Some(outcome) = rename::run_overlay_command(cmd, lsp_extras) {
        return outcome;
    }

    // LSP code-action picker intercept (M18). Modal: Up/Down
    // navigate, Enter commits, Esc dismisses.
    if let Some(outcome) = code_actions::run_overlay_command(cmd, lsp_extras) {
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
        Command::Suspend => DispatchOutcome::Suspend,
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
            save_with_optional_format(tabs, edits, lsp_extras, alerts);
            DispatchOutcome::Continue
        }
        Command::SaveAll => {
            request_save_all(tabs, edits);
            DispatchOutcome::Continue
        }
        Command::SaveNoFormat => {
            // Skip format; save directly.
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
                move_selection(browser, fs, tabs, path_chains, -1);
            } else {
                move_cursor(tabs, edits, store, terminal, browser, Move::Up);
            }
            DispatchOutcome::Continue
        }
        Command::CursorDown => {
            if browser_focused {
                move_selection(browser, fs, tabs, path_chains, 1);
            } else {
                move_cursor(tabs, edits, store, terminal, browser, Move::Down);
            }
            DispatchOutcome::Continue
        }
        Command::CursorLeft => {
            move_cursor(tabs, edits, store, terminal, browser, Move::Left);
            DispatchOutcome::Continue
        }
        Command::CursorRight => {
            move_cursor(tabs, edits, store, terminal, browser, Move::Right);
            DispatchOutcome::Continue
        }
        Command::CursorLineStart => {
            move_cursor(tabs, edits, store, terminal, browser, Move::LineStart);
            DispatchOutcome::Continue
        }
        Command::CursorLineEnd => {
            move_cursor(tabs, edits, store, terminal, browser, Move::LineEnd);
            DispatchOutcome::Continue
        }
        Command::CursorPageUp => {
            let page = terminal
                .dims
                .map(|d| d.rows.saturating_sub(2) as usize)
                .unwrap_or(1);
            if browser_focused {
                page_selection(browser, fs, tabs, path_chains, page, /* down= */ false);
            } else {
                move_cursor(tabs, edits, store, terminal, browser, Move::PageUp);
            }
            DispatchOutcome::Continue
        }
        Command::CursorPageDown => {
            let page = terminal
                .dims
                .map(|d| d.rows.saturating_sub(2) as usize)
                .unwrap_or(1);
            if browser_focused {
                page_selection(browser, fs, tabs, path_chains, page, /* down= */ true);
            } else {
                move_cursor(tabs, edits, store, terminal, browser, Move::PageDown);
            }
            DispatchOutcome::Continue
        }
        Command::CursorFileStart => {
            if browser_focused {
                select_first(browser, fs, tabs, path_chains);
            } else {
                move_cursor(tabs, edits, store, terminal, browser, Move::FileStart);
            }
            DispatchOutcome::Continue
        }
        Command::CursorFileEnd => {
            if browser_focused {
                select_last(browser, fs, tabs, path_chains);
            } else {
                move_cursor(tabs, edits, store, terminal, browser, Move::FileEnd);
            }
            DispatchOutcome::Continue
        }
        Command::CursorWordLeft => {
            move_cursor(tabs, edits, store, terminal, browser, Move::WordLeft);
            DispatchOutcome::Continue
        }
        Command::CursorWordRight => {
            move_cursor(tabs, edits, store, terminal, browser, Move::WordRight);
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
            alerts.set_info(
                "Mark set".to_string(),
                std::time::Instant::now(),
                std::time::Duration::from_secs(2),
            );
            DispatchOutcome::Continue
        }
        Command::KillRegion => {
            let killed = kill_region(tabs, edits, kill_ring, clip);
            if !killed {
                alerts.set_info(
                    "No region".to_string(),
                    std::time::Instant::now(),
                    std::time::Duration::from_secs(2),
                );
            }
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
        Command::NextIssue => {
            next_issue_active(
                tabs,
                edits,
                diagnostics,
                git,
                jumps,
                alerts,
                terminal,
                browser,
            );
            DispatchOutcome::Continue
        }
        Command::PrevIssue => {
            prev_issue_active(
                tabs,
                edits,
                diagnostics,
                git,
                jumps,
                alerts,
                terminal,
                browser,
            );
            DispatchOutcome::Continue
        }
        Command::ExpandDir => {
            expand_dir(browser, fs, tabs);
            DispatchOutcome::Continue
        }
        Command::CollapseDir => {
            collapse_dir(browser, fs, tabs);
            DispatchOutcome::Continue
        }
        Command::CollapseAll => {
            collapse_all(browser);
            DispatchOutcome::Continue
        }
        Command::OpenSelected => {
            open_selected(browser, fs, tabs, path_chains);
            DispatchOutcome::Continue
        }
        Command::OpenSelectedBg => {
            open_selected_bg(browser, fs, tabs, path_chains);
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
            file_search::activate(file_search, browser, tabs, edits);
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
        // LSP extras (M18). Goto-definition queues a
        // `RequestGotoDefinition`; the rest land in later stages.
        Command::LspGotoDefinition => {
            lsp_goto_definition(tabs, edits, lsp_extras);
            DispatchOutcome::Continue
        }
        Command::LspRename => {
            rename::activate(lsp_extras, tabs, edits);
            DispatchOutcome::Continue
        }
        Command::LspCodeAction => {
            code_actions::activate(lsp_extras, tabs, edits);
            DispatchOutcome::Continue
        }
        Command::LspToggleInlayHints => {
            let on = lsp_extras.toggle_inlay_hints();
            let msg = if on {
                "Inlay hints: on"
            } else {
                "Inlay hints: off"
            };
            alerts.set_info(
                msg.to_string(),
                std::time::Instant::now(),
                std::time::Duration::from_secs(2),
            );
            DispatchOutcome::Continue
        }
        Command::LspFormat => {
            request_format_active(tabs, edits, lsp_extras);
            DispatchOutcome::Continue
        }
        Command::Outline => {
            // Legacy orphan: `alt+o` was bound with no handler.
            // Rewrite reserves the key so it doesn't fall
            // through to InsertChar; the full symbol-outline
            // UI (backed by `textDocument/documentSymbol`) is
            // post-M18 polish.
            alerts.set_info(
                "Outline: not yet implemented".to_string(),
                std::time::Instant::now(),
                std::time::Duration::from_secs(2),
            );
            DispatchOutcome::Continue
        }
    }
}

/// `Save` + format-on-save: fire a format request and mark the
/// path `pending_save_after_format`. The ingest side (applying
/// `LspEvent::Edits { origin: Format }`) applies the edits and
/// unconditionally slots the path into `edits.pending_saves` so
/// the save driver picks it up next tick. No LSP attached →
/// format returns empty edits → save still fires.
///
/// A clean buffer is **not** a reason to bail here: format can
/// mutate a buffer that was clean on disk (e.g. trailing
/// whitespace the user didn't type but the server wants gone),
/// and "save" should always write. Legacy Emacs behaviour
/// ("(No changes need to be saved)") is intentionally diverged
/// from for this reason.
fn save_with_optional_format(
    tabs: &Tabs,
    edits: &mut BufferEdits,
    lsp_extras: &mut LspExtrasState,
    alerts: &mut AlertState,
) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return;
    };
    if edits.buffers.get(&tab.path).is_none() {
        return;
    }
    lsp_extras.queue_format(tab.path.clone());
    lsp_extras.pending_save_after_format.insert(tab.path.clone());
    alerts.set_info(
        "Formatting...".to_string(),
        std::time::Instant::now(),
        std::time::Duration::from_secs(2),
    );
}

fn request_format_active(
    tabs: &Tabs,
    edits: &BufferEdits,
    lsp_extras: &mut LspExtrasState,
) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return;
    };
    if edits.buffers.get(&tab.path).is_none() {
        return;
    }
    lsp_extras.queue_format(tab.path.clone());
}

/// Queue a `textDocument/definition` request for the identifier
/// under the active tab's cursor. Silent no-op when no active
/// tab, no loaded buffer, or the tab is a preview viewer (legacy
/// parity — definition from a preview would cross the
/// modal-tab boundary in a confusing way).
fn lsp_goto_definition(
    tabs: &Tabs,
    edits: &BufferEdits,
    lsp_extras: &mut LspExtrasState,
) {
    let Some(id) = tabs.active else { return };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return;
    };
    if tab.preview {
        return;
    }
    if edits.buffers.get(&tab.path).is_none() {
        return;
    }
    let line = tab.cursor.line as u32;
    let col = tab.cursor.col as u32;
    lsp_extras.queue_goto_definition(tab.path.clone(), line, col);
}


#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers, Terminal};
    use led_state_alerts::AlertState;
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    use led_state_completions::CompletionsState;
    use led_state_diagnostics::DiagnosticsStates;
    use led_state_git::GitState;
    use led_state_kill_ring::KillRing;
    use led_state_lsp::LspExtrasState;
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
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();

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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
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
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
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

        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
            &km,
            &mut chord,);
        assert_eq!(outcome, DispatchOutcome::Continue);
    }

    #[test]
    fn suspend_command_returns_dispatch_outcome_suspend() {
        // M20: Ctrl-Z bound to Command::Suspend in the default
        // keymap routes through to DispatchOutcome::Suspend so
        // the main loop can invoke `suspend_and_resume`.
        let mut tabs = tabs_with(&[("a", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        let km = crate::keymap::default_keymap();
        let mut chord = ChordState::default();
        let mut kill_ring = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();

        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let outcome = dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('z')),
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
            &km,
            &mut chord,
        );
        assert_eq!(outcome, DispatchOutcome::Suspend);
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
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
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
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
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
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("B")),
                version: 1,
                saved_version: 0,
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        // Use Ctrl-X Ctrl-D (SaveNoFormat) for this test — it
        // enqueues `pending_saves` directly without going
        // through the format-on-save round trip. The
        // format-on-save path is covered by the M18 save tests.
        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('d')),
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

    // ── M18 goto-definition ───────────────────────────────

    #[test]
    fn alt_enter_queues_goto_definition_at_cursor() {
        // Fixture seeds a loaded "file.rs" buffer; move the cursor
        // to (2, 4) then press Alt-Enter.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("line0\nline1\nline2 word", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = led_state_tabs::Cursor {
            line: 2,
            col: 4,
            preferred_col: 4,
        };

        let mut lsp_extras = LspExtrasState::default();
        let mut completions = CompletionsState::default();
        let mut chord = ChordState::default();
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let km = default_keymap();
        dispatch_key(
            key(KeyModifiers::ALT, KeyCode::Enter),
            &mut tabs,
            &mut edits,
            &mut kr,
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
            &km,
            &mut chord,
        );
        assert_eq!(lsp_extras.pending_goto.len(), 1);
        let req = &lsp_extras.pending_goto[0];
        assert_eq!(req.path, canon("file.rs"));
        assert_eq!(req.line, 2);
        assert_eq!(req.col, 4);
        assert_eq!(lsp_extras.latest_goto_seq, Some(req.seq));
    }

    #[test]
    fn alt_enter_is_noop_without_active_tab() {
        let mut tabs = Tabs::default();
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 20, rows: 5 }));

        let mut lsp_extras = LspExtrasState::default();
        let mut completions = CompletionsState::default();
        let mut chord = ChordState::default();
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let km = default_keymap();
        dispatch_key(
            key(KeyModifiers::ALT, KeyCode::Enter),
            &mut tabs,
            &mut edits,
            &mut kr,
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
            &mut path_chains,
            &mut completions,
            &mut lsp_extras,
            &DiagnosticsStates::default(),
            &GitState::default(),
            &km,
            &mut chord,
        );
        assert!(lsp_extras.pending_goto.is_empty());
    }
}
