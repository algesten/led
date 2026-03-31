use std::path::Path;

use led_core::PanelSlot;
use led_state::AppState;

use super::helpers::{renumber_tabs, reveal_active_buffer};

pub(crate) fn close_preview(state: &mut AppState) {
    if let Some(preview_path) = state.preview.buffer.take() {
        state.buffers_mut().remove(&preview_path);
        state
            .notify_hash_to_buffer
            .retain(|_, v| *v != preview_path);
        renumber_tabs(state);
    }
    if let Some(restore_path) = state.preview.pre_preview_buffer.take() {
        if state.buffers.contains_key(&restore_path) {
            state.active_buffer = Some(restore_path);
            // Only reveal in the browser when focus is on the editor.
            // When browsing the side panel, revealing would jump the
            // browser selection away from where the user is navigating.
            if state.focus == PanelSlot::Main {
                reveal_active_buffer(state);
            }
        }
    }
    if state.buffers.is_empty() {
        state.focus = PanelSlot::Side;
    }
}

pub(crate) fn promote_preview(state: &mut AppState, path: &Path) -> bool {
    let Some(ref preview_path) = state.preview.buffer else {
        return false;
    };
    let matches = state
        .buffers
        .get(preview_path)
        .and_then(|b| b.path_buf())
        .map_or(false, |p| p == path);
    if !matches {
        return false;
    }
    let preview_path = preview_path.clone();
    if let Some(buf) = state.buf_mut(&preview_path) {
        buf.set_preview(false);
    }
    state.preview.buffer = None;
    state.preview.pre_preview_buffer = None;
    true
}

pub(super) fn promote_preview_active(state: &mut AppState) {
    if let Some(preview_path) = state.preview.buffer.take() {
        if let Some(buf) = state.buf_mut(&preview_path) {
            buf.set_preview(false);
        }
        state.preview.pre_preview_buffer = None;
    }
}

pub(crate) fn evict_one_buffer(state: &mut AppState) {
    let victim = state
        .buffers
        .values()
        .filter(|b| b.is_materialized() && !b.is_preview())
        .filter(|b| b.path_buf() != state.active_buffer.as_ref())
        .filter(|b| !b.is_dirty())
        .min_by_key(|b| b.last_used())
        .and_then(|b| b.path_buf().cloned());
    if let Some(path) = victim {
        state.buffers_mut().remove(&path);
        state.notify_hash_to_buffer.retain(|_, v| *v != path);
        renumber_tabs(state);
    }
}
