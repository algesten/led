//! Kill-region / kill-line / yank (M7).
//!
//! Writes to both the kill ring and a pending clipboard-write slot
//! so the runtime's execute phase can push the text to the system
//! clipboard. `apply_yank` is the ingest-side counterpart — the
//! runtime calls it when the clipboard driver reports text.

use led_state_buffer_edits::BufferEdits;
use led_state_clipboard::ClipboardState;
use led_state_kill_ring::KillRing;
use led_state_tabs::{TabId, Tabs};
use std::sync::Arc;

use super::mark::region_range;
use super::shared::{bump, char_to_cursor, cursor_to_char, line_char_len};

pub(super) fn kill_region(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    kill_ring: &mut KillRing,
    clip: &mut ClipboardState,
) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let tab = &tabs.open[idx];
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    let Some((start, end)) = region_range(tab, &eb.rope) else {
        return;
    };
    let before = tab.cursor;

    let mut rope = (*eb.rope).clone();
    let killed: Arc<str> = Arc::from(rope.slice(start..end).to_string());
    rope.remove(start..end);

    let eb = edits.buffers.get_mut(&tab.path).expect("checked above");
    bump(eb, rope);

    let tab = &mut tabs.open[idx];
    // Cursor lands at the start of the killed region.
    tab.cursor = char_to_cursor(start, &eb.rope);
    tab.cursor.preferred_col = tab.cursor.col;
    tab.mark = None;
    let after = tab.cursor;

    kill_ring.latest = Some(killed.clone());
    kill_ring.last_was_kill_line = false;
    clip.pending_write = Some(killed.clone());

    eb.history.record_delete(start, killed, before, after);
}

pub(super) fn kill_line(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    kill_ring: &mut KillRing,
    clip: &mut ClipboardState,
) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let tab = &tabs.open[idx];
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    let rope_arc = eb.rope.clone();
    let line_count = rope_arc.len_lines();
    let line = tab.cursor.line.min(line_count.saturating_sub(1));
    let line_len = line_char_len(&rope_arc, line);
    let col = tab.cursor.col.min(line_len);

    let start = rope_arc.line_to_char(line) + col;
    // At EOL: kill the newline (join with next line). At end of
    // file: no-op.
    let end = if col < line_len {
        rope_arc.line_to_char(line) + line_len
    } else if line + 1 < line_count {
        rope_arc.line_to_char(line + 1)
    } else {
        return;
    };
    if start == end {
        return;
    }

    let before = tab.cursor;
    let mut rope = (*rope_arc).clone();
    let killed_slice: Arc<str> = Arc::from(rope.slice(start..end).to_string());
    rope.remove(start..end);

    let new_latest: Arc<str> = if kill_ring.last_was_kill_line {
        match &kill_ring.latest {
            Some(prev) => {
                let mut joined = String::with_capacity(prev.len() + killed_slice.len());
                joined.push_str(prev);
                joined.push_str(&killed_slice);
                Arc::from(joined)
            }
            None => killed_slice.clone(),
        }
    } else {
        killed_slice.clone()
    };

    let eb = edits.buffers.get_mut(&tab.path).expect("checked above");
    bump(eb, rope);
    // Cursor stays at `start` — kill-to-EOL doesn't move it.
    let tab = &mut tabs.open[idx];
    tab.cursor = char_to_cursor(start, &eb.rope);
    tab.cursor.preferred_col = tab.cursor.col;
    let after = tab.cursor;

    kill_ring.latest = Some(new_latest.clone());
    kill_ring.last_was_kill_line = true;
    clip.pending_write = Some(new_latest);

    // Record the actual characters this kill removed (not the
    // coalesced kill-ring contents) — undo should restore exactly
    // what this command's rope.remove took out.
    eb.history.record_delete(start, killed_slice, before, after);
}

/// Mark a yank as pending against the currently-active tab. The
/// runtime later fires a clipboard read; when it returns,
/// [`apply_yank`] inserts at the pending tab's cursor.
pub(super) fn request_yank(tabs: &Tabs, clip: &mut ClipboardState) {
    let Some(id) = tabs.active else {
        return;
    };
    // Ignore if a read is already in flight — double-tap yank
    // shouldn't kick off a second clipboard read.
    if clip.read_in_flight {
        return;
    }
    clip.pending_yank = Some(id);
}

/// Insert `text` at the cursor of the tab that originally requested
/// the yank. Clears `pending_yank`. No-op when the target tab no
/// longer exists (closed while the clipboard read was in flight) or
/// isn't materialised in `edits`.
///
/// `content_cols` is the painter's editor body width — used to
/// refresh `preferred_col` as the within-sub-line col so a later
/// vertical move over the yanked range lands on the right visual
/// column. Dispatch computes this from terminal + browser; the
/// ingest-side caller mirrors that.
pub fn apply_yank(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    target: TabId,
    text: &str,
    content_cols: usize,
) {
    let Some(idx) = tabs.open.iter().position(|t| t.id == target) else {
        return;
    };
    let tab = &tabs.open[idx];
    let Some(eb) = edits.buffers.get(&tab.path) else {
        return;
    };
    let before = tab.cursor;

    let mut rope = (*eb.rope).clone();
    let char_idx = cursor_to_char(&tab.cursor, &rope);
    rope.insert(char_idx, text);

    let eb = edits.buffers.get_mut(&tab.path).expect("checked above");
    bump(eb, rope);

    // Advance cursor past the inserted text.
    let inserted_chars = text.chars().count();
    let new_idx = char_idx + inserted_chars;
    let tab = &mut tabs.open[idx];
    tab.cursor = char_to_cursor(new_idx, &eb.rope);
    super::shared::refresh_preferred_col(&mut tab.cursor, &eb.rope, content_cols);
    let after = tab.cursor;

    eb.history
        .record_insert(char_idx, Arc::from(text), before, after);
}

#[cfg(test)]
mod tests {
    

    
    
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    
    
    use led_state_kill_ring::KillRing;
    use led_state_tabs::{Cursor, TabId};
    

    use super::*;
    use super::super::testutil::*;
    
    

    #[test]
    fn kill_region_removes_marked_range_into_ring() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdefgh", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 2,
            preferred_col: 2,
        };
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 6,
            preferred_col: 6,
        });
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abgh");
        assert_eq!(kr.latest.as_deref(), Some("cdef"));
        assert_eq!(tabs.open[0].cursor.col, 2);
        assert!(tabs.open[0].mark.is_none());
    }

    #[test]
    fn kill_region_handles_mark_after_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdefgh", Dims { cols: 20, rows: 5 });
        // Cursor at 6, mark at 2 — reverse of the previous test.
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 6,
            preferred_col: 6,
        };
        tabs.open[0].mark = Some(Cursor {
            line: 0,
            col: 2,
            preferred_col: 2,
        });
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abgh");
        assert_eq!(kr.latest.as_deref(), Some("cdef"));
        // Cursor lands at region start (col 2), not where it started.
        assert_eq!(tabs.open[0].cursor.col, 2);
    }

    #[test]
    fn kill_line_kills_to_eol() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo bar\nbaz", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 4,
            preferred_col: 4,
        };
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foo \nbaz");
        assert_eq!(kr.latest.as_deref(), Some("bar"));
        assert!(kr.last_was_kill_line);
    }

    #[test]
    fn kill_line_at_eol_joins_with_next() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo\nbar", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foobar");
        assert_eq!(kr.latest.as_deref(), Some("\n"));
    }

    #[test]
    fn consecutive_kill_lines_coalesce() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("aaa\nbbb\nccc", Dims { cols: 20, rows: 5 });
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        // First kill: kill "aaa" on line 0.
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        // Second kill: kill the newline that now precedes "bbb".
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        // Coalesced: "aaa" + "\n".
        assert_eq!(kr.latest.as_deref(), Some("aaa\n"));
    }

    #[test]
    fn non_kill_command_breaks_coalescing() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("aaa\nbbb", Dims { cols: 20, rows: 5 });
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('k')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert!(kr.last_was_kill_line);
        // Any other command resets the flag.
        dispatch_with_ring(
            key(KeyModifiers::NONE, KeyCode::Right),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert!(!kr.last_was_kill_line);
    }

    #[test]
    fn yank_sets_pending_on_active_tab() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("x", Dims { cols: 20, rows: 5 });
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('y')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert_eq!(clip.pending_yank, Some(TabId(1)));
    }

    #[test]
    fn apply_yank_inserts_text_at_cursor() {
        let (mut tabs, mut edits, _store, _term) =
            fixture_with_content("hello", Dims { cols: 20, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        apply_yank(&mut tabs, &mut edits, TabId(1), "XYZ", 18);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "helXYZlo");
        assert_eq!(tabs.open[0].cursor.col, 6);
    }

    #[test]
    fn kill_region_noop_when_no_mark() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc", Dims { cols: 20, rows: 5 });
        assert!(tabs.open[0].mark.is_none());
        let mut kr = KillRing::default();
        let mut clip = ClipboardState::default();
        dispatch_with_ring(
            key(KeyModifiers::CONTROL, KeyCode::Char('w')),
            &mut tabs,
            &mut edits,
            &mut kr,
            &mut clip,
            &store,
            &term,
        );
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc");
        assert!(kr.latest.is_none());
    }
}
