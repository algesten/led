use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use led_core::{Action, BufferId, PanelSlot};
use led_state::{
    AppState, BufferState, Dimensions, EditKind, EntryKind, LspRequest, PreviewRequest,
    RenameState, SaveState,
};

use led_state::JumpPosition;

use super::{edit, file_search, find_file, jump, mov, search};

pub fn handle_action(state: &mut AppState, action: Action) {
    // Handle confirmation prompt for dirty buffer kill
    if state.confirm_kill {
        state.confirm_kill = false;
        state.alerts.warn = None;
        if matches!(action, Action::InsertChar('y' | 'Y')) {
            force_kill_buffer(state);
            return;
        }
        // Any other action: cancel and fall through to normal handling
        if matches!(action, Action::Abort) {
            return;
        }
    }

    // Filter mutating input while indent is in flight
    if let Some(id) = state.active_buffer {
        if let Some(buf) = state.buffers.get(&id) {
            if buf.pending_indent_row.is_some() && is_editing_action(&action) {
                return;
            }
        }
    }

    // Auto-promote preview if user edits in it
    if state.preview.buffer.is_some()
        && state.active_buffer == state.preview.buffer
        && is_editing_action(&action)
    {
        promote_preview_active(state);
    }

    // Intercept actions during LSP completion
    if state.lsp.completion.is_some() {
        if handle_completion_action(state, &action) {
            return;
        }
    }

    // Intercept actions during LSP code action picker
    if state.lsp.code_actions.is_some() {
        if handle_code_action_picker(state, &action) {
            return;
        }
    }

    // Intercept actions during LSP rename
    if state.lsp.rename.is_some() && state.focus == PanelSlot::Overlay {
        if handle_rename_action(state, &action) {
            return;
        }
    }

    // Intercept actions during file search
    if state.file_search.is_some() {
        if file_search::handle_file_search_action(state, &action) {
            return;
        }
    }

    // Intercept actions during find-file
    if state.find_file.is_some() {
        if find_file::handle_find_file_action(state, &action) {
            return;
        }
    }

    // Intercept actions during incremental search
    if let Some(id) = state.active_buffer {
        let in_search = state
            .buffers
            .get(&id)
            .map_or(false, |b| b.isearch.is_some());
        if in_search {
            if handle_isearch_action(state, &action) {
                return;
            }
        }
    }

    // Any action other than KillLine breaks kill accumulation
    if !matches!(action, Action::KillLine) {
        state.kill_ring.break_accumulation();
    }

    match action {
        // ── UI ──
        Action::ToggleSidePanel => {
            state.show_side_panel = !state.show_side_panel;
            if let Some(ref mut dims) = state.dims {
                dims.show_side_panel = state.show_side_panel;
            }
        }
        Action::ToggleFocus => {
            state.focus = match state.focus {
                PanelSlot::Main => PanelSlot::Side,
                PanelSlot::Side => PanelSlot::Main,
                other => other,
            };
        }
        Action::Quit => {
            state.quit = true;
        }
        Action::Suspend => {
            state.suspend = true;
        }

        // ── Resize ──
        Action::Resize(w, h) => {
            state.dims = Some(Dimensions::new(w, h, state.show_side_panel));
        }

        // ── Movement (routed by focus) ──
        Action::MoveUp
        | Action::MoveDown
        | Action::PageUp
        | Action::PageDown
        | Action::FileStart
        | Action::FileEnd => {
            if state.focus == PanelSlot::Side {
                handle_browser_nav(state, &action);
            } else {
                handle_editor_movement(state, &action);
            }
        }
        Action::MoveLeft => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::move_left(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::MoveRight => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::move_right(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::LineStart => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::line_start(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::LineEnd => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::line_end(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),

        // ── Browser ──
        Action::ExpandDir => handle_browser_expand(state),
        Action::CollapseDir => handle_browser_collapse(state),
        Action::CollapseAll => handle_browser_collapse_all(state),
        Action::OpenSelected => handle_browser_open(state),

        // ── Mark / Kill ring ──
        Action::SetMark => {
            with_buf(state, |buf, _dims| {
                buf.mark = Some((buf.cursor_row, buf.cursor_col));
            });
            state.alerts.info = Some("Mark set".into());
        }

        Action::Abort => with_buf(state, |buf, _dims| {
            buf.mark = None;
        }),

        // ── Editing ──
        Action::InsertChar(ch) => {
            with_buf(state, |buf, dims| {
                buf.mark = None;
                maybe_close_group(buf, EditKind::Insert, ch);
                let (doc, r, c, _) = edit::insert_char(buf, ch);
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
                buf.last_edit_kind = Some(EditKind::Insert);
                if buf.reindent_chars.contains(&ch) {
                    buf.pending_indent_row = Some(r);
                }
            });
            // Auto-trigger completion when no popup is showing
            if state.lsp.completion.is_none() {
                if let Some(id) = state.active_buffer {
                    if let Some(buf) = state.buffers.get(&id) {
                        if !buf.completion_triggers.is_empty() {
                            let line = buf.doc.line(buf.cursor_row);
                            let col = buf.cursor_col;
                            if col > 0 {
                                let prev = line.chars().nth(col - 1).unwrap_or(' ');
                                if prev.is_alphanumeric() || prev == '_' {
                                    state
                                        .lsp_mut()
                                        .pending_request
                                        .set(Some(LspRequest::Complete));
                                }
                            }
                        }
                    }
                }
            }
        }
        Action::InsertNewline => with_buf(state, |buf, _dims| {
            buf.mark = None;
            close_group_on_move(buf);
            let (doc, r, c, a) = edit::insert_newline(buf);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            buf.pending_indent_row = Some(r);
        }),
        Action::InsertTab => with_buf(state, |buf, _dims| {
            buf.mark = None;
            close_group_on_move(buf);
            buf.pending_indent_row = Some(buf.cursor_row);
            buf.pending_tab_fallback = true;
        }),
        Action::DeleteBackward => with_buf(state, |buf, dims| {
            buf.mark = None;
            if buf.last_edit_kind != Some(EditKind::Delete) {
                buf.doc = buf.doc.close_undo_group();
            }
            if let Some((doc, r, c, _)) = edit::delete_backward(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
                buf.last_edit_kind = Some(EditKind::Delete);
            }
        }),
        Action::DeleteForward => with_buf(state, |buf, dims| {
            buf.mark = None;
            if buf.last_edit_kind != Some(EditKind::Delete) {
                buf.doc = buf.doc.close_undo_group();
            }
            if let Some((doc, r, c, _)) = edit::delete_forward(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
                buf.last_edit_kind = Some(EditKind::Delete);
            }
        }),
        Action::KillLine => {
            let mut killed_text = None;
            if let (Some(dims), Some(id)) = (state.dims, state.active_buffer) {
                if let Some(buf) = state.buf_mut(id) {
                    let old_lines = buf.doc.line_count();
                    let edit_row = buf.cursor_row;
                    close_group_on_move(buf);
                    if let Some((doc, killed, r, c, a)) = edit::kill_line(buf) {
                        buf.doc = doc;
                        buf.cursor_row = r;
                        buf.cursor_col = c;
                        buf.cursor_col_affinity = a;
                        killed_text = Some(killed);
                    }
                    shift_highlights(buf, edit_row, old_lines);
                    let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                    buf.scroll_row = sr;
                    buf.scroll_sub_line = ssl;
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        buf.save_state = SaveState::Modified;
                    }
                    buf.last_used = Instant::now();
                }
            }
            if let Some(killed) = killed_text {
                state.kill_ring.accumulate(&killed);
            }
        }
        Action::KillRegion => {
            let mut killed_text = None;
            let mut no_region = false;
            if let (Some(dims), Some(id)) = (state.dims, state.active_buffer) {
                if let Some(buf) = state.buf_mut(id) {
                    let old_lines = buf.doc.line_count();
                    let edit_row = buf.cursor_row;
                    close_group_on_move(buf);
                    if let Some((doc, killed, r, c, a)) = edit::kill_region(buf) {
                        buf.doc = doc;
                        buf.cursor_row = r;
                        buf.cursor_col = c;
                        buf.cursor_col_affinity = a;
                        buf.mark = None;
                        killed_text = Some(killed);
                    } else {
                        buf.mark = None;
                        no_region = true;
                    }
                    shift_highlights(buf, edit_row, old_lines);
                    let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                    buf.scroll_row = sr;
                    buf.scroll_sub_line = ssl;
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        buf.save_state = SaveState::Modified;
                    }
                    buf.last_used = Instant::now();
                }
            } else {
                no_region = true;
            }
            if let Some(killed) = killed_text {
                state.kill_ring.set(killed);
            }
            if no_region {
                state.alerts.warn = Some("No region".into());
            }
        }
        Action::Yank => {
            state.kill_ring.pending_yank.set(());
        }

        // ── Undo / Redo ──
        Action::Undo => with_buf(state, |buf, dims| {
            close_group_on_move(buf);
            if let Some((doc, cursor)) = buf.doc.undo() {
                let row = doc.char_to_line(cursor);
                let col = cursor - doc.line_to_char(row);
                buf.doc = doc;
                buf.cursor_row = row;
                buf.cursor_col = col;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            }
        }),
        Action::Redo => with_buf(state, |buf, dims| {
            close_group_on_move(buf);
            if let Some((doc, cursor)) = buf.doc.redo() {
                let row = doc.char_to_line(cursor);
                let col = cursor - doc.line_to_char(row);
                buf.doc = doc;
                buf.cursor_row = row;
                buf.cursor_col = col;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            }
        }),

        // ── Save ──
        Action::Save => {
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buf_mut(id) {
                    close_group_on_move(buf);
                    buf.save_state = SaveState::Saving;
                    buf.last_used = Instant::now();
                }
            }
            // If we have an LSP server for this file, format first then save
            let has_lsp = state
                .active_buffer
                .and_then(|id| state.buffers.get(&id))
                .and_then(|b| b.path.as_ref())
                .is_some_and(|_| !state.lsp.server_name.is_empty());
            if has_lsp {
                state.lsp_mut().pending_save_after_format = true;
                state
                    .lsp_mut()
                    .pending_request
                    .set(Some(LspRequest::Format));
                state.alerts.info = Some("Formatting...".into());
            } else {
                state.save_request.set(());
            }
        }

        Action::SaveAll => {
            let dirty_ids: Vec<_> = state
                .buffers
                .values()
                .filter(|b| b.doc.dirty() && b.path.is_some())
                .map(|b| b.id)
                .collect();
            for id in &dirty_ids {
                if let Some(buf) = state.buf_mut(*id) {
                    close_group_on_move(buf);
                    buf.save_state = SaveState::Saving;
                }
            }
            if !dirty_ids.is_empty() {
                state.save_all_request.set(());
            }
        }

        Action::SaveNoFormat => {
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buf_mut(id) {
                    close_group_on_move(buf);
                    buf.save_state = SaveState::Saving;
                    buf.last_used = Instant::now();
                }
            }
            state.save_request.set(());
        }

        // ── Tabs ──
        Action::NextTab => cycle_tab(state, 1),
        Action::PrevTab => cycle_tab(state, -1),
        Action::KillBuffer => kill_buffer(state),

        // ── Search ──
        Action::InBufferSearch => with_buf(state, |buf, _dims| {
            search::start_search(buf);
        }),

        // ── Jump list ──
        Action::JumpBack => jump::jump_back(state),
        Action::JumpForward => jump::jump_forward(state),

        // ── Bracket matching ──
        Action::MatchBracket => with_buf(state, |buf, dims| {
            if let Some((row, col)) = buf.matching_bracket {
                buf.cursor_row = row;
                buf.cursor_col = col;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
                close_group_on_move(buf);
            }
        }),

        // ── Find file / Save as ──
        Action::FindFile => find_file::activate(state),
        Action::SaveAs => find_file::activate_save_as(state),

        // ── File search ──
        Action::OpenFileSearch => file_search::activate(state),

        // ── Sort imports ──
        Action::SortImports => {
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buffers.get(&id) {
                    if let Some(path) = &buf.path {
                        if let Some(ss) =
                            led_syntax::SyntaxState::from_path_and_doc(path, &*buf.doc)
                        {
                            let import_items = ss.imports(&*buf.doc);
                            if let Some((start_byte, end_byte, replacement)) =
                                led_syntax::import::sort_imports_text(&*buf.doc, &import_items)
                            {
                                let start_char = buf.doc.byte_to_char(start_byte);
                                let end_char = buf.doc.byte_to_char(end_byte);
                                let buf = state.buf_mut(id).unwrap();
                                close_group_on_move(buf);
                                let doc = buf.doc.remove(start_char, end_char);
                                let doc = doc.insert(start_char, &replacement);
                                buf.doc = doc;
                                buf.last_used = Instant::now();
                                state.alerts.info = Some("Imports sorted".into());
                            } else {
                                state.alerts.info = Some("Imports already sorted".into());
                            }
                        }
                    }
                }
            }
        }

        // ── LSP ──
        Action::LspGotoDefinition => {
            state
                .lsp_mut()
                .pending_request
                .set(Some(LspRequest::GotoDefinition));
        }
        Action::LspFormat => {
            state
                .lsp_mut()
                .pending_request
                .set(Some(LspRequest::Format));
        }
        Action::LspCodeAction => {
            state
                .lsp_mut()
                .pending_request
                .set(Some(LspRequest::CodeAction));
        }
        Action::LspRename => {
            // Open rename overlay with word under cursor
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buffers.get(&id) {
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
        Action::LspNextDiagnostic => {
            navigate_diagnostic(state, true);
        }
        Action::LspPrevDiagnostic => {
            navigate_diagnostic(state, false);
        }
        Action::LspToggleInlayHints => {
            let lsp = state.lsp_mut();
            lsp.inlay_hints_enabled = !lsp.inlay_hints_enabled;
            if !lsp.inlay_hints_enabled {
                lsp.inlay_hints.clear();
            }
        }

        _ => {}
    }
}

/// Extract the word under the cursor.
fn word_under_cursor(buf: &BufferState) -> String {
    let line = buf.doc.line(buf.cursor_row);
    let chars: Vec<char> = line.chars().collect();
    let col = buf.cursor_col;
    if col >= chars.len() {
        return String::new();
    }
    let mut start = col;
    while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
        end += 1;
    }
    chars[start..end].iter().collect()
}

/// Navigate to the next or previous diagnostic in the current file.
fn navigate_diagnostic(state: &mut AppState, forward: bool) {
    let Some(id) = state.active_buffer else {
        return;
    };
    let Some(buf) = state.buffers.get(&id) else {
        return;
    };
    let Some(ref path) = buf.path else { return };
    let Some(diags) = state.lsp.diagnostics.get(path) else {
        return;
    };
    if diags.is_empty() {
        return;
    }

    let row = buf.cursor_row;
    let col = buf.cursor_col;

    let target = if forward {
        diags
            .iter()
            .find(|d| (d.start_row, d.start_col) > (row, col))
            .or_else(|| diags.first())
    } else {
        diags
            .iter()
            .rev()
            .find(|d| (d.start_row, d.start_col) < (row, col))
            .or_else(|| diags.last())
    };

    if let Some(d) = target {
        let target_row = d.start_row;
        let target_col = d.start_col;
        if let Some(buf) = state.buf_mut(id) {
            super::action::close_group_on_move(buf);
            buf.cursor_row = target_row;
            buf.cursor_col = target_col;
            buf.cursor_col_affinity = target_col;
        }
    }
}

// ── LSP completion interception ──

fn handle_completion_action(state: &mut AppState, action: &Action) -> bool {
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
                    if let Some(id) = state.active_buffer {
                        let buf = &state.buffers[&id];
                        let cursor_row = buf.cursor_row;
                        let cursor_col = buf.cursor_col;

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
                        if let Some(buf) = state.buf_mut(id) {
                            super::apply_text_edits(buf, &[te.clone()]);
                            let new_text = &te.new_text;
                            let newline_count = new_text.chars().filter(|c| *c == '\n').count();
                            if newline_count == 0 {
                                buf.cursor_row = te.start_row;
                                buf.cursor_col = te.start_col + new_text.chars().count();
                            } else {
                                buf.cursor_row = te.start_row + newline_count;
                                buf.cursor_col = new_text
                                    .rsplit('\n')
                                    .next()
                                    .map(|l| l.chars().count())
                                    .unwrap_or(0);
                            }
                            buf.cursor_col_affinity = buf.cursor_col;
                        }

                        // Apply additional edits (auto-imports etc.)
                        if !item.additional_edits.is_empty() {
                            if let Some(buf) = state.buf_mut(id) {
                                super::apply_text_edits(buf, &item.additional_edits);
                            }
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

// ── LSP code action picker interception ──

fn handle_code_action_picker(state: &mut AppState, action: &Action) -> bool {
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

// ── LSP rename interception ──

fn handle_rename_action(state: &mut AppState, action: &Action) -> bool {
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

/// Handle action while in incremental search mode.
/// Returns true if the action was consumed (don't fall through to normal handling).
fn handle_isearch_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::InsertChar(c) => {
            with_buf(state, |buf, _dims| {
                buf.isearch.as_mut().unwrap().query.push(*c);
                search::update_search(buf);
            });
            true
        }
        Action::DeleteBackward => {
            with_buf(state, |buf, _dims| {
                let empty = {
                    let is = buf.isearch.as_mut().unwrap();
                    is.query.pop();
                    is.query.is_empty()
                };
                if empty {
                    let is = buf.isearch.as_ref().unwrap();
                    buf.cursor_row = is.origin.0;
                    buf.cursor_col = is.origin.1;
                    let is = buf.isearch.as_mut().unwrap();
                    is.matches.clear();
                    is.match_idx = None;
                    is.failed = false;
                } else {
                    search::update_search(buf);
                }
            });
            true
        }
        Action::InBufferSearch => {
            with_buf(state, |buf, _dims| {
                search::search_next(buf);
            });
            true
        }
        Action::Abort => {
            with_buf(state, |buf, _dims| {
                search::search_cancel(buf);
            });
            true
        }
        Action::InsertNewline => {
            // Record jump from search origin before accepting
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buffers.get(&id) {
                    if let (Some(is), Some(path)) = (&buf.isearch, &buf.path) {
                        let cursor_moved =
                            buf.cursor_row != is.origin.0 || buf.cursor_col != is.origin.1;
                        if cursor_moved {
                            let pos = JumpPosition {
                                path: path.clone(),
                                row: is.origin.0,
                                col: is.origin.1,
                                scroll_offset: is.origin_scroll,
                            };
                            jump::record_jump(state, pos);
                        }
                    }
                }
            }
            with_buf(state, |buf, _dims| {
                search::search_accept(buf);
            });
            true
        }
        // Movement keys: accept search, then fall through to normal handling
        Action::MoveUp
        | Action::MoveDown
        | Action::MoveLeft
        | Action::MoveRight
        | Action::LineStart
        | Action::LineEnd
        | Action::PageUp
        | Action::PageDown
        | Action::FileStart
        | Action::FileEnd => {
            with_buf(state, |buf, _dims| {
                search::search_accept(buf);
            });
            false
        }
        // Pass through without exiting search
        Action::Resize(..) | Action::Quit | Action::Suspend => false,
        // Everything else: accept search and fall through
        _ => {
            with_buf(state, |buf, _dims| {
                search::search_accept(buf);
            });
            false
        }
    }
}

fn cycle_tab(state: &mut AppState, direction: i32) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    let mut tabs: Vec<(BufferId, usize)> = state
        .buffers
        .iter()
        .filter(|(_, buf)| !buf.is_preview)
        .map(|(id, buf)| (*id, buf.tab_order))
        .collect();
    tabs.sort_by_key(|&(_, order)| order);

    let Some(pos) = tabs.iter().position(|&(id, _)| id == active_id) else {
        return;
    };
    let len = tabs.len() as i32;
    let next = ((pos as i32 + direction).rem_euclid(len)) as usize;
    state.active_buffer = Some(tabs[next].0);
    reveal_active_buffer(state);
}

fn kill_buffer(state: &mut AppState) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    let Some(buf) = state.buffers.get(&active_id) else {
        return;
    };

    if buf.doc.dirty() {
        let filename = buf
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("[{}]", active_id.0));
        state.confirm_kill = true;
        state.alerts.warn = Some(format!("Buffer {filename} modified; kill anyway? (y or n)"));
        return;
    }

    do_kill_buffer(state, active_id);
}

fn force_kill_buffer(state: &mut AppState) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    do_kill_buffer(state, active_id);
}

fn do_kill_buffer(state: &mut AppState, id: BufferId) {
    if state.preview.buffer == Some(id) {
        close_preview(state);
        return;
    }

    let Some(buf) = state.buffers.get(&id) else {
        return;
    };

    let filename = buf
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let killed_order = buf.tab_order;

    // Find next buffer to activate (next by tab_order, wrapping)
    let mut tabs: Vec<(BufferId, usize)> = state
        .buffers
        .iter()
        .filter(|(bid, _)| *bid != &id)
        .map(|(bid, buf)| (*bid, buf.tab_order))
        .collect();
    tabs.sort_by_key(|&(_, order)| order);

    let next_active = tabs
        .iter()
        .find(|&&(_, order)| order > killed_order)
        .or_else(|| tabs.last())
        .map(|&(bid, _)| bid);

    state.buffers_mut().remove(&id);
    state.active_buffer = next_active;
    reveal_active_buffer(state);
    renumber_tabs(state);

    if state.buffers.is_empty() {
        state.focus = PanelSlot::Side;
    }

    state.alerts.info = Some(format!("Killed {filename}"));
}

// ── Editor movement (focus = Main) ──

fn handle_editor_movement(state: &mut AppState, action: &Action) {
    match action {
        Action::MoveUp => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::move_up(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::MoveDown => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::move_down(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::PageUp => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::page_up(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::PageDown => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::page_down(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::FileStart => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::file_start();
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::FileEnd => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::file_end(&*buf.doc);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        _ => {}
    }
}

// ── Browser navigation (focus = Side) ──

fn handle_browser_nav(state: &mut AppState, action: &Action) {
    let len = state.browser.entries.len();
    if len == 0 {
        return;
    }
    let height = state.dims.map_or(20, |d| d.buffer_height());
    let selected = state.browser.selected;
    let scroll_offset = state.browser.scroll_offset;
    let b = state.browser_mut();
    match action {
        Action::MoveUp => {
            b.selected = selected.saturating_sub(1);
        }
        Action::MoveDown => {
            b.selected = (selected + 1).min(len - 1);
        }
        Action::PageUp => {
            b.selected = selected.saturating_sub(height);
        }
        Action::PageDown => {
            b.selected = (selected + height).min(len - 1);
        }
        Action::FileStart => {
            b.selected = 0;
        }
        Action::FileEnd => {
            b.selected = len - 1;
        }
        _ => {}
    }
    // Keep selection visible
    if b.selected < scroll_offset {
        b.scroll_offset = b.selected;
    } else if b.selected >= scroll_offset + height {
        b.scroll_offset = b.selected + 1 - height;
    }

    // Emit preview for selected entry
    if let Some(entry) = state.browser.entries.get(state.browser.selected) {
        match &entry.kind {
            EntryKind::File => {
                state.preview.pending.set(Some(PreviewRequest {
                    path: entry.path.clone(),
                    row: 0,
                    col: 0,
                }));
            }
            EntryKind::Directory { .. } => {
                close_preview(state);
            }
        }
    }
}

fn handle_browser_expand(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    if !matches!(entry.kind, EntryKind::Directory { expanded: false }) {
        return;
    }
    let path = entry.path.clone();
    let has_contents = state.browser.dir_contents.contains_key(&path);
    let b = state.browser_mut();
    b.expanded_dirs.insert(path.clone());
    if has_contents {
        b.rebuild_entries();
    }
    // Always request a fresh listing so changes made while collapsed become visible.
    state.pending_lists.set(vec![path]);
}

fn handle_browser_collapse(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    let collapse_path = match &entry.kind {
        EntryKind::Directory { expanded: true } => entry.path.clone(),
        _ => match entry.path.parent() {
            Some(parent) if state.browser.expanded_dirs.contains(parent) => parent.to_path_buf(),
            _ => return,
        },
    };
    let b = state.browser_mut();
    b.expanded_dirs.remove(&collapse_path);
    b.rebuild_entries();
    if let Some(pos) = b.entries.iter().position(|e| e.path == collapse_path) {
        b.selected = pos;
    }
}

fn handle_browser_collapse_all(state: &mut AppState) {
    let b = state.browser_mut();
    b.expanded_dirs.clear();
    b.rebuild_entries();
    b.selected = 0;
    b.scroll_offset = 0;
}

fn handle_browser_open(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected).cloned() else {
        return;
    };
    match &entry.kind {
        EntryKind::File => {
            if promote_preview(state, &entry.path) {
                state.focus = PanelSlot::Main;
                return;
            }
            // Clear pending_preview so the preview stream doesn't race
            state.preview.pending.set(None);
            close_preview(state);
            state.pending_open.set(Some(entry.path.clone()));
            state.focus = PanelSlot::Main;
        }
        EntryKind::Directory { expanded } => {
            if *expanded {
                handle_browser_collapse(state);
            } else {
                handle_browser_expand(state);
            }
        }
    }
}

/// Run `f` on the active buffer, then ensure cursor stays visible.
fn with_buf(state: &mut AppState, f: impl FnOnce(&mut BufferState, &Dimensions)) {
    let dims = match state.dims {
        Some(d) => d,
        None => return,
    };
    if let Some(id) = state.active_buffer {
        if let Some(buf) = state.buf_mut(id) {
            let old_lines = buf.doc.line_count();
            let edit_row = buf.cursor_row;
            f(buf, &dims);
            shift_highlights(buf, edit_row, old_lines);
            let (sr, ssl) = mov::adjust_scroll(buf, &dims);
            buf.scroll_row = sr;
            buf.scroll_sub_line = ssl;
            buf.matching_bracket = led_state::BracketPair::find_match(
                &buf.bracket_pairs,
                buf.cursor_row,
                buf.cursor_col,
            );
            // Track save state: transition to Modified when doc becomes dirty
            if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                buf.save_state = SaveState::Modified;
            }
            buf.last_used = Instant::now();
        }
    }
}

/// Adjust cached highlight line numbers when lines are inserted or removed.
/// Pure coordinate shift — the driver's full recompute replaces these within
/// one frame.
fn shift_highlights(buf: &mut BufferState, edit_row: usize, old_line_count: usize) {
    let new_line_count = buf.doc.line_count();
    if new_line_count == old_line_count {
        return;
    }
    let delta = new_line_count as isize - old_line_count as isize;
    let shifted: Vec<_> = buf
        .syntax_highlights
        .iter()
        .filter_map(|(line, span)| {
            if *line <= edit_row {
                Some((*line, span.clone()))
            } else {
                let new_line = (*line as isize + delta) as usize;
                if new_line < new_line_count {
                    Some((new_line, span.clone()))
                } else {
                    None
                }
            }
        })
        .collect();
    buf.syntax_highlights = Rc::new(shifted);
}

/// Close undo group and clear edit kind tracking.
pub(super) fn close_group_on_move(buf: &mut BufferState) {
    if buf.last_edit_kind.is_some() {
        buf.doc = buf.doc.close_undo_group();
        buf.last_edit_kind = None;
    }
}

/// Renumber tab_order to be contiguous 0..n, preserving relative order.
pub(super) fn renumber_tabs(state: &mut AppState) {
    let mut ordered: Vec<BufferId> = state.buffers.keys().copied().collect();
    ordered.sort_by_key(|bid| state.buffers[bid].tab_order);
    for (i, bid) in ordered.into_iter().enumerate() {
        state.buf_mut(bid).unwrap().tab_order = i;
    }
}

pub(super) fn reveal_active_buffer(state: &mut AppState) {
    let path = state
        .active_buffer
        .and_then(|id| state.buffers.get(&id))
        .and_then(|b| b.path.clone());
    let Some(path) = path else { return };
    // Canonicalize to match browser.root (which is canonicalized by the workspace driver)
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    let new_dirs = state.browser_mut().reveal(&path);
    if !new_dirs.is_empty() {
        state.pending_lists.set(new_dirs);
    }
    browser_scroll_to_selected(state);
}

pub(super) fn browser_scroll_to_selected(state: &mut AppState) {
    let height = state.dims.map_or(20, |d| d.buffer_height());
    let sel = state.browser.selected;
    let scroll_offset = state.browser.scroll_offset;
    if sel < scroll_offset {
        state.browser_mut().scroll_offset = sel;
    } else if sel >= scroll_offset + height {
        state.browser_mut().scroll_offset = sel + 1 - height;
    }
}

/// Close undo group if the edit kind changes or on word boundary (whitespace after non-whitespace).
fn maybe_close_group(buf: &mut BufferState, kind: EditKind, ch: char) {
    if buf.last_edit_kind != Some(kind) {
        buf.doc = buf.doc.close_undo_group();
    } else if kind == EditKind::Insert {
        // Word boundary: whitespace after non-whitespace
        if ch.is_whitespace() {
            let line = buf.doc.line(buf.cursor_row);
            let prev = line.chars().nth(buf.cursor_col.saturating_sub(1));
            if let Some(p) = prev {
                if !p.is_whitespace() {
                    buf.doc = buf.doc.close_undo_group();
                }
            }
        }
    }
}

// ── Preview helpers ──

pub(super) fn close_preview(state: &mut AppState) {
    if let Some(preview_id) = state.preview.buffer.take() {
        state.buffers_mut().remove(&preview_id);
        state.notify_hash_to_buffer.retain(|_, v| *v != preview_id);
        renumber_tabs(state);
    }
    if let Some(restore_id) = state.preview.pre_preview_buffer.take() {
        if state.buffers.contains_key(&restore_id) {
            state.active_buffer = Some(restore_id);
            // Only reveal in the browser when focus is on the editor.
            // When browsing the side panel, revealing would jump the
            // browser selection away from where the user is navigating.
            if state.focus == PanelSlot::Main {
                reveal_active_buffer(state);
            }
        }
    }
    if state.buffers.is_empty() {
        state.focus = PanelSlot::Side;
    }
}

pub(super) fn promote_preview(state: &mut AppState, path: &Path) -> bool {
    let Some(preview_id) = state.preview.buffer else {
        return false;
    };
    let matches = state
        .buffers
        .get(&preview_id)
        .and_then(|b| b.path.as_ref())
        .map_or(false, |p| p == path);
    if !matches {
        return false;
    }
    if let Some(buf) = state.buf_mut(preview_id) {
        buf.is_preview = false;
    }
    state.preview.buffer = None;
    state.preview.pre_preview_buffer = None;
    true
}

fn promote_preview_active(state: &mut AppState) {
    if let Some(preview_id) = state.preview.buffer.take() {
        if let Some(buf) = state.buf_mut(preview_id) {
            buf.is_preview = false;
        }
        state.preview.pre_preview_buffer = None;
    }
}

fn is_editing_action(action: &Action) -> bool {
    matches!(
        action,
        Action::InsertChar(_)
            | Action::InsertNewline
            | Action::DeleteBackward
            | Action::DeleteForward
            | Action::InsertTab
            | Action::KillLine
            | Action::KillRegion
            | Action::Yank
            | Action::Undo
            | Action::Redo
            | Action::SortImports
    )
}

pub(super) fn evict_one_buffer(state: &mut AppState) {
    let victim = state
        .buffers
        .values()
        .filter(|b| !b.is_preview)
        .filter(|b| Some(b.id) != state.active_buffer)
        .filter(|b| !b.doc.dirty())
        .min_by_key(|b| b.last_used)
        .map(|b| b.id);
    if let Some(id) = victim {
        state.buffers_mut().remove(&id);
        state.notify_hash_to_buffer.retain(|_, v| *v != id);
        renumber_tabs(state);
    }
}
