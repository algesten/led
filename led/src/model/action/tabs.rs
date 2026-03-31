use std::path::PathBuf;

use led_core::PanelSlot;
use led_state::AppState;

use super::helpers::{renumber_tabs, reveal_active_buffer};
use super::preview::close_preview;

pub(super) fn cycle_tab(state: &mut AppState, direction: i32) {
    let Some(ref active_path) = state.active_buffer else {
        return;
    };
    let mut tabs: Vec<(PathBuf, usize)> = state
        .buffers
        .iter()
        .filter(|(_, buf)| buf.is_materialized() && !buf.is_preview())
        .map(|(path, buf)| (path.clone(), buf.tab_order().0))
        .collect();
    tabs.sort_by_key(|&(_, order)| order);

    let Some(pos) = tabs.iter().position(|(path, _)| path == active_path) else {
        return;
    };
    let len = tabs.len() as i32;
    let next = ((pos as i32 + direction).rem_euclid(len)) as usize;
    state.active_buffer = Some(tabs[next].0.clone());
    reveal_active_buffer(state);
}

pub(super) fn kill_buffer(state: &mut AppState) {
    let Some(ref active_path) = state.active_buffer else {
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
        state.alerts.warn = Some(format!("Buffer {filename} modified; kill anyway? (y or n)"));
        return;
    }

    let active_path = active_path.clone();
    do_kill_buffer(state, &active_path);
}

pub(super) fn force_kill_buffer(state: &mut AppState) {
    let Some(active_path) = state.active_buffer.clone() else {
        return;
    };
    do_kill_buffer(state, &active_path);
}

fn do_kill_buffer(state: &mut AppState, path: &std::path::Path) {
    if state.preview.buffer.as_deref() == Some(path) {
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
    let killed_order = buf.tab_order().0;

    // Find next buffer to activate (next by tab_order, wrapping)
    let mut tabs: Vec<(PathBuf, usize)> = state
        .buffers
        .iter()
        .filter(|(p, buf)| p.as_path() != path && buf.is_materialized())
        .map(|(p, buf)| (p.clone(), buf.tab_order().0))
        .collect();
    tabs.sort_by_key(|&(_, order)| order);

    let next_active = tabs
        .iter()
        .find(|&&(_, order)| order > killed_order)
        .or_else(|| tabs.last())
        .map(|(p, _)| p.clone());

    state.buffers_mut().remove(path);
    state.active_buffer = next_active;
    reveal_active_buffer(state);
    renumber_tabs(state);

    if state.buffers.is_empty() {
        state.focus = PanelSlot::Side;
    }

    state.alerts.info = Some(format!("Killed {filename}"));
}
