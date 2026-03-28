use std::path::Path;

use led_core::PanelSlot;
use led_state::AppState;

use super::helpers::{renumber_tabs, reveal_active_buffer};

pub(crate) fn close_preview(state: &mut AppState) {
    if let Some(preview_id) = state.preview.buffer.take() {
        state.buffers_mut().remove(&preview_id);
        state.notify_hash_to_buffer.retain(|_, v| *v != preview_id);
        renumber_tabs(state);
    }
    if let Some(restore_id) = state.preview.pre_preview_buffer.take() {
        if state.buffers.contains_key(&restore_id) {
            state.active_buffer = Some(restore_id);
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
    let Some(preview_id) = state.preview.buffer else {
        return false;
    };
    let matches = state
        .buffers
        .get(&preview_id)
        .and_then(|b| b.path.as_ref())
        .map_or(false, |p| p == path);
    if !matches {
        return false;
    }
    if let Some(buf) = state.buf_mut(preview_id) {
        buf.is_preview = false;
    }
    state.preview.buffer = None;
    state.preview.pre_preview_buffer = None;
    true
}

pub(super) fn promote_preview_active(state: &mut AppState) {
    if let Some(preview_id) = state.preview.buffer.take() {
        if let Some(buf) = state.buf_mut(preview_id) {
            buf.is_preview = false;
        }
        state.preview.pre_preview_buffer = None;
    }
}

pub(crate) fn evict_one_buffer(state: &mut AppState) {
    let victim = state
        .buffers
        .values()
        .filter(|b| !b.is_preview)
        .filter(|b| Some(b.id) != state.active_buffer)
        .filter(|b| !b.doc.dirty())
        .min_by_key(|b| b.last_used)
        .map(|b| b.id);
    if let Some(id) = victim {
        state.buffers_mut().remove(&id);
        state.notify_hash_to_buffer.retain(|_, v| *v != id);
        renumber_tabs(state);
    }
}
