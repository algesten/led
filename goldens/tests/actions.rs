//! Auto-authored per-Action goldens. One test per Action variant (minus
//! dead/skipped). See goldens/scenarios/actions/*/setup.toml + script.txt.

use led_goldens::{run_scenario, scenario_dir};

// === Movement ===

#[test]
fn move_up() {
    run_scenario(&scenario_dir("actions/move_up"));
}

#[test]
fn move_down() {
    run_scenario(&scenario_dir("actions/move_down"));
}

#[test]
fn move_left() {
    run_scenario(&scenario_dir("actions/move_left"));
}

#[test]
fn move_right() {
    run_scenario(&scenario_dir("actions/move_right"));
}

#[test]
fn line_start() {
    run_scenario(&scenario_dir("actions/line_start"));
}

#[test]
fn line_end() {
    run_scenario(&scenario_dir("actions/line_end"));
}

#[test]
fn page_up() {
    run_scenario(&scenario_dir("actions/page_up"));
}

#[test]
fn page_down() {
    run_scenario(&scenario_dir("actions/page_down"));
}

#[test]
fn file_start() {
    run_scenario(&scenario_dir("actions/file_start"));
}

#[test]
fn file_end() {
    run_scenario(&scenario_dir("actions/file_end"));
}

#[test]
fn match_bracket() {
    run_scenario(&scenario_dir("actions/match_bracket"));
}

// === Insert / Delete ===

#[test]
fn insert_char() {
    run_scenario(&scenario_dir("actions/insert_char"));
}

#[test]
fn insert_newline() {
    run_scenario(&scenario_dir("actions/insert_newline"));
}

#[test]
fn delete_backward() {
    run_scenario(&scenario_dir("actions/delete_backward"));
}

#[test]
fn delete_forward() {
    run_scenario(&scenario_dir("actions/delete_forward"));
}

#[test]
fn insert_tab() {
    run_scenario(&scenario_dir("actions/insert_tab"));
}

#[test]
fn kill_line() {
    run_scenario(&scenario_dir("actions/kill_line"));
}

// === File / Save ===

#[test]
fn save() {
    run_scenario(&scenario_dir("actions/save"));
}

#[test]
fn save_as() {
    run_scenario(&scenario_dir("actions/save_as"));
}

// Action::SaveForce — skipped, dead variant (no handler, no binding).

#[test]
fn save_no_format() {
    run_scenario(&scenario_dir("actions/save_no_format"));
}

#[test]
fn save_all() {
    run_scenario(&scenario_dir("actions/save_all"));
}

#[test]
fn kill_buffer() {
    run_scenario(&scenario_dir("actions/kill_buffer"));
}

// === Navigation (tabs, jumps) ===

#[test]
fn prev_tab() {
    run_scenario(&scenario_dir("actions/prev_tab"));
}

#[test]
fn next_tab() {
    run_scenario(&scenario_dir("actions/next_tab"));
}

#[test]
fn jump_back() {
    run_scenario(&scenario_dir("actions/jump_back"));
}

#[test]
fn jump_forward() {
    run_scenario(&scenario_dir("actions/jump_forward"));
}

// Action::Outline — skipped, dead variant (bound to alt+o but no handler).

// === Search (in-buffer) ===

#[test]
fn in_buffer_search() {
    run_scenario(&scenario_dir("actions/in_buffer_search"));
}

// === Search (file-search overlay) ===

#[test]
fn open_file_search() {
    run_scenario(&scenario_dir("actions/open_file_search"));
}

// Action::CloseFileSearch — skipped, no default keybinding (Abort/Esc
// covers the same deactivate path). Reachable only via programmatic
// Action injection, which this axis does not use.

#[test]
fn toggle_search_case() {
    run_scenario(&scenario_dir("actions/toggle_search_case"));
}

#[test]
fn toggle_search_regex() {
    run_scenario(&scenario_dir("actions/toggle_search_regex"));
}

#[test]
fn toggle_search_replace() {
    run_scenario(&scenario_dir("actions/toggle_search_replace"));
}

#[test]
fn replace_all() {
    run_scenario(&scenario_dir("actions/replace_all"));
}

// === Find (find-file overlay) ===

#[test]
fn find_file() {
    run_scenario(&scenario_dir("actions/find_file"));
}

// === Edit (undo/redo, marks, yank, sort, reflow) ===

#[test]
fn undo() {
    run_scenario(&scenario_dir("actions/undo"));
}

// Action::Redo — skipped, no default keybinding anywhere in
// default_keys.toml.

#[test]
fn set_mark() {
    run_scenario(&scenario_dir("actions/set_mark"));
}

#[test]
fn kill_region() {
    // Exercises the "No region" alert branch; SetMark's Ctrl-Space isn't
    // sendable via the PTY runner so we can't pre-set a mark.
    run_scenario(&scenario_dir("actions/kill_region"));
}

#[test]
fn yank() {
    run_scenario(&scenario_dir("actions/yank"));
}

#[test]
fn sort_imports() {
    run_scenario(&scenario_dir("actions/sort_imports"));
}

#[test]
fn reflow_paragraph() {
    run_scenario(&scenario_dir("actions/reflow_paragraph"));
}

// === LSP ===

#[test]
fn lsp_goto_definition() {
    run_scenario(&scenario_dir("actions/lsp_goto_definition"));
}

#[test]
fn lsp_rename() {
    run_scenario(&scenario_dir("actions/lsp_rename"));
}

#[test]
fn lsp_code_action() {
    run_scenario(&scenario_dir("actions/lsp_code_action"));
}

// Action::LspFormat — skipped, no default keybinding (reachable only via
// programmatic injection, not covered by this axis). The Save path
// exercises the Format LSP request indirectly.

#[test]
fn next_issue() {
    run_scenario(&scenario_dir("actions/next_issue"));
}

#[test]
fn prev_issue() {
    run_scenario(&scenario_dir("actions/prev_issue"));
}

#[test]
fn lsp_toggle_inlay_hints() {
    run_scenario(&scenario_dir("actions/lsp_toggle_inlay_hints"));
}

// === UI ===

#[test]
fn toggle_focus() {
    run_scenario(&scenario_dir("actions/toggle_focus"));
}

#[test]
fn toggle_side_panel() {
    run_scenario(&scenario_dir("actions/toggle_side_panel"));
}

#[test]
fn expand_dir() {
    run_scenario(&scenario_dir("actions/expand_dir"));
}

#[test]
fn collapse_dir() {
    run_scenario(&scenario_dir("actions/collapse_dir"));
}

#[test]
fn collapse_all() {
    run_scenario(&scenario_dir("actions/collapse_all"));
}

#[test]
fn open_selected() {
    run_scenario(&scenario_dir("actions/open_selected"));
}

// Action::OpenSelectedBg — skipped, dead variant (bound to alt+enter in
// [browser] but no handler).

// Action::OpenMessages — skipped, dead variant (bound to ctrl+h e chord
// but no handler).

#[test]
fn open_pr_url() {
    run_scenario(&scenario_dir("actions/open_pr_url"));
}

#[test]
fn abort() {
    run_scenario(&scenario_dir("actions/abort"));
}

// === Macros ===

#[test]
fn kbd_macro_start() {
    run_scenario(&scenario_dir("actions/kbd_macro_start"));
}

#[test]
fn kbd_macro_end() {
    run_scenario(&scenario_dir("actions/kbd_macro_end"));
}

#[test]
fn kbd_macro_execute() {
    run_scenario(&scenario_dir("actions/kbd_macro_execute"));
}

// === Lifecycle / test ===

#[test]
fn quit() {
    run_scenario(&scenario_dir("actions/quit"));
}

// Action::Suspend — skipped, raises SIGTSTP (libc::raise) via process_of
// inspect side effect. Running it inside a PTY test actually suspends the
// test process and requires SIGCONT to recover.

// Action::Wait(u64) — skipped, test-harness-only (no handler in the
// model; `should_record` excludes it from macro recording). Not bound,
// only injectable via drivers.actions_in.

// Action::Resize(u16, u16) — skipped, driven by the terminal driver or
// explicit actions_in injection. No keystroke can trigger it.
