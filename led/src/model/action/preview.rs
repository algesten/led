use std::path::{Path, PathBuf};
use std::rc::Rc;

use led_core::PanelSlot;
use led_state::{AppState, BufferState, PreviewTab};

use super::helpers::reveal_active_buffer;

/// Set the preview to a new file. Creates or replaces the preview tab,
/// ensures a buffer placeholder exists, and sets the active tab.
/// The normal `tabs_needing_open` derived stream will materialize it.
pub(crate) fn set_preview(state: &mut AppState, path: PathBuf, row: usize, col: usize) {
    // If the path is already open as a real tab, just activate it.
    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if let Some(tab) = state.tabs.iter().find(|t| {
        !t.is_preview()
            && std::fs::canonicalize(&t.path).unwrap_or_else(|_| t.path.clone()) == canonical
    }) {
        state.active_tab = Some(tab.path.clone());
        return;
    }

    // If the path is already the active preview, just reposition the cursor.
    if state.tabs.iter().any(|t| t.is_preview() && t.path == path) {
        state.active_tab = Some(path.clone());
        let dims = state.dims;
        if let Some(buf) = state.buf_mut(&path) {
            if buf.is_materialized() {
                let r = row.min(buf.doc().line_count().saturating_sub(1));
                buf.set_cursor(led_core::Row(r), led_core::Col(col), led_core::Col(col));
                let buffer_height = dims.map_or(20, |d| d.buffer_height());
                buf.set_scroll(
                    led_core::Row(r.saturating_sub(buffer_height / 2)),
                    led_core::SubLine(0),
                );
            }
        }
        return;
    }

    // Remember the previous active tab (only on first preview entry).
    let previous_tab = if let Some(tab) = state.tabs.iter().find(|t| t.is_preview()) {
        tab.preview.as_ref().unwrap().previous_tab.clone()
    } else {
        state.active_tab.clone().unwrap_or_default()
    };

    // Remove old preview tab and its buffer (if not used by a real tab).
    if let Some(idx) = state.tabs.iter().position(|t| t.is_preview()) {
        let old_path = state.tabs[idx].path.clone();
        state.tabs.remove(idx);
        let still_in_tabs = state.tabs.iter().any(|t| t.path == old_path);
        if !still_in_tabs {
            state.buffers_mut().remove(&old_path);
            state.notify_hash_to_buffer.retain(|_, v| *v != old_path);
        }
    }

    // Insert the new preview tab.
    state.tabs.push_back(led_state::Tab {
        path: path.clone(),
        preview: Some(PreviewTab { previous_tab }),
    });

    // Ensure buffer placeholder exists for materialization.
    if !state.buffers.contains_key(&path) {
        state
            .buffers_mut()
            .insert(path.clone(), Rc::new(BufferState::new(path.clone())));
    }

    state.active_tab = Some(path.clone());

    // Set pending jump position for cursor placement when the buffer loads.
    if row > 0 || col > 0 {
        state.jump.pending_position = Some(led_state::JumpPosition {
            path,
            row,
            col,
            scroll_offset: 0,
        });
    }
}

pub(crate) fn close_preview(state: &mut AppState) {
    let preview_tab = state.tabs.iter().find(|t| t.is_preview()).cloned();
    let Some(tab) = preview_tab else {
        return;
    };
    let preview_path = tab.path.clone();
    let restore_path = tab.preview.as_ref().map(|p| p.previous_tab.clone());

    state
        .tabs
        .retain(|t| t.path != preview_path || !t.is_preview());
    let still_in_tabs = state.tabs.iter().any(|t| t.path == preview_path);
    if !still_in_tabs {
        state.buffers_mut().remove(&preview_path);
        state
            .notify_hash_to_buffer
            .retain(|_, v| *v != preview_path);
    }

    if let Some(restore) = restore_path {
        if state.buffers.contains_key(&restore) {
            state.active_tab = Some(restore);
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
    let Some(tab) = state
        .tabs
        .iter_mut()
        .find(|t| t.is_preview() && t.path == path)
    else {
        return false;
    };
    tab.preview = None;
    true
}

pub(super) fn promote_preview_active(state: &mut AppState) {
    if let Some(tab) = state.tabs.iter_mut().find(|t| t.is_preview()) {
        tab.preview = None;
    }
}

pub(crate) fn evict_one_buffer(state: &mut AppState) {
    let preview_paths: std::collections::HashSet<_> = state
        .tabs
        .iter()
        .filter(|t| t.is_preview())
        .map(|t| &t.path)
        .collect();
    let victim = state
        .buffers
        .values()
        .filter(|b| b.is_materialized())
        .filter(|b| !b.path_buf().map_or(false, |p| preview_paths.contains(p)))
        .filter(|b| b.path_buf() != state.active_tab.as_ref())
        .filter(|b| !b.is_dirty())
        .min_by_key(|b| b.last_used())
        .and_then(|b| b.path_buf().cloned());
    if let Some(path) = victim {
        state.buffers_mut().remove(&path);
        state.notify_hash_to_buffer.retain(|_, v| *v != path);
    }
}
