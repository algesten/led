//! Jump list + match-bracket (M10).
//!
//! Three primitives:
//! - [`match_bracket`] — scan forward (open) or backward (close) for
//!   the matching bracket, move the cursor there, and record the
//!   pre-jump position on the jump list.
//! - [`jump_back`] — step one entry back in the jump list, restoring
//!   cursor and activating the target tab. From head, saves the
//!   current position first.
//! - [`jump_forward`] — the mirror.
//!
//! All three are silent no-ops when there's no active tab, no
//! buffer loaded, or (for navigation) nothing to do.

use led_state_buffer_edits::BufferEdits;
use led_state_jumps::{JumpListState, JumpPosition};
use led_state_tabs::Tabs;

use super::shared::{char_to_cursor, cursor_to_char, line_char_len};

/// Jump to the matching bracket for the character at (or
/// immediately before) the cursor. No-op if no bracket is in
/// scope or no match exists.
pub(super) fn match_bracket(tabs: &mut Tabs, edits: &BufferEdits, jumps: &mut JumpListState) {
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
    let rope = &eb.rope;
    let pos = cursor_to_char(&tab.cursor, rope);

    // Try char AT cursor first, then char BEFORE.
    let mut target: Option<usize> = None;
    if pos < rope.len_chars() {
        target = find_match(rope, pos);
    }
    if target.is_none() && pos > 0 {
        target = find_match(rope, pos - 1);
    }
    let Some(to) = target else {
        return;
    };

    // Record the pre-jump position so Alt-b round-trips.
    let current = JumpPosition {
        path: tab.path.clone(),
        line: tab.cursor.line,
        col: tab.cursor.col,
    };
    jumps.record(current);

    let tab = &mut tabs.open[idx];
    tab.cursor = char_to_cursor(to, rope);
    tab.cursor.preferred_col = tab.cursor.col;
}

/// Step back to the previous jump-list entry. From head, auto-records
/// the current position so `jump_forward` can return.
pub(super) fn jump_back(tabs: &mut Tabs, edits: &BufferEdits, jumps: &mut JumpListState) {
    if !jumps.can_back() {
        return;
    }
    let current = match current_position(tabs) {
        Some(p) => p,
        None => return,
    };
    let Some(target) = jumps.step_back(current) else {
        return;
    };
    apply_jump(tabs, edits, target);
}

/// Step forward to the next jump-list entry.
pub(super) fn jump_forward(tabs: &mut Tabs, edits: &BufferEdits, jumps: &mut JumpListState) {
    let Some(target) = jumps.step_forward() else {
        return;
    };
    apply_jump(tabs, edits, target);
}

/// Snapshot of the active tab's position for recording onto the
/// jump list. Returns None when there's no active tab.
pub(super) fn current_position(tabs: &Tabs) -> Option<JumpPosition> {
    let id = tabs.active?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    Some(JumpPosition {
        path: tab.path.clone(),
        line: tab.cursor.line,
        col: tab.cursor.col,
    })
}

/// Resolve a [`JumpPosition`] against the current tab set + buffers.
/// - If the path is an open tab AND its buffer is loaded, activate
///   it and restore the cursor (clamped to the buffer's extent).
/// - Otherwise silent no-op — M11 / M12 / M21 will add proper
///   re-open logic.
fn apply_jump(tabs: &mut Tabs, edits: &BufferEdits, pos: JumpPosition) {
    let Some(idx) = tabs.open.iter().position(|t| t.path == pos.path) else {
        return;
    };
    let Some(eb) = edits.buffers.get(&pos.path) else {
        return;
    };
    let rope = &eb.rope;
    let line_count = rope.len_lines();
    let line = pos.line.min(line_count.saturating_sub(1));
    let col = pos.col.min(line_char_len(rope, line));

    let tab = &mut tabs.open[idx];
    tab.cursor.line = line;
    tab.cursor.col = col;
    tab.cursor.preferred_col = col;
    tabs.active = Some(tab.id);
}

/// Scan for the bracket matching the char at `at`. Returns the char
/// index of the match, or None if the char isn't a bracket or no
/// match exists in-buffer.
///
/// Naïve depth-counted scan — doesn't skip brackets inside strings
/// or comments. M15 (syntax highlighting) may swap this for a
/// tree-sitter pair query.
fn find_match(rope: &ropey::Rope, at: usize) -> Option<usize> {
    let c = rope.char(at);
    let (open, close, forward) = match c {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let len = rope.len_chars();
    let mut depth: usize = 1;
    if forward {
        for i in (at + 1)..len {
            let ch = rope.char(i);
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    } else {
        for i in (0..at).rev() {
            let ch = rope.char(i);
            if ch == close {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use led_driver_terminal_core::Dims;
    use led_state_jumps::JumpListState;
    use led_state_tabs::Cursor;

    use super::super::testutil::*;
    use super::*;

    fn set_cursor(tabs: &mut led_state_tabs::Tabs, line: usize, col: usize) {
        tabs.open[0].cursor = Cursor {
            line,
            col,
            preferred_col: col,
        };
    }

    #[test]
    fn match_bracket_jumps_from_open_to_close() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("a { b } c", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 2); // on '{'
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 6); // on '}'
    }

    #[test]
    fn match_bracket_jumps_from_close_to_open() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("a { b } c", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 6); // on '}'
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 2); // on '{'
    }

    #[test]
    fn match_bracket_considers_char_before_cursor() {
        // Cursor just past '}' → fall back to char-before.
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("a { b }", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 7); // past '}'
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 2);
    }

    #[test]
    fn match_bracket_noop_when_no_bracket() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("no brackets", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 3);
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 3);
        assert!(jumps.entries.is_empty());
    }

    #[test]
    fn match_bracket_records_pre_jump_position() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("{abc}", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 0);
        let mut jumps = JumpListState::default();
        match_bracket(&mut tabs, &edits, &mut jumps);
        assert_eq!(jumps.entries.len(), 1);
        assert_eq!(jumps.entries[0].line, 0);
        assert_eq!(jumps.entries[0].col, 0);
    }

    #[test]
    fn jump_back_noop_on_empty_list() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("hello", Dims { cols: 20, rows: 5 });
        let mut jumps = JumpListState::default();
        jump_back(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 0);
    }

    #[test]
    fn jump_back_from_head_records_current_and_returns() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("aaaaaaaaaa", Dims { cols: 20, rows: 5 });
        set_cursor(&mut tabs, 0, 2); // prior "interesting" position
        let mut jumps = JumpListState::default();
        // Record a jump position manually; then move cursor and back.
        jumps.record(super::JumpPosition {
            path: canon("file.rs"),
            line: 0,
            col: 2,
        });
        set_cursor(&mut tabs, 0, 9); // "current"
        jump_back(&mut tabs, &edits, &mut jumps);
        // Cursor back at col 2.
        assert_eq!(tabs.open[0].cursor.col, 2);
        // Forward entry now exists (the save-before-back of "9").
        assert!(jumps.can_forward());
    }

    #[test]
    fn jump_back_forward_round_trip() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("aaaaaaaaaa", Dims { cols: 20, rows: 5 });
        let mut jumps = JumpListState::default();
        jumps.record(super::JumpPosition {
            path: canon("file.rs"),
            line: 0,
            col: 3,
        });
        set_cursor(&mut tabs, 0, 8);
        jump_back(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 3);
        jump_forward(&mut tabs, &edits, &mut jumps);
        assert_eq!(tabs.open[0].cursor.col, 8);
    }

    #[test]
    fn jump_to_closed_tab_is_silent_noop() {
        let (mut tabs, edits, _store, _term) =
            fixture_with_content("hello", Dims { cols: 20, rows: 5 });
        let mut jumps = JumpListState::default();
        jumps.record(super::JumpPosition {
            path: canon("other.rs"), // not open
            line: 0,
            col: 0,
        });
        set_cursor(&mut tabs, 0, 4);
        jump_back(&mut tabs, &edits, &mut jumps);
        // Cursor unchanged — target tab wasn't open.
        assert_eq!(tabs.open[0].cursor.col, 4);
    }

    #[test]
    fn tab_cycle_records_outgoing_position() {
        // Covered in tabs.rs but also needs a nav-side check: the
        // jump-list should grow by one per tab-switch.
        use super::super::tabs::cycle_active;
        let mut tabs = tabs_with(&[("a", 1), ("b", 2)], Some(1));
        tabs.open[0].cursor = Cursor {
            line: 3,
            col: 5,
            preferred_col: 5,
        };
        let mut jumps = JumpListState::default();
        cycle_active(&mut tabs, &mut jumps, 1);
        assert_eq!(jumps.entries.len(), 1);
        assert_eq!(jumps.entries[0].line, 3);
        assert_eq!(jumps.entries[0].col, 5);
    }
}

