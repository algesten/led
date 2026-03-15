mod harness;

use led_core::Action::*;
use led_state::SaveState;

use TestStep::{Do, WaitFor};
use harness::{TestHarness, TestStep};

// ── Helpers ──

fn buf(t: &harness::TestResult) -> &led_state::BufferState {
    let id = t.state.active_buffer.expect("no active buffer");
    &t.state.buffers[&id]
}

/// Shorthand: wrap a list of Actions into TestSteps
fn actions(acts: Vec<led_core::Action>) -> Vec<TestStep> {
    acts.into_iter().map(TestStep::Do).collect()
}

fn is_clean(s: &led_state::AppState) -> bool {
    s.active_buffer
        .and_then(|id| s.buffers.get(&id))
        .map_or(false, |b| b.save_state == SaveState::Clean)
}

// ── File open ──

#[test]
fn open_file() {
    let t = TestHarness::new().with_file("hello\nworld\n").run(vec![]);

    assert_eq!(buf(&t).doc.line(0), "hello");
    assert_eq!(buf(&t).doc.line(1), "world");
    assert_eq!(buf(&t).doc.line_count(), 3);
}

#[test]
fn open_empty_file() {
    let t = TestHarness::new().with_file("").run(vec![]);

    assert_eq!(buf(&t).doc.line_count(), 1);
    assert_eq!(buf(&t).doc.line(0), "");
}

#[test]
fn no_file() {
    let t = TestHarness::new().run(vec![]);

    assert!(t.state.active_buffer.is_none());
    assert!(t.state.buffers.is_empty());
}

// ── Movement: basic ──

#[test]
fn move_down() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![MoveDown, MoveDown]));

    assert_eq!(buf(&t).cursor_row, 2);
    assert_eq!(buf(&t).cursor_col, 0);
}

#[test]
fn move_up() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![MoveDown, MoveDown, MoveUp]));

    assert_eq!(buf(&t).cursor_row, 1);
}

#[test]
fn move_right_and_left() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveRight, MoveRight, MoveRight, MoveLeft]));

    assert_eq!(buf(&t).cursor_row, 0);
    assert_eq!(buf(&t).cursor_col, 2);
}

#[test]
fn move_up_at_top() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveUp, MoveUp]));

    assert_eq!(buf(&t).cursor_row, 0);
    assert_eq!(buf(&t).cursor_col, 0);
}

#[test]
fn move_down_at_bottom() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveDown, MoveDown, MoveDown]));

    let max_row = buf(&t).doc.line_count() - 1;
    assert_eq!(buf(&t).cursor_row, max_row);
}

#[test]
fn move_left_at_start() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveLeft]));

    assert_eq!(buf(&t).cursor_row, 0);
    assert_eq!(buf(&t).cursor_col, 0);
}

// ── Movement: wrapping ──

#[test]
fn move_right_wraps_to_next_line() {
    let t = TestHarness::new()
        .with_file("ab\ncd\n")
        .run(actions(vec![MoveRight, MoveRight, MoveRight]));

    assert_eq!(buf(&t).cursor_row, 1);
    assert_eq!(buf(&t).cursor_col, 0);
}

#[test]
fn move_left_wraps_to_previous_line() {
    let t = TestHarness::new()
        .with_file("ab\ncd\n")
        .run(actions(vec![MoveDown, MoveLeft]));

    assert_eq!(buf(&t).cursor_row, 0);
    assert_eq!(buf(&t).cursor_col, 2);
}

// ── Movement: line start/end ──

#[test]
fn line_start_and_end() {
    let t = TestHarness::new()
        .with_file("hello world\n")
        .run(actions(vec![MoveRight, MoveRight, MoveRight, LineStart]));

    assert_eq!(buf(&t).cursor_col, 0);

    let t = TestHarness::new()
        .with_file("hello world\n")
        .run(actions(vec![LineEnd]));

    assert_eq!(buf(&t).cursor_col, 11);
}

// ── Movement: file start/end ──

#[test]
fn file_start_and_end() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![FileEnd]));

    let max_row = buf(&t).doc.line_count() - 1;
    assert_eq!(buf(&t).cursor_row, max_row);

    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![MoveDown, MoveDown, FileStart]));

    assert_eq!(buf(&t).cursor_row, 0);
    assert_eq!(buf(&t).cursor_col, 0);
}

// ── Movement: column affinity ──

#[test]
fn column_affinity_preserved_across_short_line() {
    let t = TestHarness::new()
        .with_file("hello\nhi\nworld\n")
        .run(actions(vec![LineEnd, MoveDown, MoveDown]));

    assert_eq!(buf(&t).cursor_row, 2);
    assert_eq!(buf(&t).cursor_col, 5);
}

// ── Movement: page up/down ──

#[test]
fn page_down_and_up() {
    let mut lines = String::new();
    for i in 0..100 {
        lines.push_str(&format!("line {i}\n"));
    }

    let t = TestHarness::new()
        .with_viewport(80, 24)
        .with_file(&lines)
        .run(actions(vec![PageDown]));

    assert!(buf(&t).cursor_row > 0);

    let t = TestHarness::new()
        .with_viewport(80, 24)
        .with_file(&lines)
        .run(actions(vec![PageDown, PageUp]));

    assert_eq!(buf(&t).cursor_row, 0);
}

// ── Movement: scroll ──

#[test]
fn scroll_follows_cursor() {
    let mut lines = String::new();
    for i in 0..100 {
        lines.push_str(&format!("line {i}\n"));
    }

    let t = TestHarness::new()
        .with_viewport(80, 12)
        .with_file(&lines)
        .run(actions(vec![PageDown]));

    assert!(buf(&t).scroll_row > 0, "scroll should have moved");
}

// ── Editing: insert ──

#[test]
fn insert_chars() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x'), InsertChar('y')]));

    assert_eq!(buf(&t).doc.line(0), "xyhello");
    assert_eq!(buf(&t).cursor_col, 2);
}

#[test]
fn insert_in_middle() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        MoveRight,
        MoveRight,
        InsertChar('X'),
    ]));

    assert_eq!(buf(&t).doc.line(0), "heXllo");
    assert_eq!(buf(&t).cursor_col, 3);
}

#[test]
fn insert_newline() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        MoveRight,
        MoveRight,
        InsertNewline,
    ]));

    assert_eq!(buf(&t).doc.line(0), "he");
    assert_eq!(buf(&t).doc.line(1), "llo");
    assert_eq!(buf(&t).cursor_row, 1);
    assert_eq!(buf(&t).cursor_col, 0);
}

#[test]
fn insert_tab() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertTab]));

    assert_eq!(buf(&t).doc.line(0), "    hello");
    assert_eq!(buf(&t).cursor_col, 4);
}

#[test]
fn insert_tab_alignment() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x'), InsertTab]));

    assert_eq!(buf(&t).cursor_col, 4);
}

// ── Editing: delete backward ──

#[test]
fn delete_backward() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        MoveRight,
        MoveRight,
        DeleteBackward,
    ]));

    assert_eq!(buf(&t).doc.line(0), "hllo");
    assert_eq!(buf(&t).cursor_col, 1);
}

#[test]
fn delete_backward_at_start_does_nothing() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![DeleteBackward]));

    assert_eq!(buf(&t).doc.line(0), "hello");
    assert_eq!(buf(&t).cursor_col, 0);
}

#[test]
fn delete_backward_joins_lines() {
    let t = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(actions(vec![MoveDown, DeleteBackward]));

    assert_eq!(buf(&t).doc.line(0), "helloworld");
    assert_eq!(buf(&t).cursor_row, 0);
    assert_eq!(buf(&t).cursor_col, 5);
}

// ── Editing: delete forward ──

#[test]
fn delete_forward() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![DeleteForward]));

    assert_eq!(buf(&t).doc.line(0), "ello");
    assert_eq!(buf(&t).cursor_col, 0);
}

#[test]
fn delete_forward_joins_lines() {
    let t = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(actions(vec![LineEnd, DeleteForward]));

    assert_eq!(buf(&t).doc.line(0), "helloworld");
    assert_eq!(buf(&t).cursor_row, 0);
}

// ── Editing: kill line ──

#[test]
fn kill_line_deletes_to_end() {
    let t = TestHarness::new()
        .with_file("hello world\n")
        .run(actions(vec![MoveRight, MoveRight, KillLine]));

    assert_eq!(buf(&t).doc.line(0), "he");
    assert_eq!(buf(&t).cursor_col, 2);
}

#[test]
fn kill_line_at_end_joins_next() {
    let t = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(actions(vec![LineEnd, KillLine]));

    assert_eq!(buf(&t).doc.line(0), "helloworld");
}

// ── Undo / Redo ──

#[test]
fn undo_reverts_insert_group() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('a'),
        InsertChar('b'),
        Undo,
    ]));

    assert_eq!(buf(&t).doc.line(0), "hello");
}

#[test]
fn undo_then_redo() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('x'),
        InsertChar('y'),
        Undo,
        Redo,
    ]));

    assert_eq!(buf(&t).doc.line(0), "xyhello");
}

#[test]
fn undo_groups_split_on_movement() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('a'),
        InsertChar('b'),
        MoveRight, // closes group
        InsertChar('c'),
        InsertChar('d'),
        Undo, // only reverts "cd"
    ]));

    assert_eq!(buf(&t).doc.line(0), "abhello");
}

#[test]
fn undo_groups_split_on_word_boundary() {
    let t = TestHarness::new().with_file("\n").run(actions(vec![
        InsertChar('a'),
        InsertChar('b'),
        InsertChar(' '), // word boundary — closes "ab" group, space starts new group
        InsertChar('c'),
        Undo, // reverts " c" (space + c are in the same group)
    ]));

    assert_eq!(buf(&t).doc.line(0), "ab");
}

#[test]
fn redo_cleared_by_new_edit() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('a'),
        Undo,
        InsertChar('b'), // clears redo stack
        Undo,            // undoes 'b'
        Redo,            // redoes 'b' (original 'a' redo is gone)
    ]));

    assert_eq!(buf(&t).doc.line(0), "bhello");
}

#[test]
fn multiple_undo() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('a'),
        MoveRight, // close group
        InsertChar('b'),
        MoveRight, // close group
        InsertChar('c'),
        Undo, // reverts 'c'
        Undo, // reverts 'b'
        Undo, // reverts 'a'
    ]));

    assert_eq!(buf(&t).doc.line(0), "hello");
}

#[test]
fn undo_nothing_is_noop() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![Undo]));

    assert_eq!(buf(&t).doc.line(0), "hello");
}

#[test]
fn redo_nothing_is_noop() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![Redo]));

    assert_eq!(buf(&t).doc.line(0), "hello");
}

// ── Save state ──

#[test]
fn clean_after_open() {
    let t = TestHarness::new().with_file("hello\n").run(vec![]);

    assert_eq!(buf(&t).save_state, SaveState::Clean);
    assert!(!buf(&t).doc.dirty());
}

#[test]
fn modified_after_edit() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x')]));

    assert_eq!(buf(&t).save_state, SaveState::Modified);
    assert!(buf(&t).doc.dirty());
}

#[test]
fn saving_after_save_action() {
    // Save is async — without WaitFor, we capture the state right after the action
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x'), Save]));

    // State should be Saving (the async save hasn't completed yet)
    assert_eq!(buf(&t).save_state, SaveState::Saving);
}

#[test]
fn clean_after_save_completes() {
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(InsertChar('x')),
        Do(Save),
        WaitFor(is_clean),
    ]);

    assert_eq!(buf(&t).save_state, SaveState::Clean);
    assert!(!buf(&t).doc.dirty());
}

#[test]
fn save_writes_to_disk() {
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(InsertChar('X')),
        Do(Save),
        WaitFor(is_clean),
    ]);

    let content = std::fs::read_to_string(t.file_path.as_ref().unwrap()).unwrap();
    assert_eq!(content, "Xhello\n");
}

#[test]
fn version_increments_on_edit() {
    let t = TestHarness::new().with_file("hello\n").run(vec![]);
    let v0 = buf(&t).doc.version();

    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x')]));
    assert!(buf(&t).doc.version() > v0);
}

// ── UI ──

#[test]
fn toggle_side_panel() {
    let t = TestHarness::new().run(actions(vec![ToggleSidePanel]));
    assert!(!t.state.show_side_panel);

    let t = TestHarness::new().run(actions(vec![ToggleSidePanel, ToggleSidePanel]));
    assert!(t.state.show_side_panel);
}

#[test]
fn viewport_set() {
    let t = TestHarness::new().with_viewport(120, 40).run(vec![]);

    let dims = t.state.dims.expect("dims should be set after resize");
    assert_eq!(dims.viewport_width, 120);
    assert_eq!(dims.viewport_height, 40);
}

// ── Dimensions ──

#[test]
fn dims_side_panel_visible() {
    // Wide terminal: side panel visible
    let t = TestHarness::new().with_viewport(80, 24).run(vec![]);
    let dims = t.state.dims.unwrap();
    assert!(dims.side_panel_visible());

    // Narrow terminal: side panel hidden
    let t = TestHarness::new().with_viewport(40, 24).run(vec![]);
    let dims = t.state.dims.unwrap();
    assert!(!dims.side_panel_visible());
}

#[test]
fn dims_toggle_side_panel_updates_dims() {
    let t = TestHarness::new()
        .with_viewport(80, 24)
        .run(actions(vec![ToggleSidePanel]));

    let dims = t.state.dims.unwrap();
    assert!(!dims.show_side_panel);
    assert!(!dims.side_panel_visible());
}

#[test]
fn dims_text_width() {
    // 80 wide, side panel visible (25), gutter (2) → text_width = 80 - 25 - 2 = 53
    let t = TestHarness::new().with_viewport(80, 24).run(vec![]);
    let dims = t.state.dims.unwrap();
    assert_eq!(dims.text_width(), 53);
}

#[test]
fn dims_buffer_height() {
    // 24 tall, status bar (1), tab bar (1) → buffer_height = 22
    let t = TestHarness::new().with_viewport(80, 24).run(vec![]);
    let dims = t.state.dims.unwrap();
    assert_eq!(dims.buffer_height(), 22);
}

// ── Line Wrapping ──

#[test]
fn wrap_move_down_through_wrapped_line() {
    // Viewport: 12 cols wide, side panel hidden (too narrow).
    // gutter_width=2, text_width=10, wrap_width=9
    // "abcdefghijklmno" = 15 chars → chunks: [0..9, 9..15] (2 visual lines)
    let t = TestHarness::new()
        .with_viewport(12, 10)
        .with_file("abcdefghijklmno\nshort\n")
        .run(actions(vec![MoveDown]));

    // First MoveDown should move to the second sub-line of the first logical line
    assert_eq!(buf(&t).cursor_row, 0, "still on first logical line");
    assert!(buf(&t).cursor_col >= 9, "should be on second sub-line");
}

#[test]
fn wrap_move_down_crosses_to_next_line() {
    // From second sub-line of wrapped line, MoveDown should go to next logical line
    let t = TestHarness::new()
        .with_viewport(12, 10)
        .with_file("abcdefghijklmno\nshort\n")
        .run(actions(vec![MoveDown, MoveDown]));

    assert_eq!(buf(&t).cursor_row, 1, "should be on second logical line");
}

#[test]
fn wrap_move_up_through_wrapped_line() {
    // Start on line 1, MoveUp should land on last sub-line of line 0
    let t = TestHarness::new()
        .with_viewport(12, 10)
        .with_file("abcdefghijklmno\nshort\n")
        .run(actions(vec![MoveDown, MoveDown, MoveUp]));

    assert_eq!(buf(&t).cursor_row, 0, "back on first logical line");
    assert!(buf(&t).cursor_col >= 9, "on last sub-line");
}

#[test]
fn wrap_scroll_sub_line() {
    // Make a file with a very long line in a small viewport
    let long_line = "a".repeat(100);
    let mut content = long_line.clone();
    content.push('\n');
    for i in 0..20 {
        content.push_str(&format!("line{i}\n"));
    }

    let t = TestHarness::new()
        .with_viewport(12, 5)
        .with_file(&content)
        .run(actions(vec![
            // Move down through all sub-lines of the long wrapped line
            MoveDown, MoveDown, MoveDown, MoveDown, MoveDown, MoveDown, MoveDown, MoveDown,
            MoveDown, MoveDown, MoveDown, MoveDown,
        ]));

    // After many MoveDowns through a 100-char wrapped line, scroll should have adjusted
    assert!(
        buf(&t).scroll_row > 0 || buf(&t).scroll_sub_line > 0,
        "scroll should have moved for wrapped content"
    );
}
