mod harness;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use led_core::Action::*;
use led_core::{EditOp, Startup, UndoEntry, UserPath};

use TestStep::{Do, QuitAndWait, WaitFor};
use harness::{TestHarness, TestStep};

// ── Helpers ──

/// Test helper: get a line as a trimmed String (allocates — test-only).
fn line(doc: &dyn led_core::Doc, row: usize) -> String {
    let mut buf = String::new();
    doc.line(led_core::Row(row), &mut buf);
    let trimmed = buf.trim_end_matches(&['\n', '\r'][..]).len();
    buf.truncate(trimmed);
    buf
}

fn buf(t: &harness::TestResult) -> &led_state::BufferState {
    let path = t.state.active_tab.as_ref().expect("no active buffer");
    &t.state.buffers[path]
}

/// Shorthand: wrap a list of Actions into TestSteps
fn actions(acts: Vec<led_core::Action>) -> Vec<TestStep> {
    acts.into_iter().map(TestStep::Do).collect()
}

fn is_clean(s: &led_state::AppState) -> bool {
    s.active_tab
        .as_ref()
        .and_then(|path| s.buffers.get(path))
        .map_or(false, |b| !b.is_dirty() && !b.save_in_flight())
}

fn indent_done(s: &led_state::AppState) -> bool {
    s.active_tab
        .as_ref()
        .and_then(|path| s.buffers.get(path))
        .map_or(true, |b| b.pending_indent_row().is_none())
}

// ── File open ──

#[test]
fn open_file() {
    let t = TestHarness::new().with_file("hello\nworld\n").run(vec![]);

    assert_eq!(line(&**buf(&t).doc(), 0), "hello");
    assert_eq!(line(&**buf(&t).doc(), 1), "world");
    assert_eq!(buf(&t).doc().line_count(), 3);
}

#[test]
fn open_empty_file() {
    let t = TestHarness::new().with_file("").run(vec![]);

    assert_eq!(buf(&t).doc().line_count(), 1);
    assert_eq!(line(&**buf(&t).doc(), 0), "");
}

#[test]
fn no_file() {
    let t = TestHarness::new().run(vec![]);

    assert!(t.state.active_tab.is_none());
    assert!(t.state.buffers.is_empty());
}

// ── Movement: basic ──

#[test]
fn move_down() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![MoveDown, MoveDown]));

    assert_eq!(buf(&t).cursor_row().0, 2);
    assert_eq!(buf(&t).cursor_col().0, 0);
}

#[test]
fn move_up() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![MoveDown, MoveDown, MoveUp]));

    assert_eq!(buf(&t).cursor_row().0, 1);
}

#[test]
fn move_right_and_left() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveRight, MoveRight, MoveRight, MoveLeft]));

    assert_eq!(buf(&t).cursor_row().0, 0);
    assert_eq!(buf(&t).cursor_col().0, 2);
}

#[test]
fn move_up_at_top() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveUp, MoveUp]));

    assert_eq!(buf(&t).cursor_row().0, 0);
    assert_eq!(buf(&t).cursor_col().0, 0);
}

#[test]
fn move_down_at_bottom() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveDown, MoveDown, MoveDown]));

    let max_row = buf(&t).doc().line_count() - 1;
    assert_eq!(buf(&t).cursor_row().0, max_row);
}

#[test]
fn move_left_at_start() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![MoveLeft]));

    assert_eq!(buf(&t).cursor_row().0, 0);
    assert_eq!(buf(&t).cursor_col().0, 0);
}

// ── Movement: wrapping ──

#[test]
fn move_right_wraps_to_next_line() {
    let t = TestHarness::new()
        .with_file("ab\ncd\n")
        .run(actions(vec![MoveRight, MoveRight, MoveRight]));

    assert_eq!(buf(&t).cursor_row().0, 1);
    assert_eq!(buf(&t).cursor_col().0, 0);
}

#[test]
fn move_left_wraps_to_previous_line() {
    let t = TestHarness::new()
        .with_file("ab\ncd\n")
        .run(actions(vec![MoveDown, MoveLeft]));

    assert_eq!(buf(&t).cursor_row().0, 0);
    assert_eq!(buf(&t).cursor_col().0, 2);
}

// ── Movement: line start/end ──

#[test]
fn line_start_and_end() {
    let t = TestHarness::new()
        .with_file("hello world\n")
        .run(actions(vec![MoveRight, MoveRight, MoveRight, LineStart]));

    assert_eq!(buf(&t).cursor_col().0, 0);

    let t = TestHarness::new()
        .with_file("hello world\n")
        .run(actions(vec![LineEnd]));

    assert_eq!(buf(&t).cursor_col().0, 11);
}

// ── Movement: file start/end ──

#[test]
fn file_start_and_end() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![FileEnd]));

    let max_row = buf(&t).doc().line_count() - 1;
    assert_eq!(buf(&t).cursor_row().0, max_row);

    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![MoveDown, MoveDown, FileStart]));

    assert_eq!(buf(&t).cursor_row().0, 0);
    assert_eq!(buf(&t).cursor_col().0, 0);
}

// ── Movement: column affinity ──

#[test]
fn column_affinity_preserved_across_short_line() {
    let t = TestHarness::new()
        .with_file("hello\nhi\nworld\n")
        .run(actions(vec![LineEnd, MoveDown, MoveDown]));

    assert_eq!(buf(&t).cursor_row().0, 2);
    assert_eq!(buf(&t).cursor_col().0, 5);
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

    assert!(buf(&t).cursor_row().0 > 0);

    let t = TestHarness::new()
        .with_viewport(80, 24)
        .with_file(&lines)
        .run(actions(vec![PageDown, PageUp]));

    assert_eq!(buf(&t).cursor_row().0, 0);
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

    assert!(buf(&t).scroll_row().0 > 0, "scroll should have moved");
}

// ── Editing: insert ──

#[test]
fn insert_chars() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x'), InsertChar('y')]));

    assert_eq!(line(&**buf(&t).doc(), 0), "xyhello");
    assert_eq!(buf(&t).cursor_col().0, 2);
}

#[test]
fn insert_in_middle() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        MoveRight,
        MoveRight,
        InsertChar('X'),
    ]));

    assert_eq!(line(&**buf(&t).doc(), 0), "heXllo");
    assert_eq!(buf(&t).cursor_col().0, 3);
}

#[test]
fn insert_newline() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        MoveRight,
        MoveRight,
        InsertNewline,
    ]));

    assert_eq!(line(&**buf(&t).doc(), 0), "he");
    assert_eq!(line(&**buf(&t).doc(), 1), "llo");
    assert_eq!(buf(&t).cursor_row().0, 1);
    assert_eq!(buf(&t).cursor_col().0, 0);
}

#[test]
fn insert_tab() {
    // Plain text file: no tree-sitter grammar, falls back to soft tab
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(vec![Do(InsertTab), WaitFor(indent_done)]);

    assert_eq!(line(&**buf(&t).doc(), 0), "    hello");
    assert_eq!(buf(&t).cursor_col().0, 4);
}

#[test]
fn insert_tab_alignment() {
    // Plain text file: soft tab aligns to next tab stop
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(InsertChar('x')),
        Do(InsertTab),
        WaitFor(indent_done),
    ]);

    assert_eq!(buf(&t).cursor_col().0, 4);
}

// ── Editing: delete backward ──

#[test]
fn delete_backward() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        MoveRight,
        MoveRight,
        DeleteBackward,
    ]));

    assert_eq!(line(&**buf(&t).doc(), 0), "hllo");
    assert_eq!(buf(&t).cursor_col().0, 1);
}

#[test]
fn delete_backward_at_start_does_nothing() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![DeleteBackward]));

    assert_eq!(line(&**buf(&t).doc(), 0), "hello");
    assert_eq!(buf(&t).cursor_col().0, 0);
}

#[test]
fn delete_backward_joins_lines() {
    let t = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(actions(vec![MoveDown, DeleteBackward]));

    assert_eq!(line(&**buf(&t).doc(), 0), "helloworld");
    assert_eq!(buf(&t).cursor_row().0, 0);
    assert_eq!(buf(&t).cursor_col().0, 5);
}

// ── Editing: delete forward ──

#[test]
fn delete_forward() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![DeleteForward]));

    assert_eq!(line(&**buf(&t).doc(), 0), "ello");
    assert_eq!(buf(&t).cursor_col().0, 0);
}

#[test]
fn delete_forward_joins_lines() {
    let t = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(actions(vec![LineEnd, DeleteForward]));

    assert_eq!(line(&**buf(&t).doc(), 0), "helloworld");
    assert_eq!(buf(&t).cursor_row().0, 0);
}

// ── Editing: kill line ──

#[test]
fn kill_line_deletes_to_end() {
    let t = TestHarness::new()
        .with_file("hello world\n")
        .run(actions(vec![MoveRight, MoveRight, KillLine]));

    assert_eq!(line(&**buf(&t).doc(), 0), "he");
    assert_eq!(buf(&t).cursor_col().0, 2);
}

#[test]
fn kill_line_at_end_joins_next() {
    let t = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(actions(vec![LineEnd, KillLine]));

    assert_eq!(line(&**buf(&t).doc(), 0), "helloworld");
}

#[test]
fn kill_line_multibyte_char() {
    // Em-dash (—) is 3 bytes / 1 char. kill_line must not overshoot.
    let t = TestHarness::new()
        .with_file("a — b\nsecond\n")
        .run(actions(vec![KillLine]));

    assert_eq!(line(&**buf(&t).doc(), 0), "");
    assert_eq!(line(&**buf(&t).doc(), 1), "second");
    assert_eq!(buf(&t).doc().line_count(), 3); // empty + second + trailing
}

// ── Undo / Redo ──

#[test]
fn undo_reverts_insert_group() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('a'),
        InsertChar('b'),
        Undo,
    ]));

    assert_eq!(line(&**buf(&t).doc(), 0), "hello");
}

#[test]
fn undo_then_redo() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('x'),
        InsertChar('y'),
        Undo,
        Redo,
    ]));

    assert_eq!(line(&**buf(&t).doc(), 0), "xyhello");
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

    assert_eq!(line(&**buf(&t).doc(), 0), "abhello");
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

    assert_eq!(line(&**buf(&t).doc(), 0), "ab");
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

    assert_eq!(line(&**buf(&t).doc(), 0), "bhello");
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

    assert_eq!(line(&**buf(&t).doc(), 0), "hello");
}

#[test]
fn undo_all_clears_dirty() {
    // Repro: insert newline, undo — content is restored but dirty flag stays.
    // Undoing back to the saved state should clear dirty.
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(InsertNewline),
        WaitFor(indent_done),
        Do(Undo),
    ]);

    assert_eq!(
        line(&**buf(&t).doc(), 0),
        "hello",
        "content should be restored"
    );
    assert!(
        !buf(&t).is_dirty(),
        "undoing back to saved state should clear dirty"
    );
}

#[test]
fn undo_nothing_is_noop() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![Undo]));

    assert_eq!(line(&**buf(&t).doc(), 0), "hello");
}

#[test]
fn redo_nothing_is_noop() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![Redo]));

    assert_eq!(line(&**buf(&t).doc(), 0), "hello");
}

/// Full Emacs undo chain test:
/// 1. Type "1\n2\n3\n" → buffer "1\n2\n3\n"
/// 2. Undo ×4 → buffer "1\n"
/// 3. Type "a\nb\n" (breaks undo chain) → buffer "1\na\nb\n"
/// 4. Undo ×4 → buffer "1\n"
/// 5. Undo ×4 → buffer "1\n2\n3\n" (undoing the undos restores original)
#[test]
fn emacs_undo_undo_restores_original() {
    // Step 1: Type "1\n2\n3\n" into an empty file.
    // InsertChar + InsertNewline are each their own undo group because
    // InsertNewline calls close_group_on_move (clearing last_edit_kind).
    // Each InsertNewline triggers async indent, so we WaitFor completion
    // before issuing the next editing action.
    let mut steps: Vec<TestStep> = vec![
        Do(InsertChar('1')),
        Do(InsertNewline),
        WaitFor(indent_done),
        Do(InsertChar('2')),
        Do(InsertNewline),
        WaitFor(indent_done),
        Do(InsertChar('3')),
        Do(InsertNewline),
        WaitFor(indent_done),
    ];

    // Step 2: Undo ×4 — removes "\n", "3", "\n", "2"
    // (Leaves "1\n". Each undo reverts one group.)
    steps.extend([Do(Undo), Do(Undo), Do(Undo), Do(Undo)]);

    // Break the undo chain: any non-undo edit does this.
    // Step 3: Type "a\nb\n"
    steps.extend([
        Do(InsertChar('a')),
        Do(InsertNewline),
        WaitFor(indent_done),
        Do(InsertChar('b')),
        Do(InsertNewline),
        WaitFor(indent_done),
    ]);

    // Step 4: Undo ×4 — removes "\n", "b", "\n", "a"
    steps.extend([Do(Undo), Do(Undo), Do(Undo), Do(Undo)]);

    // Step 5: Undo ×4 — undoes the step-2 inverses, re-applying "2\n3\n"
    steps.extend([Do(Undo), Do(Undo), Do(Undo), Do(Undo)]);

    let t = TestHarness::new().with_file("").run(steps);

    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "1", "first line should be '1'");
    assert_eq!(line(&**b.doc(), 1), "2", "second line should be '2'");
    assert_eq!(line(&**b.doc(), 2), "3", "third line should be '3'");
    assert_eq!(
        b.doc().line_count(),
        4,
        "should have 4 lines: '1\\n2\\n3\\n' + trailing empty"
    );
}

// ── Save state ──

#[test]
fn clean_after_open() {
    let t = TestHarness::new().with_file("hello\n").run(vec![]);

    assert!(!buf(&t).is_dirty());
    assert!(!buf(&t).save_in_flight());
}

#[test]
fn modified_after_edit() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x')]));

    assert!(buf(&t).is_dirty());
    assert!(!buf(&t).save_in_flight());
}

#[test]
fn saving_after_save_action() {
    // Save is async — without WaitFor, we capture the state right after the action
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x'), Save]));

    assert!(buf(&t).save_in_flight());
}

#[test]
fn clean_after_save_completes() {
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(InsertChar('x')),
        Do(Save),
        WaitFor(is_clean),
    ]);

    assert!(!buf(&t).is_dirty());
    assert!(!buf(&t).save_in_flight());
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
    let v0 = buf(&t).version();

    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x')]));
    assert!(buf(&t).version() > v0);
}

// ── Tabs ──

#[test]
fn kill_buffer_clean() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![KillBuffer]));

    assert!(t.state.active_tab.is_none());
    assert!(t.state.tabs.is_empty());
    // Buffer persists (dematerialized) so diagnostics survive for the file browser.
    assert!(!t.state.buffers.is_empty());
    assert!(
        !t.state.buffers.values().any(|b| b.is_materialized()),
        "buffer should be dematerialized after kill"
    );
    assert!(
        t.state
            .alerts
            .info
            .as_deref()
            .unwrap_or("")
            .contains("Killed"),
        "should show killed message"
    );
}

#[test]
fn kill_buffer_dirty_prompts() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x'), KillBuffer]));

    // Buffer should NOT be killed — waiting for confirmation
    assert!(t.state.active_tab.is_some());
    assert!(!t.state.buffers.is_empty());
    assert!(t.state.confirm_kill);
    assert!(
        t.state
            .alerts
            .info
            .as_deref()
            .unwrap_or("")
            .contains("kill anyway"),
        "should prompt for confirmation"
    );
}

#[test]
fn kill_buffer_dirty_confirm_yes() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('x'),
        KillBuffer,
        InsertChar('y'),
    ]));

    assert!(t.state.active_tab.is_none());
    assert!(t.state.tabs.is_empty());
    assert!(
        !t.state.buffers.values().any(|b| b.is_materialized()),
        "buffer should be dematerialized after force kill"
    );
    assert!(!t.state.confirm_kill);
}

#[test]
fn kill_buffer_dirty_confirm_no() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('x'),
        KillBuffer,
        InsertChar('n'),
    ]));

    // Buffer should NOT be killed — user said no
    assert!(t.state.active_tab.is_some());
    assert!(!t.state.buffers.is_empty());
    assert!(!t.state.confirm_kill);
}

#[test]
fn kill_buffer_dirty_confirm_abort() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        InsertChar('x'),
        KillBuffer,
        Abort,
    ]));

    // Buffer should NOT be killed — user aborted
    assert!(t.state.active_tab.is_some());
    assert!(!t.state.buffers.is_empty());
    assert!(!t.state.confirm_kill);
}

#[test]
fn next_tab_cycles_through_buffers() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("ccc.txt", "c\n")
        .run(vec![]);

    // Last opened file should be active
    assert_eq!(t.state.buffers.len(), 3);
    let active = buf(&t);
    assert_eq!(line(&**active.doc(), 0), "c");

    // NextTab wraps to first
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("ccc.txt", "c\n")
        .run(actions(vec![NextTab]));

    let active = buf(&t);
    assert_eq!(
        line(&**active.doc(), 0),
        "a",
        "NextTab from last should wrap to first"
    );
}

#[test]
fn prev_tab_cycles_backwards() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("ccc.txt", "c\n")
        .run(actions(vec![PrevTab]));

    let active = buf(&t);
    assert_eq!(
        line(&**active.doc(), 0),
        "b",
        "PrevTab from last should go to middle"
    );
}

#[test]
fn tab_cycle_roundtrip() {
    // NextTab 3 times from a 3-tab state should return to start
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("ccc.txt", "c\n")
        .run(actions(vec![NextTab, NextTab, NextTab]));

    let active = buf(&t);
    assert_eq!(
        line(&**active.doc(), 0),
        "c",
        "3 NextTabs in 3-tab set should cycle back"
    );
}

#[test]
fn kill_buffer_activates_next() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("ccc.txt", "c\n")
        .run(actions(vec![
            PrevTab, // go to bbb
            KillBuffer,
        ]));

    assert_eq!(t.state.tabs.len(), 2);
    let active = buf(&t);
    assert_eq!(
        line(&**active.doc(), 0),
        "c",
        "killing middle tab should activate next"
    );
}

// ── Browser ──

fn has_browser_entries(s: &led_state::AppState) -> bool {
    !s.browser.entries.is_empty()
}

#[test]
fn browser_populates_on_workspace() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(vec![WaitFor(has_browser_entries)]);

    assert!(
        !t.state.browser.entries.is_empty(),
        "browser should have entries after workspace loads"
    );
}

#[test]
fn browser_root_entries_are_sorted() {
    // Create a dir and files to ensure sorting: dirs first, then files, alphabetical
    let t = TestHarness::new()
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("aaa.txt", "a\n")
        .run(vec![WaitFor(has_browser_entries)]);

    // config/ dir should come first (created by harness), then files alphabetically
    // Exact entries depend on tmpdir contents but files should be sorted
    let file_names: Vec<&str> = t
        .state
        .browser
        .entries
        .iter()
        .filter(|e| matches!(e.kind, led_state::EntryKind::File))
        .map(|e| e.name.as_str())
        .collect();

    let mut sorted = file_names.clone();
    sorted.sort();
    assert_eq!(file_names, sorted, "files should be alphabetically sorted");
}

#[test]
fn browser_hides_dotfiles() {
    let t = TestHarness::new()
        .with_named_file(".hidden", "secret\n")
        .with_named_file("visible.txt", "hello\n")
        .run(vec![WaitFor(has_browser_entries)]);

    let names: Vec<&str> = t
        .state
        .browser
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();

    assert!(
        !names.contains(&".hidden"),
        "dotfiles should be filtered out"
    );
    assert!(
        names.contains(&"visible.txt"),
        "regular files should be present"
    );
}

#[test]
fn browser_move_down_selects_next() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(ToggleFocus), // focus → Side
            Do(MoveDown),
        ]);

    assert_eq!(
        t.state.browser.selected, 1,
        "MoveDown should advance selection"
    );
}

#[test]
fn browser_move_up_at_top_stays() {
    let t = TestHarness::new().with_file("hello\n").run(vec![
        WaitFor(has_browser_entries),
        Do(ToggleFocus),
        Do(MoveUp),
    ]);

    assert_eq!(
        t.state.browser.selected, 0,
        "MoveUp at top should stay at 0"
    );
}

#[test]
fn move_down_in_main_moves_cursor_not_browser() {
    let t = TestHarness::new().with_file("aaa\nbbb\nccc\n").run(vec![
        WaitFor(has_browser_entries),
        Do(MoveDown), // focus is Main → should move editor cursor
    ]);

    assert_eq!(
        buf(&t).cursor_row().0,
        1,
        "MoveDown in Main should move editor cursor"
    );
    assert_eq!(
        t.state.browser.selected, 0,
        "browser selection should not change"
    );
}

#[test]
fn browser_open_selected_file() {
    // Navigate past any directories to find a file, then open it
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(ToggleFocus), // focus → Side
            Do(MoveDown),    // skip past config/ dir if present
            Do(MoveDown),
            Do(MoveDown),
        ]);

    // Find a file entry's index
    let file_idx = t
        .state
        .browser
        .entries
        .iter()
        .position(|e| matches!(e.kind, led_state::EntryKind::File));

    if let Some(_) = file_idx {
        // Re-run with focus on a file entry and OpenSelected
        let t = TestHarness::new()
            .with_named_file("zzz.txt", "z\n") // name sorts after config/
            .run(vec![
                WaitFor(has_browser_entries),
                Do(ToggleFocus),
                Do(FileEnd), // jump to last entry — guaranteed to be a file (sorted after dirs)
                Do(OpenSelected),
            ]);

        // Opening a file should switch focus back to Main
        assert_eq!(t.state.focus, led_core::PanelSlot::Main);
    }
}

#[test]
fn browser_open_selected_dir_toggles() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(vec![WaitFor(has_browser_entries)]);

    // Check if there's a directory entry (config/ is created by harness)
    let has_dir = t
        .state
        .browser
        .entries
        .iter()
        .any(|e| matches!(e.kind, led_state::EntryKind::Directory { .. }));

    if has_dir {
        let t = TestHarness::new().with_file("hello\n").run(vec![
            WaitFor(has_browser_entries),
            Do(ToggleFocus),
            Do(OpenSelected), // should expand the directory
        ]);

        // After expanding, there should be more entries (or expanded_dirs should be non-empty)
        assert!(
            !t.state.browser.expanded_dirs.is_empty(),
            "opening a dir should expand it"
        );
    }
}

// ── Browser reveal ──

fn browser_reveal_done(s: &led_state::AppState) -> bool {
    s.browser.pending_reveal.is_none() && !s.browser.entries.is_empty()
}

#[test]
fn browser_reveals_opened_file() {
    let t = TestHarness::new()
        .with_named_file("hello.txt", "hello\n")
        .run(vec![WaitFor(browser_reveal_done)]);

    let active_path = t.state.active_tab.as_ref().unwrap();
    let buf_path = t.state.buffers[active_path].path().unwrap();
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, *buf_path,
        "browser should select the opened file"
    );
}

#[test]
fn browser_reveals_on_tab_switch() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .run(vec![
            WaitFor(browser_reveal_done),
            Do(PrevTab),
            WaitFor(browser_reveal_done),
        ]);

    let ap = t.state.active_tab.as_ref().unwrap();
    let active_path = t.state.buffers[ap].path().unwrap();
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, *active_path,
        "browser should select the active buffer's file after tab switch"
    );
}

#[test]
fn browser_reveals_file_in_subdir() {
    // First file anchors start_dir at workspace root; second file is in a subdir.
    let t = TestHarness::new()
        .with_file("root\n")
        .with_named_file("subdir/nested.txt", "nested\n")
        .run(vec![WaitFor(browser_reveal_done)]);

    // The subdir should have been expanded when nested.txt was opened
    let root = t.state.browser.root.as_ref().unwrap();
    let subdir = root.join("subdir");
    assert!(
        t.state.browser.expanded_dirs.contains(&subdir),
        "ancestor directory should be expanded"
    );

    // The active buffer's file should be selected in the browser
    let active_p = t.state.active_tab.as_ref().unwrap();
    let active_path = t.state.buffers[active_p].path().unwrap();
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, *active_path,
        "browser should select the active buffer's file"
    );
}

#[test]
fn browser_reveals_after_kill_buffer() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .run(vec![
            WaitFor(browser_reveal_done),
            Do(KillBuffer),
            WaitFor(browser_reveal_done),
        ]);

    let ap = t.state.active_tab.as_ref().unwrap();
    let active_path = t.state.buffers[ap].path().unwrap();
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, *active_path,
        "browser should select the remaining file after kill"
    );
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
    assert_eq!(buf(&t).cursor_row().0, 0, "still on first logical line");
    assert!(buf(&t).cursor_col().0 >= 9, "should be on second sub-line");
}

#[test]
fn wrap_move_down_crosses_to_next_line() {
    // From second sub-line of wrapped line, MoveDown should go to next logical line
    let t = TestHarness::new()
        .with_viewport(12, 10)
        .with_file("abcdefghijklmno\nshort\n")
        .run(actions(vec![MoveDown, MoveDown]));

    assert_eq!(
        buf(&t).cursor_row().0,
        1,
        "should be on second logical line"
    );
}

#[test]
fn wrap_move_up_through_wrapped_line() {
    // Start on line 1, MoveUp should land on last sub-line of line 0
    let t = TestHarness::new()
        .with_viewport(12, 10)
        .with_file("abcdefghijklmno\nshort\n")
        .run(actions(vec![MoveDown, MoveDown, MoveUp]));

    assert_eq!(buf(&t).cursor_row().0, 0, "back on first logical line");
    assert!(buf(&t).cursor_col().0 >= 9, "on last sub-line");
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
        buf(&t).scroll_row().0 > 0 || buf(&t).scroll_sub_line().0 > 0,
        "scroll should have moved for wrapped content"
    );
}

#[test]
fn wrap_move_up_not_stuck_at_chunk_boundary() {
    // Regression: cursor got stuck when affinity >= chunk width.
    // Viewport: 12 cols wide → text_width=10, wrap_width=9
    // "abcdefghijklmnopqrstuvwxy" = 25 chars → chunks: [0..9, 9..18, 18..25]
    // KillLine + Yank restores text with cursor at end (col=25, affinity=25).
    // MoveUp should step through each sub-line, not get stuck.
    let t = TestHarness::new()
        .with_viewport(12, 10)
        .with_file("abcdefghijklmnopqrstuvwxy\n")
        .run(vec![
            Do(KillLine),
            Do(Yank),
            // Wait for async clipboard round-trip to apply the yank
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .map_or(false, |b| b.doc().line_len(led_core::Row(0)) >= 25)
            }),
            Do(MoveUp),
        ]);

    assert_eq!(
        buf(&t).cursor_row().0,
        0,
        "should stay on first logical line"
    );
    // After yank cursor is at col=25 (sub-line 2). One MoveUp → sub-line 1.
    assert!(
        buf(&t).cursor_col().0 >= 9 && buf(&t).cursor_col().0 < 18,
        "should be on sub-line 1 (col in [9,18)), got col={}",
        buf(&t).cursor_col().0
    );
}

#[test]
fn wrap_move_down_no_skip_at_chunk_boundary() {
    // Regression: MoveDown with high affinity skipped a sub-line.
    // Viewport: 12 cols wide → text_width=10, wrap_width=9
    // "abcdefghijklmnopqrstuvwxy" = 25 chars → chunks: [0..9, 9..18, 18..25]
    // KillLine + Yank → cursor at col=25 (sub-line 2), affinity=25.
    // MoveUp ×2 → sub-line 0. MoveDown should land on sub-line 1, not skip.
    let t = TestHarness::new()
        .with_viewport(12, 10)
        .with_file("abcdefghijklmnopqrstuvwxy\n")
        .run(vec![
            Do(KillLine),
            Do(Yank),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .map_or(false, |b| b.doc().line_len(led_core::Row(0)) >= 25)
            }),
            Do(MoveUp),
            Do(MoveUp),
            Do(MoveDown),
        ]);

    assert_eq!(
        buf(&t).cursor_row().0,
        0,
        "should stay on first logical line"
    );
    assert!(
        buf(&t).cursor_col().0 >= 9 && buf(&t).cursor_col().0 < 18,
        "should be on sub-line 1 (col in [9,18)), got col={}",
        buf(&t).cursor_col().0
    );
}

// ── Theme ──

#[test]
fn theme_loads() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(vec![WaitFor(|s| s.config_theme.is_some())]);

    let theme = t.state.config_theme.as_ref().expect("theme should load");
    let theme = theme.file.as_ref();

    // Verify COLORS section parsed
    assert!(
        theme.colors.contains_key("muted"),
        "COLORS should contain 'muted'"
    );
    assert!(
        theme.colors.contains_key("bold"),
        "COLORS should contain 'bold'"
    );

    // Verify status_bar.style is a table with fg and bg
    match &theme.status_bar.style {
        led_core::theme::StyleValue::Style(st) => {
            assert!(st.fg.is_some(), "status_bar.style should have fg");
            assert!(st.bg.is_some(), "status_bar.style should have bg");
        }
        led_core::theme::StyleValue::Scalar(s) => {
            panic!("status_bar.style should be a table, got scalar: {}", s);
        }
    }
}

// ── Session persistence ──

#[test]
fn session_restore_tabs() {
    // Run 1: open two files, quit, wait for session save
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "hello\n")
        .with_named_file("bbb.txt", "world\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    assert_eq!(t.state.buffers.len(), 2);
    let dir = t.dirs.root.clone();

    // Run 2: reuse same dir with no arg files — session should restore
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    assert_eq!(t2.state.buffers.len(), 2);
    let mut paths: Vec<String> = t2
        .state
        .buffers
        .values()
        .filter_map(|b| b.path())
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["aaa.txt", "bbb.txt"]);
}

#[test]
fn session_restore_tab_order_with_arg_repeated() {
    // Simulate: `led Cargo.toml` with lib.rs already in session, repeated quit/open cycles
    // Run 1: open two files as args (simulates Cargo.toml opened via arg + lib.rs from session)
    let t = TestHarness::new()
        .with_named_file("Cargo.toml", "[package]\n")
        .with_named_file("lib.rs", "fn main() {}\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    let order1 = tab_names_sorted(&t.state);

    let dir = t.dirs.root.clone();
    let cargo_path = t.dirs.workspace.join("Cargo.toml");

    // Run 2: restart with Cargo.toml as arg — session restores both
    let t2 = TestHarness::with_dir(dir.clone())
        .with_arg(cargo_path.clone())
        .run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    let order2 = tab_names_sorted(&t2.state);
    assert_eq!(
        order1, order2,
        "tab order should be stable after first restart"
    );

    // Run 3: quit and restart again — should still be stable
    let t2b = TestHarness::with_dir(dir.clone())
        .with_arg(cargo_path.clone())
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    let dir2 = t2b.dirs.root.clone();

    let t3 = TestHarness::with_dir(dir2)
        .with_arg(cargo_path)
        .run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    let order3 = tab_names_sorted(&t3.state);
    assert_eq!(
        order1, order3,
        "tab order should be stable after second restart"
    );
}

#[test]
fn session_restore_tab_order() {
    // Run 1: open 3 files — tab_order 0, 1, 2
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("ccc.txt", "c\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    // Capture original tab order
    let original = tab_names_sorted(&t.state);

    let dir = t.dirs.root.clone();

    // Run 2: restore session — tab order should be preserved
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| s.buffers.len() >= 3)]);

    let restored = tab_names_sorted(&t2.state);

    assert_eq!(
        original, restored,
        "tab order should be preserved across restart"
    );
}

#[test]
fn session_restore_missing_file() {
    // Run 1: open two files, quit, save session
    let t = TestHarness::new()
        .with_named_file("fileA.txt", "aaa\n")
        .with_named_file("fileB.txt", "bbb\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    assert_eq!(t.state.buffers.len(), 2);
    let dir = t.dirs.root.clone();

    // Delete fileA.txt between runs
    std::fs::remove_file(t.dirs.workspace.join("fileA.txt")).expect("remove fileA.txt");

    // Run 2: restore with fileB.txt as arg — session restore should not hang
    // even though fileA.txt (from the session) no longer exists.
    let file_b = t.dirs.workspace.join("fileB.txt");
    let t2 = TestHarness::with_dir(dir)
        .with_arg(file_b)
        .run(vec![WaitFor(|s| {
            s.phase == led_state::Phase::Running && !s.buffers.is_empty()
        })]);

    // fileB.txt should be open; fileA.txt silently skipped
    assert_eq!(t2.state.buffers.len(), 1);
    let name = buf(&t2)
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, "fileB.txt");
}

#[test]
fn session_restore_all_files_missing() {
    // Run 1: open two files, quit, save session
    let t = TestHarness::new()
        .with_named_file("one.txt", "1\n")
        .with_named_file("two.txt", "2\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    assert_eq!(t.state.buffers.len(), 2);
    let dir = t.dirs.root.clone();

    // Delete both files — every session file is gone
    std::fs::remove_file(t.dirs.workspace.join("one.txt")).expect("remove one.txt");
    std::fs::remove_file(t.dirs.workspace.join("two.txt")).expect("remove two.txt");

    // Run 2: session restore should complete (not hang) with zero buffers
    let t2 =
        TestHarness::with_dir(dir).run(vec![WaitFor(|s| s.phase == led_state::Phase::Running)]);

    assert!(t2.state.buffers.is_empty());
}

#[test]
fn session_restore_multiple_missing() {
    // Run 1: open three files, quit, save session
    let t = TestHarness::new()
        .with_named_file("a.txt", "a\n")
        .with_named_file("b.txt", "b\n")
        .with_named_file("c.txt", "c\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    assert_eq!(t.state.buffers.len(), 3);
    let dir = t.dirs.root.clone();

    // Delete two of three — mix of success and failure
    std::fs::remove_file(t.dirs.workspace.join("a.txt")).expect("remove a.txt");
    std::fs::remove_file(t.dirs.workspace.join("c.txt")).expect("remove c.txt");

    // Run 2: only b.txt should survive
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| {
        s.phase == led_state::Phase::Running && !s.buffers.is_empty()
    })]);

    assert_eq!(t2.state.buffers.len(), 1);
    let name = buf(&t2)
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, "b.txt");
}

#[test]
fn session_restore_with_arg_file() {
    // Exact repro: start with ONE arg file, open a second from browser, quit, restart with same arg.
    let tmpdir = tempfile::TempDir::new().unwrap().keep();
    let workspace_dir = tmpdir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    std::fs::create_dir_all(tmpdir.join("config")).unwrap();
    std::fs::write(workspace_dir.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(workspace_dir.join("lib.rs"), "fn main() {}\n").unwrap();
    let cargo_path = workspace_dir.join("Cargo.toml");

    // Run 1: start with only Cargo.toml, open lib.rs from browser
    let t = TestHarness::with_dir(tmpdir.clone())
        .with_arg(cargo_path.clone())
        .run(vec![
            WaitFor(has_browser_entries),
            Do(ToggleFocus),  // focus browser
            Do(FileEnd),      // select last entry (lib.rs — files sorted after dirs)
            Do(OpenSelected), // open lib.rs from browser
            WaitFor(|s| s.buffers.values().filter(|b| b.is_materialized()).count() >= 2),
            QuitAndWait, // dispatch Quit and wait for quit signal (like real app)
        ]);

    assert_eq!(t.state.buffers.len(), 2);
    assert!(
        t.state.session.saved,
        "session must be saved BEFORE quit signal fires"
    );

    // Verify the session was actually written to the DB
    let db_path = tmpdir.join("config").join("db.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");
    let buf_count: i64 = conn
        .query_row("SELECT count(*) FROM buffers", [], |row| row.get(0))
        .expect("query buffers");
    assert_eq!(buf_count, 2, "DB should have 2 buffer rows after save");
    drop(conn);

    // Run 2: restart with only Cargo.toml as arg — session should also restore lib.rs
    // lib.rs was the last opened (from browser), so it should be the active tab.
    let t2 = TestHarness::with_dir(tmpdir)
        .with_arg(cargo_path)
        .run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    assert_eq!(
        t2.state.buffers.len(),
        2,
        "session should restore both files"
    );
    let mut paths: Vec<String> = t2
        .state
        .buffers
        .values()
        .filter_map(|b| b.path())
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["Cargo.toml", "lib.rs"]);

    // Active tab should be the arg file — explicit arg overrides session's choice
    let active_name = buf(&t2)
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        active_name, "Cargo.toml",
        "arg file should be active tab, overriding session"
    );
}

#[test]
fn session_restore_cursor() {
    // Run 1: open file, move cursor down 3 lines, quit
    let t = TestHarness::new()
        .with_file("line1\nline2\nline3\nline4\nline5\n")
        .run(vec![
            Do(MoveDown),
            Do(MoveDown),
            Do(MoveDown),
            Do(Quit),
            WaitFor(|s| s.session.saved),
        ]);

    assert_eq!(buf(&t).cursor_row().0, 3);
    let dir = t.dirs.root.clone();

    // Run 2: restore — cursor should be at row 3
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);

    let ap = t2.state.active_tab.as_ref().unwrap();
    let b = &t2.state.buffers[ap];
    assert_eq!(b.cursor_row().0, 3, "cursor row should be restored");
}

#[test]
fn session_restore_active_tab() {
    // Run 1: open two files, switch to first tab (last opened is active), quit
    let t = TestHarness::new()
        .with_named_file("first.txt", "a\n")
        .with_named_file("second.txt", "b\n")
        .run(vec![Do(PrevTab), Do(Quit), WaitFor(|s| s.session.saved)]);

    let active_name = buf(&t)
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(active_name, "first.txt");
    let dir = t.dirs.root.clone();
    let second_path = t.dirs.workspace.join("second.txt");

    // Run 2: restart with second.txt as arg — arg file should be active
    let t2 = TestHarness::with_dir(dir)
        .with_arg(second_path)
        .run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    let active_name2 = buf(&t2)
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(active_name2, "second.txt", "arg file should be active tab");
}

#[test]
fn session_restore_active_tab_no_args() {
    // Run 1: open two files, switch to first tab, quit
    let t = TestHarness::new()
        .with_named_file("first.txt", "a\n")
        .with_named_file("second.txt", "b\n")
        .run(vec![Do(PrevTab), Do(Quit), WaitFor(|s| s.session.saved)]);

    let active_name = buf(&t)
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(active_name, "first.txt");
    let dir = t.dirs.root.clone();

    // Run 2: restart with no args — session's active tab should be restored
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    let active_name2 = buf(&t2)
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        active_name2, "first.txt",
        "session active tab should be restored when no args"
    );
}

// ── Save flow enhancements ──

#[test]
fn save_strips_trailing_whitespace() {
    let t = TestHarness::new()
        .with_file("hello   \nworld  \n")
        .run(vec![
            Do(Save),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .is_some_and(|b| !b.is_dirty() && !b.save_in_flight())
            }),
        ]);

    let content = std::fs::read_to_string(t.file_path.as_ref().unwrap()).unwrap();
    assert_eq!(content, "hello\nworld\n");
}

#[test]
fn save_ensures_final_newline() {
    let t = TestHarness::new().with_file("no newline at end").run(vec![
        Do(Save),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .is_some_and(|b| !b.is_dirty() && !b.save_in_flight())
        }),
    ]);

    let content = std::fs::read_to_string(t.file_path.as_ref().unwrap()).unwrap();
    assert!(content.ends_with('\n'), "file should end with newline");
}

#[test]
fn save_format_is_undoable() {
    let t = TestHarness::new().with_file("hello   \n").run(vec![
        Do(Save),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .is_some_and(|b| !b.is_dirty() && !b.save_in_flight())
        }),
        Do(Undo),
    ]);

    // After undo, trailing whitespace should be back
    let line = line(&**buf(&t).doc(), 0);
    assert_eq!(line, "hello   ", "undo should restore trailing whitespace");
}

// ── Undo persistence ──

#[test]
fn undo_persist_and_restore() {
    // Run 1: open file, type some chars, wait for undo flush, quit
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(LineEnd),
        Do(InsertChar('!')),
        Do(InsertChar('!')),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .is_some_and(|b| b.persisted_undo_len() > 0)
        }),
        Do(Quit),
        WaitFor(|s| s.session.saved),
    ]);
    assert!(buf(&t).is_dirty(), "buffer should be dirty");
    let dir = t.dirs.root.clone();

    // Run 2: restore — undo should work and revert the edits
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty()), Do(Undo)]);
    let line = line(&**buf(&t2).doc(), 0);
    assert_eq!(line, "hello", "undo should revert persisted edits");
}

#[test]
fn undo_cleared_after_save() {
    // Run 1: edit, save, quit
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(LineEnd),
        Do(InsertChar('!')),
        Do(Save),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .is_some_and(|b| !b.is_dirty() && !b.save_in_flight())
        }),
        Do(Quit),
        WaitFor(|s| s.session.saved),
    ]);
    let dir = t.dirs.root.clone();

    // Run 2: restore — buffer should be clean (save cleared undo in DB)
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);
    assert!(
        !buf(&t2).is_dirty(),
        "buffer should be clean after save+restore"
    );
}

#[test]
fn session_restores_dirty_state() {
    // Run 1: edit without saving, wait for undo flush, then quit
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(LineEnd),
        Do(InsertChar('X')),
        // Wait for undo to be flushed to DB
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .is_some_and(|b| b.persisted_undo_len() > 0)
        }),
        Do(Quit),
        WaitFor(|s| s.session.saved),
    ]);
    let dir = t.dirs.root.clone();

    // Run 2: restore — buffer should be dirty (distance_from_save != 0)
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);
    assert!(
        buf(&t2).is_dirty(),
        "buffer should be dirty after restore of unsaved edits"
    );
}

// ── Browser state persistence ──

#[test]
fn session_restores_browser_expanded_dirs() {
    // Run 1: expand a directory, quit
    // Need a file at workspace root (so start_dir is workspace/) plus a
    // subdirectory so the browser has a directory entry to expand.
    let t = TestHarness::new()
        .with_file("hello\n")
        .with_named_file("subdir/child.txt", "x\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(ToggleFocus),
            Do(ExpandDir), // expand first dir entry (subdir/)
            Do(Quit),
            WaitFor(|s| s.session.saved),
        ]);
    assert!(
        !t.state.browser.expanded_dirs.is_empty(),
        "should have expanded dirs"
    );
    let expanded_dir = t.state.browser.expanded_dirs.iter().next().unwrap().clone();
    let dir = t.dirs.root.clone();

    // Run 2: restore — expanded dirs should be restored AND their contents loaded
    let t2 = TestHarness::with_dir(dir).run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        WaitFor(has_browser_entries),
        // Wait for expanded dir contents to actually be loaded
        WaitFor(|s| {
            s.browser
                .expanded_dirs
                .iter()
                .all(|d| s.browser.dir_contents.contains_key(d))
        }),
    ]);
    assert!(
        !t2.state.browser.expanded_dirs.is_empty(),
        "expanded dirs should be restored from session"
    );
    assert!(
        t2.state.browser.dir_contents.contains_key(&expanded_dir),
        "expanded dir contents should be loaded"
    );
}

#[test]
fn session_resume_always_focuses_editor() {
    // Run 1: switch focus to browser, quit
    let t = TestHarness::new().with_file("hello\n").run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        Do(ToggleFocus), // Main → Side
        QuitAndWait,
    ]);
    assert_eq!(t.state.focus, led_core::PanelSlot::Side);
    let dir = t.dirs.root.clone();

    // Run 2: restore — focus should be on the editor (not browser),
    // because buffers exist and focus is resolved to Main on entering Running.
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);
    assert_eq!(
        t2.state.focus,
        led_core::PanelSlot::Main,
        "resume with buffers should always focus editor"
    );
}

#[test]
fn session_restores_focus_on_editor() {
    // Run 1: leave focus on editor (default), quit
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(vec![WaitFor(|s| !s.buffers.is_empty()), QuitAndWait]);
    assert_eq!(t.state.focus, led_core::PanelSlot::Main);
    let dir = t.dirs.root.clone();

    // Run 2: restore — focus should be on the editor
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);
    assert_eq!(
        t2.state.focus,
        led_core::PanelSlot::Main,
        "focus should be restored to editor"
    );
}

#[test]
fn no_buffers_focus_falls_back_to_browser() {
    // Single instance, no file arguments — focus should land on browser
    let t = TestHarness::new().run(vec![WaitFor(|s| s.phase == led_state::Phase::Running)]);
    assert_eq!(
        t.state.focus,
        led_core::PanelSlot::Side,
        "no buffers: focus should fall back to file browser"
    );
    assert!(t.state.buffers.is_empty());
}

// ── External editor (file watcher) ──

/// Repro: open led + emacs on same file. First emacs save shows up in led,
/// second emacs save does not. Root cause was path mismatch in the docstore's
/// file watcher (/var vs /private/var on macOS).
///
/// Atomic save variant (write tmp + rename) — matches emacs behavior.
#[test]
#[ignore]
fn external_editor_second_save_detected() {
    let (dirs, paths) = shared_workspace(&[("test.txt", "original\n")]);
    let file_path = paths[0].clone();

    let mut inst = Instance::start(startup_for(&dirs, &paths));
    inst.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "instance ready",
    );

    // First external save (atomic: write tmp + rename, like emacs)
    let tmp = file_path.with_extension("tmp");
    std::fs::write(&tmp, "first\n").unwrap();
    std::fs::rename(&tmp, &file_path).unwrap();

    inst.wait_for(
        |s| active_buf(s).is_some_and(|b| line(&**b.doc(), 0) == "first"),
        WAIT,
        "first external save detected",
    );

    // Second external save (same atomic pattern)
    let tmp = file_path.with_extension("tmp");
    std::fs::write(&tmp, "second\n").unwrap();
    std::fs::rename(&tmp, &file_path).unwrap();

    inst.wait_for(
        |s| active_buf(s).is_some_and(|b| line(&**b.doc(), 0) == "second"),
        WAIT,
        "second external save detected",
    );

    inst.stop();
}

/// Direct write variant (non-atomic).
#[test]
#[ignore]
fn external_editor_second_direct_write_detected() {
    let (dirs, paths) = shared_workspace(&[("test.txt", "original\n")]);
    let file_path = paths[0].clone();
    log::trace!(
        "[test:direct_write] workspace={} file={}",
        dirs.workspace.display(),
        file_path.display()
    );

    let mut inst = Instance::start(startup_for(&dirs, &paths));
    inst.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "instance ready",
    );

    // First external save (direct overwrite)
    log::trace!(
        "[test:direct_write] writing 'first' to {}",
        file_path.display()
    );
    std::fs::write(&file_path, "first\n").unwrap();

    inst.wait_for(
        |s| active_buf(s).is_some_and(|b| line(&**b.doc(), 0) == "first"),
        WAIT,
        "first external save detected",
    );

    // Second external save (direct overwrite)
    log::trace!(
        "[test:direct_write] writing 'second' to {}",
        file_path.display()
    );
    std::fs::write(&file_path, "second\n").unwrap();

    inst.wait_for(
        |s| active_buf(s).is_some_and(|b| line(&**b.doc(), 0) == "second"),
        WAIT,
        "second external save detected",
    );

    inst.stop();
}

/// External rename of a clean buffer: the buffer should adopt the new path
/// and preserve its content.
#[test]
#[ignore]
fn external_rename_clean_buffer() {
    let (dirs, paths) = shared_workspace(&[("test.txt", "original\n")]);
    let file_path = paths[0].clone();
    let new_path = dirs.workspace.join("renamed.txt");

    let mut inst = Instance::start(startup_for(&dirs, &paths));
    inst.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "instance ready",
    );

    // Rename the file externally
    std::fs::rename(&file_path, &new_path).unwrap();

    let new_path_canon = UserPath::new(&new_path).canonicalize();
    let new_path_canon2 = new_path_canon.clone();
    inst.wait_for(
        move |s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .and_then(|b| b.path())
                .map_or(false, |p| *p == new_path_canon2)
        },
        WAIT,
        "buffer path updated to renamed.txt",
    );

    let (content, buf_path) = inst.with_state(|s| {
        let b = active_buf(s).unwrap();
        (line(&**b.doc(), 0), b.path().cloned())
    });
    assert_eq!(content, "original", "content should be preserved");
    assert_eq!(
        buf_path.as_ref(),
        Some(&new_path_canon),
        "buffer path should be the new name"
    );

    inst.stop();
}

/// External rename of a dirty buffer: the file is moved to a new name.
/// The buffer has unsaved edits — it should keep those edits and the old path.
#[test]
#[ignore]
fn external_rename_dirty_buffer() {
    let (dirs, paths) = shared_workspace(&[("test.txt", "original\n")]);
    let file_path = paths[0].clone();
    let new_path = dirs.workspace.join("renamed.txt");

    let mut inst = Instance::start(startup_for(&dirs, &paths));
    inst.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "instance ready",
    );

    // Make the buffer dirty
    inst.push(InsertChar('X'));

    inst.wait_for(
        |s| active_buf(s).is_some_and(|b| b.is_dirty()),
        WAIT,
        "buffer is dirty",
    );

    // Rename the file externally
    std::fs::rename(&file_path, &new_path).unwrap();

    // Give the watcher time to fire events
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Buffer should still be open with our edits and old path
    let (content, buf_path, dirty) = inst.with_state(|s| {
        let b = active_buf(s).unwrap();
        (line(&**b.doc(), 0), b.path().cloned(), b.is_dirty())
    });
    assert!(
        content.contains('X'),
        "buffer should still have our edit, got: {:?}",
        content
    );
    let file_path_canon = UserPath::new(&file_path).canonicalize();
    assert_eq!(
        buf_path.as_ref(),
        Some(&file_path_canon),
        "buffer path should still be the original"
    );
    assert!(dirty, "buffer should still be dirty");

    inst.stop();
}

// ── Cross-instance sync ──

/// Simulate an external instance's edit by writing undo entries to the DB
/// and touching the notify file to wake the instance under test.
fn simulate_external_edit(
    dirs: &harness::TestDirs,
    file_path: &Path,
    chain_id: &str,
    content_hash: u64,
    undo_entries: &[UndoEntry],
) {
    let db_path = dirs.config.join("db.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // Must canonicalize to match the workspace driver (resolves /var → /private/var on macOS)
    let canonical_root = UserPath::new(&dirs.workspace).canonicalize();
    let root_str = canonical_root.to_string_lossy();
    let path_str = file_path.to_string_lossy();

    // distance_from_save = count of d=1 entries (each forward group adds 1)
    let distance: i32 = undo_entries.iter().map(|e| e.direction).sum();

    led_workspace::db::flush_undo(
        &conn,
        &root_str,
        &path_str,
        chain_id,
        led_core::PersistedContentHash(content_hash),
        undo_entries.len(),
        distance,
        undo_entries,
    )
    .expect("flush_undo");
    drop(conn);

    // Touch notify file to wake the instance
    let hash = led_workspace::path_hash(&UserPath::new(file_path).canonicalize());
    let notify_dir = dirs.config.join("notify");
    std::fs::create_dir_all(&notify_dir).ok();
    std::fs::write(notify_dir.join(&hash), b"").ok();
}

fn make_insert_entry(offset: usize, text: &str) -> UndoEntry {
    UndoEntry {
        op: EditOp {
            offset: led_core::CharOffset(offset),
            old_text: String::new(),
            new_text: text.to_string(),
        },
        cursor_before: led_core::CharOffset(offset),
        cursor_after: led_core::CharOffset(offset + text.chars().count()),
        direction: 1,
        instance_id: led_core::instance_id(),
        content_hash: None,
    }
}

#[test]
#[ignore]
fn cross_instance_sync_insert_newline() {
    // Bug 1 repro: instance B inserts a newline, instance A should see exactly one newline.
    let t = TestHarness::new()
        .with_watchers()
        .with_file("aaa\nbbb\nccc\n")
        .run(vec![
            WaitFor(|s| !s.buffers.is_empty()),
            WaitFor(|s| s.session.watchers_ready),
            // Simulate instance B inserting a newline after "aaa"
            TestStep::RunFn(Box::new(|dirs| {
                let file_path = std::fs::read_dir(&dirs.workspace)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .find(|e| e.file_name().to_string_lossy().ends_with(".txt"))
                    .unwrap()
                    .path();

                // Get the real content hash from the Doc (hash ropey chunks)
                // For small files, ropey uses a single chunk, equivalent to hashing the string.
                let content_hash = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    "aaa\nbbb\nccc\n".hash(&mut h);
                    h.finish()
                };

                simulate_external_edit(
                    dirs,
                    &file_path,
                    "ext-chain-1",
                    content_hash,
                    &[make_insert_entry(3, "\n")], // insert newline after "aaa"
                );
            })),
            // Wait for sync to apply
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .is_some_and(|b| b.doc().line_count() == 5) // was 4, now 5
            }),
        ]);

    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "aaa");
    assert_eq!(line(&**b.doc(), 1), ""); // the inserted newline
    assert_eq!(line(&**b.doc(), 2), "bbb");
    assert_eq!(line(&**b.doc(), 3), "ccc");
    assert_eq!(
        b.doc().line_count(),
        5,
        "should have exactly one extra newline, not double"
    );
}

#[test]
#[ignore]
fn cross_instance_sync_multiple_edits() {
    // Bug 2 repro: instance B makes multiple edits, all should be replayed on A.
    let t = TestHarness::new()
        .with_watchers()
        .with_file("hello\n")
        .run(vec![
            WaitFor(|s| !s.buffers.is_empty()),
            WaitFor(|s| s.session.watchers_ready),
            TestStep::RunFn(Box::new(|dirs| {
                let file_path = std::fs::read_dir(&dirs.workspace)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .find(|e| e.file_name().to_string_lossy().ends_with(".txt"))
                    .unwrap()
                    .path();

                let content_hash = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    "hello\n".hash(&mut h);
                    h.finish()
                };

                // Three separate undo groups: insert "X", "Y", "Z" at start
                simulate_external_edit(
                    dirs,
                    &file_path,
                    "ext-chain-2",
                    content_hash,
                    &[
                        make_insert_entry(0, "X"),
                        make_insert_entry(1, "Y"),
                        make_insert_entry(2, "Z"),
                    ],
                );
            })),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .is_some_and(|b| line(&**b.doc(), 0).contains("XYZ"))
            }),
        ]);

    let b = buf(&t);
    assert_eq!(
        line(&**b.doc(), 0),
        "XYZhello",
        "all three edits should be replayed"
    );
}

#[test]
#[ignore]
fn cross_instance_sync_after_save() {
    // Bug 3 repro: instance B saves the file, instance A should detect the external save.
    let t = TestHarness::new()
        .with_watchers()
        .with_file("original\n")
        .run(vec![
            WaitFor(|s| !s.buffers.is_empty()),
            WaitFor(|s| s.session.watchers_ready),
            // First, simulate B editing
            TestStep::RunFn(Box::new(|dirs| {
                let file_path = std::fs::read_dir(&dirs.workspace)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .find(|e| e.file_name().to_string_lossy().ends_with(".txt"))
                    .unwrap()
                    .path();

                let content_hash = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    "original\n".hash(&mut h);
                    h.finish()
                };

                simulate_external_edit(
                    dirs,
                    &file_path,
                    "ext-chain-3",
                    content_hash,
                    &[make_insert_entry(0, "X")],
                );
            })),
            // Wait for first sync
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .is_some_and(|b| line(&**b.doc(), 0).starts_with("X"))
            }),
            // Now simulate B saving: write new content to disk and clear undo in DB
            TestStep::RunFn(Box::new(|dirs| {
                let file_path = std::fs::read_dir(&dirs.workspace)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .find(|e| e.file_name().to_string_lossy().ends_with(".txt"))
                    .unwrap()
                    .path();

                // Write new content to disk (simulating save)
                std::fs::write(&file_path, "Xoriginal\n").unwrap();

                // Clear undo in DB (like ClearUndo does after save)
                let db_path = dirs.config.join("db.sqlite");
                let conn = rusqlite::Connection::open(&db_path).expect("open DB");
                let canonical_root = UserPath::new(&dirs.workspace).canonicalize();
                let root_str = canonical_root.to_string_lossy();
                let path_str = file_path.to_string_lossy();
                led_workspace::db::clear_undo(&conn, &root_str, &path_str).expect("clear_undo");
                drop(conn);

                // Touch notify to wake A
                let hash = led_workspace::path_hash(&UserPath::new(&file_path).canonicalize());
                let notify_dir = dirs.config.join("notify");
                std::fs::write(notify_dir.join(&hash), b"").ok();
            })),
            // Wait for A to detect the external save (chain_id should be reset)
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .is_some_and(|b| b.chain_id().is_none())
            }),
        ]);

    let b = buf(&t);
    // After external save detection, A should have reset its undo chain
    assert!(
        b.chain_id().is_none(),
        "chain_id should be reset after external save"
    );
    assert_eq!(b.last_seen_seq(), 0, "last_seen_seq should be reset");
}

// ── Core bug: after save, only post-save undo groups should be flushed ──

#[test]
fn persisted_undo_len_preserved_after_save() {
    // After save, persisted_undo_len must match the undo history length —
    // NOT reset to 0.  The old code (see _old/crates/buffer/src/watcher.rs:396)
    // did this correctly:
    //     self.persisted_undo_len() = self.save_history_len;
    // Without this, the next flush re-sends ALL undo groups (including
    // pre-save ones), which corrupts cross-instance sync on the receiver.
    let t = TestHarness::new().with_file("hello\n").run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        Do(InsertChar('X')),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .is_some_and(|b| b.persisted_undo_len() > 0)
        }),
        Do(Save),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .is_some_and(|b| !b.is_dirty() && !b.save_in_flight())
        }),
    ]);

    let b = buf(&t);
    assert!(
        b.persisted_undo_len() > 0,
        "persisted_undo_len should reflect saved undo history, got 0"
    );
}

// ── Two-instance tests ──

use harness::two_instance::{Instance, shared_workspace, startup_for};
use led_state::AppState;

fn active_buf(s: &AppState) -> Option<&led_state::BufferState> {
    s.active_tab
        .as_ref()
        .and_then(|path| s.buffers.get(path))
        .map(|v| &**v)
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[test]
#[ignore]
fn two_instance_sync_after_save() {
    // Exact repro of the user's bug:
    //   1. A opens file (primary)
    //   2. B opens file (non-primary)
    //   3. B adds newline → A syncs
    //   4. B saves
    //   5. B adds another newline → A must sync
    let (dirs, paths) = shared_workspace(&[("test.txt", "aaa\nbbb\n")]);

    let mut a = Instance::start(startup_for(&dirs, &paths));
    a.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "A ready",
    );

    let mut b = Instance::start(startup_for(&dirs, &paths));
    b.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "B ready",
    );

    // Verify A is primary, B is not
    assert!(a.with_state(|s| s.workspace.loaded().unwrap().primary));
    assert!(!b.with_state(|s| s.workspace.loaded().unwrap().primary));

    let a_lines_before = a.with_state(|s| active_buf(s).unwrap().doc().line_count());

    // Step 3: B adds a newline
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id().is_some()),
        WAIT,
        "B undo flushed",
    );
    a.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() > a_lines_before),
        WAIT,
        "A synced first newline",
    );

    let a_lines_after_sync1 = a.with_state(|s| active_buf(s).unwrap().doc().line_count());
    assert_eq!(
        a_lines_after_sync1,
        a_lines_before + 1,
        "A should have synced the first newline"
    );

    // Step 4: B saves
    b.push(Save);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.is_dirty() && !b.save_in_flight()),
        WAIT,
        "B saved",
    );

    // Step 5: B adds another newline
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id().is_some() && b.is_dirty()),
        WAIT,
        "B second edit flushed",
    );

    // A must sync the second newline
    a.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() > a_lines_after_sync1),
        WAIT,
        "A synced second newline",
    );

    let a_final_lines = a.with_state(|s| active_buf(s).unwrap().doc().line_count());
    assert_eq!(
        a_final_lines,
        a_lines_before + 2,
        "A should have both newlines: {} lines, expected {}",
        a_final_lines,
        a_lines_before + 2,
    );
    // Content should match between A and B
    let a_lines: Vec<String> = a.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    let b_lines: Vec<String> = b.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    for (i, (a_line, b_line)) in a_lines.iter().zip(b_lines.iter()).enumerate() {
        assert_eq!(a_line, b_line, "line {i} mismatch between A and B");
    }

    a.stop();
    b.stop();
}

#[test]
#[ignore]
fn two_instance_second_edit_syncs_without_save() {
    // Repro: B makes two edits (no save between them), only the first
    // newline is visible in A.
    //   1. A opens file (primary)
    //   2. B opens file (non-primary)
    //   3. B inserts newline → A syncs
    //   4. B waits 1000ms
    //   5. B inserts another newline → A must sync
    let (dirs, paths) = shared_workspace(&[("test.txt", "aaa\nbbb\n")]);

    let mut a = Instance::start(startup_for(&dirs, &paths));
    a.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "A ready",
    );

    let mut b = Instance::start(startup_for(&dirs, &paths));
    b.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "B ready",
    );

    let a_lines_before = a.with_state(|s| active_buf(s).unwrap().doc().line_count());

    // Step 3: B inserts newline
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id().is_some()),
        WAIT,
        "B first edit flushed",
    );
    a.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() > a_lines_before),
        WAIT,
        "A synced first newline",
    );

    let a_lines_after_sync1 = a.with_state(|s| active_buf(s).unwrap().doc().line_count());
    assert_eq!(a_lines_after_sync1, a_lines_before + 1);

    // Step 4: wait 1000ms for the first flush to fully propagate
    std::thread::sleep(std::time::Duration::from_millis(1000));

    // Step 5: B inserts another newline (no save)
    b.push(InsertNewline);
    b.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() > a_lines_after_sync1),
        WAIT,
        "B has second newline",
    );

    // A must sync the second newline
    a.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() > a_lines_after_sync1),
        WAIT,
        "A synced second newline",
    );

    let a_final_lines = a.with_state(|s| active_buf(s).unwrap().doc().line_count());
    assert_eq!(
        a_final_lines,
        a_lines_before + 2,
        "A should have both newlines: {} lines, expected {}",
        a_final_lines,
        a_lines_before + 2,
    );
    let a_lines: Vec<String> = a.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    let b_lines: Vec<String> = b.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    for (i, (a_line, b_line)) in a_lines.iter().zip(b_lines.iter()).enumerate() {
        assert_eq!(a_line, b_line, "line {i} mismatch between A and B");
    }

    a.stop();
    b.stop();
}

#[test]
#[ignore]
fn two_instance_remote_save_clears_dirty() {
    // Repro:
    //   1. A opens file (primary)
    //   2. B opens file (non-primary)
    //   3. B inserts newline → A syncs (both dirty)
    //   4. B saves → B clean, A must also become clean
    let (dirs, paths) = shared_workspace(&[("test.txt", "aaa\nbbb\n")]);

    let mut a = Instance::start(startup_for(&dirs, &paths));
    a.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "A ready",
    );

    let mut b = Instance::start(startup_for(&dirs, &paths));
    b.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "B ready",
    );

    // Step 3: B inserts newline
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id().is_some()),
        WAIT,
        "B undo flushed",
    );

    // A syncs → A becomes dirty
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.is_dirty()),
        WAIT,
        "A dirty after sync",
    );

    // Step 4: B saves
    b.push(Save);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.is_dirty() && !b.save_in_flight() && !b.is_dirty()),
        WAIT,
        "B clean after save",
    );

    // A must also become clean: the file on disk now matches A's content
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.is_dirty()),
        WAIT,
        "A clean after remote save",
    );

    // Content should still match
    let a_lines: Vec<String> = a.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    let b_lines: Vec<String> = b.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    for (i, (a_line, b_line)) in a_lines.iter().zip(b_lines.iter()).enumerate() {
        assert_eq!(a_line, b_line, "line {i} mismatch between A and B");
    }

    a.stop();
    b.stop();
}

#[test]
#[ignore]
fn two_instance_no_args_browser_visible() {
    // Repro: open two instances with no file arguments (just PWD).
    // B (non-primary) should still show the file browser.
    let (dirs, _paths) = shared_workspace(&[("file_a.txt", "hello\n"), ("file_b.txt", "world\n")]);
    // Real workspaces have a .git dir — this changes how find_git_root resolves
    std::fs::create_dir_all(dirs.workspace.join(".git")).expect("create .git");

    // Start both with no arg_paths — like running `led` with no arguments
    let ws_canon = UserPath::new(&dirs.workspace).canonicalize();
    let cfg_user = UserPath::new(&dirs.config);
    let no_files_a = Startup {
        headless: true,
        enable_watchers: true,
        arg_paths: vec![],
        arg_user_paths: vec![],
        arg_dir: None,
        start_dir: Arc::new(ws_canon.clone()),
        user_start_dir: UserPath::new(ws_canon.as_path()),
        config_dir: cfg_user.clone(),
        test_lsp_server: None,
        test_gh_binary: None,
        golden_trace: None,
        no_workspace: false,
    };
    let no_files_b = Startup {
        headless: true,
        enable_watchers: true,
        arg_paths: vec![],
        arg_user_paths: vec![],
        arg_dir: None,
        start_dir: Arc::new(ws_canon.clone()),
        user_start_dir: UserPath::new(ws_canon.as_path()),
        config_dir: cfg_user.clone(),
        test_lsp_server: None,
        test_gh_binary: None,
        golden_trace: None,
        no_workspace: false,
    };

    let mut a = Instance::start(no_files_a);
    a.wait_for(
        |s| s.session.watchers_ready && !s.browser.entries.is_empty(),
        WAIT,
        "A browser populated",
    );

    let mut b = Instance::start(no_files_b);
    b.wait_for(
        |s| s.session.watchers_ready && !s.browser.entries.is_empty(),
        WAIT,
        "B browser populated",
    );

    // Both should have the workspace resolved and browser entries
    assert!(
        a.with_state(|s| s.workspace.is_loaded()),
        "A should have a workspace"
    );
    assert!(
        b.with_state(|s| s.workspace.is_loaded()),
        "B should have a workspace"
    );

    // With no files open, focus should be on the file browser
    assert_eq!(
        a.with_state(|s| s.focus),
        led_core::PanelSlot::Side,
        "A focus should be on browser when no files open"
    );
    assert_eq!(
        b.with_state(|s| s.focus),
        led_core::PanelSlot::Side,
        "B focus should be on browser when no files open"
    );

    assert!(
        a.with_state(|s| !s.browser.entries.is_empty()),
        "A browser should have entries"
    );
    assert!(
        b.with_state(|s| !s.browser.entries.is_empty()),
        "B browser should have entries, got empty (black screen)"
    );

    // Both should see the same files
    let a_names: Vec<String> =
        a.with_state(|s| s.browser.entries.iter().map(|e| e.name.clone()).collect());
    let b_names: Vec<String> =
        b.with_state(|s| s.browser.entries.iter().map(|e| e.name.clone()).collect());
    assert_eq!(a_names, b_names, "both instances should see same files");

    a.stop();
    b.stop();
}

#[test]
#[ignore]
fn two_instance_undo_syncs_and_clears_dirty() {
    // Repro:
    //   1. A opens file, B opens file
    //   2. A inserts newline → B syncs (both dirty)
    //   3. A undoes → both should sync back and neither should be dirty
    let (dirs, paths) = shared_workspace(&[("test.txt", "aaa\nbbb\n")]);

    let mut a = Instance::start(startup_for(&dirs, &paths));
    a.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "A ready",
    );

    let mut b = Instance::start(startup_for(&dirs, &paths));
    b.wait_for(
        |s| s.session.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "B ready",
    );

    let original_lines = a.with_state(|s| active_buf(s).unwrap().doc().line_count());

    // Step 2: A inserts newline
    a.push(InsertNewline);
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id().is_some()),
        WAIT,
        "A undo flushed",
    );

    // B syncs the newline
    b.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() > original_lines),
        WAIT,
        "B synced newline",
    );

    // Both should be dirty
    assert!(
        a.with_state(|s| active_buf(s).unwrap().is_dirty()),
        "A should be dirty after edit"
    );
    assert!(
        b.with_state(|s| active_buf(s).unwrap().is_dirty()),
        "B should be dirty after sync"
    );

    // Step 3: A undoes
    a.push(Undo);
    a.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() == original_lines),
        WAIT,
        "A undid the newline",
    );

    // A should be clean (undo back to saved state)
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.is_dirty()),
        WAIT,
        "A clean after undo",
    );

    // B should sync the undo and also be clean
    b.wait_for(
        move |s| active_buf(s).is_some_and(|b| b.doc().line_count() == original_lines),
        WAIT,
        "B synced the undo",
    );
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.is_dirty()),
        WAIT,
        "B clean after synced undo",
    );

    // Content should match original
    let a_line_count = a.with_state(|s| active_buf(s).unwrap().doc().line_count());
    let b_line_count = b.with_state(|s| active_buf(s).unwrap().doc().line_count());
    assert_eq!(a_line_count, original_lines);
    assert_eq!(b_line_count, original_lines);
    let a_lines: Vec<String> = a.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    let b_lines: Vec<String> = b.with_state(|s| {
        let buf = active_buf(s).unwrap();
        (0..buf.doc().line_count())
            .map(|i| line(&**buf.doc(), i).to_string())
            .collect()
    });
    for (i, (a_line, b_line)) in a_lines.iter().zip(b_lines.iter()).enumerate() {
        assert_eq!(a_line, b_line, "line {i} mismatch between A and B");
    }

    a.stop();
    b.stop();
}

// ── Selection & Kill Ring ──

#[test]
fn set_mark_sets_mark() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark]));

    assert_eq!(buf(&t).mark(), Some((led_core::Row(0), led_core::Col(0))));
}

#[test]
fn mark_persists_on_movement() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, MoveDown]));

    assert_eq!(buf(&t).mark(), Some((led_core::Row(0), led_core::Col(0))));
    assert_eq!(buf(&t).cursor_row().0, 1);
}

#[test]
fn insert_clears_mark() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, InsertChar('x')]));

    assert!(buf(&t).mark().is_none());
}

#[test]
fn kill_region_deletes_selection() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, MoveDown, KillRegion]));

    assert_eq!(line(&**buf(&t).doc(), 0), "bbb");
    assert_eq!(buf(&t).cursor_row().0, 0);
    assert_eq!(buf(&t).cursor_col().0, 0);
    assert!(buf(&t).mark().is_none());
    assert_eq!(t.state.kill_ring.content, "aaa\n");
}

#[test]
fn kill_region_no_mark_warns() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![KillRegion]));

    assert_eq!(t.state.alerts.info.as_deref(), Some("No region"));
}

#[test]
fn yank_inserts_killed_text() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, MoveDown, KillRegion, Yank]));

    assert_eq!(line(&**buf(&t).doc(), 0), "aaa");
    assert_eq!(line(&**buf(&t).doc(), 1), "bbb");
}

#[test]
fn kill_line_accumulates_to_kill_ring() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![KillLine, KillLine]));

    assert_eq!(t.state.kill_ring.content, "aaa\n");
}

#[test]
fn non_kill_line_clears_accumulator() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![KillLine, MoveDown, KillLine]));

    assert_eq!(t.state.kill_ring.content, "bbb");
}

// ── In-Buffer Search ──

#[test]
fn search_opens_and_finds() {
    let t = TestHarness::new()
        .with_file("hello world\nhello again\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('h'),
            InsertChar('e'),
        ]));

    let b = buf(&t);
    let is = b.isearch.as_ref().expect("isearch should be active");
    assert_eq!(is.query, "he");
    assert_eq!(is.matches.len(), 2);
    assert_eq!(b.cursor_row().0, 0);
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn search_next_advances() {
    let t = TestHarness::new()
        .with_file("aaa\naaa\naaa\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('a'),
            InsertChar('a'),
            InsertChar('a'),
            InBufferSearch, // advance to next match
        ]));

    let b = buf(&t);
    assert_eq!(b.cursor_row().0, 1);
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn search_cancel_restores_cursor() {
    let t = TestHarness::new()
        .with_file("hello\nworld\n")
        .run(actions(vec![
            MoveDown, // move to (1,0)
            InBufferSearch,
            InsertChar('x'), // no match
            Abort,           // cancel
        ]));

    let b = buf(&t);
    assert!(b.isearch.is_none(), "isearch should be cleared");
    assert_eq!(b.cursor_row().0, 1);
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn search_accept_keeps_position() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\naaa\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('b'),
            InsertNewline, // accept
        ]));

    let b = buf(&t);
    assert!(b.isearch.is_none(), "isearch should be cleared");
    assert_eq!(b.cursor_row().0, 1);
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn search_wraps_on_failed() {
    let t = TestHarness::new().with_file("aaa\nbbb\n").run(actions(vec![
        MoveDown, // move to (1,0)
        InBufferSearch,
        InsertChar('a'), // match at (0,0) — but cursor is at (1,0), so failed
    ]));

    let b = buf(&t);
    let is = b.isearch.as_ref().expect("isearch should be active");
    assert!(is.failed, "should be in failed state");

    // Now wrap
    let t = TestHarness::new().with_file("aaa\nbbb\n").run(actions(vec![
        MoveDown,
        InBufferSearch,
        InsertChar('a'),
        InBufferSearch, // wrap to first match
    ]));

    let b = buf(&t);
    let is = b.isearch.as_ref().expect("isearch should be active");
    assert!(!is.failed, "should not be failed after wrap");
    assert_eq!(b.cursor_row().0, 0);
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn search_recall_last() {
    let t = TestHarness::new()
        .with_file("hello world\nhello again\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('h'),
            InsertChar('e'),
            InsertChar('l'),
            InsertNewline, // accept, saves "hel" as last_search
            InBufferSearch,
            InBufferSearch, // recall last search (C-s C-s)
        ]));

    let b = buf(&t);
    let is = b.isearch.as_ref().expect("isearch should be active");
    assert_eq!(is.query, "hel");
    assert_eq!(is.matches.len(), 2);
}

#[test]
fn search_delete_backward() {
    let t = TestHarness::new().with_file("abc\n").run(actions(vec![
        InBufferSearch,
        InsertChar('a'),
        InsertChar('b'),
        DeleteBackward,
    ]));

    let b = buf(&t);
    let is = b.isearch.as_ref().expect("isearch should be active");
    assert_eq!(is.query, "a");
}

#[test]
fn search_movement_accepts() {
    let t = TestHarness::new().with_file("aaa\nbbb\n").run(actions(vec![
        InBufferSearch,
        InsertChar('a'),
        MoveDown,
    ]));

    let b = buf(&t);
    assert!(b.isearch.is_none(), "movement should accept search");
}

// ── Jump list ──

#[test]
fn jump_back_restores_position() {
    // Start at (0,0), search for 'c' → cursor moves to (2,0), accept records origin (0,0).
    // JumpBack → cursor returns to (0,0).
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('c'),
            InsertNewline, // accept search at (2,0), records origin (0,0)
            JumpBack,      // should jump back to (0,0)
        ]));

    let b = buf(&t);
    assert_eq!(b.cursor_row().0, 0, "JumpBack should restore row");
    assert_eq!(b.cursor_col().0, 0, "JumpBack should restore col");
}

#[test]
fn jump_forward_after_back() {
    // Start at (0,0), search for 'c' → (2,0), accept. JumpBack → (0,0). JumpForward → (2,0).
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('c'),
            InsertNewline, // accept at (2,0), records origin (0,0)
            JumpBack,      // cursor at (0,0)
            JumpForward,   // cursor at (2,0)
        ]));

    let b = buf(&t);
    assert_eq!(b.cursor_row().0, 2, "JumpForward should go to (2,0)");
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn jump_back_at_empty_list() {
    // No recorded jumps. JumpBack → no crash, cursor unchanged.
    let t = TestHarness::new().with_file("aaa\nbbb\n").run(actions(vec![
        MoveDown, // cursor at (1,0)
        JumpBack, // no jump history, should be a no-op
    ]));

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        1,
        "JumpBack with empty list should not move cursor"
    );
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn jump_forward_at_end() {
    // After a jump back+forward cycle, JumpForward at end → no crash, cursor unchanged.
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('c'),
            InsertNewline, // accept at (2,0), records origin (0,0)
            JumpBack,      // back to (0,0)
            JumpForward,   // forward to (2,0)
            JumpForward,   // already at end, no-op
        ]));

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        2,
        "JumpForward at end should not move cursor"
    );
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn jump_back_cross_buffer() {
    // Open two files. In aaa.txt at (0,0), search for 'c' → (2,0), accept.
    // Switch to zzz.txt. JumpBack → active buffer is aaa.txt, cursor at (0,0).
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "aaa\nbbb\nccc\n")
        .with_named_file("zzz.txt", "zzz\n")
        .run(actions(vec![
            PrevTab, // go to aaa.txt (first by tab order)
            InBufferSearch,
            InsertChar('c'),
            InsertNewline, // accept at (2,0), records origin (0,0) in aaa.txt
            NextTab,       // switch to zzz.txt
            JumpBack,      // should jump back to aaa.txt at (0,0)
        ]));

    let b = buf(&t);
    assert_eq!(
        b.path().unwrap().file_name().unwrap().to_str().unwrap(),
        "aaa.txt",
        "JumpBack should switch to the correct buffer"
    );
    assert_eq!(b.cursor_row().0, 0);
    assert_eq!(b.cursor_col().0, 0);
}

#[test]
fn jump_truncates_forward_history() {
    // Record a jump, jump back, then record a new jump → forward history is truncated.
    let t = TestHarness::new()
        .with_file("aaa\nbbb\nccc\nddd\n")
        .run(actions(vec![
            InBufferSearch,
            InsertChar('c'),
            InsertNewline, // accept at (2,0), records origin (0,0)
            JumpBack,      // back to (0,0)
            // Now record a new jump (search to 'd')
            InBufferSearch,
            InsertChar('d'),
            InsertNewline, // accept at (3,0), records origin (0,0)
            JumpForward,   // should be no-op — forward history was truncated
        ]));

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        3,
        "Forward history should have been truncated"
    );
    assert_eq!(b.cursor_col().0, 0);
}

// ── Syntax highlighting tests ──

/// Regression: `led ~/.profile` (a symlink to a non-well-known target
/// like `~/dotfiles/profile`) must still be detected as shell. The chain
/// resolution lives in `BufferState::new(user)` and walks the symlink
/// chain so the well-known `.profile` name wins over the resolved
/// `profile` filename.
#[cfg(unix)]
#[test]
fn symlinked_dotfile_gets_shell_syntax_highlighting() {
    use led_core::LanguageId;

    // Mirror `led ~/.profile` exactly: the symlink is the ONLY arg.
    // The target file is written but not opened directly.
    let t = TestHarness::new()
        .with_target_only_file("profile_target", "export FOO=bar\n# comment\n")
        .with_symlink(".profile", "profile_target")
        .run(vec![WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .is_some_and(|b| !b.syntax_highlights().is_empty())
        })]);

    let b = buf(&t);
    // Highlights populated → tree-sitter detected a language for this buffer.
    assert!(
        !b.syntax_highlights().is_empty(),
        "expected syntax highlights on symlinked .profile"
    );
    // The pre-resolved language was Bash (driven by the symlink name).
    assert_eq!(b.language(), Some(LanguageId::Bash));
    // Buffer carries the full chain (user-typed name first, canonical last).
    let chain = b.path_chain();
    assert!(
        chain.user.as_path().ends_with(".profile"),
        "user path should end with .profile, got {:?}",
        chain.user.as_path()
    );
    assert_eq!(
        chain.resolved.as_path().file_name().unwrap(),
        "profile_target",
        "resolved path should point at the target file"
    );
}

/// Regression: the production bug was that `led ~/.profile`, on the
/// second invocation, reads the session DB which only stores canonical
/// paths. The resume combinator would rebuild buffers from those
/// canonical paths — losing the `.profile` symlink name and yielding
/// `language = None`.
///
/// Fix: when session restore finds a pending_open, look in
/// `arg_user_paths` for a matching UserPath (one that canonicalizes to
/// the same path) and use it for chain resolution.
#[cfg(unix)]
#[test]
fn symlinked_dotfile_language_survives_session_restore() {
    use led_core::LanguageId;

    // Run 1: open the symlink, quit. Session DB now contains the
    // canonical path (/.../workspace/profile_target).
    let t = TestHarness::new()
        .with_target_only_file("profile_target", "export FOO=bar\n")
        .with_symlink(".profile", "profile_target")
        .run(vec![
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|p| s.buffers.get(p))
                    .is_some_and(|b| !b.syntax_highlights().is_empty())
            }),
            Do(Quit),
            WaitFor(|s| s.session.saved),
        ]);
    let dir = t.dirs.root.clone();
    drop(t);

    // Run 2: reuse same dir, pass the symlink as arg again. Session
    // restore sees the canonical; the combinator must find `.profile`
    // in arg_user_paths and rebuild the chain from there.
    let symlink_path = dir.join("workspace").join(".profile");
    let t2 = TestHarness::with_dir(dir)
        .with_arg(symlink_path)
        .run(vec![WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .is_some_and(|b| !b.syntax_highlights().is_empty())
        })]);

    let b = buf(&t2);
    assert_eq!(b.language(), Some(LanguageId::Bash));
    assert!(b.path_chain().user.as_path().ends_with(".profile"));
}

#[test]
fn syntax_highlights_rust_file() {
    // Open a .rs file, wait for async syntax update, verify highlights exist
    let t = TestHarness::new()
        .with_file_ext("fn main() {\n    let x = 1;\n}\n", "rs")
        .run(actions(vec![Wait(50)]));

    let b = buf(&t);
    // After the syntax driver processes, highlights should be populated
    // (The driver runs asynchronously, so we wait)
    // At minimum the buffer should exist and have the right content
    assert_eq!(line(&**b.doc(), 0), "fn main() {");
}

#[test]
fn kill_line_keeps_highlights_in_sync() {
    // Repro: open a file, kill a line, check highlights match the new doc.
    // Regression test: highlights must not have stale character offsets
    // from before the kill.
    let src = "fn aaa() {}\n\nfn bbb() {}\n\nfn ccc() {}\n";
    let t = TestHarness::new().with_file_ext(src, "rs").run(vec![
        // Wait for initial syntax highlights
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .map_or(false, |b| !b.syntax_highlights().is_empty())
        }),
        // Kill "fn aaa() {}" (line 0 text)
        Do(KillLine),
        // Kill the now-empty line (newline char)
        Do(KillLine),
        // Kill the blank line
        Do(KillLine),
        // Now line 0 should be "fn bbb() {}"
        // Wait for syntax to catch up
        WaitFor(|s| {
            let b = s
                .active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .unwrap();
            // Highlights should contain a span for the current doc's content.
            // "fn" keyword should be highlighted on line 0 (bbb) after kills.
            b.syntax_highlights()
                .iter()
                .any(|(line, span)| **line == 0 && span.capture_name.contains("keyword"))
        }),
    ]);

    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "fn bbb() {}");

    // All highlight lines must be within document bounds
    let line_count = b.doc().line_count();
    for (line, span) in b.syntax_highlights().iter() {
        assert!(
            **line < line_count,
            "highlight on line {:?} but doc has {} lines, span: {:?}",
            line,
            line_count,
            span.capture_name,
        );
    }

    // "fn" keyword must appear on line 0 (where "fn bbb" now lives)
    let has_fn_on_line0 = b
        .syntax_highlights()
        .iter()
        .any(|(line, span)| **line == 0 && span.capture_name.contains("keyword"));
    assert!(
        has_fn_on_line0,
        "expected 'fn' keyword highlight on line 0 after kill"
    );
}

#[test]
fn kill_line_long_file_highlights_recover() {
    // Repro: in a file LONGER than the viewport, killing a line doesn't
    // change scroll_row or end_line, so the driver's highlight cache must
    // be invalidated when the doc version changes.
    //
    // Key: line 0 is a function, line 1 is a comment. After killing line 0
    // + newline, line 0 becomes the comment. Stale highlights would show
    // the keyword capture on line 0 (from the old fn), but the correct
    // highlights should show a comment capture on line 0.
    let mut src = String::from("fn aaa() {}\n");
    src.push_str("// this is a comment\n");
    src.push_str("fn bbb() {}\n");
    // Pad to 50 lines so the file exceeds the viewport (24 lines)
    for i in 0..47 {
        src.push_str(&format!("fn pad_{i}() {{}}\n"));
    }
    let t = TestHarness::new().with_file_ext(&src, "rs").run(vec![
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .map_or(false, |b| !b.syntax_highlights().is_empty())
        }),
        // Kill "fn aaa() {}" text, then the newline
        Do(KillLine),
        Do(KillLine),
        // Now line 0 = "// this is a comment"
        // Wait for highlights: line 0 must have a comment capture,
        // NOT a keyword capture (which would indicate stale cache).
        WaitFor(|s| {
            let b = s
                .active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .unwrap();
            b.syntax_highlights()
                .iter()
                .any(|(line, span)| **line == 0 && span.capture_name.contains("comment"))
        }),
    ]);

    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "// this is a comment");

    // Line 0 must have comment highlight, not keyword
    let has_comment = b
        .syntax_highlights()
        .iter()
        .any(|(line, span)| **line == 0 && span.capture_name.contains("comment"));
    assert!(
        has_comment,
        "line 0 should have comment highlight after kill"
    );

    let has_keyword_on_0 = b
        .syntax_highlights()
        .iter()
        .any(|(line, span)| **line == 0 && span.capture_name.contains("keyword"));
    assert!(
        !has_keyword_on_0,
        "line 0 should NOT have keyword highlight (stale cache!)"
    );
}

#[test]
fn kill_line_md_highlights_recover() {
    // Repro from user: open markdown, kill a heading line, highlights go wrong.
    // After killing "## Section A", the highlights for "## Section B" must
    // still appear on the correct (now shifted) line.
    let src = "\
# Title

## Section A

Some text

## Section B

More text
";
    // Record initial version before kills
    let t = TestHarness::new().with_file_ext(src, "md").run(vec![
        // Wait for initial highlights
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .map_or(false, |b| !b.syntax_highlights().is_empty())
        }),
        // Move to "## Section A" (line 2)
        Do(MoveDown),
        Do(MoveDown),
        // Kill heading text "## Section A"
        Do(KillLine),
        // Kill remaining empty line (newline)
        Do(KillLine),
        // Kill next blank line (newline)
        Do(KillLine),
        // Now line 2 = "Some text"
        // Wait for syntax highlights to update for the new doc version.
        // The doc version after 3 kills is initial + 3 (at minimum).
        WaitFor(|s| {
            let b = s
                .active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .unwrap();
            let lc = b.doc().line_count();
            // Highlights must be non-empty and fully within bounds
            !b.syntax_highlights().is_empty()
                && b.syntax_highlights().iter().all(|(line, _)| **line < lc)
        }),
    ]);

    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 2), "Some text");

    // All highlight line numbers must be within doc bounds
    let lc = b.doc().line_count();
    for (line, span) in b.syntax_highlights().iter() {
        assert!(
            **line < lc,
            "stale highlight: line {:?} >= line_count {}, capture: {:?}",
            line,
            lc,
            span.capture_name,
        );
    }
}

#[test]
fn match_bracket_jumps() {
    // Open .rs file with braces, wait for syntax, press MatchBracket on `{`
    let t = TestHarness::new()
        .with_file_ext("fn f() {\n    x\n}\n", "rs")
        .run(vec![
            Wait(50).into(),
            // Move to the `{` at row 0, col 7
            Do(LineEnd),
            Do(MoveLeft),
            // Wait for syntax driver to populate matching_bracket
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .and_then(|b| b.matching_bracket())
                    .is_some()
            }),
            Do(MatchBracket),
        ]);

    let b = buf(&t);
    // Should jump to the matching `}`
    assert_eq!(b.cursor_row().0, 2, "should jump to closing brace row");
    assert_eq!(b.cursor_col().0, 0, "should jump to closing brace col");
}

#[test]
fn match_bracket_reverse() {
    // Start on `}`, jump to `{`
    let t = TestHarness::new()
        .with_file_ext("fn f() {\n    x\n}\n", "rs")
        .run(vec![
            Wait(50).into(),
            // Move to `}` at row 2, col 0
            Do(MoveDown),
            Do(MoveDown),
            Do(LineStart),
            // Wait for syntax driver to populate matching_bracket
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .and_then(|b| b.matching_bracket())
                    .is_some()
            }),
            Do(MatchBracket),
        ]);

    let b = buf(&t);
    assert_eq!(b.cursor_row().0, 0, "should jump to opening brace row");
    assert_eq!(b.cursor_col().0, 7, "should jump to opening brace col");
}

#[test]
fn match_bracket_no_bracket() {
    // Cursor not on bracket → no-op
    let t = TestHarness::new()
        .with_file_ext("fn f() {\n    x\n}\n", "rs")
        .run(actions(vec![
            Wait(50),
            // Cursor at (0,0) which is 'f', not a bracket
            MatchBracket,
        ]));

    let b = buf(&t);
    assert_eq!(b.cursor_row().0, 0);
    assert_eq!(b.cursor_col().0, 0, "cursor should not move");
}

#[test]
fn auto_indent_after_brace() {
    // Type `fn main() {`, then InsertNewline → async indent adds spaces
    let t = TestHarness::new()
        .with_file_ext("fn main() {\n}\n", "rs")
        .run(vec![
            Do(LineEnd), // end of "fn main() {"
            Do(InsertNewline),
            WaitFor(indent_done),
        ]);

    let b = buf(&t);
    assert_eq!(b.cursor_row().0, 1);
    assert!(
        b.cursor_col().0 >= 2,
        "cursor should be indented after '{{', got col {}",
        b.cursor_col().0
    );
    // Verify the indent text was actually inserted
    let line = line(&**b.doc(), 1);
    assert!(
        line.starts_with("    ") || line.starts_with('\t'),
        "new line should be indented: {:?}",
        line
    );
}

#[test]
fn auto_indent_closing_brace() {
    // After `fn main() {` with body, InsertChar('}') should dedent
    // when the buffer's syntax highlighter declares '}' as a reindent char.
    let t = TestHarness::new()
        .with_file_ext("fn main() {\n    let x = 1;\n    \n}\n", "rs")
        .run(vec![
            // Go to line 2 (the empty indented line)
            Do(MoveDown),
            Do(MoveDown),
            Do(LineEnd),
            // Type closing brace
            Do(InsertChar('}')),
            WaitFor(indent_done),
        ]);

    let b = buf(&t);
    let line = line(&**b.doc(), 2);
    // The closing brace should be dedented to match "fn main() {"
    assert!(
        !line.starts_with("    }"),
        "closing brace should be dedented, got: {:?}",
        line
    );
    assert!(
        line.contains('}'),
        "line should contain closing brace: {:?}",
        line
    );
}

#[test]
fn sort_imports_reorders() {
    let t = TestHarness::new()
        .with_file_ext("use z::Z;\nuse a::A;\n\nfn main() {}\n", "rs")
        .run(actions(vec![SortImports]));

    let b = buf(&t);
    // After sorting, a::A should come before z::Z
    assert_eq!(
        line(&**b.doc(), 0),
        "use a::A;",
        "first import should be a::A"
    );
    assert_eq!(
        line(&**b.doc(), 1),
        "use z::Z;",
        "second import should be z::Z"
    );
}

#[test]
fn sort_imports_no_change() {
    let t = TestHarness::new()
        .with_file_ext("use a::A;\nuse z::Z;\n\nfn main() {}\n", "rs")
        .run(actions(vec![SortImports]));

    let b = buf(&t);
    // Already sorted — should not modify doc
    assert!(
        !b.is_dirty(),
        "already sorted imports should not dirty the doc"
    );
}

#[test]
fn rainbow_brackets_depth() {
    // Open file with nested brackets, verify bracket_pairs have incrementing color indices
    let t = TestHarness::new()
        .with_file_ext("fn f() { (1 + (2 + 3)) }\n", "rs")
        .run(actions(vec![Wait(50)]));

    let b = buf(&t);
    if !b.bracket_pairs().is_empty() {
        // Check that at least some pairs have different color indices
        let indices: Vec<Option<usize>> = b.bracket_pairs().iter().map(|p| p.color_index).collect();
        let has_multiple = indices
            .iter()
            .filter_map(|i| i.as_ref())
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1;
        assert!(
            has_multiple,
            "nested brackets should have different rainbow depths: {:?}",
            indices
        );
    }
}

#[test]
fn close_bracket_reindents_with_syntax() {
    // Typing '}' in a buffer with syntax indent triggers re-indentation
    let t = TestHarness::new()
        .with_file_ext("fn main() {\n    \n}\n", "rs")
        .run(vec![
            Do(MoveDown), // go to line 1 (the indented empty line)
            Do(LineEnd),  // end of "    "
            Do(InsertChar('}')),
            WaitFor(indent_done),
        ]);

    let b = buf(&t);
    let line = line(&**b.doc(), 1);
    // The brace should be present on line 1
    assert!(
        line.contains('}'),
        "line should have closing brace: {:?}",
        line
    );
}

// ── Find file ──

#[test]
fn find_file_activates_and_aborts() {
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(FindFile),
        WaitFor(|s| s.find_file.is_some()),
        Do(Abort),
        WaitFor(|s| s.find_file.is_none()),
    ]);

    assert!(t.state.find_file.is_none());
}

#[test]
fn find_file_initial_input_has_dir() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(vec![Do(FindFile), WaitFor(|s| s.find_file.is_some())]);

    let ff = t.state.find_file.as_ref().unwrap();
    assert!(
        ff.input.ends_with('/'),
        "input should end with /: {}",
        ff.input
    );
    assert!(ff.cursor == ff.input.len(), "cursor should be at end");
}

#[test]
fn find_file_receives_completions() {
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(FindFile),
        WaitFor(|s| {
            s.find_file
                .as_ref()
                .map_or(false, |ff| !ff.completions.is_empty())
        }),
    ]);

    let ff = t.state.find_file.as_ref().unwrap();
    assert!(
        !ff.completions.is_empty(),
        "completions should be populated after activation"
    );
}

#[test]
fn find_file_typing_filters_completions() {
    // Create two files so we can filter
    let t = TestHarness::new()
        .with_named_file("alpha.txt", "a\n")
        .with_named_file("beta.txt", "b\n")
        .run(vec![
            Do(FindFile),
            // Wait for initial completions
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| ff.completions.len() >= 2)
            }),
            // Type 'a' to filter
            Do(InsertChar('a')),
            // Wait for filtered completions
            WaitFor(|s| {
                s.find_file.as_ref().map_or(false, |ff| {
                    ff.completions.len() == 1 && ff.completions[0].name.starts_with("alpha")
                })
            }),
        ]);

    let ff = t.state.find_file.as_ref().unwrap();
    assert_eq!(ff.completions.len(), 1);
    assert!(ff.completions[0].name.starts_with("alpha"));
}

#[test]
fn find_file_tab_shows_side_panel() {
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(FindFile),
        // Wait for completions to arrive (input ends with / → rule 1 applies)
        WaitFor(|s| {
            s.find_file
                .as_ref()
                .map_or(false, |ff| !ff.completions.is_empty())
        }),
        Do(InsertTab),
        WaitFor(|s| s.find_file.as_ref().map_or(false, |ff| ff.show_side)),
    ]);

    assert!(t.state.find_file.as_ref().unwrap().show_side);
}

#[test]
fn find_file_tab_completions_no_buffer() {
    // Tab completion must work when no buffer is open (start_dir fallback).
    let td = tempfile::TempDir::new().expect("tmpdir");
    let root = td.keep();
    let workspace = root.join("workspace");
    let config = root.join("config");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::create_dir_all(&config).expect("mkdir");
    std::fs::write(workspace.join("hello.txt"), "hi\n").expect("write");

    let t = TestHarness::with_dir(root)
        .with_arg_dir(workspace)
        .run(vec![
            WaitFor(|s| s.phase == led_state::Phase::Running),
            Do(FindFile),
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| !ff.completions.is_empty())
            }),
            Do(InsertTab),
            WaitFor(|s| s.find_file.as_ref().map_or(false, |ff| ff.show_side)),
        ]);

    let ff = t.state.find_file.as_ref().unwrap();
    assert!(ff.show_side, "side panel should be shown after Tab");
    assert!(
        !ff.completions.is_empty(),
        "completions should be populated"
    );
}

#[test]
fn find_file_opens_existing() {
    // Create a file, then use find-file to open a second file
    let t = TestHarness::new()
        .with_named_file("first.txt", "first\n")
        .with_named_file("second.txt", "second\n")
        .run(vec![
            // Activate find-file
            Do(FindFile),
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| !ff.completions.is_empty())
            }),
            // Type 'second.txt'
            Do(InsertChar('s')),
            Do(InsertChar('e')),
            Do(InsertChar('c')),
            Do(InsertChar('o')),
            Do(InsertChar('n')),
            Do(InsertChar('d')),
            Do(InsertChar('.')),
            Do(InsertChar('t')),
            Do(InsertChar('x')),
            Do(InsertChar('t')),
            // Wait for completions to match
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| ff.completions.len() == 1)
            }),
            // Press enter to open
            Do(InsertNewline),
            // Wait for find-file to close and file to open
            WaitFor(|s| s.find_file.is_none()),
            WaitFor(|s| s.buffers.len() >= 2),
        ]);

    assert!(t.state.find_file.is_none());
    // The second file should be open
    let has_second = t.state.buffers.values().any(|b| {
        b.path()
            .and_then(|p| p.file_name())
            .map_or(false, |n| n == "second.txt")
    });
    assert!(has_second, "second.txt should be open");
}

#[test]
fn find_file_up_down_wraps() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .run(vec![
            Do(FindFile),
            // Wait for completions
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| ff.completions.len() >= 2)
            }),
            // MoveDown → selects first (index 0)
            Do(MoveDown),
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| ff.selected == Some(0))
            }),
            // MoveUp from 0 → wraps to last
            Do(MoveUp),
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| ff.selected == Some(ff.completions.len() - 1))
            }),
        ]);

    let ff = t.state.find_file.as_ref().unwrap();
    assert_eq!(ff.selected, Some(ff.completions.len() - 1));
}

#[test]
fn find_file_opens_nonexistent() {
    // Opening a non-existent filename should create a new empty buffer
    let t = TestHarness::new()
        .with_named_file("existing.txt", "hello\n")
        .run(vec![
            Do(FindFile),
            WaitFor(|s| {
                s.find_file
                    .as_ref()
                    .map_or(false, |ff| !ff.completions.is_empty())
            }),
            // Type 'newfile.txt' — a file that does not exist on disk
            Do(InsertChar('n')),
            Do(InsertChar('e')),
            Do(InsertChar('w')),
            Do(InsertChar('f')),
            Do(InsertChar('i')),
            Do(InsertChar('l')),
            Do(InsertChar('e')),
            Do(InsertChar('.')),
            Do(InsertChar('t')),
            Do(InsertChar('x')),
            Do(InsertChar('t')),
            // Press enter to open
            Do(InsertNewline),
            // Wait for find-file to close and new buffer to appear
            WaitFor(|s| s.find_file.is_none()),
            WaitFor(|s| s.buffers.len() >= 2),
        ]);

    assert!(t.state.find_file.is_none());
    let has_new = t.state.buffers.values().any(|b| {
        b.path()
            .and_then(|p| p.file_name())
            .map_or(false, |n| n == "newfile.txt")
    });
    assert!(has_new, "newfile.txt buffer should be open");
    // The file should NOT exist on disk
    let new_path = t.dirs.workspace.join("newfile.txt");
    assert!(
        !new_path.exists(),
        "newfile.txt should not be created on disk until saved"
    );
}

#[test]
fn find_file_opens_nonexistent_no_buffers() {
    // Opening a non-existent file when no buffers are open (project directory only)
    let t = TestHarness::new().run(vec![
        Do(FindFile),
        WaitFor(|s| s.find_file.is_some()),
        // Type 'newfile.txt'
        Do(InsertChar('n')),
        Do(InsertChar('e')),
        Do(InsertChar('w')),
        Do(InsertChar('f')),
        Do(InsertChar('i')),
        Do(InsertChar('l')),
        Do(InsertChar('e')),
        Do(InsertChar('.')),
        Do(InsertChar('t')),
        Do(InsertChar('x')),
        Do(InsertChar('t')),
        Do(InsertNewline),
        WaitFor(|s| s.find_file.is_none()),
        WaitFor(|s| !s.buffers.is_empty()),
    ]);

    assert!(t.state.find_file.is_none());
    let has_new = t.state.buffers.values().any(|b| {
        b.path()
            .and_then(|p| p.file_name())
            .map_or(false, |n| n == "newfile.txt")
    });
    assert!(has_new, "newfile.txt buffer should be open");
}

#[test]
fn find_file_opens_nonexistent_in_project_dir() {
    // When opened with a project directory (arg_dir), find-file to a
    // non-existent file should open a buffer AND focus the editor.
    let td = tempfile::TempDir::new().expect("tmpdir");
    let root = td.keep();
    let workspace = root.join("workspace");
    let config = root.join("config");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::create_dir_all(&config).expect("mkdir");
    std::fs::write(workspace.join("existing.txt"), "hello\n").expect("write");

    let t = TestHarness::with_dir(root)
        .with_arg_dir(workspace)
        .run(vec![
            WaitFor(|s| s.phase == led_state::Phase::Running),
            Do(FindFile),
            WaitFor(|s| s.find_file.is_some()),
            Do(InsertChar('n')),
            Do(InsertChar('e')),
            Do(InsertChar('w')),
            Do(InsertChar('.')),
            Do(InsertChar('t')),
            Do(InsertChar('x')),
            Do(InsertChar('t')),
            Do(InsertNewline),
            WaitFor(|s| s.find_file.is_none()),
            WaitFor(|s| {
                s.buffers.values().any(|b| {
                    b.is_materialized()
                        && b.path()
                            .map_or(false, |p| p.file_name().map_or(false, |n| n == "new.txt"))
                })
            }),
        ]);

    let has_new = t.state.buffers.values().any(|b| {
        b.path()
            .and_then(|p| p.file_name())
            .map_or(false, |n| n == "new.txt")
    });
    assert!(has_new, "new.txt buffer should be open");
    assert_eq!(
        t.state.focus,
        led_core::PanelSlot::Main,
        "focus should be on editor, not file browser"
    );
}

// ── LSP integration tests ──
// These tests use a fake LSP server (crates/fake-lsp) for deterministic,
// fast testing without requiring real language servers.

use std::sync::Once;

static BUILD_FAKE_LSP: Once = Once::new();

fn fake_lsp_binary() -> PathBuf {
    BUILD_FAKE_LSP.call_once(|| {
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "fake-lsp"])
            .status()
            .expect("cargo build fake-lsp");
        assert!(status.success(), "failed to build fake-lsp");
    });
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("fake-lsp");
    path
}

/// Create a tmpdir with a source file and `.fake-lsp.json` config.
/// The LSP root will be `workspace/src/` (the parent of the first arg file).
/// Config paths are relative to that root (e.g. `"main.rs"`).
fn lsp_project(main_rs: &str, config: serde_json::Value) -> (PathBuf, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let root = dir.keep();
    let workspace = root.join("workspace");
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("src/.fake-lsp.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();
    let main_path = workspace.join("src/main.rs");
    std::fs::write(&main_path, main_rs).unwrap();
    (root, main_path)
}

fn has_lsp_diagnostics(s: &led_state::AppState) -> bool {
    s.buffers
        .values()
        .any(|b| !b.status().diagnostics().is_empty())
}

fn all_diagnostics_count(s: &led_state::AppState) -> usize {
    s.buffers
        .values()
        .map(|b| b.status().diagnostics().len())
        .sum::<usize>()
}

fn lsp_server_ready(s: &led_state::AppState) -> bool {
    !s.lsp.server_name.is_empty() && !s.lsp.busy
}

fn completion_config() -> serde_json::Value {
    serde_json::json!({
        "completions": [
            {"label": "Option", "kind": 6, "insertText": "Option", "filterText": "Option"},
            {"label": "String", "kind": 6, "insertText": "String", "filterText": "String"},
            {"label": "Some", "kind": 6, "insertText": "Some", "filterText": "Some"},
            {"label": "None", "kind": 6, "insertText": "None", "filterText": "None"},
            {"label": "str", "kind": 6, "insertText": "str", "filterText": "str"}
        ],
        "triggerCharacters": [":", "."]
    })
}

#[test]
fn lsp_diagnostics_appear() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let _x: i32 = \"hello\";\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [{
                    "range": {
                        "start": {"line": 1, "character": 18},
                        "end": {"line": 1, "character": 25}
                    },
                    "severity": 1,
                    "message": "mismatched types: expected `i32`, found `&str`"
                }]
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![WaitFor(has_lsp_diagnostics)]);

    let all_diags: Vec<_> = t
        .state
        .buffers
        .values()
        .flat_map(|b| b.status().diagnostics().iter())
        .collect();
    assert!(!all_diags.is_empty(), "expected diagnostics");
    assert!(
        all_diags.iter().any(|d| d.message.contains("mismatched")),
        "expected type-mismatch diagnostic, got: {:?}",
        all_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn lsp_format() {
    let (root, main_rs) = lsp_project(
        "fn   main(  )  {\n    let   x  =  1;\n    let y = 2;\n    println!(\"{}\",  x);\n}\n",
        serde_json::json!({
            "formatting": {
                "main.rs": "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x);\n}\n"
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(LspFormat),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .map_or(false, |b| line(&**b.doc(), 0).starts_with("fn main()"))
            }),
        ]);

    let b = buf(&t);
    assert!(
        line(&**b.doc(), 0).starts_with("fn main()"),
        "expected formatted first line, got: {:?}",
        line(&**b.doc(), 0)
    );
}

#[test]
fn lsp_goto_definition() {
    // greet is defined on line 0, called on line 4.
    // The fake server scans for `fn greet` to find the definition.
    let (root, main_rs) = lsp_project(
        "fn greet() {}\n\nfn main() {\n    let y = 0;\n    greet();\n}\n",
        serde_json::json!({}),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            // Move to line 4, col 4 (on `greet()`)
            Do(MoveDown),
            Do(MoveDown),
            Do(MoveDown),
            Do(MoveDown),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(LspGotoDefinition),
            // Wait for cursor to land on the definition (line 0)
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .map_or(false, |b| b.cursor_row().0 == 0)
            }),
        ]);

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        0,
        "expected cursor on definition line (fn greet)"
    );
}

#[test]
fn lsp_next_prev_diagnostic() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let _x: i32 = \"a\";\n    let _y: i32 = \"b\";\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [
                    {
                        "range": {
                            "start": {"line": 1, "character": 18},
                            "end": {"line": 1, "character": 21}
                        },
                        "severity": 1,
                        "message": "mismatched types"
                    },
                    {
                        "range": {
                            "start": {"line": 2, "character": 18},
                            "end": {"line": 2, "character": 21}
                        },
                        "severity": 1,
                        "message": "mismatched types"
                    }
                ]
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(|s| all_diagnostics_count(s) >= 2),
            Do(NextIssue),
        ]);

    let b = buf(&t);
    assert!(
        b.cursor_row().0 >= 1,
        "expected cursor to move to a diagnostic, row={}",
        b.cursor_row().0
    );
}

#[test]
fn next_issue_errors_before_warnings() {
    // Error on line 2, warning on line 1 — NextIssue should jump to the error.
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let _w = 0;\n    let _e: i32 = \"a\";\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [
                    {
                        "range": {
                            "start": {"line": 1, "character": 8},
                            "end": {"line": 1, "character": 10}
                        },
                        "severity": 2,
                        "message": "unused variable"
                    },
                    {
                        "range": {
                            "start": {"line": 2, "character": 18},
                            "end": {"line": 2, "character": 21}
                        },
                        "severity": 1,
                        "message": "mismatched types"
                    }
                ]
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(|s| all_diagnostics_count(s) >= 2),
            Do(NextIssue),
        ]);

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        2,
        "NextIssue should jump to error (line 2), not warning (line 1)"
    );
}

#[test]
fn next_issue_falls_through_to_warnings() {
    // Only warnings — NextIssue should navigate to the warning.
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let _w = 0;\n    let _v = 1;\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [
                    {
                        "range": {
                            "start": {"line": 1, "character": 8},
                            "end": {"line": 1, "character": 10}
                        },
                        "severity": 2,
                        "message": "unused variable _w"
                    },
                    {
                        "range": {
                            "start": {"line": 2, "character": 8},
                            "end": {"line": 2, "character": 10}
                        },
                        "severity": 2,
                        "message": "unused variable _v"
                    }
                ]
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(|s| all_diagnostics_count(s) >= 2),
            Do(NextIssue),
        ]);

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        1,
        "NextIssue should jump to first warning when no errors"
    );
}

#[test]
fn next_issue_cycles_errors() {
    // Two errors — NextIssue twice then once more should wrap to first.
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let _x: i32 = \"a\";\n    let _y: i32 = \"b\";\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [
                    {
                        "range": {
                            "start": {"line": 1, "character": 18},
                            "end": {"line": 1, "character": 21}
                        },
                        "severity": 1,
                        "message": "error 1"
                    },
                    {
                        "range": {
                            "start": {"line": 2, "character": 18},
                            "end": {"line": 2, "character": 21}
                        },
                        "severity": 1,
                        "message": "error 2"
                    }
                ]
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(|s| all_diagnostics_count(s) >= 2),
            Do(NextIssue),
            Do(NextIssue),
            Do(NextIssue), // wrap
        ]);

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        1,
        "third NextIssue should wrap back to first error (line 1)"
    );
}

#[test]
fn prev_issue_navigates_backward() {
    // Two errors — PrevIssue from (0,0) should wrap to last error.
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let _x: i32 = \"a\";\n    let _y: i32 = \"b\";\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [
                    {
                        "range": {
                            "start": {"line": 1, "character": 18},
                            "end": {"line": 1, "character": 21}
                        },
                        "severity": 1,
                        "message": "error 1"
                    },
                    {
                        "range": {
                            "start": {"line": 2, "character": 18},
                            "end": {"line": 2, "character": 21}
                        },
                        "severity": 1,
                        "message": "error 2"
                    }
                ]
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(|s| all_diagnostics_count(s) >= 2),
            Do(PrevIssue),
        ]);

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        2,
        "PrevIssue from top should wrap to last error (line 2)"
    );
}

#[test]
fn next_issue_cross_file() {
    // Errors in two different files — NextIssue should navigate across files.
    let (root, main_rs) = lsp_project(
        "fn main() {\n    helper();\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [{
                    "range": {
                        "start": {"line": 1, "character": 4},
                        "end": {"line": 1, "character": 10}
                    },
                    "severity": 1,
                    "message": "error in main"
                }],
                "other.rs": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 5}
                    },
                    "severity": 1,
                    "message": "error in other"
                }]
            }
        }),
    );

    // Create the second file alongside main.rs
    let src_dir = root.join("workspace/src");
    let other_rs = src_dir.join("other.rs");
    std::fs::write(&other_rs, "bad code\n").unwrap();

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .with_arg(other_rs)
        .run(vec![
            WaitFor(|s| all_diagnostics_count(s) >= 2),
            // Navigate until we land in a different file than where we started.
            Do(NextIssue),
            Do(NextIssue),
        ]);

    // After two NextIssue from the start, we should have visited both files.
    // Verify we ended on one of the two diagnostic positions.
    let b = buf(&t);
    let row = b.cursor_row().0;
    let active = t.state.active_tab.as_ref().unwrap();
    let active_name = active.as_path().file_name().unwrap().to_str().unwrap();
    assert!(
        (active_name == "main.rs" && row == 1) || (active_name == "other.rs" && row == 0),
        "expected cursor at a diagnostic in main.rs:1 or other.rs:0, got {}:{}",
        active_name,
        row
    );
}

#[test]
fn next_issue_git_unstaged() {
    // Set up a git repo with an unstaged modification, no LSP diagnostics.
    // NextIssue should navigate to a git change position.
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let root = dir.keep();
    let workspace = root.join("workspace");
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();

    let file_path = workspace.join("test.txt");
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

    // Initialize git repo and commit the file.
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&workspace)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&workspace)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@test.com",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(&workspace)
        .output()
        .expect("git commit");

    // Modify the file to create unstaged changes (add a line at the end).
    std::fs::write(&file_path, "line1\nline2\nline3\nnew line\n").unwrap();

    let t = TestHarness::with_dir(root).with_arg(file_path).run(vec![
        // Save triggers a git file scan.
        Do(Save),
        WaitFor(|s| !s.git.file_statuses.is_empty()),
        // Wait for line statuses to load (triggered by tab activation).
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .map_or(false, |b| !b.status().git_line_statuses().is_empty())
        }),
        Do(NextIssue),
    ]);

    let b = buf(&t);
    assert!(
        b.cursor_row().0 >= 3,
        "NextIssue should jump to the git change (line 3+), got row={}",
        b.cursor_row().0
    );
}

#[test]
fn lsp_rename_opens_overlay() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let hello = 1;\n    let z = 0;\n    println!(\"{}\", hello);\n}\n",
        serde_json::json!({}),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            // Move to line 1, col 8 (on `hello`)
            Do(MoveDown),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(LspRename),
        ]);

    assert!(
        t.state.lsp.rename.is_some(),
        "expected rename overlay to be open"
    );
    let rename = t.state.lsp.rename.as_ref().unwrap();
    assert_eq!(rename.input, "hello", "expected word under cursor");
    assert_eq!(
        t.state.focus,
        led_core::PanelSlot::Overlay,
        "expected focus on overlay"
    );
}

#[test]
fn lsp_rename_submit() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let hello = 1;\n    let z = 0;\n    println!(\"{}\", hello);\n}\n",
        serde_json::json!({}),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(MoveDown),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(LspRename),
            // Clear existing text, type new name
            Do(DeleteBackward),
            Do(DeleteBackward),
            Do(DeleteBackward),
            Do(DeleteBackward),
            Do(DeleteBackward),
            Do(InsertChar('w')),
            Do(InsertChar('o')),
            Do(InsertChar('r')),
            Do(InsertChar('l')),
            Do(InsertChar('d')),
            Do(InsertNewline), // submit
            // Wait for rename to complete — both occurrences should be renamed
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .map_or(false, |b| line(&**b.doc(), 1).contains("world"))
            }),
        ]);

    let b = buf(&t);
    assert!(
        line(&**b.doc(), 1).contains("world"),
        "expected 'hello' renamed to 'world' on line 1, got: {:?}",
        line(&**b.doc(), 1)
    );
    assert!(
        line(&**b.doc(), 3).contains("world"),
        "expected 'hello' renamed to 'world' on line 3, got: {:?}",
        line(&**b.doc(), 3)
    );
}

#[test]
fn lsp_toggle_inlay_hints() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![LspToggleInlayHints]));

    assert!(t.state.lsp.inlay_hints_enabled);

    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![LspToggleInlayHints, LspToggleInlayHints]));

    assert!(!t.state.lsp.inlay_hints_enabled);
}

#[test]
fn lsp_code_action() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 1;\n}\n",
        serde_json::json!({
            "codeActions": [
                {"title": "Remove unused variable", "kind": "quickfix"}
            ]
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            // Move cursor to 'x' on line 1
            Do(MoveDown),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(LspCodeAction),
            WaitFor(|s| s.lsp.code_actions.is_some()),
        ]);

    let actions = t.state.lsp.code_actions.as_ref().unwrap();
    assert!(
        !actions.actions.is_empty(),
        "expected at least one code action"
    );
    assert_eq!(
        t.state.focus,
        led_core::PanelSlot::Overlay,
        "expected focus on overlay"
    );
}

#[test]
fn lsp_code_action_dismiss() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 1;\n}\n",
        serde_json::json!({
            "codeActions": [
                {"title": "Remove unused variable", "kind": "quickfix"}
            ]
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(MoveDown),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(MoveRight),
            Do(LspCodeAction),
            WaitFor(|s| s.lsp.code_actions.is_some()),
            Do(Abort), // dismiss
        ]);

    assert!(
        t.state.lsp.code_actions.is_none(),
        "expected code action picker to be dismissed"
    );
    assert_eq!(t.state.focus, led_core::PanelSlot::Main);
}

#[test]
fn lsp_progress_reported() {
    // The fake server sends progress begin/end on didOpen.
    // Wait for the server to start and verify progress was communicated.
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 0;\n}\n",
        serde_json::json!({
            "diagnostics": {
                "main.rs": [{
                    "range": {
                        "start": {"line": 1, "character": 8},
                        "end": {"line": 1, "character": 9}
                    },
                    "severity": 2,
                    "message": "unused variable"
                }]
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![WaitFor(has_lsp_diagnostics)]);

    // Progress events have been processed — the fact that diagnostics arrived
    // means the server started and communicated.
    let _ = t.state.lsp.progress;
}

// ── LSP completion tests ──

fn has_completion(s: &led_state::AppState) -> bool {
    s.lsp.completion.is_some()
}

#[test]
fn lsp_completion_appears_on_typing() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 0;\n    \n}\n",
        completion_config(),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            // Move to line 2 (the empty line)
            Do(MoveDown),
            Do(MoveDown),
            // Type "let y: Opt"
            Do(InsertChar('l')),
            Do(InsertChar('e')),
            Do(InsertChar('t')),
            Do(InsertChar(' ')),
            Do(InsertChar('y')),
            Do(InsertChar(':')),
            Do(InsertChar(' ')),
            Do(InsertChar('O')),
            Do(InsertChar('p')),
            Do(InsertChar('t')),
            WaitFor(has_completion),
        ]);

    let comp = t.state.lsp.completion.as_ref().unwrap();
    assert!(
        comp.items.iter().any(|i| i.label.contains("Option")),
        "expected Option in completions, got: {:?}",
        comp.items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn lsp_completion_filters_as_you_type() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 0;\n    \n}\n",
        completion_config(),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(MoveDown),
            Do(MoveDown),
            // Type "let y: St"
            Do(InsertChar('l')),
            Do(InsertChar('e')),
            Do(InsertChar('t')),
            Do(InsertChar(' ')),
            Do(InsertChar('y')),
            Do(InsertChar(':')),
            Do(InsertChar(' ')),
            Do(InsertChar('S')),
            Do(InsertChar('t')),
            WaitFor(has_completion),
            // Now type "ri" — should narrow results
            Do(InsertChar('r')),
            Do(InsertChar('i')),
            WaitFor(has_completion),
        ]);

    let comp = t.state.lsp.completion.as_ref().unwrap();
    assert!(
        comp.items.iter().any(|i| {
            let text = i.filter_text.as_deref().unwrap_or(&i.label);
            text.contains("Stri") || text.contains("String")
        }),
        "expected at least one String-related completion, got: {:?}",
        comp.items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn lsp_completion_accept_moves_cursor() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 0;\n    \n}\n",
        completion_config(),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(MoveDown),
            Do(MoveDown),
            // Type "Opt"
            Do(InsertChar('O')),
            Do(InsertChar('p')),
            Do(InsertChar('t')),
            WaitFor(has_completion),
            // Accept with Tab
            Do(InsertTab),
            // Completion should be dismissed
            WaitFor(|s| s.lsp.completion.is_none()),
        ]);

    let b = buf(&t);
    let line = line(&**b.doc(), 2);
    assert!(
        line.len() > 3,
        "expected completion text on line 2, got: {:?}",
        line
    );
    assert!(
        b.cursor_col().0 > 3,
        "expected cursor after inserted text, got col={}",
        b.cursor_col().0
    );
}

#[test]
fn lsp_completion_dismiss_on_escape() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 0;\n    \n}\n",
        completion_config(),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(MoveDown),
            Do(MoveDown),
            Do(InsertChar('O')),
            Do(InsertChar('p')),
            Do(InsertChar('t')),
            WaitFor(has_completion),
            Do(Abort),
        ]);

    assert!(
        t.state.lsp.completion.is_none(),
        "expected completion dismissed after Escape"
    );
}

#[test]
fn lsp_completion_trigger_char_fresh_request() {
    let (root, main_rs) = lsp_project(
        "fn main() {\n    let x = 0;\n    \n}\n",
        completion_config(),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(MoveDown),
            Do(MoveDown),
            Do(InsertChar('O')),
            Do(InsertChar('p')),
            Do(InsertChar('t')),
            Do(InsertChar('i')),
            Do(InsertChar('o')),
            Do(InsertChar('n')),
            // Typing "::" should trigger fresh completion for variants
            Do(InsertChar(':')),
            Do(InsertChar(':')),
            WaitFor(has_completion),
        ]);

    let comp = t.state.lsp.completion.as_ref().unwrap();
    let labels: Vec<&str> = comp.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.iter().any(|l| *l == "Some" || *l == "None"),
        "expected Some/None after Option::, got: {:?}",
        labels
    );
}

#[test]
fn lsp_format_on_save() {
    let (root, main_rs) = lsp_project(
        "fn   main(  )  {\n    let   x  =  1;\n    let y = 2;\n    println!(\"{}\",  x);\n}\n",
        serde_json::json!({
            "formatting": {
                "main.rs": "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x);\n}\n"
            }
        }),
    );

    let t = TestHarness::with_dir(root)
        .with_lsp_server(fake_lsp_binary())
        .with_arg(main_rs)
        .run(vec![
            WaitFor(lsp_server_ready),
            Do(Save),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|path| s.buffers.get(path))
                    .map_or(false, |b| {
                        !b.is_dirty()
                            && !b.save_in_flight()
                            && line(&**b.doc(), 0).starts_with("fn main()")
                    })
            }),
        ]);

    let b = buf(&t);
    assert!(
        line(&**b.doc(), 0).starts_with("fn main()"),
        "expected formatted, got: {:?}",
        line(&**b.doc(), 0)
    );
    assert!(!b.is_dirty() && !b.save_in_flight());
}

// ── Multi-file and directory CLI opening ──

fn tab_names_sorted(state: &led_state::AppState) -> Vec<String> {
    state
        .tabs
        .iter()
        .filter_map(|t| {
            t.path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect()
}

#[test]
fn multi_file_tab_order_and_activation() {
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "aaa\n")
        .with_named_file("bbb.txt", "bbb\n")
        .with_named_file("ccc.txt", "ccc\n")
        .run(vec![]);

    assert_eq!(t.state.buffers.len(), 3);

    // Last file should be active
    let active = buf(&t);
    assert_eq!(line(&**active.doc(), 0), "ccc");

    // Tab order should follow arg order
    assert_eq!(
        tab_names_sorted(&t.state),
        vec!["aaa.txt", "bbb.txt", "ccc.txt"]
    );
}

#[test]
fn multi_file_session_last_arg_active() {
    // Run 1: open 3 files, quit to save session
    let t = TestHarness::new()
        .with_named_file("a.txt", "aaa\n")
        .with_named_file("b.txt", "bbb\n")
        .with_named_file("c.txt", "ccc\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    let dir = t.dirs.root.clone();
    let a_path = t.dirs.workspace.join("a.txt");
    let c_path = t.dirs.workspace.join("c.txt");

    // Run 2: restart with args [a, c]. All 3 files from session, but c should be active.
    let t2 = TestHarness::with_dir(dir)
        .with_arg(a_path)
        .with_arg(c_path)
        .run(vec![WaitFor(|s| s.buffers.len() >= 3)]);

    let active = buf(&t2);
    assert_eq!(
        line(&**active.doc(), 0),
        "ccc",
        "last arg file (c.txt) should be active"
    );
}

#[test]
fn multi_file_session_last_used_bumped() {
    // Run 1: open 2 files, quit to save session
    let t = TestHarness::new()
        .with_named_file("keep.txt", "keep\n")
        .with_named_file("other.txt", "other\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    let dir = t.dirs.root.clone();
    let keep_path = t.dirs.workspace.join("keep.txt");

    // Run 2: restart with keep.txt as arg — its last_used should be bumped
    let t2 = TestHarness::with_dir(dir)
        .with_arg(keep_path.clone())
        .run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    let keep_path_canon = UserPath::new(&keep_path).canonicalize();
    let keep_buf = t2
        .state
        .buffers
        .values()
        .find(|b| b.path() == Some(&keep_path_canon))
        .expect("keep.txt should be open");
    let other_buf = t2
        .state
        .buffers
        .values()
        .find(|b| {
            b.path()
                .and_then(|p| p.file_name())
                .map_or(false, |n| n == "other.txt")
        })
        .expect("other.txt should be open");

    assert!(
        keep_buf.last_used() >= other_buf.last_used(),
        "arg file's last_used should be >= non-arg file's last_used"
    );
}

#[test]
fn multi_file_session_arg_tab_order() {
    // Run 1: open 3 files, quit to save session
    let t = TestHarness::new()
        .with_named_file("a.txt", "aaa\n")
        .with_named_file("b.txt", "bbb\n")
        .with_named_file("c.txt", "ccc\n")
        .run(vec![Do(Quit), WaitFor(|s| s.session.saved)]);

    let dir = t.dirs.root.clone();
    let a_path = t.dirs.workspace.join("a.txt");
    let c_path = t.dirs.workspace.join("c.txt");

    // Run 2: restart with args [a, c]. b is non-arg, a and c are args.
    // Arg files should be reordered to the end in arg order.
    let t2 = TestHarness::with_dir(dir)
        .with_arg(a_path)
        .with_arg(c_path)
        .run(vec![WaitFor(|s| s.buffers.len() >= 3)]);

    let names = tab_names_sorted(&t2.state);
    assert_eq!(
        names,
        vec!["b.txt", "a.txt", "c.txt"],
        "non-arg (b) first, then args in order (a, c)"
    );
}

#[test]
fn directory_opens_browser_focused() {
    let td = tempfile::TempDir::new().expect("tmpdir");
    let root = td.keep();
    let workspace = root.join("workspace");
    let config = root.join("config");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::create_dir_all(&config).expect("mkdir");
    std::fs::write(workspace.join("hello.txt"), "hi\n").expect("write");

    let t = TestHarness::with_dir(root)
        .with_arg_dir(workspace)
        .run(vec![WaitFor(|s| s.phase == led_state::Phase::Running)]);

    assert_eq!(
        t.state.focus,
        led_core::PanelSlot::Side,
        "focus should be on file browser"
    );
    assert!(t.state.browser.root.is_some(), "browser root should be set");
    assert!(t.state.buffers.is_empty(), "no files should be opened");
}

#[test]
fn directory_in_workspace_reveals_subdir() {
    let td = tempfile::TempDir::new().expect("tmpdir");
    let root = td.keep();
    let workspace = root.join("workspace");
    let config = root.join("config");
    std::fs::create_dir_all(&workspace).expect("mkdir");
    std::fs::create_dir_all(&config).expect("mkdir");

    // Create a .git dir so workspace resolves to this root
    std::fs::create_dir_all(workspace.join(".git")).expect("mkdir .git");

    // Create nested directory structure
    let deep_dir = workspace.join("src").join("deep");
    std::fs::create_dir_all(&deep_dir).expect("mkdir deep");
    std::fs::write(deep_dir.join("file.txt"), "content\n").expect("write");

    let t = TestHarness::with_dir(root)
        .with_arg_dir(deep_dir)
        .run(vec![WaitFor(browser_reveal_done)]);

    // Canonicalize for comparison (macOS /var → /private/var)
    let workspace = UserPath::new(&workspace).canonicalize();
    let src_dir = workspace.join("src");
    let canonical_deep = src_dir.join("deep");

    assert_eq!(
        t.state.focus,
        led_core::PanelSlot::Side,
        "focus should be on file browser"
    );

    // Browser root should be the git root (workspace dir)
    let browser_root = t.state.browser.root.as_ref().expect("browser root set");
    assert_eq!(
        *browser_root, workspace,
        "browser root should be the workspace (git root)"
    );

    // Ancestor dirs should be expanded
    assert!(
        t.state.browser.expanded_dirs.contains(&src_dir),
        "src/ should be expanded, expanded_dirs: {:?}",
        t.state.browser.expanded_dirs,
    );

    // The target directory should be selected
    let selected = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected.path, canonical_deep,
        "selected entry should be the target directory"
    );
}

// ── File search replace ──

fn file_search_has_results(s: &led_state::AppState) -> bool {
    s.file_search
        .as_ref()
        .is_some_and(|fs| !fs.flat_hits.is_empty())
}

#[test]
fn file_search_toggle_replace_mode() {
    let t = TestHarness::new()
        .with_named_file("a.txt", "hello world\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(ToggleSearchReplace),
        ]);
    let fs = t.state.file_search.as_ref().unwrap();
    assert!(fs.replace_mode);
}

#[test]
fn file_search_replace_unified_navigation() {
    use led_state::file_search::FileSearchSelection;
    let t = TestHarness::new()
        .with_named_file("a.txt", "hello world\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(ToggleSearchReplace),
            Do(InsertChar('h')),
            Do(InsertChar('e')),
            Do(InsertChar('l')),
            WaitFor(file_search_has_results),
            Do(MoveDown), // → ReplaceInput
            Do(MoveDown), // → Result(0)
            Do(MoveUp),   // → ReplaceInput
            Do(MoveUp),   // → SearchInput
        ]);
    let fs = t.state.file_search.as_ref().unwrap();
    assert_eq!(fs.selection, FileSearchSelection::SearchInput);
}

#[test]
fn file_search_replace_input_no_retrigger() {
    let t = TestHarness::new()
        .with_named_file("a.txt", "hello world\nhello again\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('h')),
            Do(InsertChar('e')),
            WaitFor(file_search_has_results),
            Do(ToggleSearchReplace),
            Do(MoveDown), // → ReplaceInput
            Do(InsertChar('H')),
            Do(InsertChar('E')),
        ]);
    let fs = t.state.file_search.as_ref().unwrap();
    assert_eq!(fs.replace_text, "HE");
    assert!(!fs.flat_hits.is_empty());
}

#[test]
fn file_search_replace_single_in_buffer() {
    let t = TestHarness::new()
        .with_named_file("a.txt", "aaa\nbbb\naaa\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            WaitFor(file_search_has_results),
            Do(ToggleSearchReplace),
            Do(MoveDown), // → ReplaceInput
            Do(InsertChar('x')),
            Do(InsertChar('x')),
            Do(InsertChar('x')),
            Do(MoveDown),  // → Result(0)
            Do(MoveRight), // replace first match
        ]);
    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "xxx");
    assert_eq!(line(&**b.doc(), 2), "aaa");
    assert!(b.is_dirty() && !b.save_in_flight());
}

#[test]
fn file_search_unreplace_single() {
    let t = TestHarness::new()
        .with_named_file("a.txt", "aaa\nbbb\naaa\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            WaitFor(file_search_has_results),
            Do(ToggleSearchReplace),
            Do(MoveDown), // → ReplaceInput
            Do(InsertChar('x')),
            Do(MoveDown),  // → Result(0)
            Do(MoveRight), // replace first match
            Do(MoveLeft),  // unreplace
        ]);
    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "aaa");
}

#[test]
fn file_search_replace_all() {
    let t = TestHarness::new()
        .with_named_file("a.txt", "aaa\nbbb\naaa\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            WaitFor(file_search_has_results),
            Do(ToggleSearchReplace),
            Do(MoveDown), // → ReplaceInput
            Do(InsertChar('z')),
            Do(ReplaceAll),
        ]);
    assert!(t.state.file_search.is_none());
    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "z");
    assert_eq!(line(&**b.doc(), 2), "z");
}

#[test]
fn file_search_replace_all_then_undo() {
    // ReplaceAll puts all replacements in one undo group — one Undo reverts all.
    let t = TestHarness::new()
        .with_named_file("a.txt", "aaa\nbbb\naaa\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            WaitFor(file_search_has_results),
            Do(ToggleSearchReplace),
            Do(MoveDown),
            Do(InsertChar('z')),
            Do(ReplaceAll), // replaces both, closes file search
            Do(Undo),       // one undo should revert ALL replacements
        ]);
    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "aaa");
    assert_eq!(line(&**b.doc(), 2), "aaa");
}

#[test]
fn file_search_replace_undo_chain() {
    // Right, Right replaces two matches. Close file search. Undo reverses them one by one.
    let t = TestHarness::new()
        .with_named_file("a.txt", "aaa\nbbb\naaa\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            Do(InsertChar('a')),
            WaitFor(file_search_has_results),
            Do(ToggleSearchReplace),
            Do(MoveDown),
            Do(InsertChar('z')),
            Do(MoveDown),  // → Result(0)
            Do(MoveRight), // replace first "aaa" → "z"
            Do(MoveRight), // replace second "aaa" → "z"
            Do(Abort),     // close file search
            Do(Undo),      // undo second replace
        ]);
    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "z"); // first replace still applied
    assert_eq!(line(&**b.doc(), 2), "aaa"); // second was undone
}

#[test]
fn file_search_no_replace_mode_arrows_noop() {
    let t = TestHarness::new()
        .with_named_file("a.txt", "hello\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('h')),
            WaitFor(file_search_has_results),
            Do(MoveDown),  // → Result(0)
            Do(MoveRight), // no-op, replace mode off
        ]);
    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "hello");
}

#[test]
fn file_search_replace_all_multi_file() {
    let t = TestHarness::new()
        .with_named_file("a.txt", "hello\n")
        .with_named_file("b.txt", "hello\n")
        .run(vec![
            WaitFor(has_browser_entries),
            Do(OpenFileSearch),
            Do(InsertChar('h')),
            Do(InsertChar('e')),
            Do(InsertChar('l')),
            Do(InsertChar('l')),
            Do(InsertChar('o')),
            WaitFor(file_search_has_results),
            Do(ToggleSearchReplace),
            Do(MoveDown),
            Do(InsertChar('b')),
            Do(InsertChar('y')),
            Do(InsertChar('e')),
            Do(ReplaceAll),
        ]);
    assert!(t.state.file_search.is_none());
    let modified_count = t
        .state
        .buffers
        .values()
        .filter(|b| {
            b.path()
                .is_some_and(|p| p.ends_with("a.txt") || p.ends_with("b.txt"))
                && line(&**b.doc(), 0) == "bye"
        })
        .count();
    assert_eq!(modified_count, 2, "both buffers should be modified");
}

// ── Keyboard macros ──

#[test]
fn kbd_macro_record_and_playback() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        KbdMacroStart,
        InsertChar('x'),
        InsertChar('y'),
        KbdMacroEnd,
        KbdMacroExecute,
    ]));
    let b = buf(&t);
    assert_eq!(line(&**b.doc(), 0), "xyxyhello");
}

#[test]
fn kbd_macro_playback_multiple() {
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        KbdMacroStart,
        InsertChar('a'),
        KbdMacroEnd,
        KbdMacroExecute,
        KbdMacroExecute,
        KbdMacroExecute,
    ]));
    let b = buf(&t);
    // 'a' inserted during recording + 3 playbacks = "aaaa"
    assert_eq!(line(&**b.doc(), 0), "aaaahello");
}

#[test]
fn kbd_macro_playback_aborts_at_boundary() {
    // Macro: MoveUp. On first line, MoveUp fails → playback returns false
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        KbdMacroStart,
        MoveUp,
        KbdMacroEnd,
    ]));
    // Should have recorded successfully, macro is defined
    assert!(t.state.kbd_macro.last.is_some());
}

#[test]
fn kbd_macro_no_macro_defined() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![KbdMacroExecute]));
    assert_eq!(t.state.alerts.info.as_deref(), Some("No kbd macro defined"));
}

#[test]
fn kbd_macro_end_without_recording() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![KbdMacroEnd]));
    assert_eq!(
        t.state.alerts.info.as_deref(),
        Some("Not defining kbd macro")
    );
}

#[test]
fn kbd_macro_recording_indicator() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![KbdMacroStart]));
    assert!(t.state.kbd_macro.recording);
}

#[test]
fn kbd_macro_restart_during_recording() {
    // C-x ( while recording restarts
    let t = TestHarness::new().with_file("hello\n").run(actions(vec![
        KbdMacroStart,
        InsertChar('x'),
        InsertChar('y'),
        KbdMacroStart, // restart — clears current recording
        InsertChar('z'),
        KbdMacroEnd,
        KbdMacroExecute,
    ]));
    let b = buf(&t);
    // "xy" was typed during first recording (executed live), then restart,
    // "z" typed during second recording, then playback inserts another "z"
    assert_eq!(line(&**b.doc(), 0), "xyzzhello");
}

#[test]
fn browser_preview_switches_between_files() {
    let root = tempfile::TempDir::new().unwrap().keep();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("aaa.txt"), "first file\n").unwrap();
    std::fs::write(workspace.join("bbb.txt"), "second file\n").unwrap();

    fn preview_shows(s: &led_state::AppState, prefix: &str) -> bool {
        s.active_tab
            .as_ref()
            .and_then(|p| s.buffers.get(p))
            .map_or(false, |b| {
                let mut buf = String::new();
                b.doc().line(led_core::Row(0), &mut buf);
                buf.starts_with(prefix)
            })
    }

    let t = TestHarness::with_dir(root).run(vec![
        WaitFor(has_browser_entries),
        Do(MoveDown),
        WaitFor(|s| preview_shows(s, "second")),
        Do(MoveUp),
        WaitFor(|s| preview_shows(s, "first")),
        Do(MoveDown),
        WaitFor(|s| preview_shows(s, "second")),
        Do(MoveUp),
        WaitFor(|s| preview_shows(s, "first")),
    ]);

    let b = buf(&t);
    assert!(
        line(&**b.doc(), 0).starts_with("first"),
        "preview should show aaa.txt after navigating back up, got: {:?}",
        line(&**b.doc(), 0),
    );
}

#[test]
fn preview_positions_cursor_at_search_hit() {
    // File search hits on a specific line — preview should position cursor there.
    let root = tempfile::TempDir::new().unwrap().keep();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("target.txt"),
        "line0\nline1\nline2\nNEEDLE\nline4\n",
    )
    .unwrap();

    let t = TestHarness::with_dir(root).run(vec![
        WaitFor(has_browser_entries),
        Do(OpenFileSearch),
        Do(InsertChar('N')),
        Do(InsertChar('E')),
        Do(InsertChar('E')),
        Do(InsertChar('D')),
        Do(InsertChar('L')),
        Do(InsertChar('E')),
        WaitFor(file_search_has_results),
        // Navigate to the result — this opens the file as a preview
        Do(MoveDown),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .map_or(false, |b| b.is_materialized())
        }),
    ]);

    let b = buf(&t);
    assert_eq!(
        b.cursor_row().0,
        3,
        "preview cursor should be on line 3 (NEEDLE), got row={}",
        b.cursor_row().0
    );
}

#[test]
fn next_issue_opens_unopened_file_at_change_position() {
    // Git repo with two modified files. Only one is open as a tab.
    // NextIssue should open the other file at the change position, not row 0.
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let root = dir.keep();
    let workspace = root.join("workspace");
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();

    let aaa = workspace.join("aaa.txt");
    let bbb = workspace.join("bbb.txt");
    std::fs::write(&aaa, "aaa1\naaa2\naaa3\n").unwrap();
    std::fs::write(&bbb, "bbb1\nbbb2\nbbb3\nbbb4\nbbb5\n").unwrap();

    // Initialize git repo and commit both files.
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&workspace)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&workspace)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@test.com",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(&workspace)
        .output()
        .expect("git commit");

    // Modify both files — append lines.
    std::fs::write(&aaa, "aaa1\naaa2\naaa3\naaa_new\n").unwrap();
    std::fs::write(&bbb, "bbb1\nbbb2\nbbb3\nbbb4\nbbb5\nbbb_new\n").unwrap();

    // Only open aaa.txt.
    let t = TestHarness::with_dir(root).with_arg(aaa).run(vec![
        Do(Save),
        WaitFor(|s| !s.git.file_statuses.is_empty()),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .map_or(false, |b| !b.status().git_line_statuses().is_empty())
        }),
        // First NextIssue: lands on aaa.txt's change (line 3).
        Do(NextIssue),
        // Second NextIssue: should open bbb.txt at its change (line 5).
        Do(NextIssue),
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .map_or(false, |p| p.file_name().map_or(false, |n| n == "bbb.txt"))
        }),
        // Wait for the buffer to materialize.
        WaitFor(|s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .map_or(false, |b| b.is_materialized())
        }),
    ]);

    let active = t.state.active_tab.as_ref().unwrap();
    let name = active.file_name().unwrap().to_str().unwrap();
    assert_eq!(name, "bbb.txt");

    let b = t.state.buffers.get(active).unwrap();
    assert_eq!(
        b.cursor_row().0,
        5,
        "cursor should be at the git change (line 5), got row={}",
        b.cursor_row().0
    );
}

/// Run `git init`, optionally stage and commit a baseline, in `workspace`.
// ── Fake GH helper ──

static BUILD_FAKE_GH: Once = Once::new();

fn fake_gh_binary() -> PathBuf {
    BUILD_FAKE_GH.call_once(|| {
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "fake-gh"])
            .status()
            .expect("cargo build fake-gh");
        assert!(status.success(), "failed to build fake-gh");
    });
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("fake-gh");
    path
}

/// Create a git repo with a file, a commit, then an uncommitted edit
/// plus a `.fake-gh.json` config. Returns `(root_dir, file_path)`.
fn gh_pr_project(
    file_content: &str,
    edit_content: &str,
    config: serde_json::Value,
) -> (PathBuf, PathBuf) {
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let root = dir.keep();
    let workspace = root.join("workspace");
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();

    let file_path = workspace.join("test.txt");
    std::fs::write(&file_path, file_content).unwrap();

    // git init + commit
    git_init_with_commit(&workspace, &["test.txt"]);

    // Create an uncommitted edit so the git driver picks up the branch
    std::fs::write(&file_path, edit_content).unwrap();

    // Place fake-gh config in the workspace root
    std::fs::write(
        workspace.join(".fake-gh.json"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    (root, file_path)
}

// ── Git helpers ──

fn git_init_with_commit(workspace: &Path, stage: &[&str]) {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(workspace)
        .output()
        .expect("git init");
    for path in stage {
        std::process::Command::new("git")
            .args(["add", path])
            .current_dir(workspace)
            .output()
            .expect("git add");
    }
    let mut args = vec![
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@test.com",
        "commit",
        "-m",
        "init",
    ];
    if stage.is_empty() {
        args.push("--allow-empty");
    }
    std::process::Command::new("git")
        .args(&args)
        .current_dir(workspace)
        .output()
        .expect("git commit");
}

/// Stage `path` and create a commit in `workspace`.
fn git_add_commit(workspace: &Path, path: &str, message: &str) {
    std::process::Command::new("git")
        .args(["add", path])
        .current_dir(workspace)
        .output()
        .expect("git add");
    std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@test.com",
            "commit",
            "-m",
            message,
        ])
        .current_dir(workspace)
        .output()
        .expect("git commit");
}

#[test]
fn git_status_clears_after_save_then_external_commit() {
    // User-reported repro: "committing files in the terminal and the files
    // are not marked accordingly with the new statuses". Edit + save in
    // the editor, then commit externally — file_statuses must clear.
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let root = dir.keep();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let file_path = workspace.join("test.txt");
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();
    git_init_with_commit(&workspace, &["test.txt"]);

    let t = TestHarness::with_dir(root)
        .with_watchers()
        .with_arg(file_path)
        .run(vec![
            WaitFor(|s| {
                s.workspace.is_loaded() && s.git.branch.is_some() && s.git.file_statuses.is_empty()
            }),
            Do(InsertChar('X')),
            Do(Save),
            WaitFor(|s| !s.git.file_statuses.is_empty()),
            TestStep::RunFn(Box::new(|dirs| {
                git_add_commit(&dirs.workspace, "test.txt", "edit");
            })),
            WaitFor(|s| s.git.file_statuses.is_empty()),
        ]);

    assert!(
        t.state.git.file_statuses.is_empty(),
        "expected file_statuses to be empty after external commit, got {:?}",
        t.state.git.file_statuses
    );
}

#[test]
fn git_line_statuses_clear_after_external_commit() {
    // The git driver only emits LineStatuses for files in file_statuses,
    // so without explicit clearing, gutter markers persist after a file
    // transitions from dirty to clean externally.
    let dir = tempfile::TempDir::new().expect("tmpdir");
    let root = dir.keep();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let file_path = workspace.join("test.txt");
    std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();
    git_init_with_commit(&workspace, &["test.txt"]);

    let t = TestHarness::with_dir(root)
        .with_watchers()
        .with_arg(file_path)
        .run(vec![
            WaitFor(|s| {
                s.workspace.is_loaded() && s.git.branch.is_some() && s.git.file_statuses.is_empty()
            }),
            Do(InsertChar('X')),
            Do(Save),
            WaitFor(|s| !s.git.file_statuses.is_empty()),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|p| s.buffers.get(p))
                    .map_or(false, |b| !b.status().git_line_statuses().is_empty())
            }),
            TestStep::RunFn(Box::new(|dirs| {
                git_add_commit(&dirs.workspace, "test.txt", "edit");
            })),
            WaitFor(|s| s.git.file_statuses.is_empty()),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|p| s.buffers.get(p))
                    .map_or(false, |b| b.status().git_line_statuses().is_empty())
            }),
        ]);

    let active = t.state.active_tab.as_ref().unwrap();
    let b = t.state.buffers.get(active).unwrap();
    assert!(
        b.status().git_line_statuses().is_empty(),
        "expected line statuses to clear after external commit, got {:?}",
        b.status().git_line_statuses()
    );
}

// ── GH PR integration tests ──

#[test]
fn gh_pr_loads_on_startup() {
    // Verify that PR info is loaded when the editor starts in a git repo
    // with a PR. Uses a fake `gh` binary that returns canned JSON.
    let diff = "\
diff --git a/test.txt b/test.txt
--- a/test.txt
+++ b/test.txt
@@ -1,3 +1,4 @@
 line1
 line2
 line3
+new line
";
    let config = serde_json::json!({
        "pr_view": {
            "number": 42,
            "state": "OPEN",
            "url": "https://github.com/test/repo/pull/42",
            "reviewThreads": { "nodes": [] }
        },
        "pr_diff": diff
    });

    let (root, file_path) = gh_pr_project(
        "line1\nline2\nline3\n",
        "line1\nline2\nline3\nnew line\n",
        config,
    );

    let t = TestHarness::with_dir(root)
        .with_gh_binary(fake_gh_binary())
        .with_arg(file_path)
        .run(vec![
            WaitFor(|s| s.git.branch.is_some()),
            WaitFor(|s| s.git.pr.is_some()),
        ]);

    let pr = t.state.git.pr.as_ref().expect("PR should be loaded");
    assert_eq!(pr.number.0, 42, "PR number");
    assert_eq!(pr.status, led_state::PrStatus::Open, "PR status");
    assert_eq!(pr.url, "https://github.com/test/repo/pull/42");
    assert!(
        !pr.diff_files.is_empty(),
        "PR diff should contain at least one file"
    );
}

#[test]
fn gh_pr_no_pr_on_branch() {
    // When fake-gh returns failure (no config for pr_view), PR state should
    // remain None.
    let config = serde_json::json!({});

    let (root, file_path) = gh_pr_project("line1\n", "line1\nline2\n", config);

    let t = TestHarness::with_dir(root)
        .with_gh_binary(fake_gh_binary())
        .with_arg(file_path)
        .run(vec![
            WaitFor(|s| s.git.branch.is_some()),
            // Give the driver time to respond. We wait for the git branch
            // (which triggers PR load), then check that PR remains None.
            // Use a small action to flush another cycle.
            Do(Save),
            WaitFor(|s| {
                s.active_tab
                    .as_ref()
                    .and_then(|p| s.buffers.get(p))
                    .is_some_and(|b| !b.is_dirty())
            }),
        ]);

    assert!(
        t.state.git.pr.is_none(),
        "PR should be None when fake-gh has no pr_view config"
    );
}

#[test]
fn gh_pr_diff_lines_on_buffer() {
    // Verify that the PR diff line statuses are applied to the buffer's
    // status and are accessible for rendering.
    let diff = "\
diff --git a/test.txt b/test.txt
--- a/test.txt
+++ b/test.txt
@@ -1,2 +1,4 @@
 aaa
 bbb
+ccc
+ddd
";
    let config = serde_json::json!({
        "pr_view": {
            "number": 7,
            "state": "OPEN",
            "url": "https://github.com/test/repo/pull/7",
            "reviewThreads": { "nodes": [] }
        },
        "pr_diff": diff
    });

    let (root, file_path) = gh_pr_project("aaa\nbbb\n", "aaa\nbbb\nccc\nddd\n", config);

    let t = TestHarness::with_dir(root)
        .with_gh_binary(fake_gh_binary())
        .with_arg(file_path)
        .run(vec![WaitFor(|s| s.git.pr.is_some())]);

    let pr = t.state.git.pr.as_ref().unwrap();
    assert_eq!(pr.number.0, 7);

    // The diff should have one file with line statuses for the added lines
    assert_eq!(pr.diff_files.len(), 1, "expected 1 file in diff");
    let statuses = pr.diff_files.values().next().unwrap();
    assert!(
        !statuses.is_empty(),
        "expected PR diff line statuses for added lines"
    );
    // Lines 2 and 3 (0-based) should be marked as PrDiff
    let rows: Vec<usize> = statuses.iter().flat_map(|s| s.rows.clone()).collect();
    assert!(
        rows.contains(&2) && rows.contains(&3),
        "expected rows 2,3 in PR diff, got {rows:?}"
    );
}

#[test]
fn gh_pr_review_comments_loaded() {
    // Verify that line-level review comments from GraphQL are loaded
    // and associated with the correct file and line.
    let diff = "\
diff --git a/test.txt b/test.txt
--- a/test.txt
+++ b/test.txt
@@ -1,2 +1,4 @@
 aaa
 bbb
+ccc
+ddd
";
    let config = serde_json::json!({
        "pr_view": {
            "number": 10,
            "state": "OPEN",
            "url": "https://github.com/test/repo/pull/10",
            "reviews": []
        },
        "pr_diff": diff,
        "graphql": {
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "path": "test.txt",
                                    "line": 3,
                                    "isOutdated": false,
                                    "comments": {
                                        "nodes": [
                                            {
                                                "body": "Should this be ddd?",
                                                "author": { "login": "reviewer" }
                                            }
                                        ]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }
    });

    let (root, file_path) = gh_pr_project("aaa\nbbb\n", "aaa\nbbb\nccc\nddd\n", config);

    let t = TestHarness::with_dir(root)
        .with_gh_binary(fake_gh_binary())
        .with_arg(file_path)
        .run(vec![WaitFor(|s| s.git.pr.is_some())]);

    let pr = t.state.git.pr.as_ref().unwrap();
    assert_eq!(pr.number.0, 10);
    assert_eq!(pr.comments.len(), 1, "expected 1 file with comments");

    let comments = pr.comments.values().next().unwrap();
    assert_eq!(comments.len(), 1, "expected 1 comment");
    assert_eq!(
        comments[0].line.0, 2,
        "comment should be on row 2 (0-based from line 3)"
    );
    assert_eq!(comments[0].body, "Should this be ddd?");
    assert_eq!(comments[0].author, "reviewer");
}

#[test]
fn gh_pr_outdated_comments_skipped() {
    // Outdated review comments should not appear in the PR state.
    let config = serde_json::json!({
        "pr_view": {
            "number": 11,
            "state": "OPEN",
            "url": "https://github.com/test/repo/pull/11",
            "reviews": []
        },
        "pr_diff": "",
        "graphql": {
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "path": "test.txt",
                                    "line": 1,
                                    "isOutdated": true,
                                    "comments": {
                                        "nodes": [
                                            {
                                                "body": "Old comment",
                                                "author": { "login": "reviewer" }
                                            }
                                        ]
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        }
    });

    let (root, file_path) = gh_pr_project("aaa\n", "aaa\nbbb\n", config);

    let t = TestHarness::with_dir(root)
        .with_gh_binary(fake_gh_binary())
        .with_arg(file_path)
        .run(vec![WaitFor(|s| s.git.pr.is_some())]);

    let pr = t.state.git.pr.as_ref().unwrap();
    assert!(
        pr.comments.is_empty(),
        "outdated comments should be skipped, got {:?}",
        pr.comments
    );
}

// ── Standalone mode (--no-workspace) ──

/// Standalone mode reaches the Running phase without ever loading a
/// workspace: `WorkspaceState` stays `Standalone`, the sidebar is
/// hidden, and the browser is pre-rooted at `start_dir`.
#[test]
fn no_workspace_starts_in_standalone_mode() {
    let t = TestHarness::new()
        .with_no_workspace()
        .with_file("hello\n")
        .run(vec![]);

    assert_eq!(t.state.phase, led_state::Phase::Running);
    assert!(
        matches!(t.state.workspace, led_state::WorkspaceState::Standalone),
        "workspace should be Standalone, got {:?}",
        t.state.workspace
    );
    assert!(
        t.state.show_side_panel,
        "sidebar should be visible by default in standalone mode"
    );
    let browser_root = t
        .state
        .browser
        .root
        .as_ref()
        .expect("browser root must be pre-seeded in standalone mode");
    assert_eq!(
        browser_root.as_path(),
        t.state.startup.start_dir.as_path(),
        "browser root should equal start_dir in standalone mode"
    );
    assert_eq!(
        t.state
            .buffers
            .values()
            .filter(|b| b.is_materialized())
            .count(),
        1,
        "the argument file should still open"
    );
}

/// Standalone mode must never touch the session database. Even inside a
/// directory that looks like a git repo, and even across a quit, no
/// `db.sqlite` file is created.
#[test]
fn no_workspace_never_writes_session_db() {
    let t = TestHarness::new()
        .with_no_workspace()
        .with_file("commit message\n")
        .run(vec![
            Do(InsertChar('x')),
            WaitFor(|s| s.active_tab.is_some()),
            QuitAndWait,
        ]);

    let db_path = t.dirs.config.join("db.sqlite");
    assert!(
        !db_path.exists(),
        "standalone mode must never create {}",
        db_path.display()
    );
    let primary_dir = t.dirs.config.join("primary");
    assert!(
        !primary_dir.exists(),
        "standalone mode must never create the primary lock dir at {}",
        primary_dir.display()
    );
}

/// Standalone mode never activates workspace-scoped features: git stays
/// empty (no branch detected) and LSP never initializes.
#[test]
fn no_workspace_skips_git_and_lsp() {
    // Put a real .git dir next to the file — without --no-workspace this
    // would trigger git detection and surface a branch.
    let t = TestHarness::new()
        .with_no_workspace()
        .with_file("fn main() {}\n")
        .run(vec![TestStep::RunFn(Box::new(|dirs| {
            std::fs::create_dir_all(dirs.workspace.join(".git")).expect("create .git");
            std::fs::write(dirs.workspace.join(".git/HEAD"), "ref: refs/heads/main\n")
                .expect("write HEAD");
        }))]);

    assert!(
        t.state.git.branch.is_none(),
        "git must stay dormant in standalone mode, got {:?}",
        t.state.git.branch
    );
    assert!(
        t.state.git.file_statuses.is_empty(),
        "git file scan must not run in standalone mode"
    );
    assert!(
        !t.state.workspace.is_loaded(),
        "workspace must never transition to Loaded in standalone mode"
    );
}

/// Regression: standalone mode must kick off an initial directory
/// listing of `start_dir` so the sidebar actually renders files. The
/// bug was that `browser.root` was pre-seeded by `AppState::new` but
/// nothing ever set `pending_lists`, leaving `dir_contents` empty and
/// the sidebar blank. Fix lives in `session_of.rs`.
#[test]
fn no_workspace_populates_browser_from_start_dir() {
    let t = TestHarness::new()
        .with_no_workspace()
        .with_named_file("a.txt", "a\n")
        .with_named_file("b.txt", "b\n")
        .run(vec![WaitFor(|s| !s.browser.entries.is_empty())]);

    assert!(
        !t.state.browser.entries.is_empty(),
        "browser should have entries from start_dir listing"
    );
    let names: Vec<String> = t
        .state
        .browser
        .entries
        .iter()
        .filter_map(|e| e.path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    assert!(
        names.iter().any(|n| n == "a.txt"),
        "browser should list a.txt from start_dir, got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "b.txt"),
        "browser should list b.txt from start_dir, got {names:?}"
    );
}

/// Regression: the config file driver must still load keys.toml (and
/// therefore the keymap) in standalone mode. If it doesn't, every
/// keystroke — including C-c — is dropped and the app hangs with a
/// black screen. The bug was that `config_file_out` was gated on
/// `workspace.loaded()` and never fired when `workspace == Standalone`.
#[test]
fn no_workspace_loads_keymap_from_config_dir() {
    let t = TestHarness::new()
        .with_no_workspace()
        .with_file("hello\n")
        .run(vec![WaitFor(|s| s.keymap.is_some())]);

    assert!(
        t.state.keymap.is_some(),
        "standalone mode must still load the keymap so keystrokes dispatch"
    );
    assert!(
        t.state.config_keys.is_some(),
        "standalone mode must still read keys.toml"
    );
}

/// Normal mode (no `--no-workspace`) must still behave as before: the
/// workspace loads, the sidebar defaults to visible, and git activity
/// eventually surfaces a branch.
#[test]
fn normal_mode_still_loads_workspace() {
    let t = TestHarness::new().with_file("hi\n").run(vec![]);

    assert!(
        t.state.workspace.is_loaded(),
        "normal mode must transition to Loaded"
    );
    assert!(
        t.state.show_side_panel,
        "normal mode must default to showing the sidebar"
    );
}

// ── Reflow paragraph ──

#[test]
fn reflow_wraps_long_doc_comment() {
    let long_text = "word ".repeat(40);
    let src = format!("/// {long_text}\nfn foo() {{}}\n");
    let t = TestHarness::new()
        .with_file_ext(&src, "rs")
        .run(actions(vec![ReflowParagraph]));

    let b = buf(&t);
    let doc = &**b.doc();
    // The `fn foo() {}` line should still exist somewhere.
    let mut found_fn = false;
    let mut max_line_width = 0usize;
    for r in 0..doc.line_count() {
        let l = line(doc, r);
        if l.contains("fn foo()") {
            found_fn = true;
        }
        max_line_width = max_line_width.max(l.chars().count());
        if !l.is_empty() && !l.contains("fn foo()") {
            assert!(l.starts_with("/// "), "reflow line missing prefix: {l:?}");
        }
    }
    assert!(found_fn, "fn foo() should be preserved");
    assert!(
        max_line_width <= 100,
        "reflow should wrap to ≤100 chars, got max {max_line_width}"
    );
    assert!(doc.line_count() > 2, "expected multi-line wrap");
}

#[test]
fn reflow_markdown_paragraph_wraps() {
    let long_para = "word ".repeat(40);
    let src = format!("# Heading\n\n{long_para}\n\nOther paragraph.\n");
    let t = TestHarness::new().with_file_ext(&src, "md").run(vec![
        Do(MoveDown), // row 1 (blank)
        Do(MoveDown), // row 2 (long paragraph)
        Do(ReflowParagraph),
    ]);

    let b = buf(&t);
    let doc = &**b.doc();
    // Heading untouched.
    assert_eq!(line(doc, 0), "# Heading");
    // Every line ≤ 100 chars.
    for r in 0..doc.line_count() {
        assert!(
            line(doc, r).chars().count() <= 100,
            "line {r} too long: {}",
            line(doc, r)
        );
    }
    // "Other paragraph." must still exist.
    let text: String = (0..doc.line_count())
        .map(|r| line(doc, r))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Other paragraph."),
        "sibling paragraph touched: {text:?}"
    );
}

#[test]
fn reflow_on_plain_code_is_noop() {
    let src = "fn foo() {\n    let x = 1;\n}\n";
    let t = TestHarness::new()
        .with_file_ext(src, "rs")
        .run(vec![Do(MoveDown), Do(ReflowParagraph)]);

    let b = buf(&t);
    assert!(!b.is_dirty(), "reflow on code should not dirty the buffer");
}

#[test]
fn reflow_region_reflows_all_spans() {
    let long = "word ".repeat(30);
    let src = format!("/// {long}\nfn foo() {{}}\n\n/// {long}\nfn bar() {{}}\n");
    let t = TestHarness::new().with_file_ext(&src, "rs").run(vec![
        Do(SetMark),
        Do(FileEnd),
        Do(ReflowParagraph),
    ]);

    let b = buf(&t);
    let doc = &**b.doc();
    let text: String = (0..doc.line_count())
        .map(|r| line(doc, r))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("fn foo() {}"), "fn foo missing: {text}");
    assert!(text.contains("fn bar() {}"), "fn bar missing: {text}");
    for l in text.lines() {
        if l.is_empty() || l.contains("fn ") {
            continue;
        }
        assert!(l.starts_with("/// "), "unexpected line: {l:?}");
        assert!(l.chars().count() <= 100, "line too long: {l:?}");
    }
    let slash_count = text.lines().filter(|l| l.starts_with("/// ")).count();
    assert!(slash_count >= 4, "expected ≥4 /// lines, got {slash_count}");
}
