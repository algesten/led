use led_core::{Action, BufferId, PanelSlot};
use led_state::{AppState, BufferState, Dimensions, EditKind, EntryKind, SaveState};

use super::{edit, mov};

pub fn handle_action(state: &mut AppState, action: Action) {
    // Handle confirmation prompt for dirty buffer kill
    if state.confirm_kill {
        state.confirm_kill = false;
        state.warn = None;
        if matches!(action, Action::InsertChar('y' | 'Y')) {
            force_kill_buffer(state);
            return;
        }
        // Any other action: cancel and fall through to normal handling
        if matches!(action, Action::Abort) {
            return;
        }
    }

    // Any action other than KillLine breaks kill accumulation
    if !matches!(action, Action::KillLine) {
        state.kill_accumulator = None;
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
            state.info = Some("Mark set".into());
        }

        Action::Abort => with_buf(state, |buf, _dims| {
            buf.mark = None;
        }),

        // ── Editing ──
        Action::InsertChar(ch) => with_buf(state, |buf, dims| {
            buf.mark = None;
            maybe_close_group(buf, EditKind::Insert, ch);
            let (doc, r, c, _) = edit::insert_char(buf, ch);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            buf.last_edit_kind = Some(EditKind::Insert);
        }),
        Action::InsertNewline => with_buf(state, |buf, dims| {
            buf.mark = None;
            close_group_on_move(buf);
            let (doc, r, c, _) = edit::insert_newline(buf);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
        }),
        Action::InsertTab => with_buf(state, |buf, dims| {
            buf.mark = None;
            maybe_close_group(buf, EditKind::Insert, ' ');
            let (doc, r, c, _) = edit::insert_tab(buf, dims);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            buf.last_edit_kind = Some(EditKind::Insert);
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
            if let (Some(dims), Some(id)) = (state.dims, state.active_buffer) {
                if let Some(buf) = state.buffers.get_mut(&id) {
                    close_group_on_move(buf);
                    if let Some((doc, killed, r, c, a)) = edit::kill_line(buf) {
                        buf.doc = doc;
                        buf.cursor_row = r;
                        buf.cursor_col = c;
                        buf.cursor_col_affinity = a;
                        state
                            .kill_accumulator
                            .get_or_insert_with(String::new)
                            .push_str(&killed);
                        state.kill_ring = state.kill_accumulator.clone().unwrap();
                    }
                    let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                    buf.scroll_row = sr;
                    buf.scroll_sub_line = ssl;
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        buf.save_state = SaveState::Modified;
                    }
                }
            }
        }
        Action::KillRegion => {
            if let (Some(dims), Some(id)) = (state.dims, state.active_buffer) {
                if let Some(buf) = state.buffers.get_mut(&id) {
                    close_group_on_move(buf);
                    if let Some((doc, killed, r, c, a)) = edit::kill_region(buf) {
                        buf.doc = doc;
                        buf.cursor_row = r;
                        buf.cursor_col = c;
                        buf.cursor_col_affinity = a;
                        buf.mark = None;
                        state.kill_ring = killed;
                    } else {
                        buf.mark = None;
                        state.warn = Some("No region".into());
                    }
                    let (sr, ssl) = mov::adjust_scroll(buf, &dims);
                    buf.scroll_row = sr;
                    buf.scroll_sub_line = ssl;
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        buf.save_state = SaveState::Modified;
                    }
                }
            } else {
                state.warn = Some("No region".into());
            }
        }
        Action::Yank => {
            state.pending_yank.set(());
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
                if let Some(buf) = state.buffers.get_mut(&id) {
                    close_group_on_move(buf);
                    buf.save_state = SaveState::Saving;
                }
            }
            state.save_request.set(());
        }

        // ── Tabs ──
        Action::NextTab => cycle_tab(state, 1),
        Action::PrevTab => cycle_tab(state, -1),
        Action::KillBuffer => kill_buffer(state),

        _ => {}
    }
}

fn cycle_tab(state: &mut AppState, direction: i32) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    let mut tabs: Vec<(BufferId, usize)> = state
        .buffers
        .iter()
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
        state.warn = Some(format!("Buffer {filename} modified; kill anyway? (y or n)"));
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

    state.buffers.remove(&id);
    state.active_buffer = next_active;
    reveal_active_buffer(state);
    renumber_tabs(state);

    if state.buffers.is_empty() {
        state.focus = PanelSlot::Side;
    }

    state.info = Some(format!("Killed {filename}"));
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
    match action {
        Action::MoveUp => {
            state.browser.selected = state.browser.selected.saturating_sub(1);
        }
        Action::MoveDown => {
            state.browser.selected = (state.browser.selected + 1).min(len - 1);
        }
        Action::PageUp => {
            state.browser.selected = state.browser.selected.saturating_sub(height);
        }
        Action::PageDown => {
            state.browser.selected = (state.browser.selected + height).min(len - 1);
        }
        Action::FileStart => {
            state.browser.selected = 0;
        }
        Action::FileEnd => {
            state.browser.selected = len - 1;
        }
        _ => {}
    }
    // Keep selection visible
    if state.browser.selected < state.browser.scroll_offset {
        state.browser.scroll_offset = state.browser.selected;
    } else if state.browser.selected >= state.browser.scroll_offset + height {
        state.browser.scroll_offset = state.browser.selected + 1 - height;
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
    state.browser.expanded_dirs.insert(path.clone());
    if state.browser.dir_contents.contains_key(&path) {
        state.browser.rebuild_entries();
    } else {
        state.pending_lists.set(vec![path]);
    }
}

fn handle_browser_collapse(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    let collapse_path = match &entry.kind {
        EntryKind::Directory { expanded: true } => entry.path.clone(),
        _ => {
            // File or collapsed dir — collapse parent
            match entry.path.parent() {
                Some(parent) if state.browser.expanded_dirs.contains(parent) => {
                    parent.to_path_buf()
                }
                _ => return,
            }
        }
    };
    state.browser.expanded_dirs.remove(&collapse_path);
    state.browser.rebuild_entries();
    // Move selection to the collapsed directory
    if let Some(pos) = state
        .browser
        .entries
        .iter()
        .position(|e| e.path == collapse_path)
    {
        state.browser.selected = pos;
    }
}

fn handle_browser_collapse_all(state: &mut AppState) {
    state.browser.expanded_dirs.clear();
    state.browser.rebuild_entries();
    state.browser.selected = 0;
    state.browser.scroll_offset = 0;
}

fn handle_browser_open(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    match &entry.kind {
        EntryKind::File => {
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
        if let Some(buf) = state.buffers.get_mut(&id) {
            f(buf, &dims);
            let (sr, ssl) = mov::adjust_scroll(buf, &dims);
            buf.scroll_row = sr;
            buf.scroll_sub_line = ssl;
            // Track save state: transition to Modified when doc becomes dirty
            if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                buf.save_state = SaveState::Modified;
            }
        }
    }
}

/// Yank (paste) text into the active buffer. Called when clipboard text arrives.
pub fn yank_text(state: &mut AppState, text: String) {
    if text.is_empty() {
        return;
    }
    if let (Some(dims), Some(id)) = (state.dims, state.active_buffer) {
        if let Some(buf) = state.buffers.get_mut(&id) {
            close_group_on_move(buf);
            buf.mark = None;
            let (doc, r, c, a) = edit::yank(buf, &text);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            let (sr, ssl) = mov::adjust_scroll(buf, &dims);
            buf.scroll_row = sr;
            buf.scroll_sub_line = ssl;
            if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                buf.save_state = SaveState::Modified;
            }
        }
    }
}

/// Close undo group and clear edit kind tracking.
fn close_group_on_move(buf: &mut BufferState) {
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
        state.buffers.get_mut(&bid).unwrap().tab_order = i;
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
    let new_dirs = state.browser.reveal(&path);
    if !new_dirs.is_empty() {
        state.pending_lists.set(new_dirs);
    }
    browser_scroll_to_selected(state);
}

pub(super) fn browser_scroll_to_selected(state: &mut AppState) {
    let height = state.dims.map_or(20, |d| d.buffer_height());
    let sel = state.browser.selected;
    if sel < state.browser.scroll_offset {
        state.browser.scroll_offset = sel;
    } else if sel >= state.browser.scroll_offset + height {
        state.browser.scroll_offset = sel + 1 - height;
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
