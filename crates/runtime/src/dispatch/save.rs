//! Save-request helpers (M4, M6). Dispatch side only — the runtime's
//! query + execute phase turns `pending_saves` entries into actual
//! writes via `FileWriteDriver`.

use led_core::CanonPath;
use led_state_buffer_edits::{BufferEdits, EditedBuffer};
use led_state_tabs::{Cursor, Tabs};
use ropey::Rope;
use std::sync::Arc;

/// Insert the active tab's path into `pending_saves` if the
/// buffer is loaded. "Save should always save" — the dirty
/// check is deliberately absent so Ctrl-X Ctrl-D (SaveNoFormat)
/// on a clean buffer still touches disk, matching the Ctrl-X
/// Ctrl-S (Save with format-on-save) behaviour and the user's
/// explicit request.
///
/// Pre-save cleanup (strip trailing whitespace, ensure final
/// newline) runs first so the bytes we ship match what the user
/// expects from "save". The cleanup lands as one undo group, so
/// `Ctrl-/` after a save brings the original whitespace back.
pub(super) fn request_save_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let path = tabs.open[idx].path.clone();
    if !edits.buffers.contains_key(&path) {
        return;
    }
    let cursor = tabs.open[idx].cursor;
    if let Some(eb) = edits.buffers.get_mut(&path) {
        let new_cursor = apply_save_cleanup(eb, cursor);
        if new_cursor != cursor {
            tabs.open[idx].cursor = new_cursor;
        }
    }
    edits.pending_saves.insert(path);
}

/// Insert every dirty-buffer path into `pending_saves`. Paths not
/// currently attached to any open tab are skipped — "save all" means
/// "save everything the user currently has open that has changed."
///
/// Each saved buffer is cleaned the same way `request_save_active`
/// cleans the active one (pre-save trim + final newline as a single
/// undo group).
pub(super) fn request_save_all(tabs: &mut Tabs, edits: &mut BufferEdits) {
    let dirty_paths: Vec<(usize, CanonPath, Cursor)> = tabs
        .open
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            edits
                .buffers
                .get(&t.path)
                .filter(|eb| eb.dirty())
                .map(|_| (i, t.path.clone(), t.cursor))
        })
        .collect();
    for (idx, path, cursor) in dirty_paths {
        if let Some(eb) = edits.buffers.get_mut(&path) {
            let new_cursor = apply_save_cleanup(eb, cursor);
            if new_cursor != cursor {
                tabs.open[idx].cursor = new_cursor;
            }
        }
        edits.pending_saves.insert(path);
    }
}

/// Pre-save buffer cleanup: strip trailing whitespace from every
/// line, append a trailing newline if the rope doesn't end in one.
/// Lands as a single undo group via `record_replace_batch` so a
/// post-save `Ctrl-/` restores the pre-cleanup bytes.
///
/// Returns the cursor after the cleanup — clamped to the new line
/// length when its grapheme col landed inside stripped whitespace,
/// unchanged otherwise. Caller writes it back onto the active tab.
///
/// No-op when the rope is empty or already clean (no edit
/// recorded, no version bump). Idempotent — saving twice in a row
/// records exactly one cleanup group, not two.
pub(crate) fn apply_save_cleanup(eb: &mut EditedBuffer, cursor_before: Cursor) -> Cursor {
    let total_chars = eb.rope.len_chars();
    if total_chars == 0 {
        return cursor_before;
    }
    let line_count = eb.rope.len_lines();
    // Build the (at, removed, inserted) batch in any order — we
    // sort descending by `at` before applying so each strip's
    // position stays valid at apply time.
    let mut replaces: Vec<(usize, Arc<str>, Arc<str>)> = Vec::new();
    if eb.rope.char(total_chars - 1) != '\n' {
        replaces.push((total_chars, Arc::from(""), Arc::from("\n")));
    }
    for line_idx in 0..line_count {
        let line_slice = eb.rope.line(line_idx);
        let line_str: String = line_slice.chars().collect();
        let body = line_str.trim_end_matches(['\n', '\r']);
        let body_chars = body.chars().count();
        if body_chars == 0 {
            continue;
        }
        let trimmed = body.trim_end_matches([' ', '\t']);
        let trimmed_chars = trimmed.chars().count();
        if trimmed_chars == body_chars {
            continue;
        }
        let line_start_char = eb.rope.line_to_char(line_idx);
        let strip_start = line_start_char + trimmed_chars;
        let strip_end = line_start_char + body_chars;
        let removed: String = eb.rope.slice(strip_start..strip_end).to_string();
        replaces.push((
            strip_start,
            Arc::<str>::from(removed.as_str()),
            Arc::from(""),
        ));
    }
    if replaces.is_empty() {
        return cursor_before;
    }
    // Descending position so the highest-position edit applies
    // first; strips at lower positions stay valid because the
    // higher-position edits don't shift them.
    replaces.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut new_rope: Rope = (*eb.rope).clone();
    for (at, removed, inserted) in &replaces {
        let len_removed = removed.chars().count();
        if len_removed > 0 {
            new_rope.remove(*at..*at + len_removed);
        }
        if !inserted.is_empty() {
            new_rope.insert(*at, inserted);
        }
    }
    let new_rope = Arc::new(new_rope);
    // Clamp the cursor: cleanup never removes whole lines, so
    // `cursor.line` is still valid; only `col` can land past the
    // new line end (cursor sat in stripped trailing whitespace).
    let mut cursor_after = cursor_before;
    let new_line_count = new_rope.len_lines();
    if cursor_after.line >= new_line_count {
        cursor_after.line = new_line_count.saturating_sub(1);
    }
    let new_line_grapheme_count =
        led_core::line_grapheme_len(new_rope.line(cursor_after.line));
    if cursor_after.col > new_line_grapheme_count {
        cursor_after.col = new_line_grapheme_count;
        cursor_after.preferred_col = new_line_grapheme_count;
    }
    eb.rope = new_rope;
    eb.version.0 = eb.version.0.saturating_add(1);
    eb.history
        .record_replace_batch(replaces, cursor_before, cursor_after);
    cursor_after
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    
    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    use led_state_tabs::Cursor;


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
        eb.version = led_core::BufferVersion(1);
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
                version: led_core::BufferVersion(1),
                saved_version: led_core::SavedVersion(0),
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
                version: led_core::BufferVersion(2),
                saved_version: led_core::SavedVersion(0),
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

    // ── Save cleanup (trim trailing whitespace, ensure final newline) ──

    fn buffer_from(rope_str: &str) -> EditedBuffer {
        EditedBuffer::fresh(Arc::new(Rope::from_str(rope_str)))
    }

    #[test]
    fn cleanup_strips_trailing_whitespace_per_line() {
        let mut eb = buffer_from("hello   \nworld\t\t\n");
        let cursor = Cursor::default();
        super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "hello\nworld\n");
    }

    #[test]
    fn cleanup_appends_final_newline_when_missing() {
        let mut eb = buffer_from("no newline at end");
        let cursor = Cursor::default();
        super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "no newline at end\n");
    }

    #[test]
    fn cleanup_strips_and_adds_newline_in_one_pass() {
        let mut eb = buffer_from("trailing   ");
        let cursor = Cursor::default();
        super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "trailing\n");
    }

    #[test]
    fn cleanup_clean_buffer_is_noop() {
        let mut eb = buffer_from("hello\nworld\n");
        let v0 = eb.version;
        let cursor = Cursor::default();
        super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "hello\nworld\n");
        assert_eq!(eb.version, v0, "no edit recorded on clean buffer");
    }

    #[test]
    fn cleanup_empty_buffer_is_noop() {
        let mut eb = buffer_from("");
        let v0 = eb.version;
        let cursor = Cursor::default();
        super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "");
        assert_eq!(eb.version, v0);
    }

    #[test]
    fn cleanup_preserves_blank_lines_inside_buffer() {
        // Blank lines (including ones with only whitespace) get
        // trimmed but stay as line terminators — we don't collapse
        // multiple blank lines or strip trailing blank lines.
        let mut eb = buffer_from("a\n   \n\nb\n");
        let cursor = Cursor::default();
        super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "a\n\n\nb\n");
    }

    #[test]
    fn cleanup_is_undoable() {
        // Save cleanup lands as one undo group. Ctrl-/ after save
        // restores the pre-cleanup bytes verbatim.
        let mut eb = buffer_from("hello   \n");
        let cursor = Cursor {
            line: 0,
            col: 5,
            preferred_col: 5,
        };
        super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "hello\n");

        // Apply the inverse: reverse-walk the undo group's ops.
        let group = eb.history.take_undo().expect("cleanup recorded a group");
        let mut rope = (*eb.rope).clone();
        for op in group.ops.iter().rev() {
            match op {
                led_state_buffer_edits::EditOp::Insert { at, text } => {
                    let len = text.chars().count();
                    rope.remove(*at..*at + len);
                }
                led_state_buffer_edits::EditOp::Delete { at, text } => {
                    rope.insert(*at, text);
                }
            }
        }
        assert_eq!(rope.to_string(), "hello   \n");
    }

    #[test]
    fn cleanup_clamps_cursor_when_it_sits_in_stripped_whitespace() {
        let mut eb = buffer_from("hello   \n");
        let cursor = Cursor {
            line: 0,
            col: 8, // sits past "hello", inside the trailing spaces
            preferred_col: 8,
        };
        let after = super::apply_save_cleanup(&mut eb, cursor);
        assert_eq!(eb.rope.to_string(), "hello\n");
        assert_eq!(after.col, 5);
        assert_eq!(after.preferred_col, 5);
    }

    #[test]
    fn cleanup_via_save_dispatch_strips_trailing_whitespace() {
        // End-to-end: dispatching Ctrl-X Ctrl-S routes through
        // request_save_active, which now runs the cleanup before
        // queueing the save.
        let (mut tabs, mut edits, store, term) = fixture_with_content(
            "hello   \nworld\n",
            Dims { cols: 20, rows: 5 },
        );
        dispatch_chord_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('x')),
            key(KeyModifiers::CONTROL, KeyCode::Char('s')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hello\nworld\n");
        assert!(edits.pending_saves.contains(&canon("file.rs")));
    }
}
