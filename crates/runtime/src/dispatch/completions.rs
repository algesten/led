//! Completion popup overlay — dispatch interception when
//! `CompletionsState.session` is `Some`.
//!
//! The popup behaves like an overlay: navigation keys move
//! `selected` within the filtered list, `Tab` / `Enter` commit
//! the highlighted item, `Esc` dismisses. `InsertChar` /
//! `DeleteBack` pass through to the buffer so the user keeps
//! typing — the post-command hook in `mod.rs` refilters after.
//!
//! Matches legacy `crates/lsp/src/manager.rs` + `led/src/model/action/lsp.rs`:
//! the action set intercepted is exactly (MoveUp, MoveDown,
//! InsertNewline, InsertTab, Abort, InsertChar, DeleteBack);
//! everything else dismisses and falls through.

use std::sync::Arc;

use led_state_buffer_edits::BufferEdits;
use led_state_completions::{CompletionSession, CompletionsState};
use led_state_tabs::Tabs;

use super::shared::bump;
use super::DispatchOutcome;
use crate::keymap::Command;

/// Attempt to consume `cmd` against an active completion popup.
/// Returns `Some(Continue)` when the command was consumed (the
/// outer dispatcher should skip `run_command`); `None` means
/// "pass through" — either no popup is active, or the command
/// wants the normal dispatch path.
///
/// Pass-through commands that land here while a session is live
/// typically need a post-command refilter / dismiss; that's
/// handled by `handle_completion_trigger` in the outer
/// dispatch boundary.
pub(super) fn run_overlay_command(
    cmd: Command,
    completions: &mut CompletionsState,
    completions_pending: &mut led_state_completions::CompletionsPending,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
) -> Option<DispatchOutcome> {
    // Fast path: nothing to intercept without a live session.
    completions.session.as_ref()?;

    match cmd {
        Command::CursorUp => {
            move_selection(completions, -1);
            Some(DispatchOutcome::Continue)
        }
        Command::CursorDown => {
            move_selection(completions, 1);
            Some(DispatchOutcome::Continue)
        }
        Command::InsertNewline => {
            commit_active(completions, completions_pending, tabs, edits);
            Some(DispatchOutcome::Continue)
        }
        Command::Abort => {
            completions.dismiss();
            Some(DispatchOutcome::Continue)
        }
        // Every other command flows through. Identifier-char
        // inserts keep the popup alive (they queue a fresh
        // request in `handle_completion_trigger` and rebuild the
        // session when the server responds). Any other command
        // falls through to `run_command`, and the outer hook
        // dismisses the now-irrelevant popup.
        _ => None,
    }
}

fn move_selection(completions: &mut CompletionsState, delta: isize) {
    let Some(session) = completions.session.as_mut() else {
        return;
    };
    let n = session.filtered.len();
    if n == 0 {
        return;
    }
    // Clamp rather than wrap — matches legacy. Users who
    // overshoot simply hit the edge; a separate command would
    // be needed to cycle.
    let idx = session.selected as isize + delta;
    let idx = idx.clamp(0, n.saturating_sub(1) as isize) as usize;
    session.selected = idx;
    ensure_visible(session);
}

/// Window-size for the popover. Legacy uses 10 rows max; we
/// carry the same constant in dispatch so the keep-selected-
/// visible math lines up with what the painter will render in
/// stage 7.
const POPOVER_ROWS: usize = 10;

/// Scroll `scroll` until `selected` is inside the visible
/// POPOVER_ROWS window.
fn ensure_visible(session: &mut CompletionSession) {
    if session.selected < session.scroll {
        session.scroll = session.selected;
    } else if session.selected >= session.scroll + POPOVER_ROWS {
        session.scroll = session.selected + 1 - POPOVER_ROWS;
    }
}

/// Apply the currently-selected completion to the active tab's
/// buffer. Replaces `[prefix_start_col, cursor_col)` on
/// `prefix_line` with the item's `textEdit.new_text` (when
/// present) or `insertText` (fallback) or `label` (last
/// resort). Moves the cursor past the inserted text, records
/// one history group, queues `ResolveCompletion` if the server
/// advertised `resolveProvider` AND the item didn't already
/// ship its additional edits, then dismisses the popup.
fn commit_active(
    completions: &mut CompletionsState,
    completions_pending: &mut led_state_completions::CompletionsPending,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
) {
    let Some(session) = completions.session.as_ref() else {
        return;
    };
    // Only commit into the tab the session was opened on. If
    // the user switched tabs mid-request, the session would
    // already have been dismissed — this is defensive.
    let target_tab = session.tab;
    let path = session.path.clone();
    let prefix_line = session.prefix_line as usize;
    let prefix_start_col = session.prefix_start_col as usize;
    let Some(&item_ix) = session.filtered.get(session.selected) else {
        completions.dismiss();
        return;
    };
    let item = session.items[item_ix].clone();

    // Resolve target tab + its buffer. Bail silently if either
    // went away mid-flight.
    let Some(tab_idx) = tabs.open.iter().position(|t| t.id == target_tab) else {
        completions.dismiss();
        return;
    };
    if tabs.open[tab_idx].preview {
        // Preview tabs are strict viewers; committing into one
        // would create dirty state the user didn't ask for.
        completions.dismiss();
        return;
    }
    if !edits.buffers.contains_key(&path) {
        completions.dismiss();
        return;
    }

    let tab = &mut tabs.open[tab_idx];
    let before = tab.cursor;

    // Choose the replacement range + new text. textEdit wins
    // (servers use it to delete the whole typed prefix + insert
    // the full identifier); otherwise fall back to
    // insertText / label.
    let (replace_start_col, replace_end_col, new_text) = match item.text_edit.as_ref() {
        Some(te) => (
            te.col_start as usize,
            te.col_end as usize,
            te.new_text.clone(),
        ),
        None => {
            let text = item
                .insert_text
                .clone()
                .unwrap_or_else(|| item.label.clone());
            (prefix_start_col, before.col, text)
        }
    };

    // Clamp to the actual rope so a stale item (cursor moved
    // since the session opened) can't panic on out-of-range
    // indices.
    let eb = edits.buffers.get_mut(&path).expect("checked above");
    let rope_len = eb.rope.len_chars();
    let line_char_start = eb.rope.line_to_char(prefix_line);
    let line_end_char = if prefix_line + 1 < eb.rope.len_lines() {
        eb.rope.line_to_char(prefix_line + 1)
    } else {
        rope_len
    };
    let replace_from = (line_char_start + replace_start_col).min(line_end_char);
    let replace_to = (line_char_start + replace_end_col).min(line_end_char);
    if replace_to < replace_from {
        completions.dismiss();
        return;
    }

    let mut rope = (*eb.rope).clone();
    let removed_text: String = rope.slice(replace_from..replace_to).to_string();
    rope.remove(replace_from..replace_to);
    rope.insert(replace_from, &new_text);
    bump(eb, rope);

    let inserted_char_count = new_text.chars().count();
    let new_cursor_char = replace_from + inserted_char_count;
    let new_line = eb.rope.char_to_line(new_cursor_char);
    let new_col = new_cursor_char - eb.rope.line_to_char(new_line);
    tab.cursor.line = new_line;
    tab.cursor.col = new_col;
    tab.cursor.preferred_col = new_col;
    let after = tab.cursor;

    // History: record the delete (if the typed prefix actually
    // had content) then the insert. Two ops grouped so undo
    // takes both back in one C-_.
    if !removed_text.is_empty() {
        eb.history.record_delete(
            replace_from,
            Arc::<str>::from(removed_text),
            before,
            before,
        );
    }
    eb.history.record_insert(
        replace_from,
        Arc::<str>::from(new_text.as_ref()),
        before,
        after,
    );

    // Queue resolve for additional edits (imports, etc.) if the
    // server said it could provide them.
    if item.resolve_needed {
        completions_pending.queue_resolve(path, item);
    }

    completions.dismiss();
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{CanonPath, UserPath};
    use led_driver_lsp_core::CompletionItem;
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    use led_state_completions::CompletionSession;
    use led_state_tabs::{Tab, TabId, Tabs};
    use ropey::Rope;
    use std::sync::Arc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn seed_session(
        tabs: &mut Tabs,
        edits: &mut BufferEdits,
        rope: &str,
        prefix_start_col: u32,
        items: Vec<CompletionItem>,
    ) -> CompletionsState {
        let path = canon("test.rs");
        let rope = Arc::new(Rope::from_str(rope));
        edits
            .buffers
            .insert(path.clone(), EditedBuffer::fresh(rope.clone()));
        let tab_id = TabId(1);
        tabs.open.push_back(Tab {
            id: tab_id,
            path: path.clone(),
            ..Default::default()
        });
        tabs.active = Some(tab_id);
        let filtered: Vec<usize> = (0..items.len()).collect();
        let mut state = CompletionsState::default();
        state.session = Some(CompletionSession {
            tab: tab_id,
            path,
            seq: 1,
            prefix_line: 0,
            prefix_start_col,
            items: Arc::new(items),
            filtered: Arc::new(filtered),
            selected: 0,
            scroll: 0,
        });
        state
    }

    fn mk_item(label: &str, insert: Option<&str>) -> CompletionItem {
        CompletionItem {
            label: Arc::<str>::from(label),
            detail: None,
            sort_text: None,
            insert_text: insert.map(Arc::<str>::from),
            text_edit: None,
            kind: None,
            resolve_needed: false,
            resolve_data: None,
        }
    }

    #[test]
    fn cursor_down_advances_selected_and_stops_at_last() {
        let mut tabs = Tabs::default();
        let mut edits = BufferEdits::default();
        let mut pending = led_state_completions::CompletionsPending::default();
        let mut state = seed_session(
            &mut tabs,
            &mut edits,
            "p",
            0,
            vec![mk_item("pr", None), mk_item("pub", None), mk_item("pop", None)],
        );
        let outcome = run_overlay_command(Command::CursorDown, &mut state, &mut pending, &mut tabs, &mut edits);
        assert_eq!(outcome, Some(DispatchOutcome::Continue));
        assert_eq!(state.session.as_ref().unwrap().selected, 1);
        run_overlay_command(Command::CursorDown, &mut state, &mut pending, &mut tabs, &mut edits);
        assert_eq!(state.session.as_ref().unwrap().selected, 2);
        // Clamp at the last item — no wrap.
        run_overlay_command(Command::CursorDown, &mut state, &mut pending, &mut tabs, &mut edits);
        assert_eq!(state.session.as_ref().unwrap().selected, 2);
    }

    #[test]
    fn abort_dismisses_the_session() {
        let mut tabs = Tabs::default();
        let mut edits = BufferEdits::default();
        let mut pending = led_state_completions::CompletionsPending::default();
        let mut state = seed_session(&mut tabs, &mut edits, "p", 0, vec![mk_item("pr", None)]);
        let outcome = run_overlay_command(Command::Abort, &mut state, &mut pending, &mut tabs, &mut edits);
        assert_eq!(outcome, Some(DispatchOutcome::Continue));
        assert!(state.session.is_none());
    }

    #[test]
    fn commit_replaces_prefix_and_dismisses() {
        // Buffer: "pr", cursor at col 2 (end of "pr"). Prefix
        // starts at col 0 — so the commit should replace [0, 2)
        // with the selected item's insert_text.
        let mut tabs = Tabs::default();
        let mut edits = BufferEdits::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("test.rs"),
            cursor: led_state_tabs::Cursor {
                line: 0,
                col: 2,
                preferred_col: 2,
            },
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        let path = canon("test.rs");
        let rope = Arc::new(Rope::from_str("pr"));
        edits
            .buffers
            .insert(path.clone(), EditedBuffer::fresh(rope));
        let filtered: Vec<usize> = vec![0];
        let mut state = CompletionsState::default();
        state.session = Some(CompletionSession {
            tab: TabId(1),
            path: path.clone(),
            seq: 1,
            prefix_line: 0,
            prefix_start_col: 0,
            items: Arc::new(vec![mk_item("println!", Some("println!"))]),
            filtered: Arc::new(filtered),
            selected: 0,
            scroll: 0,
        });

        let mut pending = led_state_completions::CompletionsPending::default();
        let outcome = run_overlay_command(Command::InsertNewline, &mut state, &mut pending, &mut tabs, &mut edits);
        assert_eq!(outcome, Some(DispatchOutcome::Continue));
        assert!(state.session.is_none());
        let eb = edits.buffers.get(&path).unwrap();
        assert_eq!(eb.rope.to_string(), "println!");
        assert_eq!(tabs.open[0].cursor.col, 8);
    }

    #[test]
    fn no_session_passes_through() {
        let mut tabs = Tabs::default();
        let mut edits = BufferEdits::default();
        let mut state = CompletionsState::default();
        let mut pending = led_state_completions::CompletionsPending::default();
        let outcome =
            run_overlay_command(Command::CursorUp, &mut state, &mut pending, &mut tabs, &mut edits);
        assert_eq!(outcome, None);
    }
}
