//! Smoke tests for the golden runner. Each scenario lives in
//! `goldens/scenarios/smoke/<name>/` as setup.toml + script.txt; the
//! runner generates and verifies frame.snap + dispatched.snap.

use led_goldens::{run_scenario, scenario_dir};

#[test]
fn open_empty_file() {
    run_scenario(&scenario_dir("smoke/open_empty_file"));
}

#[test]
fn type_and_save() {
    run_scenario(&scenario_dir("smoke/type_and_save"));
}

#[test]
fn move_cursor_down_right() {
    run_scenario(&scenario_dir("smoke/move_cursor_down_right"));
}

#[test]
fn lsp_diagnostic() {
    run_scenario(&scenario_dir("smoke/lsp_diagnostic"));
}

#[test]
fn external_change() {
    run_scenario(&scenario_dir("smoke/external_change"));
}
