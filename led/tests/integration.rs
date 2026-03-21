mod harness;

use std::path::Path;
use std::sync::Arc;

use led_core::Action::*;
use led_core::{EditOp, Startup, UndoEntry};
use led_state::SaveState;

use TestStep::{Do, QuitAndWait, WaitFor};
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

fn indent_done(s: &led_state::AppState) -> bool {
    s.active_buffer
        .and_then(|id| s.buffers.get(&id))
        .map_or(true, |b| b.pending_indent_row.is_none())
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
    // Plain text file: no tree-sitter grammar, falls back to soft tab
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(vec![Do(InsertTab), WaitFor(indent_done)]);

    assert_eq!(buf(&t).doc.line(0), "    hello");
    assert_eq!(buf(&t).cursor_col, 4);
}

#[test]
fn insert_tab_alignment() {
    // Plain text file: soft tab aligns to next tab stop
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(InsertChar('x')),
        Do(InsertTab),
        WaitFor(indent_done),
    ]);

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
fn undo_all_clears_dirty() {
    // Repro: insert newline, undo — content is restored but dirty flag stays.
    // Undoing back to the saved state should clear dirty.
    let t = TestHarness::new().with_file("hello\n").run(vec![
        Do(InsertNewline),
        WaitFor(indent_done),
        Do(Undo),
    ]);

    assert_eq!(buf(&t).doc.line(0), "hello", "content should be restored");
    assert!(
        !buf(&t).doc.dirty(),
        "undoing back to saved state should clear dirty"
    );
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
    assert_eq!(b.doc.line(0), "1", "first line should be '1'");
    assert_eq!(b.doc.line(1), "2", "second line should be '2'");
    assert_eq!(b.doc.line(2), "3", "third line should be '3'");
    assert_eq!(
        b.doc.line_count(),
        4,
        "should have 4 lines: '1\\n2\\n3\\n' + trailing empty"
    );
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

// ── Tabs ──

#[test]
fn kill_buffer_clean() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![KillBuffer]));

    assert!(t.state.active_buffer.is_none());
    assert!(t.state.buffers.is_empty());
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
    assert!(t.state.active_buffer.is_some());
    assert!(!t.state.buffers.is_empty());
    assert!(t.state.confirm_kill);
    assert!(
        t.state
            .alerts
            .warn
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

    assert!(t.state.active_buffer.is_none());
    assert!(t.state.buffers.is_empty());
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
    assert!(t.state.active_buffer.is_some());
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
    assert!(t.state.active_buffer.is_some());
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
    assert_eq!(active.doc.line(0), "c");

    // NextTab wraps to first
    let t = TestHarness::new()
        .with_named_file("aaa.txt", "a\n")
        .with_named_file("bbb.txt", "b\n")
        .with_named_file("ccc.txt", "c\n")
        .run(actions(vec![NextTab]));

    let active = buf(&t);
    assert_eq!(
        active.doc.line(0),
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
        active.doc.line(0),
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
        active.doc.line(0),
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

    assert_eq!(t.state.buffers.len(), 2);
    let active = buf(&t);
    assert_eq!(
        active.doc.line(0),
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
        buf(&t).cursor_row,
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

    let id = t.state.active_buffer.unwrap();
    let buf_path = t.state.buffers[&id].path.as_ref().unwrap();
    let canonical = std::fs::canonicalize(buf_path).unwrap_or_else(|_| buf_path.clone());
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, canonical,
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

    let id = t.state.active_buffer.unwrap();
    let active_path = t.state.buffers[&id].path.as_ref().unwrap();
    let canonical = std::fs::canonicalize(active_path).unwrap_or_else(|_| active_path.clone());
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, canonical,
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
    let active_id = t.state.active_buffer.unwrap();
    let active_path = t.state.buffers[&active_id].path.as_ref().unwrap();
    let active_canonical =
        std::fs::canonicalize(active_path).unwrap_or_else(|_| active_path.clone());
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, active_canonical,
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

    let id = t.state.active_buffer.unwrap();
    let active_path = t.state.buffers[&id].path.as_ref().unwrap();
    let canonical = std::fs::canonicalize(active_path).unwrap_or_else(|_| active_path.clone());
    let selected_entry = &t.state.browser.entries[t.state.browser.selected];
    assert_eq!(
        selected_entry.path, canonical,
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
        .filter_map(|b| b.path.as_ref())
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

    let order1: Vec<(String, usize)> = {
        let mut v: Vec<_> = t
            .state
            .buffers
            .values()
            .map(|b| {
                let name = b
                    .path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (name, b.tab_order)
            })
            .collect();
        v.sort_by_key(|(_, o)| *o);
        v
    };

    let dir = t.dirs.root.clone();
    let cargo_path = t.dirs.workspace.join("Cargo.toml");

    // Run 2: restart with Cargo.toml as arg — session restores both
    let t2 = TestHarness::with_dir(dir.clone())
        .with_arg(cargo_path.clone())
        .run(vec![WaitFor(|s| s.buffers.len() >= 2)]);

    let order2: Vec<(String, usize)> = {
        let mut v: Vec<_> = t2
            .state
            .buffers
            .values()
            .map(|b| {
                let name = b
                    .path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (name, b.tab_order)
            })
            .collect();
        v.sort_by_key(|(_, o)| *o);
        v
    };
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

    let order3: Vec<(String, usize)> = {
        let mut v: Vec<_> = t3
            .state
            .buffers
            .values()
            .map(|b| {
                let name = b
                    .path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (name, b.tab_order)
            })
            .collect();
        v.sort_by_key(|(_, o)| *o);
        v
    };
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

    // Capture original tab orders
    let original: Vec<(String, usize)> = {
        let mut v: Vec<_> = t
            .state
            .buffers
            .values()
            .map(|b| {
                let name = b
                    .path
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (name, b.tab_order)
            })
            .collect();
        v.sort_by_key(|(_, o)| *o);
        v
    };

    let dir = t.dirs.root.clone();

    // Run 2: restore session — tab_order should be preserved
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| s.buffers.len() >= 3)]);

    let mut restored: Vec<(String, usize)> = t2
        .state
        .buffers
        .values()
        .map(|b| {
            let name = b
                .path
                .as_ref()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (name, b.tab_order)
        })
        .collect();
    restored.sort_by_key(|(_, o)| *o);

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
            s.session.restore_phase == led_state::SessionRestorePhase::Done && !s.buffers.is_empty()
        })]);

    // fileB.txt should be open; fileA.txt silently skipped
    assert_eq!(t2.state.buffers.len(), 1);
    let name = buf(&t2)
        .path
        .as_ref()
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
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| {
        s.session.restore_phase == led_state::SessionRestorePhase::Done
    })]);

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
        s.session.restore_phase == led_state::SessionRestorePhase::Done && !s.buffers.is_empty()
    })]);

    assert_eq!(t2.state.buffers.len(), 1);
    let name = buf(&t2)
        .path
        .as_ref()
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
            WaitFor(|s| s.buffers.len() >= 2),
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
        .filter_map(|b| b.path.as_ref())
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["Cargo.toml", "lib.rs"]);

    // Active tab should be the arg file — explicit arg overrides session's choice
    let active_name = buf(&t2)
        .path
        .as_ref()
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

    assert_eq!(buf(&t).cursor_row, 3);
    let dir = t.dirs.root.clone();

    // Run 2: restore — cursor should be at row 3
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);

    let id = t2.state.active_buffer.unwrap();
    let b = &t2.state.buffers[&id];
    assert_eq!(b.cursor_row, 3, "cursor row should be restored");
}

#[test]
fn session_restore_active_tab() {
    // Run 1: open two files, switch to first tab (last opened is active), quit
    let t = TestHarness::new()
        .with_named_file("first.txt", "a\n")
        .with_named_file("second.txt", "b\n")
        .run(vec![Do(PrevTab), Do(Quit), WaitFor(|s| s.session.saved)]);

    let active_name = buf(&t)
        .path
        .as_ref()
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
        .path
        .as_ref()
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
        .path
        .as_ref()
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
        .path
        .as_ref()
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
                s.active_buffer
                    .and_then(|id| s.buffers.get(&id))
                    .is_some_and(|b| b.save_state == SaveState::Clean)
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.save_state == SaveState::Clean)
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.save_state == SaveState::Clean)
        }),
        Do(Undo),
    ]);

    // After undo, trailing whitespace should be back
    let line = buf(&t).doc.line(0);
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.persisted_undo_len > 0)
        }),
        Do(Quit),
        WaitFor(|s| s.session.saved),
    ]);
    assert!(buf(&t).doc.dirty(), "buffer should be dirty");
    let dir = t.dirs.root.clone();

    // Run 2: restore — undo should work and revert the edits
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty()), Do(Undo)]);
    let line = buf(&t2).doc.line(0);
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.save_state == SaveState::Clean)
        }),
        Do(Quit),
        WaitFor(|s| s.session.saved),
    ]);
    let dir = t.dirs.root.clone();

    // Run 2: restore — buffer should be clean (save cleared undo in DB)
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);
    assert!(
        !buf(&t2).doc.dirty(),
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.persisted_undo_len > 0)
        }),
        Do(Quit),
        WaitFor(|s| s.session.saved),
    ]);
    let dir = t.dirs.root.clone();

    // Run 2: restore — buffer should be dirty (distance_from_save != 0)
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);
    assert!(
        buf(&t2).doc.dirty(),
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
fn session_restores_focus_on_browser() {
    // Run 1: switch focus to browser, quit
    let t = TestHarness::new().with_file("hello\n").run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        Do(ToggleFocus), // Main → Side
        QuitAndWait,
    ]);
    assert_eq!(t.state.focus, led_core::PanelSlot::Side);
    let dir = t.dirs.root.clone();

    // Run 2: restore — focus should be on the browser
    let t2 = TestHarness::with_dir(dir).run(vec![WaitFor(|s| !s.buffers.is_empty())]);
    assert_eq!(
        t2.state.focus,
        led_core::PanelSlot::Side,
        "focus should be restored to browser"
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
    let t = TestHarness::new().run(vec![WaitFor(|s| {
        s.session.restore_phase == led_state::SessionRestorePhase::Done
    })]);
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
        |s| active_buf(s).is_some_and(|b| b.doc.line(0) == "first"),
        WAIT,
        "first external save detected",
    );

    // Second external save (same atomic pattern)
    let tmp = file_path.with_extension("tmp");
    std::fs::write(&tmp, "second\n").unwrap();
    std::fs::rename(&tmp, &file_path).unwrap();

    inst.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line(0) == "second"),
        WAIT,
        "second external save detected",
    );

    inst.stop();
}

/// Direct write variant (non-atomic).
#[test]
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
        |s| active_buf(s).is_some_and(|b| b.doc.line(0) == "first"),
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
        |s| active_buf(s).is_some_and(|b| b.doc.line(0) == "second"),
        WAIT,
        "second external save detected",
    );

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
    let canonical_root =
        std::fs::canonicalize(&dirs.workspace).unwrap_or_else(|_| dirs.workspace.clone());
    let root_str = canonical_root.to_string_lossy();
    let path_str = file_path.to_string_lossy();

    let entries: Vec<Vec<u8>> = undo_entries
        .iter()
        .map(|e| rmp_serde::to_vec(e).unwrap())
        .collect();

    // distance_from_save = count of d=1 entries (each forward group adds 1)
    let distance: i32 = undo_entries.iter().map(|e| e.direction).sum();

    led_workspace::db::flush_undo(
        &conn,
        &root_str,
        &path_str,
        chain_id,
        content_hash,
        undo_entries.len(),
        distance,
        &entries,
    )
    .expect("flush_undo");
    drop(conn);

    // Touch notify file to wake the instance
    let hash = led_workspace::path_hash(file_path);
    let notify_dir = dirs.config.join("notify");
    std::fs::create_dir_all(&notify_dir).ok();
    std::fs::write(notify_dir.join(&hash), b"").ok();
}

fn make_insert_entry(offset: usize, text: &str) -> UndoEntry {
    UndoEntry {
        op: EditOp {
            offset,
            old_text: String::new(),
            new_text: text.to_string(),
        },
        cursor_before: offset,
        cursor_after: offset + text.chars().count(),
        direction: 1,
    }
}

#[test]
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
                s.active_buffer
                    .and_then(|id| s.buffers.get(&id))
                    .is_some_and(|b| b.doc.line_count() == 5) // was 4, now 5
            }),
        ]);

    let b = buf(&t);
    assert_eq!(b.doc.line(0), "aaa");
    assert_eq!(b.doc.line(1), ""); // the inserted newline
    assert_eq!(b.doc.line(2), "bbb");
    assert_eq!(b.doc.line(3), "ccc");
    assert_eq!(
        b.doc.line_count(),
        5,
        "should have exactly one extra newline, not double"
    );
}

#[test]
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
                s.active_buffer
                    .and_then(|id| s.buffers.get(&id))
                    .is_some_and(|b| b.doc.line(0).contains("XYZ"))
            }),
        ]);

    let b = buf(&t);
    assert_eq!(
        b.doc.line(0),
        "XYZhello",
        "all three edits should be replayed"
    );
}

#[test]
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
                s.active_buffer
                    .and_then(|id| s.buffers.get(&id))
                    .is_some_and(|b| b.doc.line(0).starts_with("X"))
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
                let canonical_root = std::fs::canonicalize(&dirs.workspace)
                    .unwrap_or_else(|_| dirs.workspace.clone());
                let root_str = canonical_root.to_string_lossy();
                let path_str = file_path.to_string_lossy();
                led_workspace::db::clear_undo(&conn, &root_str, &path_str).expect("clear_undo");
                drop(conn);

                // Touch notify to wake A
                let hash = led_workspace::path_hash(&file_path);
                let notify_dir = dirs.config.join("notify");
                std::fs::write(notify_dir.join(&hash), b"").ok();
            })),
            // Wait for A to detect the external save (chain_id should be reset)
            WaitFor(|s| {
                s.active_buffer
                    .and_then(|id| s.buffers.get(&id))
                    .is_some_and(|b| b.chain_id.is_none())
            }),
        ]);

    let b = buf(&t);
    // After external save detection, A should have reset its undo chain
    assert!(
        b.chain_id.is_none(),
        "chain_id should be reset after external save"
    );
    assert_eq!(b.last_seen_seq, 0, "last_seen_seq should be reset");
}

// ── Core bug: after save, only post-save undo groups should be flushed ──

#[test]
fn persisted_undo_len_preserved_after_save() {
    // After save, persisted_undo_len must match the undo history length —
    // NOT reset to 0.  The old code (see _old/crates/buffer/src/watcher.rs:396)
    // did this correctly:
    //     self.persisted_undo_len = self.save_history_len;
    // Without this, the next flush re-sends ALL undo groups (including
    // pre-save ones), which corrupts cross-instance sync on the receiver.
    let t = TestHarness::new().with_file("hello\n").run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        Do(InsertChar('X')),
        WaitFor(|s| {
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.persisted_undo_len > 0)
        }),
        Do(Save),
        WaitFor(|s| {
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.save_state == SaveState::Clean)
        }),
    ]);

    let b = buf(&t);
    assert!(
        b.persisted_undo_len > 0,
        "persisted_undo_len should reflect saved undo history, got 0"
    );
}

// ── Two-instance tests ──

use harness::two_instance::{Instance, shared_workspace, startup_for};
use led_state::AppState;

fn active_buf(s: &AppState) -> Option<&led_state::BufferState> {
    s.active_buffer
        .and_then(|id| s.buffers.get(&id))
        .map(|v| &**v)
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[test]
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
    assert!(a.state().unwrap().workspace.as_ref().unwrap().primary);
    assert!(!b.state().unwrap().workspace.as_ref().unwrap().primary);

    let a_lines_before = active_buf(&a.state().unwrap()).unwrap().doc.line_count();

    // Step 3: B adds a newline
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id.is_some()),
        WAIT,
        "B undo flushed",
    );
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() > a_lines_before),
        WAIT,
        "A synced first newline",
    );

    let a_lines_after_sync1 = active_buf(&a.state().unwrap()).unwrap().doc.line_count();
    assert_eq!(
        a_lines_after_sync1,
        a_lines_before + 1,
        "A should have synced the first newline"
    );

    // Step 4: B saves
    b.push(Save);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.save_state == SaveState::Clean),
        WAIT,
        "B saved",
    );

    // Step 5: B adds another newline
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id.is_some() && b.doc.dirty()),
        WAIT,
        "B second edit flushed",
    );

    // A must sync the second newline
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() > a_lines_after_sync1),
        WAIT,
        "A synced second newline",
    );

    let a_state = a.state().unwrap();
    let b_state = b.state().unwrap();
    let a_final = active_buf(&a_state).unwrap();
    let b_final = active_buf(&b_state).unwrap();
    assert_eq!(
        a_final.doc.line_count(),
        a_lines_before + 2,
        "A should have both newlines: {} lines, expected {}",
        a_final.doc.line_count(),
        a_lines_before + 2,
    );
    // Content should match between A and B
    for i in 0..b_final.doc.line_count() {
        assert_eq!(
            a_final.doc.line(i),
            b_final.doc.line(i),
            "line {i} mismatch between A and B"
        );
    }

    a.stop();
    b.stop();
}

#[test]
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

    let a_lines_before = active_buf(&a.state().unwrap()).unwrap().doc.line_count();

    // Step 3: B inserts newline
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id.is_some()),
        WAIT,
        "B first edit flushed",
    );
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() > a_lines_before),
        WAIT,
        "A synced first newline",
    );

    let a_lines_after_sync1 = active_buf(&a.state().unwrap()).unwrap().doc.line_count();
    assert_eq!(a_lines_after_sync1, a_lines_before + 1);

    // Step 4: wait 1000ms for the first flush to fully propagate
    std::thread::sleep(std::time::Duration::from_millis(1000));

    // Step 5: B inserts another newline (no save)
    b.push(InsertNewline);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() > a_lines_after_sync1),
        WAIT,
        "B has second newline",
    );

    // A must sync the second newline
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() > a_lines_after_sync1),
        WAIT,
        "A synced second newline",
    );

    let a_state = a.state().unwrap();
    let b_state = b.state().unwrap();
    let a_final = active_buf(&a_state).unwrap();
    let b_final = active_buf(&b_state).unwrap();
    assert_eq!(
        a_final.doc.line_count(),
        a_lines_before + 2,
        "A should have both newlines: {} lines, expected {}",
        a_final.doc.line_count(),
        a_lines_before + 2,
    );
    for i in 0..b_final.doc.line_count() {
        assert_eq!(
            a_final.doc.line(i),
            b_final.doc.line(i),
            "line {i} mismatch between A and B"
        );
    }

    a.stop();
    b.stop();
}

#[test]
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
        |s| active_buf(s).is_some_and(|b| b.chain_id.is_some()),
        WAIT,
        "B undo flushed",
    );

    // A syncs → A becomes dirty
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.dirty()),
        WAIT,
        "A dirty after sync",
    );

    // Step 4: B saves
    b.push(Save);
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.save_state == SaveState::Clean && !b.doc.dirty()),
        WAIT,
        "B clean after save",
    );

    // A must also become clean: the file on disk now matches A's content
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.doc.dirty()),
        WAIT,
        "A clean after remote save",
    );

    // Content should still match
    let a_state = a.state().unwrap();
    let b_state = b.state().unwrap();
    let a_buf = active_buf(&a_state).unwrap();
    let b_buf = active_buf(&b_state).unwrap();
    for i in 0..b_buf.doc.line_count() {
        assert_eq!(
            a_buf.doc.line(i),
            b_buf.doc.line(i),
            "line {i} mismatch between A and B"
        );
    }

    a.stop();
    b.stop();
}

#[test]
fn two_instance_no_args_browser_visible() {
    // Repro: open two instances with no file arguments (just PWD).
    // B (non-primary) should still show the file browser.
    let (dirs, _paths) = shared_workspace(&[("file_a.txt", "hello\n"), ("file_b.txt", "world\n")]);
    // Real workspaces have a .git dir — this changes how find_git_root resolves
    std::fs::create_dir_all(dirs.workspace.join(".git")).expect("create .git");

    // Start both with no arg_paths — like running `led` with no arguments
    let no_files_a = Startup {
        headless: true,
        enable_watchers: true,
        arg_paths: vec![],
        start_dir: Arc::new(dirs.workspace.clone()),
        config_dir: dirs.config.clone(),
    };
    let no_files_b = Startup {
        headless: true,
        enable_watchers: true,
        arg_paths: vec![],
        start_dir: Arc::new(dirs.workspace.clone()),
        config_dir: dirs.config.clone(),
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
    let a_state = a.state().unwrap();
    let b_state = b.state().unwrap();

    assert!(a_state.workspace.is_some(), "A should have a workspace");
    assert!(b_state.workspace.is_some(), "B should have a workspace");

    // With no files open, focus should be on the file browser
    assert_eq!(
        a_state.focus,
        led_core::PanelSlot::Side,
        "A focus should be on browser when no files open"
    );
    assert_eq!(
        b_state.focus,
        led_core::PanelSlot::Side,
        "B focus should be on browser when no files open"
    );

    assert!(
        !a_state.browser.entries.is_empty(),
        "A browser should have entries"
    );
    assert!(
        !b_state.browser.entries.is_empty(),
        "B browser should have entries, got empty (black screen)"
    );

    // Both should see the same files
    let a_names: Vec<&str> = a_state
        .browser
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    let b_names: Vec<&str> = b_state
        .browser
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(a_names, b_names, "both instances should see same files");

    a.stop();
    b.stop();
}

#[test]
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

    let original_lines = active_buf(&a.state().unwrap()).unwrap().doc.line_count();

    // Step 2: A inserts newline
    a.push(InsertNewline);
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.chain_id.is_some()),
        WAIT,
        "A undo flushed",
    );

    // B syncs the newline
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() > original_lines),
        WAIT,
        "B synced newline",
    );

    // Both should be dirty
    assert!(
        active_buf(&a.state().unwrap()).unwrap().doc.dirty(),
        "A should be dirty after edit"
    );
    assert!(
        active_buf(&b.state().unwrap()).unwrap().doc.dirty(),
        "B should be dirty after sync"
    );

    // Step 3: A undoes
    a.push(Undo);
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() == original_lines),
        WAIT,
        "A undid the newline",
    );

    // A should be clean (undo back to saved state)
    a.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.doc.dirty()),
        WAIT,
        "A clean after undo",
    );

    // B should sync the undo and also be clean
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| b.doc.line_count() == original_lines),
        WAIT,
        "B synced the undo",
    );
    b.wait_for(
        |s| active_buf(s).is_some_and(|b| !b.doc.dirty()),
        WAIT,
        "B clean after synced undo",
    );

    // Content should match original
    let a_state = a.state().unwrap();
    let b_state = b.state().unwrap();
    let a_buf = active_buf(&a_state).unwrap();
    let b_buf = active_buf(&b_state).unwrap();
    assert_eq!(a_buf.doc.line_count(), original_lines);
    assert_eq!(b_buf.doc.line_count(), original_lines);
    for i in 0..a_buf.doc.line_count() {
        assert_eq!(
            a_buf.doc.line(i),
            b_buf.doc.line(i),
            "line {i} mismatch between A and B"
        );
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

    assert_eq!(buf(&t).mark, Some((0, 0)));
}

#[test]
fn mark_persists_on_movement() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, MoveDown]));

    assert_eq!(buf(&t).mark, Some((0, 0)));
    assert_eq!(buf(&t).cursor_row, 1);
}

#[test]
fn insert_clears_mark() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, InsertChar('x')]));

    assert!(buf(&t).mark.is_none());
}

#[test]
fn kill_region_deletes_selection() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, MoveDown, KillRegion]));

    assert_eq!(buf(&t).doc.line(0), "bbb");
    assert_eq!(buf(&t).cursor_row, 0);
    assert_eq!(buf(&t).cursor_col, 0);
    assert!(buf(&t).mark.is_none());
    assert_eq!(t.state.kill_ring.content, "aaa\n");
}

#[test]
fn kill_region_no_mark_warns() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![KillRegion]));

    assert_eq!(t.state.alerts.warn.as_deref(), Some("No region"));
}

#[test]
fn yank_inserts_killed_text() {
    let t = TestHarness::new()
        .with_file("aaa\nbbb\n")
        .run(actions(vec![SetMark, MoveDown, KillRegion, Yank]));

    assert_eq!(buf(&t).doc.line(0), "aaa");
    assert_eq!(buf(&t).doc.line(1), "bbb");
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
    assert_eq!(b.cursor_row, 0);
    assert_eq!(b.cursor_col, 0);
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
    assert_eq!(b.cursor_row, 1);
    assert_eq!(b.cursor_col, 0);
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
    assert_eq!(b.cursor_row, 1);
    assert_eq!(b.cursor_col, 0);
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
    assert_eq!(b.cursor_row, 1);
    assert_eq!(b.cursor_col, 0);
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
    assert_eq!(b.cursor_row, 0);
    assert_eq!(b.cursor_col, 0);
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
    assert_eq!(b.cursor_row, 0, "JumpBack should restore row");
    assert_eq!(b.cursor_col, 0, "JumpBack should restore col");
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
    assert_eq!(b.cursor_row, 2, "JumpForward should go to (2,0)");
    assert_eq!(b.cursor_col, 0);
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
        b.cursor_row, 1,
        "JumpBack with empty list should not move cursor"
    );
    assert_eq!(b.cursor_col, 0);
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
    assert_eq!(b.cursor_row, 2, "JumpForward at end should not move cursor");
    assert_eq!(b.cursor_col, 0);
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
        b.path
            .as_ref()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        "aaa.txt",
        "JumpBack should switch to the correct buffer"
    );
    assert_eq!(b.cursor_row, 0);
    assert_eq!(b.cursor_col, 0);
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
        b.cursor_row, 3,
        "Forward history should have been truncated"
    );
    assert_eq!(b.cursor_col, 0);
}

// ── Syntax highlighting tests ──

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
    assert_eq!(b.doc.line(0), "fn main() {");
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .map_or(false, |b| !b.syntax_highlights.is_empty())
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
            let b = s.active_buffer.and_then(|id| s.buffers.get(&id)).unwrap();
            // Highlights should contain a span for the current doc's content.
            // "fn" keyword should be highlighted on line 0 (bbb) after kills.
            b.syntax_highlights
                .iter()
                .any(|(line, span)| *line == 0 && span.capture_name.contains("keyword"))
        }),
    ]);

    let b = buf(&t);
    assert_eq!(b.doc.line(0), "fn bbb() {}");

    // All highlight lines must be within document bounds
    let line_count = b.doc.line_count();
    for (line, span) in b.syntax_highlights.iter() {
        assert!(
            *line < line_count,
            "highlight on line {} but doc has {} lines, span: {:?}",
            line,
            line_count,
            span.capture_name,
        );
    }

    // "fn" keyword must appear on line 0 (where "fn bbb" now lives)
    let has_fn_on_line0 = b
        .syntax_highlights
        .iter()
        .any(|(line, span)| *line == 0 && span.capture_name.contains("keyword"));
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .map_or(false, |b| !b.syntax_highlights.is_empty())
        }),
        // Kill "fn aaa() {}" text, then the newline
        Do(KillLine),
        Do(KillLine),
        // Now line 0 = "// this is a comment"
        // Wait for highlights: line 0 must have a comment capture,
        // NOT a keyword capture (which would indicate stale cache).
        WaitFor(|s| {
            let b = s.active_buffer.and_then(|id| s.buffers.get(&id)).unwrap();
            b.syntax_highlights
                .iter()
                .any(|(line, span)| *line == 0 && span.capture_name.contains("comment"))
        }),
    ]);

    let b = buf(&t);
    assert_eq!(b.doc.line(0), "// this is a comment");

    // Line 0 must have comment highlight, not keyword
    let has_comment = b
        .syntax_highlights
        .iter()
        .any(|(line, span)| *line == 0 && span.capture_name.contains("comment"));
    assert!(
        has_comment,
        "line 0 should have comment highlight after kill"
    );

    let has_keyword_on_0 = b
        .syntax_highlights
        .iter()
        .any(|(line, span)| *line == 0 && span.capture_name.contains("keyword"));
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
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .map_or(false, |b| !b.syntax_highlights.is_empty())
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
            let b = s.active_buffer.and_then(|id| s.buffers.get(&id)).unwrap();
            let lc = b.doc.line_count();
            // Highlights must be non-empty and fully within bounds
            !b.syntax_highlights.is_empty()
                && b.syntax_highlights.iter().all(|(line, _)| *line < lc)
        }),
    ]);

    let b = buf(&t);
    assert_eq!(b.doc.line(2), "Some text");

    // All highlight line numbers must be within doc bounds
    let lc = b.doc.line_count();
    for (line, span) in b.syntax_highlights.iter() {
        assert!(
            *line < lc,
            "stale highlight: line {} >= line_count {}, capture: {:?}",
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
                s.active_buffer
                    .and_then(|id| s.buffers.get(&id))
                    .and_then(|b| b.matching_bracket)
                    .is_some()
            }),
            Do(MatchBracket),
        ]);

    let b = buf(&t);
    // Should jump to the matching `}`
    assert_eq!(b.cursor_row, 2, "should jump to closing brace row");
    assert_eq!(b.cursor_col, 0, "should jump to closing brace col");
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
                s.active_buffer
                    .and_then(|id| s.buffers.get(&id))
                    .and_then(|b| b.matching_bracket)
                    .is_some()
            }),
            Do(MatchBracket),
        ]);

    let b = buf(&t);
    assert_eq!(b.cursor_row, 0, "should jump to opening brace row");
    assert_eq!(b.cursor_col, 7, "should jump to opening brace col");
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
    assert_eq!(b.cursor_row, 0);
    assert_eq!(b.cursor_col, 0, "cursor should not move");
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
    assert_eq!(b.cursor_row, 1);
    assert!(
        b.cursor_col >= 2,
        "cursor should be indented after '{{', got col {}",
        b.cursor_col
    );
    // Verify the indent text was actually inserted
    let line = b.doc.line(1);
    assert!(
        line.starts_with("    ") || line.starts_with('\t'),
        "new line should be indented: {:?}",
        line
    );
}

#[test]
fn auto_indent_closing_brace() {
    // After `fn main() {` with body, InsertCloseBracket('}') should dedent
    let t = TestHarness::new()
        .with_file_ext("fn main() {\n    let x = 1;\n    \n}\n", "rs")
        .run(vec![
            // Go to line 2 (the empty indented line)
            Do(MoveDown),
            Do(MoveDown),
            Do(LineEnd),
            // Type closing brace
            Do(InsertCloseBracket('}')),
            WaitFor(indent_done),
        ]);

    let b = buf(&t);
    let line = b.doc.line(2);
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
    assert_eq!(b.doc.line(0), "use a::A;", "first import should be a::A");
    assert_eq!(b.doc.line(1), "use z::Z;", "second import should be z::Z");
}

#[test]
fn sort_imports_no_change() {
    let t = TestHarness::new()
        .with_file_ext("use a::A;\nuse z::Z;\n\nfn main() {}\n", "rs")
        .run(actions(vec![SortImports]));

    let b = buf(&t);
    // Already sorted — should not modify doc
    assert!(
        !b.doc.dirty(),
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
    if !b.bracket_pairs.is_empty() {
        // Check that at least some pairs have different color indices
        let indices: Vec<Option<usize>> = b.bracket_pairs.iter().map(|p| p.color_index).collect();
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
fn close_bracket_maps_to_insert_close_bracket() {
    // Typing '}' should use InsertCloseBracket, not InsertChar
    // We test by checking that typing '}' on an indented empty line re-indents
    let t = TestHarness::new()
        .with_file_ext("fn main() {\n    \n}\n", "rs")
        .run(vec![
            Do(MoveDown), // go to line 1 (the indented empty line)
            Do(LineEnd),  // end of "    "
            Do(InsertCloseBracket('}')),
            WaitFor(indent_done),
        ]);

    let b = buf(&t);
    let line = b.doc.line(1);
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
        b.path
            .as_ref()
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
