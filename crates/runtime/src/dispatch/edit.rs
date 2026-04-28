//! Edit primitives (M3): insert_char, insert_newline, delete_back,
//! delete_forward. Each records its op on `EditedBuffer.history`.
//!
//! M23 adds `insert_tab` and upgrades `insert_newline` to consult
//! the tree-sitter indent suggestion (per-language `indents.scm`)
//! before falling back to the M3 "match previous line's leading
//! whitespace" rule.

use led_core::grapheme_col_to_char;
use led_state_buffer_edits::BufferEdits;
use led_state_syntax::SyntaxStates;
use led_state_tabs::Tabs;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

use super::shared::{bump, char_to_cursor, line_grapheme_len, with_active};

/// Width of the soft tab the InsertTab fallback inserts when no
/// language / no syntax tree is available. Hard-coded to 4 to
/// match legacy `Dimensions.tab_stop` (also 4) and the painter's
/// `\t` expansion in `query.rs::body_model`.
const TAB_STOP: usize = 4;

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
        // Convert the cursor's grapheme col to a rope char index.
        let line_char_start = eb.rope.line_to_char(tab.cursor.line);
        let cur_line_slice = eb.rope.line(tab.cursor.line);
        let cur_char_in_line = grapheme_col_to_char(cur_line_slice, tab.cursor.col);
        let char_idx = line_char_start + cur_char_in_line;
        let mut rope = (*eb.rope).clone();
        rope.insert_char(char_idx, ch);
        bump(eb, rope);
        // Re-derive the cursor from the new rope. If `ch` extended
        // a preceding grapheme cluster (combining mark / ZWJ), col
        // stays put; if it started a fresh cluster, col advances by
        // one. `char_to_cursor` reads the post-edit line slice so
        // the conversion is exact.
        tab.cursor = char_to_cursor(char_idx + 1, &eb.rope);
        let after = tab.cursor;
        eb.history.record_insert_char(char_idx, ch, before, after);
    });
}

pub(super) fn insert_newline(tabs: &mut Tabs, edits: &mut BufferEdits, syntax: &SyntaxStates) {
    with_active(tabs, edits, |tab, eb| {
        if tab.preview {
            return;
        }
        let before = tab.cursor;
        let line_idx = tab.cursor.line;

        // M23: ask the tree-sitter indent query what the *current*
        // line's structural indent should be, and use that as the
        // new line's leading whitespace.
        //
        // Asking for `line_idx + 1` (the about-to-be-created line)
        // gets confused when the line below `line_idx` is itself
        // a closing bracket (`}` / `)` / `]`): `suggest_indent`'s
        // closing-bracket short-circuit kicks in and returns the
        // OPENER's line indent (often empty), which would land
        // the cursor flush-left. Asking for `line_idx` instead
        // returns the structural indent of the line we're
        // splitting — which is exactly what the new line wants
        // (Enter at EOL preserves the current line's indent
        // level).
        //
        // None falls through to the M3 literal-copy rule.
        let suggested = syntax
            .by_path
            .get(&tab.path)
            .and_then(|s| s.tree.as_ref().map(|t| (s.language, t)))
            .and_then(|(lang, tree)| {
                led_state_syntax::indent::suggest_indent(
                    lang,
                    tree,
                    &eb.rope,
                    line_idx,
                )
            });

        let indent: String = if let Some(s) = suggested {
            s
        } else {
            // Fallback: copy the current line's leading whitespace.
            // Same shape as the M3 path before this milestone.
            eb.rope
                .line(line_idx)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect()
        };
        // Indent length in graphemes (M25). For ASCII whitespace
        // indent this equals the char count; for any future indent
        // string with combining marks the grapheme count is what
        // `Cursor::col` measures.
        let indent_len = indent.graphemes(true).count();
        let mut inserted: String = String::with_capacity(1 + indent.len());
        inserted.push('\n');
        inserted.push_str(&indent);
        let mut rope = (*eb.rope).clone();
        let line_char_start = rope.line_to_char(line_idx);
        let cur_line_slice = rope.line(line_idx);
        let cur_char_in_line = grapheme_col_to_char(cur_line_slice, tab.cursor.col);
        let char_idx = line_char_start + cur_char_in_line;
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

/// `Tab` (M23). Replaces the active line's leading whitespace
/// with the tree-sitter-suggested indent when one is available;
/// falls back to inserting spaces from the cursor's current
/// column up to the next 4-col tab stop.
///
/// Mirrors legacy `Action::InsertTab` —
/// `request_indent(cursor_row, tab_fallback=true)`. The two
/// paths diverge in WHERE the inserted whitespace lands:
/// tree-path replaces leading whitespace; fallback inserts at
/// the cursor.
pub(super) fn insert_tab(tabs: &mut Tabs, edits: &mut BufferEdits, syntax: &SyntaxStates) {
    with_active(tabs, edits, |tab, eb| {
        if tab.preview {
            return;
        }
        let line_idx = tab.cursor.line;

        let suggested = syntax
            .by_path
            .get(&tab.path)
            .and_then(|s| s.tree.as_ref().map(|t| (s.language, t)))
            .and_then(|(lang, tree)| {
                led_state_syntax::indent::suggest_indent(lang, tree, &eb.rope, line_idx)
            });

        if let Some(target_indent) = suggested {
            // Tree path — replace existing leading whitespace.
            let line_start_char = eb.rope.line_to_char(line_idx);
            let existing_indent: String = eb
                .rope
                .line(line_idx)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            // Indent is whitespace-only — every char is its own
            // grapheme cluster, so `chars().count()` and the
            // grapheme count agree.
            let existing_len = existing_indent.chars().count();
            if target_indent == existing_indent {
                // Already correctly indented. If the cursor
                // sits inside the whitespace prefix, snap it
                // to the indent boundary; otherwise the line
                // is fine as-is and Tab is a no-op (legacy
                // parity — Tab in the middle of typed content
                // doesn't yank the cursor backwards).
                if tab.cursor.col < existing_len {
                    tab.cursor.col = existing_len;
                    tab.cursor.preferred_col = existing_len;
                }
                return;
            }
            let before = tab.cursor;
            let target_len = target_indent.chars().count();

            // Build new rope: remove old indent, insert new.
            let mut rope = (*eb.rope).clone();
            let removed: String = if existing_len > 0 {
                rope.slice(line_start_char..line_start_char + existing_len).to_string()
            } else {
                String::new()
            };
            if existing_len > 0 {
                rope.remove(line_start_char..line_start_char + existing_len);
            }
            rope.insert(line_start_char, &target_indent);
            bump(eb, rope);
            tab.cursor.col = target_len;
            tab.cursor.preferred_col = target_len;
            let after = tab.cursor;

            // Record one undo entry — replace existing indent
            // with target indent. close_group around this so
            // the next keystroke starts a fresh group.
            eb.history.finalise();
            eb.history.record_replace(
                line_start_char,
                Arc::<str>::from(removed.as_str()),
                Arc::<str>::from(target_indent.as_str()),
                before,
                after,
                None,
            );
            return;
        }

        // Fallback: insert spaces at the cursor up to the next
        // 4-col tab stop. Cursor advances by the inserted span.
        // The fallback is grapheme-bounded too — `tab.cursor.col`
        // is a grapheme idx; converting to char idx via
        // `grapheme_col_to_char` keeps the rope insert at the
        // right position even when the line contains wide chars.
        let before = tab.cursor;
        // Tab stops are display-cell stops; for the fallback path
        // (no syntax tree) we treat one grapheme as one cell. The
        // fallback only fires on language-less / tree-less buffers,
        // which are typically ASCII anyway. Wide-char fallback
        // tab-stop alignment is a follow-up if it ever surfaces.
        let target_col = (tab.cursor.col / TAB_STOP + 1) * TAB_STOP;
        let pad = target_col - tab.cursor.col;
        let mut rope = (*eb.rope).clone();
        let line_start = rope.line_to_char(line_idx);
        let cur_line_slice = rope.line(line_idx);
        let cur_char_in_line = grapheme_col_to_char(cur_line_slice, tab.cursor.col);
        let char_idx = line_start + cur_char_in_line;
        let inserted: String = " ".repeat(pad);
        rope.insert(char_idx, &inserted);
        bump(eb, rope);
        tab.cursor.col = target_col;
        tab.cursor.preferred_col = target_col;
        let after = tab.cursor;
        eb.history.finalise();
        eb.history
            .record_insert(char_idx, Arc::<str>::from(inserted.as_str()), before, after);
        eb.history.finalise();
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
        let line_char_start = eb.rope.line_to_char(tab.cursor.line);
        let cur_line_slice = eb.rope.line(tab.cursor.line);

        // Determine the char range to delete. M25 deletes the
        // entire grapheme cluster before the cursor, even if
        // multi-codepoint (e.g. `e` + combining acute → both
        // chars vanish in one Backspace).
        let (delete_start, delete_end) = if tab.cursor.col > 0 {
            let prev_char_in_line =
                grapheme_col_to_char(cur_line_slice, tab.cursor.col - 1);
            let cur_char_in_line =
                grapheme_col_to_char(cur_line_slice, tab.cursor.col);
            (
                line_char_start + prev_char_in_line,
                line_char_start + cur_char_in_line,
            )
        } else {
            // At col 0 with line > 0: delete the line terminator
            // at the end of the previous line, joining the two.
            // line_char_start == 0 means line == 0 (caught above),
            // so `line_char_start - 1` is safe.
            (line_char_start - 1, line_char_start)
        };
        let mut rope = (*eb.rope).clone();
        let removed: String = rope.slice(delete_start..delete_end).to_string();
        rope.remove(delete_start..delete_end);
        bump(eb, rope);
        // Re-derive cursor from the deletion's start position. The
        // post-edit char_to_cursor walks the new rope's grapheme
        // boundaries, landing at the correct grapheme col on the
        // (possibly joined) line.
        tab.cursor = char_to_cursor(delete_start, &eb.rope);
        let after = tab.cursor;
        eb.history
            .record_delete(delete_start, Arc::from(removed), before, after);
    });
}

pub(super) fn delete_forward(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        if tab.preview {
            return;
        }
        let line_count = eb.rope.len_lines();
        let line_grapheme_count = line_grapheme_len(&eb.rope, tab.cursor.line);
        let on_last_line = tab.cursor.line + 1 >= line_count;
        let at_line_end = tab.cursor.col >= line_grapheme_count;
        if on_last_line && at_line_end {
            return;
        }
        let before = tab.cursor;
        let line_char_start = eb.rope.line_to_char(tab.cursor.line);
        let cur_line_slice = eb.rope.line(tab.cursor.line);

        let (delete_start, delete_end) = if tab.cursor.col < line_grapheme_count {
            // In-line: delete the grapheme cluster at cursor.
            let cur_char_in_line =
                grapheme_col_to_char(cur_line_slice, tab.cursor.col);
            let next_char_in_line =
                grapheme_col_to_char(cur_line_slice, tab.cursor.col + 1);
            (
                line_char_start + cur_char_in_line,
                line_char_start + next_char_in_line,
            )
        } else {
            // At end-of-line: delete the trailing newline (1 char)
            // to join with the next line. Mirrors M3 behaviour
            // (legacy delete_forward removed exactly one char at
            // EOL); proper `\r\n` handling is a separate concern.
            let line_chars_total = cur_line_slice.len_chars();
            if line_chars_total == 0 {
                return;
            }
            let line_end = line_char_start + line_chars_total;
            (line_end - 1, line_end)
        };
        let mut rope = (*eb.rope).clone();
        let removed: String = rope.slice(delete_start..delete_end).to_string();
        rope.remove(delete_start..delete_end);
        bump(eb, rope);
        // Cursor stays put logically; re-derive in case the join
        // changed line geometry (col may shift if a wide grapheme
        // shifts forward — the post-edit char_to_cursor handles
        // it cleanly).
        tab.cursor = char_to_cursor(delete_start, &eb.rope);
        let after = tab.cursor;
        eb.history
            .record_delete(delete_start, Arc::from(removed), before, after);
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
        assert_eq!(version_of(&edits, "file.rs"), led_core::BufferVersion(1));
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
