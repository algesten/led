//! Undo / redo (M8).
//!
//! Each reverses / reapplies the most recent [`EditGroup`] in the
//! buffer's history. Cursor is restored to the captured bookend
//! (cursor_before for undo, cursor_after for redo).
//!
//! The "what does the reverse step look like?" derivation lives in
//! the [`crate::query::undo_action`] / [`crate::query::redo_action`]
//! memos: they peek the topmost group via
//! [`led_state_buffer_edits::History::peek_undo`], invert the ops
//! against the current rope, and return a structured
//! [`crate::query::UndoApply`] payload. The reducers in this module
//! consume that payload (rope bump, cursor move, file-search overlay
//! sync, pending driver cmd) and then pop the group from history
//! via `take_undo` / `take_redo`.

use led_core::{EditSeq, SavedVersion};
use led_state_buffer_edits::BufferEdits;
use led_state_file_search::{FileSearchSelection, FileSearchState};
use led_state_tabs::Tabs;

use super::shared::bump;
use crate::query::{
    redo_action, redo_target_path, undo_action, undo_target_path, EditedBuffersInput,
    RedoAction, UndoAction, UndoApply,
};

pub(super) fn undo_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    // Finalise the open group on the active tab BEFORE peeking so
    // the user undoes what they just typed (matching the legacy
    // contract where `take_undo` does the finalise inline). The
    // memo path uses `peek_undo`, which does NOT finalise — so we
    // do it explicitly here.
    let Some(id) = tabs.active else { return };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let path = tabs.open[idx].path.clone();
    if let Some(eb) = edits.buffers.get_mut(&path) {
        eb.history.finalise();
    }
    apply_undo(tabs, edits, None, 0, Some(path));
}

pub(super) fn redo_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    let Some(id) = tabs.active else { return };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let path = tabs.open[idx].path.clone();
    if let Some(eb) = edits.buffers.get_mut(&path) {
        eb.history.finalise();
    }
    apply_redo(tabs, edits, None, 0, Some(path));
}

/// Cross-buffer undo used by the file-search overlay. Pops the
/// group with the largest seq > `floor` across all loaded buffers,
/// applies its inverse to that buffer's rope, and — if the group
/// carries a `FileSearchMark` — resyncs
/// `FileSearchState.hit_replacements` so the overlay's marks stay
/// consistent with what the buffer content shows.
///
/// `floor` is `FileSearchState.overlay_open_seq`: pre-overlay
/// edits get smaller seqs and are never popped here.
pub(super) fn undo_global(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    file_search: Option<&mut FileSearchState>,
    floor: EditSeq,
    body_rows: usize,
) {
    // Finalise every buffer's open current group so the seq picker
    // sees a stable top-of-past. Without this, an in-flight
    // typing-group on some buffer would get an unstamped seq=0
    // (skipped by undo_target_path) but then immediately
    // finalised + popped by the later take_undo call — the memo
    // would have peeked at a different group than the one that
    // actually gets popped.
    finalise_all(edits);
    let target = undo_target_path(EditedBuffersInput::new(edits), floor);
    apply_undo(tabs, edits, file_search, body_rows, target);
}

/// Cross-buffer redo mirror of `undo_global`. Uses the
/// max-seq-`> floor` group across `future` stacks.
pub(super) fn redo_global(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    file_search: Option<&mut FileSearchState>,
    floor: EditSeq,
    body_rows: usize,
) {
    // See [`undo_global`] for the rationale; redo's take_redo also
    // defensively finalises, so we mirror it here for symmetry.
    finalise_all(edits);
    let target = redo_target_path(EditedBuffersInput::new(edits), floor);
    apply_redo(tabs, edits, file_search, body_rows, target);
}

/// Close every buffer's open `current` group into `past` before
/// the seq-picker memo runs. `imbl::HashMap` doesn't expose
/// `values_mut`, so we walk the keys + `get_mut` per entry.
fn finalise_all(edits: &mut BufferEdits) {
    let paths: Vec<led_core::CanonPath> = edits.buffers.keys().cloned().collect();
    for path in paths {
        if let Some(eb) = edits.buffers.get_mut(&path) {
            eb.history.finalise();
        }
    }
}

/// Read the [`crate::query::undo_action`] memo for `target_path`
/// and apply its payload. Pure assignment: rope swap, cursor
/// follow, file-search overlay sync, queued driver cmd, then pop
/// the group from `past` into `future`.
fn apply_undo(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    file_search: Option<&mut FileSearchState>,
    body_rows: usize,
    target_path: Option<led_core::CanonPath>,
) {
    let action = undo_action(EditedBuffersInput::new(edits), &target_path);
    let UndoAction::Apply(apply) = action else {
        return;
    };
    apply_action_payload(tabs, edits, file_search, body_rows, *apply, UndoDir::Undo);
}

/// Mirror of [`apply_undo`] for the redo direction.
fn apply_redo(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    file_search: Option<&mut FileSearchState>,
    body_rows: usize,
    target_path: Option<led_core::CanonPath>,
) {
    let action = redo_action(EditedBuffersInput::new(edits), &target_path);
    let RedoAction::Apply(apply) = action else {
        return;
    };
    apply_action_payload(tabs, edits, file_search, body_rows, *apply, UndoDir::Redo);
}

#[derive(Copy, Clone)]
enum UndoDir {
    Undo,
    Redo,
}

/// Shared reducer body for undo + redo. The memo computes the
/// payload; this fn just writes the new state.
fn apply_action_payload(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    file_search: Option<&mut FileSearchState>,
    body_rows: usize,
    apply: UndoApply,
    dir: UndoDir,
) {
    let target_path = apply.path.clone();
    // Mutate the buffer: rope bump, optional disk-anchor refresh
    // for preview disk_write groups.
    {
        let Some(eb) = edits.buffers.get_mut(&target_path) else {
            return;
        };
        bump(eb, (*apply.new_rope).clone());
        if apply.disk_write_pending.is_some() {
            eb.saved_version = SavedVersion(eb.version.0);
            eb.disk_content_hash =
                led_core::EphemeralContentHash::of_rope(&eb.draft).persist();
        }
    }

    // Cursor-follow when the affected buffer is the active tab.
    if let Some(active_id) = tabs.active
        && let Some(tab) = tabs.open.iter_mut().find(|t| t.id == active_id)
        && tab.path == target_path
    {
        tab.cursor = apply.cursor;
        tab.cursor.preferred_col = tab.cursor.col;
    }

    // File-search overlay sync + inverse / forward driver cmd for
    // disk_write groups.
    if let (Some(mark), Some(state)) = (&apply.mark, file_search) {
        apply_mark_to_state(state, mark.hit_idx, mark.target_replaced);
        focus_affected_hit(state, mark.hit_idx, body_rows);
        if let Some(pending) = apply.disk_write_pending.as_ref()
            && let Some(hit) = state.flat_hits.get(mark.hit_idx).cloned()
        {
            edits
                .pending_single_replace
                .push(led_state_buffer_edits::PendingSingleReplace {
                    path: target_path.clone(),
                    line: hit.line,
                    match_start: hit.match_start,
                    match_end: hit.match_start + pending.match_byte_len,
                    original: pending.original.clone(),
                    replacement: pending.replacement.clone(),
                });
        }
    }

    // Pop the group + transfer to the other stack. The memo
    // peeked at exactly the group we're popping here, so the data
    // we just applied IS this group's reverse / forward step.
    if let Some(eb) = edits.buffers.get_mut(&target_path) {
        let popped = match dir {
            UndoDir::Undo => eb.history.take_undo(),
            UndoDir::Redo => eb.history.take_redo(),
        };
        if let Some(group) = popped {
            match dir {
                UndoDir::Undo => eb.history.push_future(group),
                UndoDir::Redo => eb.history.push_past(group),
            }
        }
    }
}

/// Move the overlay's selection onto the just-affected hit and,
/// when that row is currently off-screen, scroll it to roughly
/// `body_rows / 3` from the top (with context above). Leaves the
/// scroll alone when the row is already visible — no jitter when
/// the user's already looking at it.
fn focus_affected_hit(
    state: &mut FileSearchState,
    hit_idx: usize,
    body_rows: usize,
) {
    if hit_idx >= state.flat_hits.len() {
        return;
    }
    state.selection = FileSearchSelection::Result(hit_idx);
    let input_rows = 1 + 1 + state.replace_mode as usize;
    let tree_visible = body_rows.saturating_sub(input_rows);
    if tree_visible == 0 {
        return;
    }
    let stream = tree_row_index_for_hit_ref(&state.results, hit_idx);
    let top = state.scroll_offset;
    let bottom = top + tree_visible.saturating_sub(1);
    if stream < top || stream > bottom {
        let third = tree_visible / 3;
        state.scroll_offset = stream.saturating_sub(third);
    }
}

/// Mirror of `file_search::tree_row_index_for_hit`. Kept local to
/// this module to avoid a pub cycle; the implementation is the
/// same stream-walk (group header + hits, in order).
fn tree_row_index_for_hit_ref(
    groups: &[led_state_file_search::FileSearchGroup],
    flat_idx: usize,
) -> usize {
    let mut stream = 0usize;
    let mut seen = 0usize;
    for group in groups {
        stream += 1; // group header
        if flat_idx < seen + group.hits.len() {
            return stream + (flat_idx - seen);
        }
        stream += group.hits.len();
        seen += group.hits.len();
    }
    stream.saturating_sub(1)
}

/// Toggle the overlay's view of a hit to match a new "replaced?"
/// value. Rebuilds the `ReplaceEntry` when the mark flips true —
/// we don't need the full entry for display, just Some(placeholder)
/// vs None. Forward-applying a Right gives `target=true`, its undo
/// gives `target=false`, and vice versa for Left's inverse.
fn apply_mark_to_state(state: &mut FileSearchState, hit_idx: usize, target_replaced: bool) {
    if hit_idx >= state.flat_hits.len() || hit_idx >= state.hit_replacements.len() {
        return;
    }
    if target_replaced {
        // Rebuild a minimal entry from the hit; the exact
        // rope_char_start / replacement_char_len aren't needed for
        // display, and the Left-arrow path recomputes them from
        // hit.preview when necessary.
        let hit = state.flat_hits[hit_idx].clone();
        let original_char_len = hit
            .preview
            .get(hit.match_start..hit.match_end)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        let replacement_text = state.replace.text.clone();
        state.hit_replacements[hit_idx] = Some(led_state_file_search::ReplaceEntry {
            hit: hit.clone(),
            replacement_text: replacement_text.clone(),
            replacement_char_len: replacement_text.chars().count(),
            original_char_len,
            rope_char_start: 0,
            path: hit.path,
        });
    } else {
        state.hit_replacements[hit_idx] = None;
    }
    // If the selection was on this row, keep it. Nothing else to
    // do — the sidebar redraw picks up the new state.
    let _ = FileSearchSelection::Result(hit_idx);
}

#[cfg(test)]
mod tests {
    use led_state_completions::CompletionsState;
    use led_driver_lsp_core::DiagnosticsStates;
    use led_state_file_search::FileSearchState;
    use led_state_find_file::FindFileState;
    use led_state_git::GitState;
    use led_state_isearch::IsearchState;


    
    
    use led_driver_fs_list_core::FsTree;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    use led_state_alerts::AlertState;
    use led_state_browser::BrowserUi;
    use led_state_clipboard::ClipboardIntent;
    use led_state_jumps::JumpListState;

    use led_state_kill_ring::KillRing;
    use led_state_lsp::LspExtrasState;
    use led_state_tabs::Cursor;
    

    
    use super::super::testutil::*;
    use super::super::{ChordState, Dispatcher};
    use crate::keymap::{default_keymap, Command};

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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let mut chat_sessions = led_state_chat::ChatSessions::default();
        let mut chat_prefs = led_state_chat::ChatPrefs::default();
        let mut kr = KillRing::default();
        let mut clip = ClipboardIntent::default();
        let clipboard_driver = led_driver_clipboard_core::ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();

        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_driver_lsp_core::LspStatuses::default();
        let git = GitState::default();
        let syntax = led_state_syntax::SyntaxStates::default();
        let clock = crate::Clock::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kr,
                clip: &mut clip,
                clipboard_driver: &clipboard_driver,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &km,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
                chat_sessions: &mut chat_sessions,
                chat_prefs: &mut chat_prefs,
                syntax: &syntax,
                clock: &clock,
            };
            // Undo: ""
            dispatcher.dispatch_key(key(KeyModifiers::CONTROL, KeyCode::Char('/')));
            assert_eq!(dispatcher.edits.buffers.values().next().unwrap().draft.to_string(), "");
            // Redo: "hi"
            dispatcher.dispatch_key(key(KeyModifiers::CONTROL, KeyCode::Char('y')));
        }
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
        let mut clip = ClipboardIntent::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
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
        let mut kbd_macro = led_state_kbd_macro::KbdMacroState::default();
        let mut chat_sessions = led_state_chat::ChatSessions::default();
        let mut chat_prefs = led_state_chat::ChatPrefs::default();
        let mut kr = KillRing::default();
        let mut clip = ClipboardIntent::default();
        let clipboard_driver = led_driver_clipboard_core::ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut completions_pending = led_state_completions::CompletionsPending::default();
        let mut lsp_extras = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        let diagnostics = DiagnosticsStates::default();
        let lsp_status = led_driver_lsp_core::LspStatuses::default();
        let git = GitState::default();
        let syntax = led_state_syntax::SyntaxStates::default();
        let clock = crate::Clock::default();
        {
            let mut dispatcher = Dispatcher {
                tabs: &mut tabs,
                edits: &mut edits,
                kill_ring: &mut kr,
                clip: &mut clip,
                clipboard_driver: &clipboard_driver,
                alerts: &mut alerts,
                jumps: &mut jumps,
                browser: &mut browser,
                fs: &fs,
                store: &store,
                terminal: &term,
                find_file: &mut find_file,
                isearch: &mut isearch,
                file_search: &mut file_search,
                completions: &mut completions,
                completions_pending: &mut completions_pending,
                lsp_extras: &mut lsp_extras,
                lsp_pending: &mut lsp_pending,
                diagnostics: &diagnostics,
                lsp_status: &lsp_status,
                git: &git,
                path_chains: &mut path_chains,
                keymap: &km,
                chord: &mut chord,
                kbd_macro: &mut kbd_macro,
                chat_sessions: &mut chat_sessions,
                chat_prefs: &mut chat_prefs,
                syntax: &syntax,
                clock: &clock,
            };
            dispatcher.dispatch_key(key(KeyModifiers::CONTROL, KeyCode::Char('y')));
        }
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
}
