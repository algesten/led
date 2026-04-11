use led_core::{Action, PanelSlot};
use led_state::{AppState, LspRequest};

use super::super::mov;
use super::helpers::close_group_on_move;

pub fn handle_completion_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::MoveUp => {
            let lsp = state.lsp_mut();
            if let Some(ref mut comp) = lsp.completion {
                comp.selected = comp.selected.saturating_sub(1);
                if comp.selected < comp.scroll_offset {
                    comp.scroll_offset = comp.selected;
                }
            }
            true
        }
        Action::MoveDown => {
            let lsp = state.lsp_mut();
            if let Some(ref mut comp) = lsp.completion {
                comp.selected = (comp.selected + 1).min(comp.items.len().saturating_sub(1));
                let max_visible = 10;
                if comp.selected >= comp.scroll_offset + max_visible {
                    comp.scroll_offset = comp.selected + 1 - max_visible;
                }
            }
            true
        }
        Action::InsertNewline | Action::InsertTab => {
            // Accept completion
            let comp = state.lsp.completion.clone();
            if let Some(comp) = comp {
                let index = comp.selected;
                if let Some(item) = comp.items.get(index) {
                    if let Some(path) = state.active_tab.clone() {
                        let buf = &state.buffers[&path];
                        let cursor_row = buf.cursor_row();
                        let cursor_col = buf.cursor_col();

                        // Build text edit: replace from prefix_start to current cursor
                        let te = led_lsp::TextEdit {
                            start_row: cursor_row,
                            start_col: led_core::Col(comp.prefix_start_col),
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
                                    led_core::Col(
                                        new_text
                                            .rsplit('\n')
                                            .next()
                                            .map(|l| l.chars().count())
                                            .unwrap_or(0),
                                    ),
                                )
                            };
                            buf.set_cursor(r, c, c);
                            close_group_on_move(buf);
                        }
                        mov::shift_annotations(state, &path, *edit_row, old_lines, old_ver);

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
                                .unwrap_or(led_core::Row(0));
                            if let Some(buf) = state.buf_mut(&path) {
                                super::super::apply_text_edits(buf, &item.additional_edits);
                                close_group_on_move(buf);
                            }
                            mov::shift_annotations(state, &path, *edit_row, old_lines, old_ver);
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

pub fn handle_code_action_picker(state: &mut AppState, action: &Action) -> bool {
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

pub fn handle_rename_action(state: &mut AppState, action: &Action) -> bool {
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
