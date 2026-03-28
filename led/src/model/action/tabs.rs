use led_core::{BufferId, PanelSlot};
use led_state::AppState;

use super::helpers::{renumber_tabs, reveal_active_buffer};
use super::preview::close_preview;

pub(super) fn cycle_tab(state: &mut AppState, direction: i32) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    let mut tabs: Vec<(BufferId, usize)> = state
        .buffers
        .iter()
        .filter(|(_, buf)| !buf.is_preview)
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

pub(super) fn kill_buffer(state: &mut AppState) {
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
        state.alerts.warn = Some(format!("Buffer {filename} modified; kill anyway? (y or n)"));
        return;
    }

    do_kill_buffer(state, active_id);
}

pub(super) fn force_kill_buffer(state: &mut AppState) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    do_kill_buffer(state, active_id);
}

fn do_kill_buffer(state: &mut AppState, id: BufferId) {
    if state.preview.buffer == Some(id) {
        close_preview(state);
        return;
    }

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

    state.buffers_mut().remove(&id);
    state.active_buffer = next_active;
    reveal_active_buffer(state);
    renumber_tabs(state);

    if state.buffers.is_empty() {
        state.focus = PanelSlot::Side;
    }

    state.alerts.info = Some(format!("Killed {filename}"));
}
