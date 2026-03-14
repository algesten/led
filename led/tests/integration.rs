mod harness;

use led_core::Action::*;

use harness::TestHarness;

#[test]
fn test_open_file() {
    let state = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(vec![Wait(10)]);

    let buf_id = state.active_buffer.expect("should have active buffer");
    let buf = &state.buffers[&buf_id];
    assert_eq!(buf.doc.line(0), "hello");
    assert_eq!(buf.doc.line(1), "world");
    assert_eq!(buf.doc.line_count(), 3);
}

#[test]
fn test_insert_chars() {
    let state = TestHarness::new()
        .with_file("hello\n")
        .run(vec![Wait(10), InsertChar('x'), InsertChar('y')]);

    let buf_id = state.active_buffer.unwrap();
    let buf = &state.buffers[&buf_id];
    assert_eq!(buf.doc.line(0), "xyhello");
    assert_eq!(buf.cursor_row, 0);
    assert_eq!(buf.cursor_col, 2);
}

#[test]
fn test_cursor_movement() {
    let state = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(vec![Wait(10), MoveDown, MoveRight, MoveRight]);

    let buf_id = state.active_buffer.unwrap();
    let buf = &state.buffers[&buf_id];
    assert_eq!(buf.cursor_row, 1);
    assert_eq!(buf.cursor_col, 2);
}

#[test]
fn test_undo() {
    let state = TestHarness::new()
        .with_file("hello\n")
        .run(vec![Wait(10), InsertChar('a'), InsertChar('b'), Undo]);

    let buf_id = state.active_buffer.unwrap();
    let buf = &state.buffers[&buf_id];
    assert_eq!(buf.doc.line(0), "hello");
}

#[test]
fn test_dirty_flag() {
    let state = TestHarness::new()
        .with_file("hello\n")
        .run(vec![Wait(10), InsertChar('x')]);

    let buf_id = state.active_buffer.unwrap();
    let buf = &state.buffers[&buf_id];
    assert!(buf.doc.dirty());
}

#[test]
fn test_no_file() {
    let state = TestHarness::new().run(vec![Wait(10)]);

    assert!(state.active_buffer.is_none());
    assert!(state.buffers.is_empty());
}

#[test]
fn test_viewport_set() {
    let state = TestHarness::new()
        .with_viewport(120, 40)
        .run(vec![Wait(10)]);

    assert_eq!(state.viewport, (120, 40));
}
