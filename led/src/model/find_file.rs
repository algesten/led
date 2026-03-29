use std::path::{Path, PathBuf};

use led_core::Action;
use led_fs::FindFileEntry;
use led_state::{AppState, FindFileMode, FindFileState, PreviewRequest};

// ── Path helpers ──

fn abbreviate_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if path.starts_with(home.as_ref()) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

pub(super) fn expand_path(input: &str) -> PathBuf {
    let input = if input.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            home.join(&input[1..].trim_start_matches('/'))
                .to_string_lossy()
                .into_owned()
        } else {
            input.to_string()
        }
    } else {
        input.to_string()
    };

    let path = Path::new(&input);
    let mut result = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            other => result.push(other),
        }
    }
    result
}

/// Everything up to and including the last `/`.
fn input_dir_prefix(input: &str) -> &str {
    match input.rfind('/') {
        Some(i) => &input[..=i],
        None => "",
    }
}

// ── UTF-8 cursor helpers ──

fn prev_char_boundary(s: &str, byte_pos: usize) -> usize {
    s[..byte_pos]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_len(s: &str, byte_pos: usize) -> usize {
    s[byte_pos..]
        .chars()
        .next()
        .map(|c| c.len_utf8())
        .unwrap_or(0)
}

// ── Selection wrapping ──

fn wrap_selection_up(current: Option<usize>, len: usize) -> usize {
    match current {
        Some(0) | None => len - 1,
        Some(i) => i - 1,
    }
}

fn wrap_selection_down(current: Option<usize>, len: usize) -> usize {
    match current {
        None => 0,
        Some(i) if i + 1 >= len => 0,
        Some(i) => i + 1,
    }
}

// ── Requesting completions ──

fn request_completions(state: &mut AppState) {
    let ff = state.find_file.as_mut().unwrap();
    ff.selected = None;
    ff.base_input = ff.input.clone();

    let input = &ff.input;
    let expanded = expand_path(input);

    if input.ends_with('/') {
        let prefix = String::new();
        let show_hidden = false;
        state
            .pending_find_file_list
            .set(Some((expanded, prefix, show_hidden)));
    } else {
        let dir = expanded.parent().unwrap_or(Path::new("/")).to_path_buf();
        let prefix = expanded
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let show_hidden = prefix.starts_with('.');
        state
            .pending_find_file_list
            .set(Some((dir, prefix, show_hidden)));
    }
}

// ── Longest common prefix ──

fn longest_common_prefix(completions: &[FindFileEntry]) -> String {
    if completions.is_empty() {
        return String::new();
    }
    // Use the raw name (without trailing / for dirs) for prefix computation
    let names: Vec<String> = completions
        .iter()
        .map(|c| {
            c.full
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();

    let first = &names[0];
    let first_chars: Vec<char> = first.chars().collect();
    let mut prefix_len = first_chars.len();
    for name in &names[1..] {
        let name_chars: Vec<char> = name.chars().collect();
        prefix_len = prefix_len.min(name_chars.len());
        for (i, (a, b)) in first_chars.iter().zip(name_chars.iter()).enumerate() {
            if a.to_lowercase().ne(b.to_lowercase()) {
                prefix_len = prefix_len.min(i);
                break;
            }
        }
    }
    first_chars[..prefix_len].iter().collect()
}

// ── Tab completion ──

fn tab_complete(state: &mut AppState) {
    let ff = state.find_file.as_mut().unwrap();
    let completions = &ff.completions;

    // Rule 1: input ends with / and completions populated → show side panel
    if ff.input.ends_with('/') && !completions.is_empty() {
        ff.show_side = true;
        ff.selected = None;
        return;
    }

    // Single match that is a dir (without trailing /) → append /
    if completions.len() == 1 && completions[0].is_dir && !ff.input.ends_with('/') {
        let dir_prefix = input_dir_prefix(&ff.input).to_string();
        let name = &completions[0].name; // has trailing /
        ff.input = format!("{dir_prefix}{name}");
        ff.cursor = ff.input.len();
        request_completions(state);
        return;
    }

    // Single match (non-dir or dir with / already) → complete fully
    if completions.len() == 1 {
        let dir_prefix = input_dir_prefix(&ff.input).to_string();
        let name = &completions[0].name;
        ff.input = format!("{dir_prefix}{name}");
        ff.cursor = ff.input.len();
        if completions[0].is_dir {
            request_completions(state);
        }
        return;
    }

    // Multiple matches → show side, extend to LCP
    if completions.len() > 1 {
        let ff = state.find_file.as_mut().unwrap();
        ff.show_side = true;
        let common = longest_common_prefix(&ff.completions);
        let dir_prefix = input_dir_prefix(&ff.input).to_string();
        let expanded = expand_path(&ff.input);
        let current_prefix = expanded
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if common.len() > current_prefix.len() {
            ff.input = format!("{dir_prefix}{common}");
            ff.cursor = ff.input.len();
        }
        request_completions(state);
        return;
    }

    // Empty completions → nothing to do
}

// ── Activation ──

pub fn activate(state: &mut AppState) {
    // Parent dir of active buffer's path, or start_dir
    let dir = state
        .active_buffer
        .as_ref()
        .and_then(|path| state.buffers.get(path))
        .and_then(|buf| buf.path_buf().cloned())
        .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
        .unwrap_or_else(|| (*state.startup.start_dir).clone());

    let dir_str = dir.to_string_lossy().into_owned();
    let mut input = abbreviate_home(&dir_str);
    if !input.ends_with('/') {
        input.push('/');
    }
    let cursor = input.len();

    state.find_file = Some(FindFileState {
        mode: FindFileMode::Open,
        input: input.clone(),
        cursor,
        base_input: input,
        completions: Vec::new(),
        selected: None,
        show_side: false,
    });

    // Request initial listing
    let expanded = expand_path(&state.find_file.as_ref().unwrap().input);
    state
        .pending_find_file_list
        .set(Some((expanded, String::new(), false)));
}

pub fn activate_save_as(state: &mut AppState) {
    // Start with the current buffer's full path (or parent dir like find_file)
    let input = state
        .active_buffer
        .as_ref()
        .and_then(|path| state.buffers.get(path))
        .and_then(|buf| buf.path_buf().cloned())
        .map(|p| abbreviate_home(&p.to_string_lossy()))
        .unwrap_or_else(|| {
            let dir = (*state.startup.start_dir).to_string_lossy().into_owned();
            let mut s = abbreviate_home(&dir);
            if !s.ends_with('/') {
                s.push('/');
            }
            s
        });
    let cursor = input.len();

    state.find_file = Some(FindFileState {
        mode: FindFileMode::SaveAs,
        input: input.clone(),
        cursor,
        base_input: input,
        completions: Vec::new(),
        selected: None,
        show_side: false,
    });

    // Request initial listing for the directory
    let expanded = expand_path(&state.find_file.as_ref().unwrap().input);
    let dir = expanded.parent().unwrap_or(Path::new("/")).to_path_buf();
    let prefix = expanded
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let show_hidden = prefix.starts_with('.');
    state
        .pending_find_file_list
        .set(Some((dir, prefix, show_hidden)));
}

fn deactivate(state: &mut AppState) {
    super::action::close_preview(state);
    state.find_file = None;
}

fn deactivate_without_close_preview(state: &mut AppState) {
    state.find_file = None;
}

fn preview_selected(state: &mut AppState) {
    let Some(ref ff) = state.find_file else {
        return;
    };
    let Some(sel) = ff.selected else { return };
    let Some(comp) = ff.completions.get(sel) else {
        return;
    };
    if !comp.is_dir {
        state.preview.pending.set(Some(PreviewRequest {
            path: comp.full.clone(),
            row: 0,
            col: 0,
        }));
    }
}

// ── Action handling ──

/// Handle action while find-file is active.
/// Returns true if the action was consumed.
pub fn handle_find_file_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::InsertChar(c) => {
            let ff = state.find_file.as_mut().unwrap();
            ff.input.insert(ff.cursor, *c);
            ff.cursor += c.len_utf8();
            request_completions(state);
            true
        }

        Action::DeleteBackward => {
            let ff = state.find_file.as_mut().unwrap();
            if ff.cursor > 0 {
                let prev = prev_char_boundary(&ff.input, ff.cursor);
                ff.input.drain(prev..ff.cursor);
                ff.cursor = prev;
                request_completions(state);
            }
            true
        }

        Action::DeleteForward => {
            let ff = state.find_file.as_mut().unwrap();
            if ff.cursor < ff.input.len() {
                let len = next_char_len(&ff.input, ff.cursor);
                ff.input.drain(ff.cursor..ff.cursor + len);
                request_completions(state);
            }
            true
        }

        Action::InsertTab => {
            tab_complete(state);
            true
        }

        Action::InsertNewline => {
            handle_enter(state);
            true
        }

        Action::MoveUp => {
            let ff = state.find_file.as_mut().unwrap();
            if ff.completions.is_empty() {
                return true;
            }
            ff.show_side = true;
            let dir_prefix = input_dir_prefix(&ff.base_input).to_string();
            ff.selected = Some(wrap_selection_up(ff.selected, ff.completions.len()));
            if let Some(sel) = ff.selected {
                if let Some(comp) = ff.completions.get(sel) {
                    ff.input = format!("{dir_prefix}{}", comp.name);
                    ff.cursor = ff.input.len();
                }
            }
            preview_selected(state);
            true
        }

        Action::MoveDown => {
            let ff = state.find_file.as_mut().unwrap();
            if ff.completions.is_empty() {
                return true;
            }
            ff.show_side = true;
            let dir_prefix = input_dir_prefix(&ff.base_input).to_string();
            ff.selected = Some(wrap_selection_down(ff.selected, ff.completions.len()));
            if let Some(sel) = ff.selected {
                if let Some(comp) = ff.completions.get(sel) {
                    ff.input = format!("{dir_prefix}{}", comp.name);
                    ff.cursor = ff.input.len();
                }
            }
            preview_selected(state);
            true
        }

        Action::MoveLeft => {
            let ff = state.find_file.as_mut().unwrap();
            if ff.cursor > 0 {
                ff.cursor = prev_char_boundary(&ff.input, ff.cursor);
            }
            true
        }

        Action::MoveRight => {
            let ff = state.find_file.as_mut().unwrap();
            if ff.cursor < ff.input.len() {
                ff.cursor += next_char_len(&ff.input, ff.cursor);
            }
            true
        }

        Action::LineStart => {
            let ff = state.find_file.as_mut().unwrap();
            ff.cursor = 0;
            true
        }

        Action::LineEnd => {
            let ff = state.find_file.as_mut().unwrap();
            ff.cursor = ff.input.len();
            true
        }

        Action::KillLine => {
            let ff = state.find_file.as_mut().unwrap();
            ff.input.truncate(ff.cursor);
            request_completions(state);
            true
        }

        Action::Abort => {
            deactivate(state);
            true
        }

        // Pass through without consuming
        Action::Resize(..) | Action::Quit | Action::Suspend => false,

        // Any other action → deactivate, don't consume
        _ => {
            deactivate(state);
            false
        }
    }
}

// ── Enter logic ──

fn handle_enter(state: &mut AppState) {
    let mode = state.find_file.as_ref().unwrap().mode;
    match mode {
        FindFileMode::Open => handle_enter_open(state),
        FindFileMode::SaveAs => handle_enter_save_as(state),
    }
}

fn handle_enter_open(state: &mut AppState) {
    let ff = state.find_file.as_ref().unwrap();

    // Path A: selected completion
    if let Some(sel) = ff.selected {
        if let Some(comp) = ff.completions.get(sel).cloned() {
            if comp.is_dir {
                // Descend into directory
                let dir_prefix = input_dir_prefix(&ff.base_input).to_string();
                let ff = state.find_file.as_mut().unwrap();
                ff.input = format!("{dir_prefix}{}", comp.name);
                ff.cursor = ff.input.len();
                request_completions(state);
            } else {
                // Promote preview if it matches
                if super::action::promote_preview(state, &comp.full) {
                    deactivate_without_close_preview(state);
                } else {
                    state.pending_open.set(Some(comp.full));
                    deactivate(state);
                }
            }
            return;
        }
    }

    // Path B: no selection — check completions for exact match
    let expanded = expand_path(&ff.input);
    let input = ff.input.clone();

    // Find matching completion (clone to release borrow)
    let matched = ff.completions.iter().find(|c| c.full == expanded).cloned();

    if let Some(comp) = matched {
        if comp.is_dir {
            if input.ends_with('/') {
                request_completions(state);
            }
            return;
        } else {
            if super::action::promote_preview(state, &comp.full) {
                deactivate_without_close_preview(state);
            } else {
                state.pending_open.set(Some(comp.full));
                deactivate(state);
            }
            return;
        }
    }

    // Path C: non-existent path (not ending /, not empty) → open (creates new file)
    if !input.ends_with('/') && !input.is_empty() {
        if super::action::promote_preview(state, &expanded) {
            deactivate_without_close_preview(state);
        } else {
            state.pending_open.set(Some(expanded));
            deactivate(state);
        }
        return;
    }
}

fn handle_enter_save_as(state: &mut AppState) {
    let ff = state.find_file.as_ref().unwrap();

    // Resolve the target path — from selection or input
    let path = if let Some(sel) = ff.selected {
        if let Some(comp) = ff.completions.get(sel).cloned() {
            if comp.is_dir {
                // Descend into directory
                let dir_prefix = input_dir_prefix(&ff.base_input).to_string();
                let ff = state.find_file.as_mut().unwrap();
                ff.input = format!("{dir_prefix}{}", comp.name);
                ff.cursor = ff.input.len();
                request_completions(state);
                return;
            }
            comp.full
        } else {
            return;
        }
    } else {
        let expanded = expand_path(&ff.input);
        let input = ff.input.clone();

        // Check completions for exact dir match → descend
        let matched = ff.completions.iter().find(|c| c.full == expanded).cloned();
        if let Some(comp) = matched {
            if comp.is_dir {
                if input.ends_with('/') {
                    request_completions(state);
                }
                return;
            }
        }

        // Don't save to a directory path
        if input.ends_with('/') || input.is_empty() {
            return;
        }

        expanded
    };

    // Save the active buffer to the new path
    if let Some(active_path) = state.active_buffer.clone() {
        if let Some(buf) = state.buf_mut(&active_path) {
            super::action::close_group_on_move(buf);
            buf.mark_saving();
        }
        state.pending_save_as.set(Some(path));
    }

    deactivate(state);
}
