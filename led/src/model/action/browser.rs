use led_core::Action;
use led_state::{AppState, EntryKind};

use super::preview::{close_preview, promote_preview, set_preview};

pub(super) fn handle_browser_nav(state: &mut AppState, action: &Action) {
    let len = state.browser.entries.len();
    if len == 0 {
        return;
    }
    let height = state.dims.map_or(20, |d| d.buffer_height());
    let selected = state.browser.selected;
    let scroll_offset = state.browser.scroll_offset;
    let b = state.browser_mut();
    match action {
        Action::MoveUp => {
            b.selected = selected.saturating_sub(1);
        }
        Action::MoveDown => {
            b.selected = (selected + 1).min(len - 1);
        }
        Action::PageUp => {
            b.selected = selected.saturating_sub(height);
        }
        Action::PageDown => {
            b.selected = (selected + height).min(len - 1);
        }
        Action::FileStart => {
            b.selected = 0;
        }
        Action::FileEnd => {
            b.selected = len - 1;
        }
        _ => {}
    }
    // Keep selection visible
    if b.selected < scroll_offset {
        b.scroll_offset = b.selected;
    } else if b.selected >= scroll_offset + height {
        b.scroll_offset = b.selected + 1 - height;
    }

    // Emit preview for selected entry
    if let Some(entry) = state.browser.entries.get(state.browser.selected) {
        match &entry.kind {
            EntryKind::File => {
                set_preview(state, entry.path.clone(), 0, 0);
            }
            EntryKind::Directory { .. } => {
                close_preview(state);
            }
        }
    }
}

pub(super) fn handle_browser_expand(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    if !matches!(entry.kind, EntryKind::Directory { expanded: false }) {
        return;
    }
    let path = entry.path.clone();
    let has_contents = state.browser.dir_contents.contains_key(&path);
    let b = state.browser_mut();
    b.expanded_dirs.insert(path.clone());
    if has_contents {
        b.rebuild_entries();
    }
    // Always request a fresh listing so changes made while collapsed become visible.
    state.pending_lists.set(vec![path]);
}

pub(super) fn handle_browser_collapse(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    let collapse_path = match &entry.kind {
        EntryKind::Directory { expanded: true } => entry.path.clone(),
        _ => match entry.path.parent() {
            Some(parent) if state.browser.expanded_dirs.contains(parent) => parent.to_path_buf(),
            _ => return,
        },
    };
    let b = state.browser_mut();
    b.expanded_dirs.remove(&collapse_path);
    b.rebuild_entries();
    if let Some(pos) = b.entries.iter().position(|e| e.path == collapse_path) {
        b.selected = pos;
    }
}

pub(super) fn handle_browser_collapse_all(state: &mut AppState) {
    let b = state.browser_mut();
    b.expanded_dirs.clear();
    b.rebuild_entries();
    b.selected = 0;
    b.scroll_offset = 0;
}

pub(super) fn handle_browser_open(state: &mut AppState) {
    use led_core::PanelSlot;

    let Some(entry) = state.browser.entries.get(state.browser.selected).cloned() else {
        return;
    };
    match &entry.kind {
        EntryKind::File => {
            if promote_preview(state, &entry.path) {
                state.focus = PanelSlot::Main;
                return;
            }
            close_preview(state);
            super::super::request_open(state, entry.path.clone(), true);
            state.active_tab = Some(entry.path.clone());
            state.focus = PanelSlot::Main;
        }
        EntryKind::Directory { expanded } => {
            if *expanded {
                handle_browser_collapse(state);
            } else {
                handle_browser_expand(state);
            }
        }
    }
}
