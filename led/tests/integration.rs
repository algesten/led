mod harness;

use std::path::Path;

use led_core::Action::*;
use led_core::{EditOp, UndoGroup};
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

// ── Tabs ──

#[test]
fn kill_buffer_clean() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![KillBuffer]));

    assert!(t.state.active_buffer.is_none());
    assert!(t.state.buffers.is_empty());
    assert!(
        t.state.info.as_deref().unwrap_or("").contains("Killed"),
        "should show killed message"
    );
}

#[test]
fn kill_buffer_dirty_warns() {
    let t = TestHarness::new()
        .with_file("hello\n")
        .run(actions(vec![InsertChar('x'), KillBuffer]));

    // Buffer should NOT be killed
    assert!(t.state.active_buffer.is_some());
    assert!(!t.state.buffers.is_empty());
    assert!(
        t.state.warn.as_deref().unwrap_or("").contains("unsaved"),
        "should warn about unsaved changes"
    );
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
        .run(vec![Do(Quit), WaitFor(|s| s.session_saved)]);

    assert_eq!(t.state.buffers.len(), 2);
    let dir = t.tmpdir.clone();

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
fn session_restore_with_arg_file() {
    // Exact repro: start with ONE arg file, open a second from browser, quit, restart with same arg.
    let tmpdir = tempfile::TempDir::new().unwrap().keep();
    std::fs::write(tmpdir.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(tmpdir.join("lib.rs"), "fn main() {}\n").unwrap();
    let cargo_path = tmpdir.join("Cargo.toml");

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
        t.state.session_saved,
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

    // Active tab should be what the session saved, NOT overridden by the arg file
    let active_name = buf(&t2)
        .path
        .as_ref()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        active_name, "lib.rs",
        "active tab should be session's choice, not the arg file"
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
            WaitFor(|s| s.session_saved),
        ]);

    assert_eq!(buf(&t).cursor_row, 3);
    let dir = t.tmpdir.clone();

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
        .run(vec![Do(PrevTab), Do(Quit), WaitFor(|s| s.session_saved)]);

    let active_name = buf(&t)
        .path
        .as_ref()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(active_name, "first.txt");
    let dir = t.tmpdir.clone();
    let second_path = dir.join("second.txt");

    // Run 2: restart with second.txt as arg — first.txt should still be the active tab
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
    assert_eq!(active_name2, "first.txt", "active tab should be restored");
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
        WaitFor(|s| s.session_saved),
    ]);
    assert!(buf(&t).doc.dirty(), "buffer should be dirty");
    let dir = t.tmpdir.clone();

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
        WaitFor(|s| s.session_saved),
    ]);
    let dir = t.tmpdir.clone();

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
        WaitFor(|s| s.session_saved),
    ]);
    let dir = t.tmpdir.clone();

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
    let t = TestHarness::new().with_file("hello\n").run(vec![
        WaitFor(has_browser_entries),
        Do(ToggleFocus),
        Do(ExpandDir), // expand first dir entry (config/)
        Do(Quit),
        WaitFor(|s| s.session_saved),
    ]);
    assert!(
        !t.state.browser.expanded_dirs.is_empty(),
        "should have expanded dirs"
    );
    let expanded_dir = t.state.browser.expanded_dirs.iter().next().unwrap().clone();
    let dir = t.tmpdir.clone();

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

// ── Cross-instance sync ──

/// Simulate an external instance's edit by writing undo entries to the DB
/// and touching the notify file to wake the instance under test.
fn simulate_external_edit(
    tmpdir: &Path,
    file_path: &Path,
    chain_id: &str,
    content_hash: u64,
    groups: &[UndoGroup],
) {
    let config_dir = tmpdir.join("config");
    let db_path = config_dir.join("db.sqlite");
    let conn = rusqlite::Connection::open(&db_path).expect("open DB");

    // Must canonicalize to match the workspace driver (resolves /var → /private/var on macOS)
    let canonical_root = std::fs::canonicalize(tmpdir).unwrap_or_else(|_| tmpdir.to_path_buf());
    let root_str = canonical_root.to_string_lossy();
    let path_str = file_path.to_string_lossy();

    let entries: Vec<Vec<u8>> = groups
        .iter()
        .map(|g| rmp_serde::to_vec(g).unwrap())
        .collect();

    led_workspace::db::flush_undo(
        &conn,
        &root_str,
        &path_str,
        chain_id,
        content_hash,
        groups.len(),
        0,
        &entries,
    )
    .expect("flush_undo");
    drop(conn);

    // Touch notify file to wake the instance
    let hash = led_workspace::path_hash(file_path);
    let notify_dir = config_dir.join("notify");
    std::fs::create_dir_all(&notify_dir).ok();
    std::fs::write(notify_dir.join(&hash), b"").ok();
}

fn make_insert_group(offset: usize, text: &str) -> UndoGroup {
    UndoGroup {
        ops: vec![EditOp {
            offset,
            old_text: String::new(),
            new_text: text.to_string(),
        }],
        cursor_before: offset,
    }
}

fn make_remove_group(offset: usize, old_text: &str) -> UndoGroup {
    UndoGroup {
        ops: vec![EditOp {
            offset,
            old_text: old_text.to_string(),
            new_text: String::new(),
        }],
        cursor_before: offset,
    }
}

#[test]
fn cross_instance_sync_insert_newline() {
    // Bug 1 repro: instance B inserts a newline, instance A should see exactly one newline.
    let t = TestHarness::new().with_file("aaa\nbbb\nccc\n").run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        WaitFor(|s| s.watchers_ready),
        // Simulate instance B inserting a newline after "aaa"
        TestStep::RunFn(Box::new(|tmpdir| {
            let file_path = std::fs::read_dir(tmpdir)
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
                tmpdir,
                &file_path,
                "ext-chain-1",
                content_hash,
                &[make_insert_group(3, "\n")], // insert newline after "aaa"
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
    let t = TestHarness::new().with_file("hello\n").run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        WaitFor(|s| s.watchers_ready),
        TestStep::RunFn(Box::new(|tmpdir| {
            let file_path = std::fs::read_dir(tmpdir)
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
                tmpdir,
                &file_path,
                "ext-chain-2",
                content_hash,
                &[
                    make_insert_group(0, "X"),
                    make_insert_group(1, "Y"),
                    make_insert_group(2, "Z"),
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
    let t = TestHarness::new().with_file("original\n").run(vec![
        WaitFor(|s| !s.buffers.is_empty()),
        WaitFor(|s| s.watchers_ready),
        // First, simulate B editing
        TestStep::RunFn(Box::new(|tmpdir| {
            let file_path = std::fs::read_dir(tmpdir)
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
                tmpdir,
                &file_path,
                "ext-chain-3",
                content_hash,
                &[make_insert_group(0, "X")],
            );
        })),
        // Wait for first sync
        WaitFor(|s| {
            s.active_buffer
                .and_then(|id| s.buffers.get(&id))
                .is_some_and(|b| b.doc.line(0).starts_with("X"))
        }),
        // Now simulate B saving: write new content to disk and clear undo in DB
        TestStep::RunFn(Box::new(|tmpdir| {
            let file_path = std::fs::read_dir(tmpdir)
                .unwrap()
                .filter_map(|e| e.ok())
                .find(|e| e.file_name().to_string_lossy().ends_with(".txt"))
                .unwrap()
                .path();

            // Write new content to disk (simulating save)
            std::fs::write(&file_path, "Xoriginal\n").unwrap();

            // Clear undo in DB (like ClearUndo does after save)
            let config_dir = tmpdir.join("config");
            let db_path = config_dir.join("db.sqlite");
            let conn = rusqlite::Connection::open(&db_path).expect("open DB");
            let canonical_root =
                std::fs::canonicalize(tmpdir).unwrap_or_else(|_| tmpdir.to_path_buf());
            let root_str = canonical_root.to_string_lossy();
            let path_str = file_path.to_string_lossy();
            led_workspace::db::clear_undo(&conn, &root_str, &path_str).expect("clear_undo");
            drop(conn);

            // Touch notify to wake A
            let hash = led_workspace::path_hash(&file_path);
            let notify_dir = config_dir.join("notify");
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
    s.active_buffer.and_then(|id| s.buffers.get(&id))
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
    let (tmpdir, paths) = shared_workspace(&[("test.txt", "aaa\nbbb\n")]);

    let mut a = Instance::start(startup_for(&tmpdir, &paths));
    a.wait_for(
        |s| s.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "A ready",
    );

    let mut b = Instance::start(startup_for(&tmpdir, &paths));
    b.wait_for(
        |s| s.watchers_ready && !s.buffers.is_empty(),
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
    let (tmpdir, paths) = shared_workspace(&[("test.txt", "aaa\nbbb\n")]);

    let mut a = Instance::start(startup_for(&tmpdir, &paths));
    a.wait_for(
        |s| s.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "A ready",
    );

    let mut b = Instance::start(startup_for(&tmpdir, &paths));
    b.wait_for(
        |s| s.watchers_ready && !s.buffers.is_empty(),
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
    let (tmpdir, paths) = shared_workspace(&[("test.txt", "aaa\nbbb\n")]);

    let mut a = Instance::start(startup_for(&tmpdir, &paths));
    a.wait_for(
        |s| s.watchers_ready && !s.buffers.is_empty(),
        WAIT,
        "A ready",
    );

    let mut b = Instance::start(startup_for(&tmpdir, &paths));
    b.wait_for(
        |s| s.watchers_ready && !s.buffers.is_empty(),
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
