use led_core::{Action, PanelSlot};
use led_state::file_search::{FileSearchRequest, FileSearchState};
use led_state::{AppState, JumpPosition, PreviewRequest};

// ── UTF-8 cursor helpers ──

fn char_byte_position(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn insert_char_at(s: &mut String, char_idx: usize, c: char) {
    let byte_pos = char_byte_position(s, char_idx);
    s.insert(byte_pos, c);
}

// ── Activation ──

pub fn activate(state: &mut AppState) {
    // Grab selected text from active buffer (if any) to seed the query
    let selected = state
        .active_buffer
        .and_then(|id| state.buffers.get(&id))
        .and_then(|buf| super::edit::selected_text(buf));

    // Clear the mark after grabbing the selection
    if selected.is_some() {
        if let Some(id) = state.active_buffer {
            if let Some(buf) = state.buf_mut(id) {
                buf.mark = None;
            }
        }
    }

    let mut fs = state.file_search.take().unwrap_or(FileSearchState {
        query: String::new(),
        cursor_pos: 0,
        case_sensitive: false,
        use_regex: false,
        results: Vec::new(),
        flat_hits: Vec::new(),
        selected: 0,
        scroll_offset: 0,
    });

    // If we have selected text, replace the query with it
    let has_selected = if let Some(text) = selected {
        fs.query = text;
        fs.cursor_pos = fs.query.chars().count();
        fs.results.clear();
        fs.flat_hits.clear();
        fs.selected = 0;
        fs.scroll_offset = 0;
        true
    } else {
        false
    };

    state.file_search = Some(fs);
    state.focus = PanelSlot::Side;
    state.show_side_panel = true;
    if let Some(ref mut dims) = state.dims {
        dims.show_side_panel = true;
    }

    // Kick off the search if we seeded a query from the selection
    if has_selected {
        trigger_search(state);
    }
}

pub fn deactivate(state: &mut AppState) {
    super::action::close_preview(state);
    state.file_search = None;
    state.focus = PanelSlot::Main;
}

fn deactivate_without_close_preview(state: &mut AppState) {
    state.file_search = None;
    state.focus = PanelSlot::Main;
}

fn preview_selected(state: &mut AppState) {
    let Some(ref fs) = state.file_search else {
        return;
    };
    let Some((group, hit)) = fs.selected_hit() else {
        return;
    };
    state.preview.pending.set(Some(PreviewRequest {
        path: group.path.clone(),
        row: hit.row,
        col: hit.col,
    }));
}

// ── Trigger search ──

fn trigger_search(state: &mut AppState) {
    let fs = state.file_search.as_mut().unwrap();
    if fs.query.is_empty() {
        fs.results.clear();
        fs.flat_hits.clear();
        fs.selected = 0;
        fs.scroll_offset = 0;
        return;
    }

    let root = state
        .workspace
        .as_ref()
        .map(|w| w.root.clone())
        .unwrap_or_else(|| (*state.startup.start_dir).clone());

    let fs = state.file_search.as_ref().unwrap();
    let req = FileSearchRequest {
        query: fs.query.clone(),
        root,
        case_sensitive: fs.case_sensitive,
        use_regex: fs.use_regex,
    };
    state.pending_file_search.set(Some(req));
}

// ── Action handling ──

/// Handle action while file search is active.
/// Returns true if the action was consumed.
pub fn handle_file_search_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        // ── Text editing ──
        Action::InsertChar(c) => {
            let fs = state.file_search.as_mut().unwrap();
            insert_char_at(&mut fs.query, fs.cursor_pos, *c);
            fs.cursor_pos += 1;
            fs.selected = 0;
            fs.scroll_offset = 0;
            trigger_search(state);
            true
        }
        Action::DeleteBackward => {
            let fs = state.file_search.as_mut().unwrap();
            if fs.cursor_pos > 0 {
                let byte_pos = char_byte_position(&fs.query, fs.cursor_pos - 1);
                let next_byte = char_byte_position(&fs.query, fs.cursor_pos);
                fs.query.replace_range(byte_pos..next_byte, "");
                fs.cursor_pos -= 1;
                fs.selected = 0;
                fs.scroll_offset = 0;
            }
            if fs.query.is_empty() {
                fs.results.clear();
                fs.flat_hits.clear();
            } else {
                trigger_search(state);
            }
            true
        }
        Action::DeleteForward => {
            let fs = state.file_search.as_mut().unwrap();
            let char_len = fs.query.chars().count();
            if fs.cursor_pos < char_len {
                let byte_pos = char_byte_position(&fs.query, fs.cursor_pos);
                let next_byte = char_byte_position(&fs.query, fs.cursor_pos + 1);
                fs.query.replace_range(byte_pos..next_byte, "");
                fs.selected = 0;
                fs.scroll_offset = 0;
            }
            if fs.query.is_empty() {
                fs.results.clear();
                fs.flat_hits.clear();
            } else {
                trigger_search(state);
            }
            true
        }
        Action::KillLine => {
            let fs = state.file_search.as_mut().unwrap();
            let byte_pos = char_byte_position(&fs.query, fs.cursor_pos);
            fs.query.truncate(byte_pos);
            fs.selected = 0;
            fs.scroll_offset = 0;
            if fs.query.is_empty() {
                fs.results.clear();
                fs.flat_hits.clear();
            } else {
                trigger_search(state);
            }
            true
        }

        // ── Cursor movement in input ──
        Action::MoveLeft => {
            let fs = state.file_search.as_mut().unwrap();
            if fs.cursor_pos > 0 {
                fs.cursor_pos -= 1;
            }
            true
        }
        Action::MoveRight => {
            let fs = state.file_search.as_mut().unwrap();
            let char_len = fs.query.chars().count();
            if fs.cursor_pos < char_len {
                fs.cursor_pos += 1;
            }
            true
        }
        Action::LineStart => {
            let fs = state.file_search.as_mut().unwrap();
            fs.cursor_pos = 0;
            true
        }
        Action::LineEnd => {
            let fs = state.file_search.as_mut().unwrap();
            fs.cursor_pos = fs.query.chars().count();
            true
        }

        // ── Result navigation ──
        Action::MoveUp => {
            let fs = state.file_search.as_mut().unwrap();
            if fs.selected > 0 {
                fs.selected -= 1;
            }
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::MoveDown => {
            let fs = state.file_search.as_mut().unwrap();
            if !fs.flat_hits.is_empty() && fs.selected + 1 < fs.flat_hits.len() {
                fs.selected += 1;
            }
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::PageUp => {
            let height = results_height(state);
            let fs = state.file_search.as_mut().unwrap();
            fs.selected = fs.selected.saturating_sub(height);
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::PageDown => {
            let height = results_height(state);
            let fs = state.file_search.as_mut().unwrap();
            if !fs.flat_hits.is_empty() {
                fs.selected = (fs.selected + height).min(fs.flat_hits.len() - 1);
            }
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::FileStart => {
            let fs = state.file_search.as_mut().unwrap();
            fs.selected = 0;
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::FileEnd => {
            let fs = state.file_search.as_mut().unwrap();
            if !fs.flat_hits.is_empty() {
                fs.selected = fs.flat_hits.len() - 1;
            }
            scroll_to_selected(state);
            preview_selected(state);
            true
        }

        // ── Toggles ──
        Action::ToggleSearchCase => {
            let fs = state.file_search.as_mut().unwrap();
            fs.case_sensitive = !fs.case_sensitive;
            trigger_search(state);
            true
        }
        Action::ToggleSearchRegex => {
            let fs = state.file_search.as_mut().unwrap();
            fs.use_regex = !fs.use_regex;
            trigger_search(state);
            true
        }

        // ── Confirm ──
        Action::OpenSelected | Action::InsertNewline => {
            confirm_selected(state);
            true
        }

        // ── Close ──
        Action::Abort | Action::CloseFileSearch => {
            deactivate(state);
            true
        }

        // ── Pass through ──
        Action::Resize(..) | Action::Quit | Action::Suspend => false,

        // ── Everything else: deactivate, don't consume ──
        _ => {
            deactivate(state);
            false
        }
    }
}

// ── Confirm selected hit ──

fn confirm_selected(state: &mut AppState) {
    let (path, row, col) = {
        let fs = state.file_search.as_ref().unwrap();
        let Some((group, hit)) = fs.selected_hit() else {
            return;
        };
        (group.path.clone(), hit.row, hit.col)
    };

    // Promote preview if it matches
    if super::action::promote_preview(state, &path) {
        if let Some(preview_id) = state.active_buffer {
            if let Some(buf) = state.buf_mut(preview_id) {
                buf.cursor_row = row.min(buf.doc.line_count().saturating_sub(1));
                buf.cursor_col = col;
                buf.cursor_col_affinity = col;
            }
        }
        deactivate_without_close_preview(state);
        return;
    }

    // Check if this buffer is already open
    let existing = state
        .buffers
        .values()
        .find(|b| b.path.as_ref() == Some(&path))
        .map(|b| b.id);

    if let Some(id) = existing {
        // Activate and set cursor
        state.active_buffer = Some(id);
        if let Some(buf) = state.buf_mut(id) {
            buf.cursor_row = row.min(buf.doc.line_count().saturating_sub(1));
            buf.cursor_col = col;
            buf.cursor_col_affinity = col;
        }
    } else {
        // Open via pending_open + pending_jump_position
        state.pending_open.set(Some(path.clone()));
        state.jump.pending_position = Some(JumpPosition {
            path,
            row,
            col,
            scroll_offset: row.saturating_sub(5),
        });
    }

    deactivate(state);
}

// ── Scroll helpers ──

fn results_height(state: &AppState) -> usize {
    state
        .dims
        .map_or(20, |d| d.buffer_height().saturating_sub(2))
}

fn scroll_to_selected(state: &mut AppState) {
    let height = results_height(state);
    let fs = state.file_search.as_mut().unwrap();
    if fs.flat_hits.is_empty() {
        return;
    }
    let sel_row = fs.flat_hit_to_row(fs.selected);
    if sel_row < fs.scroll_offset {
        fs.scroll_offset = sel_row;
    } else if sel_row >= fs.scroll_offset + height {
        fs.scroll_offset = sel_row - height + 1;
    }
}
