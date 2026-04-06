use std::rc::Rc;

use led_core::CanonPath;
use led_core::PanelSlot;
use led_state::{AppState, BufferState, PreviewTab};

use super::helpers::reveal_active_buffer;

/// Set the preview to a new file. Creates or replaces the preview tab,
/// ensures a buffer placeholder exists, and sets the active tab.
/// The normal `tabs_needing_open` derived stream will materialize it.
pub(crate) fn set_preview(state: &mut AppState, path: CanonPath, row: usize, col: usize) {
    log::debug!(
        "[set_preview] path={} tabs={:?}",
        path.display(),
        state
            .tabs
            .iter()
            .map(|t| format!(
                "{}({})",
                t.path.display(),
                if t.is_preview() { "P" } else { "T" }
            ))
            .collect::<Vec<_>>(),
    );
    // If the path is already open as a real tab, just activate it.
    // Paths are CanonPath so simple == comparison works.
    if let Some(tab) = state
        .tabs
        .iter()
        .find(|t| !t.is_preview() && t.path == path)
    {
        log::debug!(
            "[set_preview] already a real tab, activating: {}",
            tab.path.display()
        );
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

    // Remove old preview tab. The fold dematerializes orphaned buffers.
    if let Some(idx) = state.tabs.iter().position(|t| t.is_preview()) {
        state.tabs.remove(idx);
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

    // Remove preview tab. The fold dematerializes orphaned buffers.
    state
        .tabs
        .retain(|t| t.path != preview_path || !t.is_preview());

    if let Some(restore) = restore_path {
        if state.buffers.contains_key(&restore) {
            state.active_tab = Some(restore);
            if state.focus == PanelSlot::Main {
                reveal_active_buffer(state);
            }
        }
    }
    if state.tabs.is_empty() {
        state.focus = PanelSlot::Side;
    }
}

pub(crate) fn promote_preview(state: &mut AppState, path: &CanonPath) -> bool {
    let Some(tab) = state
        .tabs
        .iter_mut()
        .find(|t| t.is_preview() && t.path == *path)
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
        .filter(|b| !b.path().map_or(false, |p| preview_paths.contains(p)))
        .filter(|b| b.path() != state.active_tab.as_ref())
        .filter(|b| !b.is_dirty())
        .min_by_key(|b| b.last_used())
        .and_then(|b| b.path().cloned());
    if let Some(path) = victim {
        if let Some(buf) = state.buf_mut(&path) {
            buf.dematerialize();
        }
        state.notify_hash_to_buffer.retain(|_, v| *v != path);
    }
}
