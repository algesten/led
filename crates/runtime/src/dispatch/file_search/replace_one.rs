use led_core::SavedVersion;
use led_state_buffer_edits::BufferEdits;
use led_state_file_search::{FileSearchSelection, FileSearchState};
use led_state_tabs::Tabs;

use super::{advance_to_next_pending, ensure_replacements_len};

/// `CursorRight` on a selected hit (replace_mode on) — if the hit
/// is still pending, apply the replacement and mark the row
/// replaced. Rows stay visible in the tree either way, so
/// Left-arrow on a specific replaced row can undo just that one
/// without disturbing others. Advances selection to the next
/// pending hit when one's available (wraps to the first pending).
pub(super) fn replace_selected(
    state: &mut FileSearchState,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
) {
    let FileSearchSelection::Result(idx) = state.selection else {
        return;
    };
    let Some(hit) = state.flat_hits.get(idx).cloned() else {
        return;
    };
    // Ensure the replacements vec is at least as long as flat_hits
    // — defensive in case something (a test, a stray path) bypassed
    // the runtime's post-search resize.
    ensure_replacements_len(state);
    // Already replaced — Right on an already-handled row is a
    // no-op (user can Left to undo).
    if state.hit_replacements[idx].is_some() {
        advance_to_next_pending(state);
        return;
    }
    if state.query.text.is_empty() && state.replace.text.is_empty() {
        return;
    }
    let original_char_len = hit
        .preview
        .get(hit.match_start..hit.match_end)
        .map(|s| s.chars().count())
        .unwrap_or(0);
    if original_char_len == 0 {
        return;
    }
    let original_text = hit
        .preview
        .get(hit.match_start..hit.match_end)
        .unwrap_or("")
        .to_string();
    let replacement = state.replace.text.clone();
    let replacement_char_len = replacement.chars().count();

    // "Is this file a real (non-preview) loaded buffer?" — the
    // one case where replace stays in-memory + dirty so the user
    // can review before saving. Preview tabs and unloaded files
    // both go through the driver (direct-to-disk) and keep their
    // buffers (if any) clean.
    let is_owned_buffer = edits.buffers.contains_key(&hit.path)
        && tabs
            .open
            .iter()
            .any(|t| t.path == hit.path && !t.preview);

    let rope_char_start = if is_owned_buffer {
        let eb = edits.buffers.get_mut(&hit.path).expect("checked above");
        let line0 = hit.line.saturating_sub(1);
        if line0 >= eb.rope.len_lines() {
            return;
        }
        let line_start = eb.rope.line_to_char(line0);
        let match_char_start = line_start + hit.col.saturating_sub(1);
        let match_char_end = match_char_start + original_char_len;
        if match_char_end > eb.rope.len_chars() {
            return;
        }
        let mut new_rope = (*eb.rope).clone();
        new_rope.remove(match_char_start..match_char_end);
        if !replacement.is_empty() {
            new_rope.insert(match_char_start, &replacement);
        }
        let cursor_anchor = led_state_tabs::Cursor {
            line: line0,
            col: hit.col.saturating_sub(1),
            preferred_col: hit.col.saturating_sub(1),
        };
        eb.history.record_replace(
            match_char_start,
            std::sync::Arc::<str>::from(original_text.as_str()),
            std::sync::Arc::<str>::from(replacement.as_str()),
            cursor_anchor,
            cursor_anchor,
            Some(led_state_buffer_edits::FileSearchMark {
                hit_idx: idx,
                forward_marks_replaced: true,
                disk_write: false,
            }),
        );
        super::super::shared::bump(eb, new_rope);
        match_char_start
    } else if let Some(eb) = edits.buffers.get_mut(&hit.path) {
        // Preview path: rope IS loaded (preview is a lens), but
        // the buffer is not user-owned. Apply the edit to the
        // rope for display, fire the driver to write disk, and
        // keep `saved_version == version` so the buffer stays
        // clean. Also record a history group tagged `disk_write`
        // so undo/redo can fire the inverse driver cmd.
        let line0 = hit.line.saturating_sub(1);
        if line0 >= eb.rope.len_lines() {
            return;
        }
        let line_start = eb.rope.line_to_char(line0);
        let match_char_start = line_start + hit.col.saturating_sub(1);
        let match_char_end = match_char_start + original_char_len;
        if match_char_end > eb.rope.len_chars() {
            return;
        }
        let mut new_rope = (*eb.rope).clone();
        new_rope.remove(match_char_start..match_char_end);
        if !replacement.is_empty() {
            new_rope.insert(match_char_start, &replacement);
        }
        let cursor_anchor = led_state_tabs::Cursor {
            line: line0,
            col: hit.col.saturating_sub(1),
            preferred_col: hit.col.saturating_sub(1),
        };
        eb.history.record_replace(
            match_char_start,
            std::sync::Arc::<str>::from(original_text.as_str()),
            std::sync::Arc::<str>::from(replacement.as_str()),
            cursor_anchor,
            cursor_anchor,
            Some(led_state_buffer_edits::FileSearchMark {
                hit_idx: idx,
                forward_marks_replaced: true,
                disk_write: true,
            }),
        );
        super::super::shared::bump(eb, new_rope);
        eb.saved_version = SavedVersion(eb.version.0);
        eb.disk_content_hash =
            led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
        edits.pending_single_replace.push(
            led_state_buffer_edits::PendingSingleReplace {
                path: hit.path.clone(),
                line: hit.line,
                match_start: hit.match_start,
                match_end: hit.match_end,
                original: original_text.clone(),
                replacement: replacement.clone(),
            },
        );
        match_char_start
    } else {
        // Unloaded file: no rope in edits.buffers. Pure driver
        // path. `rope_char_start` is meaningless here; undo uses
        // (line, col) via the driver inverse cmd.
        edits.pending_single_replace.push(
            led_state_buffer_edits::PendingSingleReplace {
                path: hit.path.clone(),
                line: hit.line,
                match_start: hit.match_start,
                match_end: hit.match_end,
                original: original_text.clone(),
                replacement: replacement.clone(),
            },
        );
        0
    };

    state.hit_replacements[idx] = Some(led_state_file_search::ReplaceEntry {
        hit: hit.clone(),
        replacement_text: replacement,
        replacement_char_len,
        original_char_len,
        rope_char_start,
        path: hit.path.clone(),
    });

    advance_to_next_pending(state);
}

/// `CursorLeft` on a selected hit — if the hit is marked replaced,
/// revert it. Selection stays on the row so the user can
/// immediately Right-arrow to redo if wanted.
pub(super) fn unreplace_selected(
    state: &mut FileSearchState,
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
) {
    let FileSearchSelection::Result(idx) = state.selection else {
        return;
    };
    ensure_replacements_len(state);
    let Some(entry) = state.hit_replacements[idx].take() else {
        return;
    };
    let original = entry
        .hit
        .preview
        .get(entry.hit.match_start..entry.hit.match_end)
        .unwrap_or("")
        .to_string();
    let is_owned_buffer = edits.buffers.contains_key(&entry.path)
        && tabs
            .open
            .iter()
            .any(|t| t.path == entry.path && !t.preview);
    if is_owned_buffer {
        let eb = edits.buffers.get_mut(&entry.path).expect("checked above");
        let rope_len = eb.rope.len_chars();
        let replacement_end = entry
            .rope_char_start
            .saturating_add(entry.replacement_char_len);
        if replacement_end > rope_len {
            state.hit_replacements[idx] = Some(entry);
            return;
        }
        let mut new_rope = (*eb.rope).clone();
        if entry.replacement_char_len > 0 {
            new_rope.remove(entry.rope_char_start..replacement_end);
        }
        if !original.is_empty() {
            new_rope.insert(entry.rope_char_start, &original);
        }
        let line0 = entry.hit.line.saturating_sub(1);
        let col0 = entry.hit.col.saturating_sub(1);
        let cursor_anchor = led_state_tabs::Cursor {
            line: line0,
            col: col0,
            preferred_col: col0,
        };
        eb.history.record_replace(
            entry.rope_char_start,
            std::sync::Arc::<str>::from(entry.replacement_text.as_str()),
            std::sync::Arc::<str>::from(original.as_str()),
            cursor_anchor,
            cursor_anchor,
            Some(led_state_buffer_edits::FileSearchMark {
                hit_idx: idx,
                forward_marks_replaced: false,
                disk_write: false,
            }),
        );
        super::super::shared::bump(eb, new_rope);
    } else if let Some(eb) = edits.buffers.get_mut(&entry.path) {
        // Preview inverse: same shape as owned-buffer inverse but
        // also fires driver cmd + keeps saved_version pinned.
        let rope_len = eb.rope.len_chars();
        let replacement_end = entry
            .rope_char_start
            .saturating_add(entry.replacement_char_len);
        if replacement_end > rope_len {
            state.hit_replacements[idx] = Some(entry);
            return;
        }
        let mut new_rope = (*eb.rope).clone();
        if entry.replacement_char_len > 0 {
            new_rope.remove(entry.rope_char_start..replacement_end);
        }
        if !original.is_empty() {
            new_rope.insert(entry.rope_char_start, &original);
        }
        let line0 = entry.hit.line.saturating_sub(1);
        let col0 = entry.hit.col.saturating_sub(1);
        let cursor_anchor = led_state_tabs::Cursor {
            line: line0,
            col: col0,
            preferred_col: col0,
        };
        eb.history.record_replace(
            entry.rope_char_start,
            std::sync::Arc::<str>::from(entry.replacement_text.as_str()),
            std::sync::Arc::<str>::from(original.as_str()),
            cursor_anchor,
            cursor_anchor,
            Some(led_state_buffer_edits::FileSearchMark {
                hit_idx: idx,
                forward_marks_replaced: false,
                disk_write: true,
            }),
        );
        super::super::shared::bump(eb, new_rope);
        eb.saved_version = SavedVersion(eb.version.0);
        eb.disk_content_hash =
            led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
        let replacement_bytes = entry.replacement_text.len();
        let replacement_end_byte = entry.hit.match_start + replacement_bytes;
        edits.pending_single_replace.push(
            led_state_buffer_edits::PendingSingleReplace {
                path: entry.path.clone(),
                line: entry.hit.line,
                match_start: entry.hit.match_start,
                match_end: replacement_end_byte,
                original: entry.replacement_text.clone(),
                replacement: original.clone(),
            },
        );
    } else {
        let replacement_bytes = entry.replacement_text.len();
        let replacement_end_byte = entry.hit.match_start + replacement_bytes;
        edits.pending_single_replace.push(
            led_state_buffer_edits::PendingSingleReplace {
                path: entry.path.clone(),
                line: entry.hit.line,
                match_start: entry.hit.match_start,
                match_end: replacement_end_byte,
                original: entry.replacement_text.clone(),
                replacement: original,
            },
        );
    }
}
