//! Edge-case goldens (axis 6). Unusual input conditions and feature
//! interactions that would expose bugs at the seams. See
//! docs/rewrite/GOLDENS-PLAN.md.
//!
//! Skipped per GOLDENS-PLAN.md (require infra not yet built or cannot
//! be expressed through the current runner):
//!
//!   * Symlinks — `FileSpec`/`with_file` writes real files; there's no
//!     way to seed a symlink in the workspace from setup.toml.
//!   * Open-same-path-twice — the led CLI takes paths as argv but the
//!     runner de-duplicates through the `FileSpec.path` map. Would need
//!     runner changes to pass the same path twice on the command line.

use led_goldens::{run_scenario, scenario_dir};

// === Unicode ===

#[test]
fn unicode_emoji() {
    run_scenario(&scenario_dir("edge/unicode_emoji"));
}

#[test]
fn unicode_rtl() {
    run_scenario(&scenario_dir("edge/unicode_rtl"));
}

#[test]
fn unicode_cjk() {
    run_scenario(&scenario_dir("edge/unicode_cjk"));
}

#[test]
fn unicode_combining() {
    run_scenario(&scenario_dir("edge/unicode_combining"));
}

// === Empty / minimal files ===

#[test]
fn empty_file_open() {
    run_scenario(&scenario_dir("edge/empty_file_open"));
}

#[test]
fn empty_file_type() {
    run_scenario(&scenario_dir("edge/empty_file_type"));
}

#[test]
fn empty_file_delete() {
    run_scenario(&scenario_dir("edge/empty_file_delete"));
}

#[test]
fn single_char_file() {
    run_scenario(&scenario_dir("edge/single_char_file"));
}

#[test]
fn blank_lines_only() {
    run_scenario(&scenario_dir("edge/blank_lines_only"));
}

// === Long lines / line endings / indentation ===

#[test]
fn very_long_line() {
    run_scenario(&scenario_dir("edge/very_long_line"));
}

#[test]
fn crlf_line_endings() {
    run_scenario(&scenario_dir("edge/crlf_line_endings"));
}

#[test]
fn mixed_tabs_spaces() {
    run_scenario(&scenario_dir("edge/mixed_tabs_spaces"));
}

#[test]
fn trailing_whitespace() {
    run_scenario(&scenario_dir("edge/trailing_whitespace"));
}

#[test]
fn no_trailing_newline() {
    run_scenario(&scenario_dir("edge/no_trailing_newline"));
}

// === Cursor motion at boundaries ===

#[test]
fn cursor_past_eol() {
    run_scenario(&scenario_dir("edge/cursor_past_eol"));
}

#[test]
fn cursor_past_eof() {
    run_scenario(&scenario_dir("edge/cursor_past_eof"));
}

// === Edit primitives at boundaries ===

#[test]
fn yank_empty_kill_ring() {
    run_scenario(&scenario_dir("edge/yank_empty_kill_ring"));
}

// === Save ===

#[test]
fn save_unchanged_twice() {
    run_scenario(&scenario_dir("edge/save_unchanged_twice"));
}

// save_all with multiple dirty buffers — REMOVED. The final "Saved <X>"
// status message reflects whichever save dispatch lands last, which races
// on HashMap iteration order. Behavior noted in
// docs/rewrite/POST-REWRITE-REVIEW.md.

// === Search ===

#[test]
fn search_empty_query() {
    run_scenario(&scenario_dir("edge/search_empty_query"));
}

#[test]
fn search_no_match() {
    run_scenario(&scenario_dir("edge/search_no_match"));
}

#[test]
fn search_wrap_around() {
    run_scenario(&scenario_dir("edge/search_wrap_around"));
}

// === Find-file overlay ===

#[test]
fn find_file_no_matches() {
    run_scenario(&scenario_dir("edge/find_file_no_matches"));
}

#[test]
fn find_file_many_matches() {
    run_scenario(&scenario_dir("edge/find_file_many_matches"));
}

// === LSP ===

#[test]
fn lsp_diagnostic_line_zero() {
    run_scenario(&scenario_dir("edge/lsp_diagnostic_line_zero"));
}

#[test]
fn lsp_diagnostic_empty_buffer() {
    run_scenario(&scenario_dir("edge/lsp_diagnostic_empty_buffer"));
}

#[test]
fn lsp_rebase_after_insert() {
    run_scenario(&scenario_dir("edge/lsp_rebase_after_insert"));
}

// === External filesystem changes ===

#[test]
fn external_change_while_dirty() {
    run_scenario(&scenario_dir("edge/external_change_while_dirty"));
}

#[test]
fn external_delete_open_file() {
    run_scenario(&scenario_dir("edge/external_delete_open_file"));
}

// === Lifecycle / prompts ===

#[test]
fn quit_with_unsaved() {
    run_scenario(&scenario_dir("edge/quit_with_unsaved"));
}
