use led_state::{AppState, JumpPosition};

const MAX_JUMP_LIST: usize = 100;

/// Record a jump position. Truncates forward history and caps at 100 entries.
pub fn record_jump(state: &mut AppState, pos: JumpPosition) {
    state.jump_list.truncate(state.jump_list_index);
    state.jump_list.push_back(pos);
    if state.jump_list.len() > MAX_JUMP_LIST {
        state.jump_list.pop_front();
        // index stays at len after pop_front since we added one and removed one
    }
    state.jump_list_index = state.jump_list.len();
}

/// Jump back in the jump list. If at present (index == len), saves current
/// position first so forward can return to it.
pub fn jump_back(state: &mut AppState) {
    if state.jump_list_index == 0 {
        return;
    }

    // If at present (past end of list), save current position
    if state.jump_list_index == state.jump_list.len() {
        if let Some(pos) = current_position(state) {
            state.jump_list.push_back(pos);
        }
    }

    state.jump_list_index -= 1;

    if let Some(pos) = state.jump_list.get(state.jump_list_index).cloned() {
        navigate_to_position(state, pos);
    }
}

/// Jump forward in the jump list.
pub fn jump_forward(state: &mut AppState) {
    if state.jump_list_index + 1 >= state.jump_list.len() {
        return;
    }

    state.jump_list_index += 1;

    if let Some(pos) = state.jump_list.get(state.jump_list_index).cloned() {
        navigate_to_position(state, pos);
    }
}

/// Get the current cursor position as a JumpPosition, if there's an active buffer with a path.
fn current_position(state: &AppState) -> Option<JumpPosition> {
    let id = state.active_buffer?;
    let buf = state.buffers.get(&id)?;
    let path = buf.path.clone()?;
    Some(JumpPosition {
        path,
        row: buf.cursor_row,
        col: buf.cursor_col,
        scroll_offset: buf.scroll_row,
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
        .find(|b| b.path.as_deref() == Some(&pos.path))
        .map(|b| b.id);

    if let Some(buf_id) = existing {
        state.active_buffer = Some(buf_id);
        if let Some(buf) = state.buffers.get_mut(&buf_id) {
            buf.cursor_row = pos.row.min(buf.doc.line_count().saturating_sub(1));
            buf.cursor_col = pos.col;
            buf.cursor_col_affinity = pos.col;
            buf.scroll_row = pos.scroll_offset;
        }
        super::action::reveal_active_buffer(state);
    } else {
        // File not open — request open and store pending position
        state.pending_open.set(Some(pos.path.clone()));
        state.pending_jump_position = Some(pos);
    }
}
