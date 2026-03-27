use led_core::{Action, PanelSlot};
use led_state::file_search::{
    FileSearchRequest, FileSearchSelection, FileSearchState, ReplaceEntry,
};
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

// ── Active input helpers ──

fn active_input(fs: &mut FileSearchState) -> Option<(&mut String, &mut usize)> {
    match fs.selection {
        FileSearchSelection::SearchInput => Some((&mut fs.query, &mut fs.cursor_pos)),
        FileSearchSelection::ReplaceInput => {
            Some((&mut fs.replace_text, &mut fs.replace_cursor_pos))
        }
        FileSearchSelection::Result(_) => None,
    }
}

fn is_search_input(fs: &FileSearchState) -> bool {
    fs.selection == FileSearchSelection::SearchInput
}

// ── Activation ──

pub fn activate(state: &mut AppState) {
    let selected = state
        .active_buffer
        .and_then(|id| state.buffers.get(&id))
        .and_then(|buf| super::edit::selected_text(buf));

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
        selection: FileSearchSelection::SearchInput,
        scroll_offset: 0,
        replace_mode: false,
        replace_text: String::new(),
        replace_cursor_pos: 0,
        replace_stack: Vec::new(),
    });

    fs.selection = FileSearchSelection::SearchInput;

    let has_selected = if let Some(text) = selected {
        fs.query = text;
        fs.cursor_pos = fs.query.chars().count();
        fs.results.clear();
        fs.flat_hits.clear();
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

// ── Unified navigation helpers ──

fn navigate_down(state: &mut AppState) {
    let fs = state.file_search.as_mut().unwrap();
    match fs.selection {
        FileSearchSelection::SearchInput => {
            if fs.replace_mode {
                fs.selection = FileSearchSelection::ReplaceInput;
            } else if !fs.flat_hits.is_empty() {
                fs.selection = FileSearchSelection::Result(0);
            }
        }
        FileSearchSelection::ReplaceInput => {
            if !fs.flat_hits.is_empty() {
                fs.selection = FileSearchSelection::Result(0);
            }
        }
        FileSearchSelection::Result(i) => {
            if i + 1 < fs.flat_hits.len() {
                fs.selection = FileSearchSelection::Result(i + 1);
            }
        }
    }
    scroll_to_selected(state);
    preview_selected(state);
}

fn navigate_up(state: &mut AppState) {
    let fs = state.file_search.as_mut().unwrap();
    match fs.selection {
        FileSearchSelection::SearchInput => {}
        FileSearchSelection::ReplaceInput => {
            fs.selection = FileSearchSelection::SearchInput;
        }
        FileSearchSelection::Result(0) => {
            if fs.replace_mode {
                fs.selection = FileSearchSelection::ReplaceInput;
            } else {
                fs.selection = FileSearchSelection::SearchInput;
            }
        }
        FileSearchSelection::Result(i) => {
            fs.selection = FileSearchSelection::Result(i - 1);
        }
    }
    scroll_to_selected(state);
    preview_selected(state);
}

// ── Action handling ──

pub fn handle_file_search_action(state: &mut AppState, action: &Action) -> bool {
    let on_input = matches!(
        state.file_search.as_ref().unwrap().selection,
        FileSearchSelection::SearchInput | FileSearchSelection::ReplaceInput
    );
    let on_result = matches!(
        state.file_search.as_ref().unwrap().selection,
        FileSearchSelection::Result(_)
    );

    match action {
        // ── Text editing (only when on an input row) ──
        Action::InsertChar(c) if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let is_search = is_search_input(fs);
            let (text, cursor) = active_input(fs).unwrap();
            insert_char_at(text, *cursor, *c);
            *cursor += 1;
            if is_search {
                let fs = state.file_search.as_mut().unwrap();
                fs.scroll_offset = 0;
                trigger_search(state);
            }
            true
        }
        Action::DeleteBackward if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let is_search = is_search_input(fs);
            let (text, cursor) = active_input(fs).unwrap();
            if *cursor > 0 {
                let byte_pos = char_byte_position(text, *cursor - 1);
                let next_byte = char_byte_position(text, *cursor);
                text.replace_range(byte_pos..next_byte, "");
                *cursor -= 1;
            }
            if is_search {
                let fs = state.file_search.as_mut().unwrap();
                fs.scroll_offset = 0;
                if fs.query.is_empty() {
                    fs.results.clear();
                    fs.flat_hits.clear();
                } else {
                    trigger_search(state);
                }
            }
            true
        }
        Action::DeleteForward if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let is_search = is_search_input(fs);
            let (text, cursor) = active_input(fs).unwrap();
            let char_len = text.chars().count();
            if *cursor < char_len {
                let byte_pos = char_byte_position(text, *cursor);
                let next_byte = char_byte_position(text, *cursor + 1);
                text.replace_range(byte_pos..next_byte, "");
            }
            if is_search {
                let fs = state.file_search.as_mut().unwrap();
                fs.scroll_offset = 0;
                if fs.query.is_empty() {
                    fs.results.clear();
                    fs.flat_hits.clear();
                } else {
                    trigger_search(state);
                }
            }
            true
        }
        Action::KillLine if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let is_search = is_search_input(fs);
            let (text, cursor) = active_input(fs).unwrap();
            let byte_pos = char_byte_position(text, *cursor);
            text.truncate(byte_pos);
            if is_search {
                let fs = state.file_search.as_mut().unwrap();
                fs.scroll_offset = 0;
                if fs.query.is_empty() {
                    fs.results.clear();
                    fs.flat_hits.clear();
                } else {
                    trigger_search(state);
                }
            }
            true
        }

        // ── Cursor movement in input ──
        Action::MoveLeft if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let (_, cursor) = active_input(fs).unwrap();
            if *cursor > 0 {
                *cursor -= 1;
            }
            true
        }
        Action::MoveRight if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let (text, cursor) = active_input(fs).unwrap();
            let char_len = text.chars().count();
            if *cursor < char_len {
                *cursor += 1;
            }
            true
        }
        Action::LineStart if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let (_, cursor) = active_input(fs).unwrap();
            *cursor = 0;
            true
        }
        Action::LineEnd if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            let (text, cursor) = active_input(fs).unwrap();
            *cursor = text.chars().count();
            true
        }

        // ── Replace/unreplace on result rows ──
        Action::MoveRight if on_result => {
            let fs = state.file_search.as_ref().unwrap();
            if fs.replace_mode {
                replace_selected(state);
            }
            true
        }
        Action::MoveLeft if on_result => {
            let fs = state.file_search.as_ref().unwrap();
            if fs.replace_mode {
                unreplace_selected(state);
            }
            true
        }

        // ── Unified vertical navigation ──
        Action::MoveUp => {
            navigate_up(state);
            true
        }
        Action::MoveDown => {
            navigate_down(state);
            true
        }
        Action::PageUp if on_result => {
            let height = results_height(state);
            let fs = state.file_search.as_mut().unwrap();
            if let FileSearchSelection::Result(i) = fs.selection {
                fs.selection = FileSearchSelection::Result(i.saturating_sub(height));
            }
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::PageDown if on_result => {
            let height = results_height(state);
            let fs = state.file_search.as_mut().unwrap();
            if let FileSearchSelection::Result(i) = fs.selection {
                if !fs.flat_hits.is_empty() {
                    fs.selection =
                        FileSearchSelection::Result((i + height).min(fs.flat_hits.len() - 1));
                }
            }
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::FileStart if on_result => {
            let fs = state.file_search.as_mut().unwrap();
            fs.selection = FileSearchSelection::Result(0);
            scroll_to_selected(state);
            preview_selected(state);
            true
        }
        Action::FileEnd if on_result => {
            let fs = state.file_search.as_mut().unwrap();
            if !fs.flat_hits.is_empty() {
                fs.selection = FileSearchSelection::Result(fs.flat_hits.len() - 1);
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
        Action::ToggleSearchReplace => {
            let fs = state.file_search.as_mut().unwrap();
            fs.replace_mode = !fs.replace_mode;
            if !fs.replace_mode {
                if fs.selection == FileSearchSelection::ReplaceInput {
                    fs.selection = FileSearchSelection::SearchInput;
                }
                fs.replace_stack.clear();
            }
            true
        }

        // ── Bulk replace ──
        Action::ReplaceAll => {
            let fs = state.file_search.as_ref().unwrap();
            if fs.replace_mode {
                replace_all(state);
            }
            deactivate(state);
            true
        }

        // ── Tab between inputs ──
        Action::InsertTab if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            if fs.replace_mode {
                fs.selection = match fs.selection {
                    FileSearchSelection::SearchInput => FileSearchSelection::ReplaceInput,
                    FileSearchSelection::ReplaceInput => FileSearchSelection::SearchInput,
                    other => other,
                };
            }
            true
        }
        Action::InsertTab if on_result => true,

        // ── Enter on inputs ──
        Action::OpenSelected | Action::InsertNewline if on_input => {
            let fs = state.file_search.as_mut().unwrap();
            match fs.selection {
                FileSearchSelection::SearchInput => {
                    if fs.replace_mode {
                        fs.selection = FileSearchSelection::ReplaceInput;
                    } else if !fs.flat_hits.is_empty() {
                        fs.selection = FileSearchSelection::Result(0);
                        scroll_to_selected(state);
                        preview_selected(state);
                    }
                }
                FileSearchSelection::ReplaceInput => {
                    if !fs.flat_hits.is_empty() {
                        fs.selection = FileSearchSelection::Result(0);
                        scroll_to_selected(state);
                        preview_selected(state);
                    }
                }
                _ => {}
            }
            true
        }

        // ── Enter on results ──
        Action::OpenSelected | Action::InsertNewline if on_result => {
            let fs = state.file_search.as_ref().unwrap();
            if fs.replace_mode {
                deactivate(state);
            } else {
                confirm_selected(state);
            }
            true
        }

        // ── Close ──
        Action::Abort | Action::CloseFileSearch => {
            deactivate(state);
            true
        }

        // ── Pass through ──
        Action::Resize(..) | Action::Quit | Action::Suspend => false,

        // Ignore when on result row
        _ if on_result => true,

        // Everything else on input rows: deactivate, don't consume
        _ => {
            deactivate(state);
            false
        }
    }
}

// ── Buffer lookup ──

fn find_buf_for_path(state: &AppState, path: &std::path::Path) -> Option<led_core::BufferId> {
    if let Some(b) = state
        .buffers
        .values()
        .find(|b| b.path.as_deref() == Some(path))
    {
        return Some(b.id);
    }
    let canonical = std::fs::canonicalize(path).ok()?;
    state
        .buffers
        .values()
        .find(|b| {
            b.path
                .as_ref()
                .and_then(|p| std::fs::canonicalize(p).ok())
                .as_deref()
                == Some(canonical.as_path())
        })
        .map(|b| b.id)
}

// ── Replace execution ──

fn remove_hit_from_results(fs: &mut FileSearchState, result_idx: usize) {
    if result_idx >= fs.flat_hits.len() {
        return;
    }
    let flat = &fs.flat_hits[result_idx];
    let gi = flat.group_idx;
    let hi = flat.hit_idx;
    fs.results[gi].hits.remove(hi);
    if fs.results[gi].hits.is_empty() {
        fs.results.remove(gi);
    }
    fs.rebuild_flat_hits();
}

fn reinsert_hit_into_results(fs: &mut FileSearchState, entry: &ReplaceEntry, line_text: &str) {
    let gi = fs
        .results
        .iter()
        .position(|g| g.path == entry.path)
        .unwrap_or_else(|| {
            let relative = entry.path.to_string_lossy().to_string();
            let group = led_state::file_search::FileGroup {
                path: entry.path.clone(),
                relative,
                hits: Vec::new(),
            };
            let pos = fs
                .results
                .iter()
                .position(|g| g.relative > group.relative)
                .unwrap_or(fs.results.len());
            fs.results.insert(pos, group);
            pos
        });

    let hit = led_state::file_search::SearchHit {
        row: entry.row,
        col: line_text[..entry.match_start].chars().count(),
        line_text: line_text.to_string(),
        match_start: entry.match_start,
        match_end: entry.match_end,
    };

    let group = &mut fs.results[gi];
    let pos = group
        .hits
        .iter()
        .position(|h| h.row > hit.row || (h.row == hit.row && h.match_start > hit.match_start))
        .unwrap_or(group.hits.len());
    group.hits.insert(pos, hit);

    fs.rebuild_flat_hits();
}

fn close_undo_group(state: &mut AppState, buf_id: led_core::BufferId) {
    if let Some(buf) = state.buf_mut(buf_id) {
        buf.doc = buf.doc.close_undo_group();
    }
}

fn replace_selected(state: &mut AppState) {
    let fs = state.file_search.as_ref().unwrap();
    let Some(result_idx) = fs.selected_result_idx() else {
        return;
    };
    let Some((group, hit)) = fs.selected_hit() else {
        return;
    };
    if fs.replace_text.is_empty() && fs.query.is_empty() {
        return;
    }

    let path = group.path.clone();
    let row = hit.row;
    let match_start = hit.match_start;
    let match_end = hit.match_end;
    let original_text = hit.line_text[match_start..match_end].to_string();
    let replacement = fs.replace_text.clone();

    let entry = ReplaceEntry {
        flat_hit_idx: result_idx,
        path: path.clone(),
        row,
        original_text,
        match_start,
        match_end,
        replacement_len: replacement.len(),
    };

    let buf_id = find_buf_for_path(state, &path);

    if let Some(id) = buf_id {
        // Each individual replace gets its own undo group
        close_undo_group(state, id);
        replace_in_buffer(state, id, row, match_start, match_end, &replacement);
        let fs = state.file_search.as_mut().unwrap();
        fs.replace_stack.push(entry);
        remove_hit_from_results(fs, result_idx);
        if !fs.flat_hits.is_empty() {
            let new_idx = result_idx.min(fs.flat_hits.len() - 1);
            fs.selection = FileSearchSelection::Result(new_idx);
        } else {
            fs.selection = if fs.replace_mode {
                FileSearchSelection::ReplaceInput
            } else {
                FileSearchSelection::SearchInput
            };
        }
    } else {
        let fs = state.file_search.as_mut().unwrap();
        fs.replace_stack.push(entry);
        if result_idx + 1 < fs.flat_hits.len() {
            fs.selection = FileSearchSelection::Result(result_idx + 1);
        }

        let root = state
            .workspace
            .as_ref()
            .map(|w| w.root.clone())
            .unwrap_or_else(|| (*state.startup.start_dir).clone());

        let fs = state.file_search.as_ref().unwrap();
        state
            .pending_file_replace
            .set(Some(led_state::file_search::FileSearchReplaceRequest {
                query: fs.query.clone(),
                replacement: fs.replace_text.clone(),
                root,
                case_sensitive: fs.case_sensitive,
                use_regex: fs.use_regex,
                scope: led_state::file_search::ReplaceScope::Single {
                    path,
                    row,
                    match_start,
                    match_end,
                },
                skip_paths: Vec::new(),
            }));
    }
}

fn unreplace_selected(state: &mut AppState) {
    let fs = state.file_search.as_mut().unwrap();
    let Some(entry) = fs.replace_stack.pop() else {
        return;
    };

    let buf_id = find_buf_for_path(state, &entry.path);

    if let Some(id) = buf_id {
        close_undo_group(state, id);
        replace_in_buffer(
            state,
            id,
            entry.row,
            entry.match_start,
            entry.match_start + entry.replacement_len,
            &entry.original_text,
        );
        let line_text = state
            .buffers
            .get(&id)
            .map(|b| b.doc.line(entry.row).to_string())
            .unwrap_or_default();
        let fs = state.file_search.as_mut().unwrap();
        reinsert_hit_into_results(fs, &entry, &line_text);
        let target_idx = fs
            .flat_hits
            .iter()
            .position(|fh| {
                let g = &fs.results[fh.group_idx];
                let h = &g.hits[fh.hit_idx];
                g.path == entry.path && h.row == entry.row && h.match_start == entry.match_start
            })
            .unwrap_or(0);
        fs.selection = FileSearchSelection::Result(target_idx);
    } else {
        let root = state
            .workspace
            .as_ref()
            .map(|w| w.root.clone())
            .unwrap_or_else(|| (*state.startup.start_dir).clone());

        let fs = state.file_search.as_ref().unwrap();
        state
            .pending_file_replace
            .set(Some(led_state::file_search::FileSearchReplaceRequest {
                query: fs.query.clone(),
                replacement: entry.original_text.clone(),
                root,
                case_sensitive: fs.case_sensitive,
                use_regex: fs.use_regex,
                scope: led_state::file_search::ReplaceScope::Single {
                    path: entry.path,
                    row: entry.row,
                    match_start: entry.match_start,
                    match_end: entry.match_start + entry.replacement_len,
                },
                skip_paths: Vec::new(),
            }));

        let fs = state.file_search.as_mut().unwrap();
        fs.selection = FileSearchSelection::Result(
            entry.flat_hit_idx.min(fs.flat_hits.len().saturating_sub(1)),
        );
    }
}

fn replace_all(state: &mut AppState) {
    let fs = state.file_search.as_ref().unwrap();
    if fs.query.is_empty() {
        return;
    }
    let replacement = fs.replace_text.clone();

    let mut hits_by_buf: std::collections::HashMap<
        std::path::PathBuf,
        Vec<(usize, usize, usize, usize, String)>,
    > = std::collections::HashMap::new();

    for (fi, flat) in fs.flat_hits.iter().enumerate() {
        let group = &fs.results[flat.group_idx];
        let hit = &group.hits[flat.hit_idx];
        let original = hit.line_text[hit.match_start..hit.match_end].to_string();
        hits_by_buf.entry(group.path.clone()).or_default().push((
            fi,
            hit.row,
            hit.match_start,
            hit.match_end,
            original,
        ));
    }

    let mut open_paths = Vec::new();
    for (path, hits) in &hits_by_buf {
        let buf_id = find_buf_for_path(state, path);

        if let Some(id) = buf_id {
            open_paths.push(path.clone());
            // ONE undo group for all replacements in this buffer
            close_undo_group(state, id);
            for (fi, row, ms, me, original) in hits.iter().rev() {
                replace_in_buffer(state, id, *row, *ms, *me, &replacement);
                let fs = state.file_search.as_mut().unwrap();
                fs.replace_stack.push(ReplaceEntry {
                    flat_hit_idx: *fi,
                    path: path.clone(),
                    row: *row,
                    original_text: original.clone(),
                    match_start: *ms,
                    match_end: *me,
                    replacement_len: replacement.len(),
                });
            }
        }
    }

    // Only open-buffer replacements are applied (undoable).
    // Non-open files stay in results for individual replace via Right arrow.
    let fs = state.file_search.as_mut().unwrap();
    fs.results.retain(|g| !open_paths.contains(&g.path));
    fs.rebuild_flat_hits();
}

fn replace_in_buffer(
    state: &mut AppState,
    buf_id: led_core::BufferId,
    row: usize,
    match_start_byte: usize,
    match_end_byte: usize,
    replacement: &str,
) {
    let Some(buf) = state.buf_mut(buf_id) else {
        return;
    };
    if row >= buf.doc.line_count() {
        return;
    }

    let line_text = buf.doc.line(row);
    let match_start_char = line_text
        .get(..match_start_byte)
        .map(|s| s.chars().count())
        .unwrap_or(0);
    let match_end_char = line_text
        .get(..match_end_byte)
        .map(|s| s.chars().count())
        .unwrap_or(match_start_char);
    let line_start = buf.doc.line_to_char(row);
    let abs_start = line_start + match_start_char;
    let abs_end = line_start + match_end_char;

    let doc = buf.doc.remove(abs_start, abs_end);
    let doc = doc.insert(abs_start, replacement);
    buf.doc = doc;
    buf.save_state = led_state::SaveState::Modified;
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

    let existing = state
        .buffers
        .values()
        .find(|b| b.path.as_ref() == Some(&path))
        .map(|b| b.id);

    if let Some(id) = existing {
        state.active_buffer = Some(id);
        if let Some(buf) = state.buf_mut(id) {
            buf.cursor_row = row.min(buf.doc.line_count().saturating_sub(1));
            buf.cursor_col = col;
            buf.cursor_col_affinity = col;
        }
    } else {
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
    let header = if state.file_search.as_ref().is_some_and(|fs| fs.replace_mode) {
        3
    } else {
        2
    };
    state
        .dims
        .map_or(20, |d| d.buffer_height().saturating_sub(header))
}

fn scroll_to_selected(state: &mut AppState) {
    let height = results_height(state);
    let fs = state.file_search.as_mut().unwrap();
    let Some(i) = fs.selected_result_idx() else {
        return;
    };
    if fs.flat_hits.is_empty() {
        return;
    }
    let sel_row = fs.flat_hit_to_row(i);
    if sel_row < fs.scroll_offset {
        fs.scroll_offset = sel_row;
    } else if sel_row >= fs.scroll_offset + height {
        fs.scroll_offset = sel_row - height + 1;
    }
}
