mod browser;
mod editor;
mod helpers;
mod isearch;
mod lsp;
mod preview;
mod tabs;

use std::time::Instant;

use led_core::{Action, PanelSlot};
use led_state::{AppState, Dimensions, EditKind, LspRequest, SaveState};

use super::{edit, file_search, find_file, jump, mov, search};
use helpers::{is_editing_action, maybe_close_group, should_record, with_buf};
use preview::promote_preview_active;

// Re-export items used by other modules in the crate.
pub(super) use helpers::{
    browser_scroll_to_selected, close_group_on_move, renumber_tabs, reveal_active_buffer,
};
pub(super) use preview::{close_preview, evict_one_buffer, promote_preview};

pub fn handle_action(state: &mut AppState, action: Action) -> bool {
    // ── Keyboard macro recording ──
    if state.kbd_macro.recording {
        match &action {
            Action::KbdMacroEnd => {
                state.kbd_macro.recording = false;
                state.kbd_macro.last = Some(std::mem::take(&mut state.kbd_macro.current));
                state.alerts.info = Some("Keyboard macro defined".into());
                return true;
            }
            Action::KbdMacroStart => {
                state.kbd_macro.current.clear();
                return true;
            }
            _ => {
                if should_record(&action) {
                    state.kbd_macro.current.push(action.clone());
                }
                // fall through to execute normally
            }
        }
    }

    // Handle confirmation prompt for dirty buffer kill
    if state.confirm_kill {
        state.confirm_kill = false;
        state.alerts.warn = None;
        if matches!(action, Action::InsertChar('y' | 'Y')) {
            tabs::force_kill_buffer(state);
            return true;
        }
        // Any other action: cancel and fall through to normal handling
        if matches!(action, Action::Abort) {
            return true;
        }
    }

    // Filter mutating input while indent is in flight
    if let Some(id) = state.active_buffer {
        if let Some(buf) = state.buffers.get(&id) {
            if buf.pending_indent_row.is_some() && is_editing_action(&action) {
                return true;
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
        if lsp::handle_completion_action(state, &action) {
            return true;
        }
    }

    // Intercept actions during LSP code action picker
    if state.lsp.code_actions.is_some() {
        if lsp::handle_code_action_picker(state, &action) {
            return true;
        }
    }

    // Intercept actions during LSP rename
    if state.lsp.rename.is_some() && state.focus == PanelSlot::Overlay {
        if lsp::handle_rename_action(state, &action) {
            return true;
        }
    }

    // Intercept actions during file search
    if state.file_search.is_some() {
        if file_search::handle_file_search_action(state, &action) {
            return true;
        }
    }

    // Intercept actions during find-file
    if state.find_file.is_some() {
        if find_file::handle_find_file_action(state, &action) {
            return true;
        }
    }

    // Intercept actions during incremental search
    if let Some(id) = state.active_buffer {
        let in_search = state
            .buffers
            .get(&id)
            .map_or(false, |b| b.isearch.is_some());
        if in_search {
            if isearch::handle_isearch_action(state, &action) {
                return true;
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
                browser::handle_browser_nav(state, &action);
            } else {
                editor::handle_editor_movement(state, &action);
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
        Action::ExpandDir => browser::handle_browser_expand(state),
        Action::CollapseDir => browser::handle_browser_collapse(state),
        Action::CollapseAll => browser::handle_browser_collapse_all(state),
        Action::OpenSelected => browser::handle_browser_open(state),

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
                let (old_lines, edit_row, old_ver) = state
                    .buffers
                    .get(&id)
                    .map(|b| (b.doc.line_count(), b.cursor_row, b.doc.version()))
                    .unwrap_or((0, 0, 0));
                if let Some(buf) = state.buf_mut(id) {
                    close_group_on_move(buf);
                    if let Some((doc, killed, r, c, a)) = edit::kill_line(buf) {
                        buf.doc = doc;
                        buf.cursor_row = r;
                        buf.cursor_col = c;
                        buf.cursor_col_affinity = a;
                        killed_text = Some(killed);
                    }
                    let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                    buf.scroll_row = sr;
                    buf.scroll_sub_line = ssl;
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        buf.save_state = SaveState::Modified;
                    }
                    buf.last_used = Instant::now();
                }
                mov::shift_annotations(state, id, edit_row, old_lines, old_ver);
            }
            if let Some(killed) = killed_text {
                state.kill_ring.accumulate(&killed);
            }
        }
        Action::KillRegion => {
            let mut killed_text = None;
            let mut no_region = false;
            if let (Some(dims), Some(id)) = (state.dims, state.active_buffer) {
                let (old_lines, edit_row, old_ver) = state
                    .buffers
                    .get(&id)
                    .map(|b| (b.doc.line_count(), b.cursor_row, b.doc.version()))
                    .unwrap_or((0, 0, 0));
                if let Some(buf) = state.buf_mut(id) {
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
                    let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                    buf.scroll_row = sr;
                    buf.scroll_sub_line = ssl;
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        buf.save_state = SaveState::Modified;
                    }
                    buf.last_used = Instant::now();
                }
                mov::shift_annotations(state, id, edit_row, old_lines, old_ver);
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
        Action::NextTab => tabs::cycle_tab(state, 1),
        Action::PrevTab => tabs::cycle_tab(state, -1),
        Action::KillBuffer => tabs::kill_buffer(state),

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
        Action::LspRename => lsp::open_rename_overlay(state),
        Action::LspNextDiagnostic => {
            lsp::navigate_diagnostic(state, true);
        }
        Action::LspPrevDiagnostic => {
            lsp::navigate_diagnostic(state, false);
        }
        Action::LspToggleInlayHints => {
            let lsp = state.lsp_mut();
            lsp.inlay_hints_enabled = !lsp.inlay_hints_enabled;
            if !lsp.inlay_hints_enabled {
                lsp.inlay_hints.clear();
            }
        }

        // ── Macros ──
        Action::KbdMacroStart => {
            state.kbd_macro.recording = true;
            state.kbd_macro.current.clear();
            state.alerts.info = Some("Defining kbd macro...".into());
        }
        Action::KbdMacroEnd => {
            state.alerts.warn = Some("Not defining kbd macro".into());
        }
        Action::KbdMacroExecute => {
            if state.kbd_macro.playback_depth >= 100 {
                state.alerts.warn = Some("Keyboard macro recursion limit".into());
                return false;
            }
            let Some(actions) = state.kbd_macro.last.clone() else {
                state.alerts.warn = Some("No kbd macro defined".into());
                return false;
            };
            let count = state.kbd_macro.execute_count.take().unwrap_or(1);
            let iterations = if count == 0 { usize::MAX } else { count };
            state.kbd_macro.playback_depth += 1;
            let mut ok = true;
            for _ in 0..iterations {
                for a in &actions {
                    if !handle_action(state, a.clone()) {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
            state.kbd_macro.playback_depth -= 1;
            if !ok {
                return false;
            }
        }

        _ => {}
    }
    true
}
