use led_state::{AppState, JumpPosition};

const MAX_JUMP_LIST: usize = 100;

/// Record a jump position. Truncates forward history and caps at 100 entries.
pub fn record_jump(state: &mut AppState, pos: JumpPosition) {
    state.jump.entries.truncate(state.jump.index);
    state.jump.entries.push_back(pos);
    if state.jump.entries.len() > MAX_JUMP_LIST {
        state.jump.entries.pop_front();
    }
    state.jump.index = state.jump.entries.len();
}

/// Jump back in the jump list. If at present (index == len), saves current
/// position first so forward can return to it.
pub fn jump_back(state: &mut AppState) {
    if state.jump.index == 0 {
        return;
    }

    // If at present (past end of list), save current position
    if state.jump.index == state.jump.entries.len() {
        if let Some(pos) = current_position(state) {
            state.jump.entries.push_back(pos);
        }
    }

    state.jump.index -= 1;

    if let Some(pos) = state.jump.entries.get(state.jump.index).cloned() {
        navigate_to_position(state, pos);
    }
}

/// Jump forward in the jump list.
pub fn jump_forward(state: &mut AppState) {
    if state.jump.index + 1 >= state.jump.entries.len() {
        return;
    }

    state.jump.index += 1;

    if let Some(pos) = state.jump.entries.get(state.jump.index).cloned() {
        navigate_to_position(state, pos);
    }
}

/// Get the current cursor position as a JumpPosition, if there's an active buffer with a path.
fn current_position(state: &AppState) -> Option<JumpPosition> {
    let active_path = state.active_tab.as_ref()?;
    let buf = state.buffers.get(active_path)?;
    let path = buf.path()?.clone();
    Some(JumpPosition {
        path,
        row: buf.cursor_row(),
        col: buf.cursor_col(),
        scroll_offset: buf.scroll_row(),
    })
}

/// Navigate to a jump position. If the target buffer is already open, activate it
/// and set cursor/scroll. Otherwise, open the file and store the position for
/// application after the buffer opens.
fn navigate_to_position(state: &mut AppState, pos: JumpPosition) {
    // Try to find an already-open buffer with this path
    let existing = state
        .buffers
        .values()
        .find(|b| b.path() == Some(&pos.path))
        .and_then(|b| b.path().cloned());

    if let Some(buf_path) = existing {
        state.active_tab = Some(buf_path.clone());
        if let Some(buf) = state.buf_mut(&buf_path) {
            let row = led_core::Row((*pos.row).min(buf.doc().line_count().saturating_sub(1)));
            buf.set_cursor(row, pos.col, pos.col);
            buf.set_scroll(pos.scroll_offset, buf.scroll_sub_line());
        }
        super::action::reveal_active_buffer(state);
    } else {
        // File not open — request open with pending cursor on tab.
        super::request_open(state, pos.path.clone(), false);
        if let Some(tab) = state.tabs.iter_mut().find(|t| *t.path() == pos.path) {
            tab.set_cursor(pos.row, pos.col, pos.scroll_offset);
        }
        state.active_tab = Some(pos.path);
    }
}
