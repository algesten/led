use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Row, SubLine};
use led_lsp::LspIn;
use led_state::AppState;

use super::Mut;

pub fn lsp_of(lsp_in: &Stream<LspIn>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    // ── Navigate: common parent, branched into children ──

    let navigate_s: Stream<_> = lsp_in
        .filter_map(|ev| match ev {
            LspIn::Navigate { path, row, col } => Some((path, row, col)),
            _ => None,
        })
        .sample_combine(state);

    // Child 1: record current position in jump list
    let nav_jump_s = navigate_s
        .filter_map(|(_, s)| {
            let active = s.active_tab.as_ref()?;
            let buf = s.buffers.get(active)?;
            let p = buf.path()?;
            Some(Mut::JumpRecord(led_state::JumpPosition {
                path: p.clone(),
                row: buf.cursor_row(),
                col: buf.cursor_col(),
                scroll_offset: buf.scroll_row(),
            }))
        })
        .stream();

    // Child 2: if buffer exists, update cursor/scroll
    let nav_existing_s = navigate_s
        .filter(|((path, _, _), s)| s.buffers.contains_key(path))
        .filter_map(|((path, row, col), s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            let half = s.dims.map_or(10, |d| d.buffer_height() / 2);
            let r = (*row).min(buf.doc().line_count().saturating_sub(1));
            buf.set_cursor(Row(r), col, col);
            buf.set_scroll(Row(buf.cursor_row().0.saturating_sub(half)), SubLine(0));
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    // Child 3: if buffer does not exist, open it
    let nav_open_s = navigate_s
        .filter(|((path, _, _), s)| !s.buffers.contains_key(path))
        .map(|((path, _, _), _)| Mut::RequestOpen(path))
        .stream();

    // Child 4: if buffer does not exist, set pending cursor on tab
    let nav_pending_cursor_s = navigate_s
        .filter(|((path, _, _), s)| !s.buffers.contains_key(path))
        .map(|((path, row, col), s)| {
            let half = s.dims.map_or(10, |d| d.buffer_height() / 2);
            Mut::SetTabPendingCursor {
                path,
                row,
                col,
                scroll_row: Row(row.saturating_sub(half)),
            }
        })
        .stream();

    // Child 5: always activate the target buffer
    let nav_activate_s = navigate_s
        .map(|((path, _, _), _)| Mut::ActivateBuffer(path))
        .stream();

    // ── Edits: common parent, split apply vs format-done ──

    let edits_parent_s: Stream<_> = lsp_in
        .filter_map(|ev| match ev {
            LspIn::Edits { edits } => Some(edits),
            _ => None,
        })
        .sample_combine(state);

    // Child 1: always apply text edits
    let edits_apply_s = edits_parent_s
        .map(|(edits, _)| Mut::LspEdits { edits })
        .stream();

    // Child 2: format-done → apply save cleanup to active buffer
    let format_done_cleanup_s = edits_parent_s
        .filter(|(edits, s)| {
            edits.iter().all(|fe| fe.edits.is_empty()) && s.lsp.pending_save_after_format
        })
        .filter_map(|(_, s)| {
            let path = s.active_tab.clone()?;
            let mut buf = (**s.buffers.get(&path)?).clone();
            buf.apply_save_cleanup();
            buf.record_diag_save_point();
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    // Child 3: format-done → clear flag + trigger save
    let format_done_save_s = edits_parent_s
        .filter(|(edits, s)| {
            edits.iter().all(|fe| fe.edits.is_empty()) && s.lsp.pending_save_after_format
        })
        .map(|_| Mut::LspFormatDone)
        .stream();

    let completion_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Completion { .. }))
        .map(|ev| match ev {
            LspIn::Completion {
                items,
                prefix_start_col,
            } => Mut::LspCompletion {
                items,
                prefix_start_col,
            },
            _ => unreachable!(),
        })
        .stream();

    let code_actions_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::CodeActions { .. }))
        .map(|ev| match ev {
            LspIn::CodeActions { actions } => Mut::LspCodeActions { actions },
            _ => unreachable!(),
        })
        .stream();

    let diagnostics_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Diagnostics { .. }))
        .map(|ev| match ev {
            LspIn::Diagnostics {
                path,
                diagnostics,
                content_hash,
            } => Mut::LspDiagnostics {
                path,
                diagnostics,
                content_hash,
            },
            _ => unreachable!(),
        })
        .stream();

    let inlay_hints_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::InlayHints { .. }))
        .map(|ev| match ev {
            LspIn::InlayHints { path, hints } => Mut::LspInlayHints { path, hints },
            _ => unreachable!(),
        })
        .stream();

    let progress_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Progress { .. }))
        .map(|ev| match ev {
            LspIn::Progress {
                server_name,
                busy,
                detail,
            } => Mut::LspProgress {
                server_name,
                busy,
                detail,
            },
            _ => unreachable!(),
        })
        .stream();

    let error_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Error { .. }))
        .map(|ev| match ev {
            LspIn::Error { message } => Mut::Warn {
                key: "lsp".to_string(),
                message: format!("LSP: {}", message),
            },
            _ => unreachable!(),
        })
        .stream();

    let trigger_chars_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::TriggerChars { .. }))
        .map(|ev| match ev {
            LspIn::TriggerChars {
                extensions,
                triggers,
            } => Mut::LspTriggerChars {
                extensions,
                triggers,
            },
            _ => unreachable!(),
        })
        .stream();

    let muts: Stream<Mut> = Stream::new();
    nav_jump_s.forward(&muts);
    nav_existing_s.forward(&muts);
    nav_open_s.forward(&muts);
    nav_pending_cursor_s.forward(&muts);
    nav_activate_s.forward(&muts);
    edits_apply_s.forward(&muts);
    format_done_cleanup_s.forward(&muts);
    format_done_save_s.forward(&muts);
    completion_s.forward(&muts);
    code_actions_s.forward(&muts);
    diagnostics_s.forward(&muts);
    inlay_hints_s.forward(&muts);
    progress_s.forward(&muts);
    error_s.forward(&muts);
    trigger_chars_s.forward(&muts);
    muts
}
