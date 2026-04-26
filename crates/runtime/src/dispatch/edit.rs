//! Edit primitives (M3): insert_char, insert_newline, delete_back,
//! delete_forward. Each records its op on `EditedBuffer.history`.

use led_state_buffer_edits::BufferEdits;
use led_state_tabs::Tabs;
use std::sync::Arc;

use super::shared::{bump, line_char_len, with_active};

pub(super) fn insert_char(tabs: &mut Tabs, edits: &mut BufferEdits, ch: char) {
    with_active(tabs, edits, |tab, eb| {
        // Preview tabs are strict viewers — typing into one would
        // create dirty in-memory state the user didn't ask for.
        // Enter-to-promote is the explicit "I want to own this"
        // gesture; until then, text input is a no-op.
        if tab.preview {
            return;
        }
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
        if tab.preview {
            return;
        }
        let before = tab.cursor;
        let line_idx = tab.cursor.line;
        // Copy the leading whitespace of the current line into the
        // new line so common edit flows ("end of line, Enter")
        // land at the same indent. Mirrors legacy
        // `editing_of.rs::insert_newline_s` which schedules
        // `request_indent` after the split; the simple "match the
        // previous line" rule covers the same goldens without a
        // syntax-tree dependency.
        let line = eb.rope.line(line_idx);
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let indent_len = indent.chars().count();
        let mut inserted: String = String::with_capacity(1 + indent.len());
        inserted.push('\n');
        inserted.push_str(&indent);
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(line_idx) + tab.cursor.col;
        rope.insert(char_idx, &inserted);
        bump(eb, rope);
        let after = {
            let mut a = before;
            a.line += 1;
            a.col = indent_len;
            a.preferred_col = indent_len;
            a
        };
        eb.history
            .record_insert(char_idx, Arc::<str>::from(inserted.as_str()), before, after);
        tab.cursor = after;
    });
}

pub(super) fn delete_back(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        if tab.preview {
            return;
        }
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
        if tab.preview {
            return;
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    
    use led_driver_buffers_core::BufferStore;
    use led_driver_terminal_core::{Dims, KeyCode, KeyModifiers};
    
    use led_state_buffer_edits::{BufferEdits, EditedBuffer};
    
    use led_state_tabs::Cursor;
    use ropey::Rope;

    
    use super::super::testutil::*;
    use super::super::DispatchOutcome;
    

    #[test]
    fn arrow_up_escapes_sub_line_after_typing_triggers_re_wrap() {
        // Regression for: append to a soft-wrapped line until it
        // re-wraps, arrow-up gets stuck bouncing on the last
        // sub-line instead of walking up through the earlier ones.
        //
        // Root cause: `insert_char` set `preferred_col = col` (raw
        // logical col), which on a wrapped line exceeds any sub-
        // line's width. `land_on_sub_line` clamps to the sub-line
        // width and lands on the wrap boundary col — which
        // `col_to_sub_line` resolves as the START of the next
        // sub-line, so the next arrow-up comes back to where it
        // started. Fix: dispatch refreshes `preferred_col` to the
        // within-sub-line col after edit-like commands.
        //
        // Geometry: `Dims { cols: 14, rows: 20 }`.
        // `editor_area.cols` = 14 (no sidebar); content_cols = 14
        // - GUTTER_WIDTH(2) - TRAILING_RESERVED_COLS(0) = 12. Wrap
        // width = 11 (one trailing col per non-last sub: `\`).
        //
        // Start: 17 chars — sub 0=[0,11), sub 1=[11,17). Two
        // sub-lines.
        let (mut tabs, mut edits, store, term) = fixture_with_content(
            "01234567890123456\n",
            Dims { cols: 14, rows: 20 },
        );
        // Park the cursor at the end of the wrapped line (col 17
        // = last sub-line, within=6).
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 17,
            preferred_col: 6,
        };

        // Type seven chars. That pushes the line to 24 chars which
        // re-wraps to 3 sub-lines: sub 0=[0,11), sub 1=[11,22),
        // sub 2=[22,24). Cursor ends at col 24, within sub-line 2
        // at col 2.
        for _ in 0..7 {
            dispatch_default(
                key(KeyModifiers::NONE, KeyCode::Char('X')),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
        }
        assert_eq!(tabs.open[0].cursor.col, 24);
        // preferred_col must be the within-sub-line col (2), not
        // the raw logical col (24) the buggy path produced.
        assert_eq!(tabs.open[0].cursor.preferred_col, 2);

        // Arrow-up: land on sub-line 1 at col 11 + 2 = 13.
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Up),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 13);

        // Arrow-up again: land on sub-line 0 at col 0 + 2 = 2.
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Up),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(tabs.open[0].cursor.line, 0);
        assert_eq!(tabs.open[0].cursor.col, 2);
    }

    #[test]
    fn insert_char_advances_cursor_and_bumps_version() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc\n", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char('X')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "aXbc\n");
        assert_eq!(tabs.open[0].cursor.col, 2);
        assert_eq!(tabs.open[0].cursor.preferred_col, 2);
        assert_eq!(version_of(&edits, "file.rs"), 1);
        assert!(dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn insert_newline_splits_line_and_drops_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abcdef\n", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Enter),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc\ndef\n");
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 1,
                col: 0,
                preferred_col: 0,
            }
        );
        assert!(dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn backspace_deletes_char_before_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hello", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 5,
            preferred_col: 5,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Backspace),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hell");
        assert_eq!(tabs.open[0].cursor.col, 4);
    }

    #[test]
    fn backspace_at_column_zero_joins_with_previous_line() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo\nbar\n", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 1,
            col: 0,
            preferred_col: 0,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Backspace),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foobar\n");
        // Cursor landed where the join point is — end of the old "foo".
        assert_eq!(
            tabs.open[0].cursor,
            Cursor {
                line: 0,
                col: 3,
                preferred_col: 3,
            }
        );
    }

    #[test]
    fn backspace_at_origin_is_a_noop() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc\n", Dims { cols: 10, rows: 5 });
        let v0 = version_of(&edits, "file.rs");

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Backspace),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc\n");
        assert_eq!(version_of(&edits, "file.rs"), v0);
        assert!(!dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn delete_forward_removes_char_at_cursor() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("hello", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Delete),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "hllo");
        // Cursor stays put.
        assert_eq!(tabs.open[0].cursor.col, 1);
    }

    #[test]
    fn delete_forward_at_end_of_line_joins_with_next() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("foo\nbar", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Delete),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "foobar");
        // Cursor position unchanged — still at the join point.
        assert_eq!(tabs.open[0].cursor.col, 3);
    }

    #[test]
    fn delete_forward_at_eof_is_a_noop() {
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("abc", Dims { cols: 10, rows: 5 });
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 3,
            preferred_col: 3,
        };
        let v0 = version_of(&edits, "file.rs");

        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Delete),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "file.rs").to_string(), "abc");
        assert_eq!(version_of(&edits, "file.rs"), v0);
    }

    #[test]
    fn edit_on_unloaded_buffer_is_swallowed() {
        // Tab is open but BufferEdits has no entry (file hasn't
        // loaded yet) — all four primitives no-op and leave the
        // cursor alone.
        let mut tabs = tabs_with(&[("file.rs", 1)], Some(1));
        let mut edits = BufferEdits::default();
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 0,
            preferred_col: 0,
        };

        for code in [
            KeyCode::Char('x'),
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Delete,
        ] {
            dispatch_default(
                key(KeyModifiers::NONE, code),
                &mut tabs,
                &mut edits,
                &store,
                &term,
            );
        }

        assert!(edits.buffers.is_empty());
        assert_eq!(tabs.open[0].cursor, Cursor::default());
    }

    #[test]
    fn ctrl_c_does_not_insert_c() {
        // Regression guard: plain ctrl+c is unbound in the M6 default
        // keymap (quit moved to ctrl+x ctrl+c), but we must still not
        // let implicit_insert turn it into `InsertChar('c')`.
        let (mut tabs, mut edits, store, term) =
            fixture_with_content("", Dims { cols: 10, rows: 5 });
        let outcome = dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Char('c')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        assert_eq!(outcome, DispatchOutcome::Continue);
        assert_eq!(rope_of(&edits, "file.rs").to_string(), "");
        assert!(!dirty_of(&edits, "file.rs"));
    }

    #[test]
    fn edits_survive_tab_switch() {
        // Two tabs, two files; edit each, switch between, confirm the
        // ropes + cursors are preserved per tab.
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        let mut edits = BufferEdits::default();
        edits.buffers.insert(
            canon("a"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("a"))),
        );
        edits.buffers.insert(
            canon("b"),
            EditedBuffer::fresh(Arc::new(Rope::from_str("b"))),
        );
        let store = BufferStore::default();
        let term = terminal_with(Some(Dims { cols: 10, rows: 5 }));
        tabs.open[0].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };

        // Edit tab a.
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char('!')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        // Switch to tab b (Ctrl-Right — plain Tab is reserved for
        // insert_tab per M11 default keymap).
        dispatch_default(
            key(KeyModifiers::CONTROL, KeyCode::Right),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );
        tabs.open[1].cursor = Cursor {
            line: 0,
            col: 1,
            preferred_col: 1,
        };
        dispatch_default(
            key(KeyModifiers::NONE, KeyCode::Char('?')),
            &mut tabs,
            &mut edits,
            &store,
            &term,
        );

        assert_eq!(rope_of(&edits, "a").to_string(), "a!");
        assert_eq!(rope_of(&edits, "b").to_string(), "b?");
        assert!(dirty_of(&edits, "a"));
        assert!(dirty_of(&edits, "b"));
    }
}
