use led_core::CanonPath;
use led_core::PanelSlot;
use led_state::AppState;

use super::helpers::reveal_active_buffer;
use super::preview::close_preview;

pub(super) fn cycle_tab(state: &mut AppState, direction: i32) {
    let Some(ref active_path) = state.active_tab else {
        return;
    };
    let tabs: Vec<&CanonPath> = state
        .tabs
        .iter()
        .filter(|t| !t.is_preview())
        .filter(|t| {
            state
                .buffers
                .get(t.path())
                .map_or(false, |b| b.is_materialized())
        })
        .map(|t| t.path())
        .collect();

    let Some(pos) = tabs.iter().position(|path| *path == active_path) else {
        return;
    };
    let len = tabs.len() as i32;
    let next = ((pos as i32 + direction).rem_euclid(len)) as usize;
    state.active_tab = Some(tabs[next].clone());
    reveal_active_buffer(state);
}

pub(super) fn kill_buffer(state: &mut AppState) {
    let Some(ref active_path) = state.active_tab else {
        return;
    };
    let Some(buf) = state.buffers.get(active_path) else {
        return;
    };

    if buf.is_dirty() {
        let filename = buf
            .path()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| active_path.to_string_lossy().into_owned());
        state.confirm_kill = true;
        state.alerts.info = Some(format!("Buffer {filename} modified; kill anyway? (y or n)"));
        return;
    }

    let active_path = active_path.clone();
    do_kill_buffer(state, &active_path);
}

pub(super) fn force_kill_buffer(state: &mut AppState) {
    let Some(active_path) = state.active_tab.clone() else {
        return;
    };
    do_kill_buffer(state, &active_path);
}

fn do_kill_buffer(state: &mut AppState, path: &CanonPath) {
    if state
        .tabs
        .iter()
        .any(|t| t.is_preview() && t.path() == path)
    {
        close_preview(state);
        return;
    }

    let Some(buf) = state.buffers.get(path) else {
        return;
    };

    let filename = buf
        .path()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Find next tab to activate: look at the killed tab's position in the VecDeque,
    // then pick the adjacent tab (next, or previous if at the end).
    let killed_index = state.tabs.iter().position(|t| t.path() == path);
    let next_active = killed_index.and_then(|idx| {
        let len = state.tabs.len();
        if len <= 1 {
            return None;
        }
        let next_idx = if idx + 1 < len { idx + 1 } else { idx - 1 };
        Some(state.tabs[next_idx].path().clone())
    });

    state.tabs.retain(|t| t.path() != path);
    state.active_tab = next_active;
    reveal_active_buffer(state);

    if state.tabs.is_empty() {
        state.focus = PanelSlot::Side;
    }

    state.alerts.info = Some(format!("Killed {filename}"));
}
