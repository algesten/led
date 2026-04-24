//! Undo / redo (M8).
//!
//! Each reverses / reapplies the most recent [`EditGroup`] in the
//! buffer's history. Cursor is restored to the captured bookend
//! (cursor_before for undo, cursor_after for redo).

use led_core::CanonPath;
use led_state_buffer_edits::{BufferEdits, EditOp};
use led_state_file_search::{FileSearchSelection, FileSearchState};
use led_state_tabs::Tabs;

use super::shared::{bump, with_active};

pub(super) fn undo_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let Some(group) = eb.history.take_undo() else {
            return;
        };
        // Apply ops in reverse order, as their inverses.
        let mut rope = (*eb.rope).clone();
        for op in group.ops.iter().rev() {
            match op {
                EditOp::Insert { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
                EditOp::Delete { at, text } => {
                    rope.insert(*at, text);
                }
            }
        }
        bump(eb, rope);
        tab.cursor = group.cursor_before;
        tab.cursor.preferred_col = tab.cursor.col;
        eb.history.push_future(group);
    });
}

pub(super) fn redo_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let Some(group) = eb.history.take_redo() else {
            return;
        };
        let mut rope = (*eb.rope).clone();
        for op in &group.ops {
            match op {
                EditOp::Insert { at, text } => {
                    rope.insert(*at, text);
                }
                EditOp::Delete { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
            }
        }
        bump(eb, rope);
        tab.cursor = group.cursor_after;
        tab.cursor.preferred_col = tab.cursor.col;
        eb.history.push_past(group);
    });
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
    floor: u64,
    body_rows: usize,
) {
    let Some(target_path) = pick_max_past_seq(edits, floor) else {
        return;
    };
    // Pop the group, apply the rope inverse, pin saved_version
    // when the group came from a preview (disk_write). Collect
    // what we need (cursor_before, replacement bytes) BEFORE
    // releasing the &mut eb so the later push into
    // pending_single_replace can take its own &mut edits.
    let (group, cursor_before, replacement_bytes, disk_write) = {
        let Some(eb) = edits.buffers.get_mut(&target_path) else {
            return;
        };
        let Some(group) = eb.history.take_undo() else {
            return;
        };
        let mut rope = (*eb.rope).clone();
        for op in group.ops.iter().rev() {
            match op {
                EditOp::Insert { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
                EditOp::Delete { at, text } => {
                    rope.insert(*at, text);
                }
            }
        }
        bump(eb, rope);
        let disk_write = group
            .file_search_mark
            .as_ref()
            .is_some_and(|m| m.disk_write);
        if disk_write {
            eb.saved_version = eb.version;
        }
        // Replacement bytes (Insert op's text length) for the
        // inverse driver cmd's match range.
        let replacement_bytes = group
            .ops
            .iter()
            .find_map(|op| match op {
                EditOp::Insert { text, .. } => Some(text.len()),
                _ => None,
            })
            .unwrap_or(0);
        let cursor_before = group.cursor_before;
        (group, cursor_before, replacement_bytes, disk_write)
    };

    // Cursor-follow when the undone buffer is the active tab.
    if let Some(active_id) = tabs.active
        && let Some(tab) = tabs.open.iter_mut().find(|t| t.id == active_id)
        && tab.path == target_path
    {
        tab.cursor = cursor_before;
        tab.cursor.preferred_col = tab.cursor.col;
    }

    // Overlay sync + inverse driver cmd for disk_write groups.
    if let (Some(mark), Some(state)) = (&group.file_search_mark, file_search) {
        apply_mark_to_state(state, mark.hit_idx, !mark.forward_marks_replaced);
        focus_affected_hit(state, mark.hit_idx, body_rows);
        if disk_write
            && let Some(hit) = state.flat_hits.get(mark.hit_idx).cloned()
        {
            let (orig, repl) = extract_delete_insert_texts(&group.ops);
            if let (Some(orig), Some(repl)) = (orig, repl) {
                edits.pending_single_replace.push(
                    led_state_buffer_edits::PendingSingleReplace {
                        path: target_path.clone(),
                        line: hit.line,
                        match_start: hit.match_start,
                        match_end: hit.match_start + replacement_bytes,
                        original: repl,
                        replacement: orig,
                    },
                );
            }
        }
    }

    if let Some(eb) = edits.buffers.get_mut(&target_path) {
        eb.history.push_future(group);
    }
}

/// Pull the Delete + Insert text fields off a replace group's
/// ops. Returns (original, replacement) strings. Both `None` when
/// the group isn't shaped as (Delete, Insert).
fn extract_delete_insert_texts(ops: &[EditOp]) -> (Option<String>, Option<String>) {
    let mut del: Option<String> = None;
    let mut ins: Option<String> = None;
    for op in ops {
        match op {
            EditOp::Delete { text, .. } if del.is_none() => del = Some(text.to_string()),
            EditOp::Insert { text, .. } if ins.is_none() => ins = Some(text.to_string()),
            _ => {}
        }
    }
    (del, ins)
}

/// Cross-buffer redo mirror of `undo_global`. Uses the
/// max-seq-`> floor` group across `future` stacks.
pub(super) fn redo_global(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    file_search: Option<&mut FileSearchState>,
    floor: u64,
    body_rows: usize,
) {
    let Some(target_path) = pick_max_future_seq(edits, floor) else {
        return;
    };
    let (group, cursor_after, original_bytes, disk_write) = {
        let Some(eb) = edits.buffers.get_mut(&target_path) else {
            return;
        };
        let Some(group) = eb.history.take_redo() else {
            return;
        };
        let mut rope = (*eb.rope).clone();
        for op in &group.ops {
            match op {
                EditOp::Insert { at, text } => {
                    rope.insert(*at, text);
                }
                EditOp::Delete { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
            }
        }
        bump(eb, rope);
        let disk_write = group
            .file_search_mark
            .as_ref()
            .is_some_and(|m| m.disk_write);
        if disk_write {
            eb.saved_version = eb.version;
        }
        let original_bytes = group
            .ops
            .iter()
            .find_map(|op| match op {
                EditOp::Delete { text, .. } => Some(text.len()),
                _ => None,
            })
            .unwrap_or(0);
        let cursor_after = group.cursor_after;
        (group, cursor_after, original_bytes, disk_write)
    };

    if let Some(active_id) = tabs.active
        && let Some(tab) = tabs.open.iter_mut().find(|t| t.id == active_id)
        && tab.path == target_path
    {
        tab.cursor = cursor_after;
        tab.cursor.preferred_col = tab.cursor.col;
    }
    if let (Some(mark), Some(state)) = (&group.file_search_mark, file_search) {
        apply_mark_to_state(state, mark.hit_idx, mark.forward_marks_replaced);
        focus_affected_hit(state, mark.hit_idx, body_rows);
        if disk_write
            && let Some(hit) = state.flat_hits.get(mark.hit_idx).cloned()
        {
            let (orig, repl) = extract_delete_insert_texts(&group.ops);
            if let (Some(orig), Some(repl)) = (orig, repl) {
                // Forward again: replace `orig` bytes
                // [hit.match_start..match_start + orig.len()]
                // with `repl`.
                edits.pending_single_replace.push(
                    led_state_buffer_edits::PendingSingleReplace {
                        path: target_path.clone(),
                        line: hit.line,
                        match_start: hit.match_start,
                        match_end: hit.match_start + original_bytes,
                        original: orig,
                        replacement: repl,
                    },
                );
            }
        }
    }
    if let Some(eb) = edits.buffers.get_mut(&target_path) {
        eb.history.push_past(group);
    }
}

fn pick_max_past_seq(edits: &BufferEdits, floor: u64) -> Option<CanonPath> {
    edits
        .buffers
        .iter()
        .filter_map(|(p, eb)| eb.history.past_top_seq().map(|s| (p.clone(), s)))
        .filter(|(_, s)| *s > floor)
        .max_by_key(|(_, s)| *s)
        .map(|(p, _)| p)
}

fn pick_max_future_seq(edits: &BufferEdits, floor: u64) -> Option<CanonPath> {
    edits
        .buffers
        .iter()
        .filter_map(|(p, eb)| eb.history.future_top_seq().map(|s| (p.clone(), s)))
        .filter(|(_, s)| *s > floor)
        .max_by_key(|(_, s)| *s)
        .map(|(p, _)| p)
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
    use led_state_diagnostics::DiagnosticsStates;
    use led_state_file_search::FileSearchState;
    use led_state_find_file::FindFileState;
    use led_state_git::GitState;
    use led_state_isearch::IsearchState;


    
    
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    use led_state_alerts::AlertState;
    use led_state_clipboard::ClipboardState;
    use led_state_jumps::JumpListState;
    use led_state_browser::{BrowserUi, FsTree};

    use led_state_kill_ring::KillRing;
    use led_state_lsp::LspExtrasState;
    use led_state_tabs::Cursor;
    

    
    use super::super::testutil::*;
    use super::super::{dispatch_key, ChordState};
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
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        let mut alerts = AlertState::default();
        let mut jumps = JumpListState::default();
        let mut browser = BrowserUi::default();
        let fs = FsTree::default();

        let mut path_chains = std::collections::HashMap::new();
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();
        // Undo: ""
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('/')),
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
            &mut chord,);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");

        // Redo: "hi"
        let mut find_file: Option<FindFileState> = None;
        let mut isearch: Option<IsearchState> = None;
        let mut file_search: Option<FileSearchState> = None;
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
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
            &mut chord,);
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
        let mut clip = ClipboardState::default();
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
        let mut completions = CompletionsState::default();
        let mut lsp_extras = LspExtrasState::default();
        dispatch_key(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
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
            &mut chord,);
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
