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
        // Force dirty by bumping version past saved_version.
        let eb = edits.buffers.get_mut(&canon("file.rs")).expect("seeded");
        eb.version = 1;
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
    fn ctrl_x_ctrl_s_on_clean_buffer_is_noop() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hi", Dims { cols: 10, rows: 5 });
        // Buffer is fresh (version == saved_version == 0).
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
