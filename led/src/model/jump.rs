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
