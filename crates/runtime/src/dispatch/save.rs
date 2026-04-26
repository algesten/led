//! Save-request helpers (M4, M6). Dispatch side only — the runtime's
//! query + execute phase turns `pending_saves` entries into actual
//! writes via `FileWriteDriver`.

use led_state_buffer_edits::BufferEdits;
use led_state_tabs::Tabs;

/// Insert the active tab's path into `pending_saves` if the
/// buffer is loaded. "Save should always save" — the dirty
/// check is deliberately absent so Ctrl-X Ctrl-D (SaveNoFormat)
/// on a clean buffer still touches disk, matching the Ctrl-X
/// Ctrl-S (Save with format-on-save) behaviour and the user's
/// explicit request.
pub(super) fn request_save_active(tabs: &Tabs, edits: &mut BufferEdits) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else {
        return;
    };
    if edits.buffers.contains_key(&tab.path) {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    
    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    
    
    use ropey::Rope;

    
    use super::super::testutil::*;
    
    

    // ── Save via legacy chord (ctrl+x ctrl+s) ───────────────────────────

    #[test]
    fn ctrl_x_ctrl_d_queues_direct_save_for_dirty_active_buffer() {
        // Ctrl-X Ctrl-D is SaveNoFormat — M18's "skip format"
        // path. Ctrl-X Ctrl-S now routes through format-on-save,
        // so directly-populating `pending_saves` is the
        // SaveNoFormat test's responsibility.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        // Force dirty by bumping version + clearing the disk anchor
        // so the rope's hash no longer matches.
        let eb = edits.buffers.get_mut(&canon("file.rs")).expect("seeded");
        eb.version = 1;
        eb.disk_content_hash = led_core::PersistedContentHash::default();
        assert!(eb.dirty());

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('d')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert!(edits.pending_saves.contains(&canon("file.rs")));
    }

    #[test]
    fn ctrl_x_ctrl_s_on_clean_buffer_writes_directly_when_no_lsp() {
        // "Save should always save": Ctrl-X Ctrl-S on a clean
        // buffer still writes to disk. With no LSP server seen
        // (the testutil fixture seeds an empty `LspStatuses`),
        // dispatch routes through the direct-save path —
        // mirrors legacy `save_of.rs` `!has_active_lsp(s)`.
        // pending_saves carries the path immediately so the
        // execute phase ships a `SaveAction::Save`.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.contains(&canon("file.rs")));
    }

    #[test]
    fn ctrl_x_ctrl_d_on_clean_buffer_still_queues_save() {
        // SaveNoFormat skips the LSP format round-trip but still
        // writes the buffer to disk — "save should always save"
        // applies to both variants.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('d')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.contains(&canon("file.rs")));
    }

    #[test]
    fn ctrl_x_ctrl_s_on_unloaded_buffer_is_noop() {
        let mut tabs = tabs_with(&[("file.rs", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.is_empty());
    }

    #[test]
    fn save_all_enqueues_every_dirty_buffer() {
        let mut tabs = tabs_with(&[("a", 1), ("b", 2), ("c", 3)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("A")),
                version: 1,
                saved_version: 0,
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        // b is clean.
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("B"))),
        );
        edits.buffers.insert(
            canon("c"),
            EditedBuffer {
                rope: Arc::new(Rope::from_str("C")),
                version: 2,
                saved_version: 0,
                disk_content_hash: led_core::PersistedContentHash::default(),
                history: Default::default(),
            },
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));

        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('a')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert!(edits.pending_saves.contains(&canon("a")));
        assert!(edits.pending_saves.contains(&canon("c")));
        assert!(!edits.pending_saves.contains(&canon("b")));
    }
}
