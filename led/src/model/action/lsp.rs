use led_core::{Action, PanelSlot};
use led_state::{AppState, LspRequest, RenameState};

use super::super::mov;
use super::helpers::{close_group_on_move, reveal_active_buffer, word_under_cursor};

pub(super) fn handle_completion_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::MoveUp => {
            let lsp = state.lsp_mut();
            if let Some(ref mut comp) = lsp.completion {
                comp.selected = comp.selected.saturating_sub(1);
            }
            true
        }
        Action::MoveDown => {
            let lsp = state.lsp_mut();
            if let Some(ref mut comp) = lsp.completion {
                comp.selected = (comp.selected + 1).min(comp.items.len().saturating_sub(1));
            }
            true
        }
        Action::InsertNewline | Action::InsertTab => {
            // Accept completion
            let comp = state.lsp.completion.clone();
            if let Some(comp) = comp {
                let index = comp.selected;
                if let Some(item) = comp.items.get(index) {
                    if let Some(path) = state.active_buffer.clone() {
                        let buf = &state.buffers[&path];
                        let cursor_row = buf.cursor_row().0;
                        let cursor_col = buf.cursor_col().0;

                        // Build text edit: replace from prefix_start to current cursor
                        let te = led_lsp::TextEdit {
                            start_row: cursor_row,
                            start_col: comp.prefix_start_col,
                            end_row: cursor_row,
                            end_col: cursor_col,
                            new_text: item
                                .text_edit
                                .as_ref()
                                .map(|e| e.new_text.clone())
                                .unwrap_or_else(|| item.insert_text.clone()),
                        };

                        // Apply edit and move cursor to end of inserted text
                        let (old_lines, old_ver) = state
                            .buffers
                            .get(&path)
                            .map(|b| (b.doc().line_count(), b.version()))
                            .unwrap_or((0, led_core::DocVersion(0)));
                        let edit_row = te.start_row;
                        if let Some(buf) = state.buf_mut(&path) {
                            super::super::apply_text_edits(buf, &[te.clone()]);
                            let new_text = &te.new_text;
                            let newline_count = new_text.chars().filter(|c| *c == '\n').count();
                            let (r, c) = if newline_count == 0 {
                                (te.start_row, te.start_col + new_text.chars().count())
                            } else {
                                (
                                    te.start_row + newline_count,
                                    new_text
                                        .rsplit('\n')
                                        .next()
                                        .map(|l| l.chars().count())
                                        .unwrap_or(0),
                                )
                            };
                            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(c));
                            close_group_on_move(buf);
                        }
                        mov::shift_annotations(state, &path, edit_row, old_lines, old_ver);

                        // Apply additional edits (auto-imports etc.)
                        if !item.additional_edits.is_empty() {
                            let (old_lines, old_ver) = state
                                .buffers
                                .get(&path)
                                .map(|b| (b.doc().line_count(), b.version()))
                                .unwrap_or((0, led_core::DocVersion(0)));
                            let edit_row = item
                                .additional_edits
                                .iter()
                                .map(|e| e.start_row)
                                .min()
                                .unwrap_or(0);
                            if let Some(buf) = state.buf_mut(&path) {
                                super::super::apply_text_edits(buf, &item.additional_edits);
                                close_group_on_move(buf);
                            }
                            mov::shift_annotations(state, &path, edit_row, old_lines, old_ver);
                        }
                    }
                    // Request resolve for additional edits from server
                    state
                        .lsp_mut()
                        .pending_request
                        .set(Some(LspRequest::CompleteAccept { index }));
                }
                state.lsp_mut().completion = None;
            }
            true
        }
        Action::Abort => {
            state.lsp_mut().completion = None;
            true
        }
        // Printable chars / backspace: pass through to normal editing, then re-filter
        Action::InsertChar(_) | Action::DeleteBackward => false,
        _ => {
            // Any other action dismisses completion
            state.lsp_mut().completion = None;
            false
        }
    }
}

pub(super) fn handle_code_action_picker(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::MoveUp => {
            let lsp = state.lsp_mut();
            if let Some(ref mut picker) = lsp.code_actions {
                picker.selected = picker.selected.saturating_sub(1);
            }
            true
        }
        Action::MoveDown => {
            let lsp = state.lsp_mut();
            if let Some(ref mut picker) = lsp.code_actions {
                picker.selected = (picker.selected + 1).min(picker.actions.len().saturating_sub(1));
            }
            true
        }
        Action::InsertNewline => {
            // Accept selection
            let index = state
                .lsp
                .code_actions
                .as_ref()
                .map(|p| p.selected)
                .unwrap_or(0);
            state.lsp_mut().code_actions = None;
            state.focus = PanelSlot::Main;
            state
                .lsp_mut()
                .pending_request
                .set(Some(LspRequest::CodeActionSelect { index }));
            true
        }
        Action::Abort => {
            state.lsp_mut().code_actions = None;
            state.focus = PanelSlot::Main;
            true
        }
        _ => true, // Absorb all other actions while picker is open
    }
}

pub(super) fn handle_rename_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::InsertChar(ch) => {
            let lsp = state.lsp_mut();
            if let Some(ref mut rename) = lsp.rename {
                rename.input.insert(rename.cursor, *ch);
                rename.cursor += ch.len_utf8();
            }
            true
        }
        Action::DeleteBackward => {
            let lsp = state.lsp_mut();
            if let Some(ref mut rename) = lsp.rename {
                if rename.cursor > 0 {
                    let ch = rename.input[..rename.cursor]
                        .chars()
                        .last()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                    rename.cursor -= ch;
                    rename.input.remove(rename.cursor);
                }
            }
            true
        }
        Action::InsertNewline => {
            // Submit rename
            let new_name = state
                .lsp
                .rename
                .as_ref()
                .map(|r| r.input.clone())
                .unwrap_or_default();
            state.lsp_mut().rename = None;
            state.focus = PanelSlot::Main;
            if !new_name.is_empty() {
                state
                    .lsp_mut()
                    .pending_request
                    .set(Some(LspRequest::Rename { new_name }));
            }
            true
        }
        Action::Abort => {
            state.lsp_mut().rename = None;
            state.focus = PanelSlot::Main;
            true
        }
        _ => true, // Absorb all other actions while rename overlay is open
    }
}

pub(super) fn navigate_diagnostic(state: &mut AppState, forward: bool) {
    let cur_path = state
        .active_buffer
        .as_ref()
        .and_then(|path| state.buffers.get(path))
        .and_then(|b| b.path_buf().cloned());

    let (row, col) = state
        .active_buffer
        .as_ref()
        .and_then(|path| state.buffers.get(path))
        .map(|b| (b.cursor_row().0, b.cursor_col().0))
        .unwrap_or((0, 0));

    // Build a sorted list of all (path, diag) across the workspace.
    let mut all: Vec<(&std::path::PathBuf, &led_lsp::Diagnostic)> = Vec::new();
    for buf in state.buffers.values() {
        if let Some(path) = buf.path_buf() {
            for d in buf.status().diagnostics() {
                all.push((path, d));
            }
        }
    }
    all.sort_by(|a, b| {
        a.0.cmp(b.0)
            .then(a.1.start_row.cmp(&b.1.start_row))
            .then(a.1.start_col.cmp(&b.1.start_col))
    });

    if all.is_empty() {
        return;
    }

    // Find next/prev diagnostic across all files.
    let cur_key = cur_path.as_ref().map(|p| (p, row, col));
    let target = if forward {
        all.iter()
            .find(|(p, d)| {
                cur_key.map_or(true, |(cp, cr, cc)| {
                    (*p, d.start_row, d.start_col) > (cp, cr, cc)
                })
            })
            .or_else(|| all.first())
    } else {
        all.iter()
            .rev()
            .find(|(p, d)| {
                cur_key.map_or(true, |(cp, cr, cc)| {
                    (*p, d.start_row, d.start_col) < (cp, cr, cc)
                })
            })
            .or_else(|| all.last())
    };

    let Some(&(target_path, target_diag)) = target else {
        return;
    };
    let target_row = target_diag.start_row;
    let target_col = target_diag.start_col;
    let target_path = target_path.clone();

    // If the target is in the current buffer, just move the cursor.
    if cur_path.as_ref() == Some(&target_path) {
        let path = state.active_buffer.clone().unwrap();
        let dims = state.dims;
        if let Some(buf) = state.buf_mut(&path) {
            close_group_on_move(buf);
            buf.set_cursor(
                led_core::Row(target_row),
                led_core::Col(target_col),
                led_core::Col(target_col),
            );
            if let Some(dims) = dims {
                let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            }
        }
        return;
    }

    // Target is in a different file — check if it's already open.
    let canonical = std::fs::canonicalize(&target_path).unwrap_or_else(|_| target_path.clone());
    let existing = state
        .buffers
        .values()
        .find(|b| {
            b.path_buf().map_or(false, |p| {
                std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == canonical
            })
        })
        .and_then(|b| b.path_buf().cloned());
    if let Some(path) = existing {
        state.active_buffer = Some(path.clone());
        let half = state.dims.map_or(10, |d| d.buffer_height() / 2);
        if let Some(buf) = state.buf_mut(&path) {
            let r = target_row.min(buf.doc().line_count().saturating_sub(1));
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(target_col),
                led_core::Col(target_col),
            );
            buf.set_scroll(led_core::Row(r.saturating_sub(half)), led_core::SubLine(0));
        }
        reveal_active_buffer(state);
    } else {
        state.pending_open.set(Some(target_path.clone()));
        state.jump.pending_position = Some(led_state::JumpPosition {
            path: target_path,
            row: target_row,
            col: target_col,
            scroll_offset: 0,
        });
    }
}

pub(super) fn open_rename_overlay(state: &mut AppState) {
    if let Some(ref path) = state.active_buffer {
        if let Some(buf) = state.buffers.get(path) {
            let word = word_under_cursor(buf);
            let cursor = word.len();
            state.lsp_mut().rename = Some(RenameState {
                input: word,
                cursor,
            });
            state.focus = PanelSlot::Overlay;
        }
    }
}
