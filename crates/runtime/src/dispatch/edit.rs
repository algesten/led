//! Edit primitives (M3): insert_char, insert_newline, delete_back,
//! delete_forward. Each records its op on `EditedBuffer.history`.

use led_state_buffer_edits::BufferEdits;
use led_state_tabs::Tabs;
use std::sync::Arc;

use super::shared::{bump, line_char_len, with_active};

pub(super) fn insert_char(tabs: &mut Tabs, edits: &mut BufferEdits, ch: char) {
    with_active(tabs, edits, |tab, eb| {
        let before = tab.cursor;
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.insert_char(char_idx, ch);
        bump(eb, rope);
        tab.cursor.col += 1;
        tab.cursor.preferred_col = tab.cursor.col;
        let after = tab.cursor;
        eb.history.record_insert_char(char_idx, ch, before, after);
    });
}

pub(super) fn insert_newline(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let before = tab.cursor;
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.insert_char(char_idx, '\n');
        bump(eb, rope);
        eb.history
            .record_insert(char_idx, Arc::<str>::from("\n"), before, {
                let mut a = before;
                a.line += 1;
                a.col = 0;
                a.preferred_col = 0;
                a
            });
        tab.cursor.line += 1;
        tab.cursor.col = 0;
        tab.cursor.preferred_col = 0;
    });
}

pub(super) fn delete_back(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        if tab.cursor.line == 0 && tab.cursor.col == 0 {
            return;
        }
        let before = tab.cursor;
        // Capture the join column *before* the remove. After the
        // newline is gone the previous line grows to include the
        // current one, so post-remove length is too large.
        let join_col = if tab.cursor.col == 0 {
            line_char_len(&eb.rope, tab.cursor.line - 1)
        } else {
            0
        };
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        let removed: String = rope.slice(char_idx - 1..char_idx).to_string();
        rope.remove(char_idx - 1..char_idx);
        if tab.cursor.col > 0 {
            tab.cursor.col -= 1;
        } else {
            tab.cursor.line -= 1;
            tab.cursor.col = join_col;
        }
        tab.cursor.preferred_col = tab.cursor.col;
        let after = tab.cursor;
        bump(eb, rope);
        eb.history
            .record_delete(char_idx - 1, Arc::from(removed), before, after);
    });
}

pub(super) fn delete_forward(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let line_count = eb.rope.len_lines();
        let on_last_line = tab.cursor.line + 1 >= line_count;
        let at_line_end = tab.cursor.col >= line_char_len(&eb.rope, tab.cursor.line);
        if on_last_line && at_line_end {
            return;
        }
        let before = tab.cursor;
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        let removed: String = rope.slice(char_idx..char_idx + 1).to_string();
        rope.remove(char_idx..char_idx + 1);
        bump(eb, rope);
        // Cursor stays put. preferred_col unchanged (col didn't move).
        let after = tab.cursor;
        eb.history
            .record_delete(char_idx, Arc::from(removed), before, after);
    });
}
