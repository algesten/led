//! Save-request helpers (M4, M6). Dispatch side only — the runtime's
//! query + execute phase turns `pending_saves` entries into actual
//! writes via `FileWriteDriver`.

use led_state_buffer_edits::BufferEdits;
use led_state_tabs::Tabs;

/// Insert the active tab's path into `pending_saves` iff the buffer
/// is loaded and dirty.
pub(super) fn request_save_active(tabs: &Tabs, edits: &mut BufferEdits) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return;
    };
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    if eb.dirty() {
        edits.pending_saves.insert(tab.path.clone());
    }
}

/// Insert every dirty-buffer path into `pending_saves`. Paths not
/// currently attached to any open tab are skipped — "save all" means
/// "save everything the user currently has open that has changed."
pub(super) fn request_save_all(tabs: &Tabs, edits: &mut BufferEdits) {
    for tab in tabs.open.iter() {
        let Some(eb) = edits.buffers.get(&tab.path) else {
            continue;
        };
        if eb.dirty() {
            edits.pending_saves.insert(tab.path.clone());
        }
    }
}
