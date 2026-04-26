//! LSP code-action picker dispatch (M18 stage 4).
//!
//! Alt-i fires a `textDocument/codeAction` for the cursor (or
//! mark..cursor selection). When a non-empty response lands, the
//! runtime installs a `CodeActionPickerState`; while that's set
//! the overlay absorbs input:
//!
//! - `CursorUp` / `CursorDown` — navigate the list (clamped,
//!   no wrap — matches legacy completion popup behaviour).
//! - `InsertNewline` — commit: queues a
//!   `LspCmd::SelectCodeAction` and dismisses the picker.
//! - `Abort` — dismiss.
//! - Everything else absorbed.
//!
//! The activation path reads the active tab's cursor + mark;
//! no active selection collapses to cursor..cursor.

use led_state_buffer_edits::BufferEdits;
use led_state_lsp::{CodeActionPickerState, LspExtrasState};
use led_state_tabs::Tabs;

use super::DispatchOutcome;
use crate::keymap::Command;

/// Max picker rows the painter draws at once. Matches the
/// completion popup constant (stage 5 paint will also use it).
const PICKER_ROWS: usize = 10;

/// Fire a code-action request. No-op without an active tab,
/// loaded buffer, or if a picker is already open (legacy parity
/// — users close the first before asking again).
pub(super) fn activate(
    lsp_extras: &mut LspExtrasState,
    lsp_pending: &mut led_state_lsp::LspPending,
    tabs: &Tabs,
    edits: &BufferEdits,
) {
    if lsp_extras.code_actions.is_some() {
        return;
    }
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
    let (start_line, start_col, end_line, end_col) = match tab.mark {
        Some(m) => {
            // Sort mark vs cursor so the LSP range always has
            // start <= end — the protocol assumes ordered.
            let a = (m.line as u32, m.col as u32);
            let b = (tab.cursor.line as u32, tab.cursor.col as u32);
            let (s, e) = if a <= b { (a, b) } else { (b, a) };
            (s.0, s.1, e.0, e.1)
        }
        None => (
            tab.cursor.line as u32,
            tab.cursor.col as u32,
            tab.cursor.line as u32,
            tab.cursor.col as u32,
        ),
    };
    lsp_pending.queue_code_action(
        tab.path.clone(),
        start_line,
        start_col,
        end_line,
        end_col,
    );
}

/// Route a command through the code-action picker when active.
/// Returns `Some(Continue)` on absorb, `None` when no picker.
pub(super) fn run_overlay_command(
    cmd: Command,
    lsp_extras: &mut LspExtrasState,
    lsp_pending: &mut led_state_lsp::LspPending,
) -> Option<DispatchOutcome> {
    lsp_extras.code_actions.as_ref()?;
    if matches!(cmd, Command::Quit | Command::Suspend) {
        return None;
    }
    match cmd {
        Command::CursorUp => move_selection(lsp_extras, -1),
        Command::CursorDown => move_selection(lsp_extras, 1),
        Command::InsertNewline => commit(lsp_extras, lsp_pending),
        Command::Abort => lsp_extras.dismiss_code_actions(),
        // Every other command is absorbed — the picker is
        // modal, matching legacy's `handle_code_actions_action`.
        _ => {}
    }
    Some(DispatchOutcome::Continue)
}

fn move_selection(lsp_extras: &mut LspExtrasState, delta: isize) {
    let Some(state) = lsp_extras.code_actions.as_mut() else {
        return;
    };
    let n = state.items.len();
    if n == 0 {
        return;
    }
    let next = (state.selected as isize + delta).clamp(0, n as isize - 1) as usize;
    state.selected = next;
    // Keep `selected` inside the visible window.
    if state.selected < state.scroll {
        state.scroll = state.selected;
    } else if state.selected >= state.scroll + PICKER_ROWS {
        state.scroll = state.selected + 1 - PICKER_ROWS;
    }
}

fn commit(
    lsp_extras: &mut LspExtrasState,
    lsp_pending: &mut led_state_lsp::LspPending,
) {
    let Some(state) = lsp_extras.code_actions.as_ref() else {
        return;
    };
    let Some(action) = state.items.get(state.selected).cloned() else {
        lsp_extras.dismiss_code_actions();
        return;
    };
    let path = state.path.clone();
    lsp_pending.queue_code_action_select(path, action);
    lsp_extras.dismiss_code_actions();
}

/// Install a picker from an incoming `LspEvent::CodeActions`.
/// Called from the ingest side. Empty item lists deliberately
/// surface as no-op: the runtime elsewhere alerts "No code
/// actions" so users know their keystroke registered.
pub fn install_picker(
    lsp_extras: &mut LspExtrasState,
    path: led_core::CanonPath,
    seq: u64,
    items: std::sync::Arc<Vec<led_driver_lsp_core::CodeActionSummary>>,
) {
    if items.is_empty() {
        return;
    }
    lsp_extras.code_actions = Some(CodeActionPickerState {
        path,
        seq,
        items,
        selected: 0,
        scroll: 0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use led_driver_lsp_core::CodeActionSummary;
    use led_state_buffer_edits::EditedBuffer;
    use led_state_tabs::{Cursor, Tab, TabId};
    use ropey::Rope;
    use std::sync::Arc;

    fn canon(s: &str) -> led_core::CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn action(title: &str, id: &str) -> CodeActionSummary {
        CodeActionSummary {
            title: Arc::<str>::from(title),
            kind: None,
            resolve_needed: false,
            action_id: Arc::<str>::from(id),
        }
    }

    fn seed_tab(cursor: Cursor, mark: Option<Cursor>) -> (Tabs, BufferEdits) {
        let mut tabs = Tabs::default();
        let id = TabId(1);
        tabs.open.push_back(Tab {
            id,
            path: canon("a.rs"),
            cursor,
            mark,
            ..Default::default()
        });
        tabs.active = Some(id);
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a.rs"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("a\nb\nc\n"))),
        );
        (tabs, edits)
    }

    #[test]
    fn activate_no_mark_uses_cursor_collapsed_range() {
        let (tabs, edits) = seed_tab(
            Cursor {
                line: 1,
                col: 2,
                preferred_col: 2,
            },
            None,
        );
        let mut lsp = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        activate(&mut lsp, &mut lsp_pending, &tabs, &edits);
        assert_eq!(lsp_pending.pending_code_action.len(), 1);
        let req = &lsp_pending.pending_code_action[0];
        assert_eq!(req.start_line, 1);
        assert_eq!(req.start_col, 2);
        assert_eq!(req.end_line, 1);
        assert_eq!(req.end_col, 2);
    }

    #[test]
    fn activate_with_mark_uses_sorted_range() {
        let (tabs, edits) = seed_tab(
            Cursor {
                line: 0,
                col: 0,
                preferred_col: 0,
            },
            Some(Cursor {
                line: 2,
                col: 1,
                preferred_col: 1,
            }),
        );
        let mut lsp = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        activate(&mut lsp, &mut lsp_pending, &tabs, &edits);
        let req = &lsp_pending.pending_code_action[0];
        assert_eq!(req.start_line, 0);
        assert_eq!(req.start_col, 0);
        assert_eq!(req.end_line, 2);
        assert_eq!(req.end_col, 1);
    }

    #[test]
    fn install_picker_sets_state() {
        let mut lsp = LspExtrasState::default();
        install_picker(
            &mut lsp,
            canon("a.rs"),
            3,
            Arc::new(vec![action("one", "id1"), action("two", "id2")]),
        );
        let p = lsp.code_actions.as_ref().expect("picker installed");
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.selected, 0);
        assert_eq!(p.seq, 3);
    }

    #[test]
    fn install_picker_empty_items_is_noop() {
        let mut lsp = LspExtrasState::default();
        install_picker(&mut lsp, canon("a.rs"), 1, Arc::new(vec![]));
        assert!(lsp.code_actions.is_none());
    }

    #[test]
    fn cursor_down_advances_selection_clamped_at_end() {
        let mut lsp = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        install_picker(
            &mut lsp,
            canon("a.rs"),
            1,
            Arc::new(vec![action("a", "a"), action("b", "b")]),
        );
        run_overlay_command(Command::CursorDown, &mut lsp, &mut lsp_pending);
        assert_eq!(lsp.code_actions.as_ref().unwrap().selected, 1);
        run_overlay_command(Command::CursorDown, &mut lsp, &mut lsp_pending);
        assert_eq!(lsp.code_actions.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn enter_commits_queues_select_and_dismisses() {
        let mut lsp = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        install_picker(
            &mut lsp,
            canon("a.rs"),
            1,
            Arc::new(vec![action("a", "id-a"), action("b", "id-b")]),
        );
        run_overlay_command(Command::CursorDown, &mut lsp, &mut lsp_pending);
        run_overlay_command(Command::InsertNewline, &mut lsp, &mut lsp_pending);
        assert!(lsp.code_actions.is_none());
        assert_eq!(lsp_pending.pending_code_action_select.len(), 1);
        let sel = &lsp_pending.pending_code_action_select[0];
        assert_eq!(sel.action.action_id.as_ref(), "id-b");
    }

    #[test]
    fn abort_dismisses_without_queuing_commit() {
        let mut lsp = LspExtrasState::default();
        let mut lsp_pending = led_state_lsp::LspPending::default();
        install_picker(
            &mut lsp,
            canon("a.rs"),
            1,
            Arc::new(vec![action("a", "a")]),
        );
        run_overlay_command(Command::Abort, &mut lsp, &mut lsp_pending);
        assert!(lsp.code_actions.is_none());
        assert!(lsp_pending.pending_code_action_select.is_empty());
    }
}
