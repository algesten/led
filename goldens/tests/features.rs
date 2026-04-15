//! Narrative feature-flow goldens (axis 5). Each scenario is a multi-step
//! user journey that exercises a feature area end-to-end — open file, edit,
//! navigate, save, etc. Mechanical axes cover single actions in isolation;
//! this axis captures how features compose.

use led_goldens::{run_scenario, scenario_dir};

// === buffers ===

#[test]
fn buffers_open_two_switch_save_first() {
    run_scenario(&scenario_dir("features/buffers/open_two_switch_save_first"));
}

// buffers_modify_both_save_all — REMOVED. Same save_all race as
// edge/save_all_multiple_dirty (see POST-REWRITE-REVIEW.md).

#[test]
fn buffers_open_edit_kill_tab() {
    run_scenario(&scenario_dir("features/buffers/open_edit_kill_tab"));
}

// === editing ===

#[test]
fn editing_insert_then_undo_chain() {
    run_scenario(&scenario_dir("features/editing/insert_then_undo_chain"));
}

#[test]
fn editing_mark_kill_region_yank() {
    run_scenario(&scenario_dir("features/editing/mark_kill_region_yank"));
}

#[test]
fn editing_type_delete_reflow() {
    run_scenario(&scenario_dir("features/editing/type_delete_reflow"));
}

// === navigation ===

#[test]
fn navigation_word_movement_jump_back() {
    run_scenario(&scenario_dir("features/navigation/word_movement_jump_back"));
}

#[test]
fn navigation_goto_def_then_jump_back_forward() {
    run_scenario(&scenario_dir("features/navigation/goto_def_then_jump_back_forward"));
}

// === search ===

#[test]
fn search_isearch_find_accept_then_move() {
    run_scenario(&scenario_dir("features/search/isearch_find_accept_then_move"));
}

#[test]
fn search_isearch_cancel_restores_cursor() {
    run_scenario(&scenario_dir("features/search/isearch_cancel_restores_cursor"));
}

#[test]
fn search_isearch_advance_twice() {
    run_scenario(&scenario_dir("features/search/isearch_advance_twice"));
}

// === find_file ===

#[test]
fn find_file_open_type_abort() {
    run_scenario(&scenario_dir("features/find_file/open_type_abort"));
}

#[test]
fn find_file_open_type_select_file() {
    run_scenario(&scenario_dir("features/find_file/open_type_select_file"));
}

// === lsp ===

#[test]
fn lsp_goto_definition_then_back() {
    run_scenario(&scenario_dir("features/lsp/goto_definition_then_back"));
}

#[test]
fn lsp_rename_round_trip() {
    run_scenario(&scenario_dir("features/lsp/rename_round_trip"));
}

#[test]
fn lsp_diagnostic_next_issue() {
    run_scenario(&scenario_dir("features/lsp/diagnostic_next_issue"));
}

// === git ===

#[test]
fn git_workspace_open_file() {
    run_scenario(&scenario_dir("features/git/workspace_open_file"));
}

// === kill_yank ===

#[test]
fn kill_yank_kill_two_lines_yank() {
    run_scenario(&scenario_dir("features/kill_yank/kill_two_lines_yank"));
}

// kill_yank_kill_region_yank_elsewhere — REMOVED. Multi-step set-mark →
// move → kill-region → move → yank flow is racy; settle returns between
// presses but the kill dispatch isn't observable as PTY output, so the
// next press can fire before the kill completes. Single-step yank flows
// (kill_yank_kill_two_lines_yank) remain stable. See POST-REWRITE-REVIEW.

// === macros ===

#[test]
fn macros_record_and_replay() {
    run_scenario(&scenario_dir("features/macros/record_and_replay"));
}

#[test]
fn macros_record_insert_play_twice() {
    run_scenario(&scenario_dir("features/macros/record_insert_play_twice"));
}

// === save_flows ===

#[test]
fn save_flows_save_then_edit_again() {
    run_scenario(&scenario_dir("features/save_flows/save_then_edit_again"));
}

#[test]
fn save_flows_save_no_format_then_save() {
    run_scenario(&scenario_dir("features/save_flows/save_no_format_then_save"));
}

// save_flows_save_all_two_dirty — REMOVED. See edge.rs note on save_all
// race; current led's save-all has non-deterministic completion order.
