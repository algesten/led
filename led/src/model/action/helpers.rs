use std::time::Instant;

use led_core::BufferId;
use led_state::{AppState, BufferState, Dimensions, EditKind, SaveState};

use super::super::mov;

/// Run `f` on the active buffer, then ensure cursor stays visible.
pub(super) fn with_buf(state: &mut AppState, f: impl FnOnce(&mut BufferState, &Dimensions)) {
    let dims = match state.dims {
        Some(d) => d,
        None => return,
    };
    if let Some(id) = state.active_buffer {
        let snapshot = state
            .buffers
            .get(&id)
            .map(|b| (b.doc.line_count(), b.cursor_row, b.doc.version()));
        let Some((old_lines, edit_row, old_ver)) = snapshot else {
            return;
        };
        if let Some(buf) = state.buf_mut(id) {
            f(buf, &dims);
            let (sr, ssl) = mov::adjust_scroll(buf, &dims);
            buf.scroll_row = sr;
            buf.scroll_sub_line = ssl;
            buf.matching_bracket = led_state::BracketPair::find_match(
                &buf.bracket_pairs,
                buf.cursor_row,
                buf.cursor_col,
            );
            if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                buf.save_state = SaveState::Modified;
            }
            buf.last_used = Instant::now();
        }
        mov::shift_annotations(state, id, edit_row, old_lines, old_ver);
    }
}

/// Close undo group and clear edit kind tracking.
pub(crate) fn close_group_on_move(buf: &mut BufferState) {
    if buf.last_edit_kind.is_some() {
        buf.doc = buf.doc.close_undo_group();
        buf.last_edit_kind = None;
    }
}

/// Renumber tab_order to be contiguous 0..n, preserving relative order.
pub(crate) fn renumber_tabs(state: &mut AppState) {
    let mut ordered: Vec<BufferId> = state.buffers.keys().copied().collect();
    ordered.sort_by_key(|bid| state.buffers[bid].tab_order);
    for (i, bid) in ordered.into_iter().enumerate() {
        state.buf_mut(bid).unwrap().tab_order = i;
    }
}

pub(crate) fn reveal_active_buffer(state: &mut AppState) {
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

pub(crate) fn browser_scroll_to_selected(state: &mut AppState) {
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
pub(super) fn maybe_close_group(buf: &mut BufferState, kind: EditKind, ch: char) {
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

pub(super) fn is_editing_action(action: &led_core::Action) -> bool {
    use led_core::Action;
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

pub(super) fn should_record(action: &led_core::Action) -> bool {
    use led_core::Action;
    !matches!(
        action,
        Action::Quit | Action::Suspend | Action::Resize(..) | Action::Wait(..)
    )
}

/// Extract the word under the cursor.
pub(super) fn word_under_cursor(buf: &BufferState) -> String {
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
