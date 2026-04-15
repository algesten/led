//! Axis 3: per-driver-event goldens. One scenario per externally-triggerable
//! `*In` variant. See `goldens/scenarios/driver_events/<driver>/<event>/`.
//!
//! Skipped events (grouped by reason) are documented inline as comments
//! next to their driver section.

use led_goldens::{run_scenario, scenario_dir};

// === docstore ===

#[test]
fn docstore_opened() {
    run_scenario(&scenario_dir("driver_events/docstore/opened"));
}

#[test]
fn docstore_saved() {
    run_scenario(&scenario_dir("driver_events/docstore/saved"));
}

#[test]
fn docstore_saved_as() {
    run_scenario(&scenario_dir("driver_events/docstore/saved_as"));
}

#[test]
fn docstore_external_change() {
    run_scenario(&scenario_dir("driver_events/docstore/external_change"));
}

// SKIP docstore_opening — inert: handler explicitly drops with None
// (buffers_of.rs). Not worth a golden.
// SKIP docstore_external_remove — inert: handler drops with None
// (buffers_of.rs). Known bug (stale buffer), not behavior to lock.
// SKIP docstore_open_failed — needs new mechanism: runner has no way to
// pass a non-existent CLI arg (every `[[file]]` entry writes to disk
// before spawn). Would need a `[[cli_arg]]` / `missing_file` setup flag,
// or pre-seeded session DB pointing at a deleted path.
// SKIP docstore_err_alert — needs new mechanism: runner cannot simulate
// a save I/O failure (readonly dir / chmod -w not exposed in setup.toml).

// === fs ===

#[test]
fn fs_dir_listed() {
    run_scenario(&scenario_dir("driver_events/fs/dir_listed"));
}

#[test]
fn fs_find_file_listed() {
    run_scenario(&scenario_dir("driver_events/fs/find_file_listed"));
}

// === lsp ===

#[test]
fn lsp_diagnostics() {
    run_scenario(&scenario_dir("driver_events/lsp/diagnostics"));
}

#[test]
fn lsp_completion() {
    run_scenario(&scenario_dir("driver_events/lsp/completion"));
}

#[test]
fn lsp_code_actions() {
    run_scenario(&scenario_dir("driver_events/lsp/code_actions"));
}

#[test]
fn lsp_edits() {
    run_scenario(&scenario_dir("driver_events/lsp/edits"));
}

#[test]
fn lsp_navigate() {
    run_scenario(&scenario_dir("driver_events/lsp/navigate"));
}

#[test]
fn lsp_trigger_chars() {
    run_scenario(&scenario_dir("driver_events/lsp/trigger_chars"));
}

// SKIP lsp_inlay_hints — needs fake-lsp extension: fake-lsp returns []
// for all textDocument/inlayHint requests. Add `inlay_hints` config
// field (HashMap<path, Vec<Value>>) to unlock.
// SKIP lsp_progress — timer-adjacent and needs fake-lsp extension
// (progress begin→end fires in one message, so spinner is effectively
// instantaneous). Out of scope for axis 3.
// SKIP lsp_error — needs fake-lsp extension: no current config path
// causes an LspIn::Error. Add `simulate_error` flag or point
// --test-lsp-server at /bin/false.

// === syntax ===

#[test]
fn syntax_buffer_parsed() {
    run_scenario(&scenario_dir("driver_events/syntax/buffer_parsed"));
}

// === git ===

#[test]
fn git_file_statuses() {
    run_scenario(&scenario_dir("driver_events/git/file_statuses"));
}

// SKIP git_line_statuses — needs new mechanism: requires a real git
// commit + in-workspace edit to produce dirty line statuses. A script
// command `git-cmd <args...>` is flagged in the inventory as the
// unblocker.

// === gh_pr ===

#[test]
fn gh_pr_pr_loaded() {
    run_scenario(&scenario_dir("driver_events/gh_pr/pr_loaded"));
}

#[test]
fn gh_pr_no_pr() {
    run_scenario(&scenario_dir("driver_events/gh_pr/no_pr"));
}

// SKIP gh_pr_pr_unchanged — needs fake-gh extension: fake-gh lacks
// If-None-Match handling and a generic `api` subcommand, so 304 path
// never fires.
// SKIP gh_pr_gh_unavailable — needs new mechanism: runner always
// supplies --test-gh-binary pointing at fake-gh. Needs a
// `gh_binary_override = "/nonexistent"` setup flag.
// SKIP gh_pr_pr_errored — flagged "needs fake-gh extension" in the
// inventory.

// === file_search ===

#[test]
fn file_search_results() {
    run_scenario(&scenario_dir("driver_events/file_search/results"));
}

#[test]
fn file_search_replace_complete() {
    run_scenario(&scenario_dir("driver_events/file_search/replace_complete"));
}

// === clipboard ===

#[test]
fn clipboard_text() {
    run_scenario(&scenario_dir("driver_events/clipboard/text"));
}

// === config_file ===

#[test]
fn config_file_keys_loaded() {
    run_scenario(&scenario_dir("driver_events/config_file/keys_loaded"));
}

#[test]
fn config_file_theme_loaded() {
    run_scenario(&scenario_dir("driver_events/config_file/theme_loaded"));
}

// SKIP config_file_err_alert — needs new mechanism: a
// `[[config_file]] path = "...", contents = "garbage"` setup hook would
// let the runner seed malformed keys.toml / theme.toml before spawn.

// === workspace ===

#[test]
fn workspace_workspace() {
    run_scenario(&scenario_dir("driver_events/workspace/workspace"));
}

#[test]
fn workspace_watchers_ready() {
    run_scenario(&scenario_dir("driver_events/workspace/watchers_ready"));
}

#[test]
fn workspace_session_restored_none() {
    run_scenario(&scenario_dir("driver_events/workspace/session_restored_none"));
}

#[test]
fn workspace_session_saved() {
    run_scenario(&scenario_dir("driver_events/workspace/session_saved"));
}

#[test]
fn workspace_undo_flushed() {
    run_scenario(&scenario_dir("driver_events/workspace/undo_flushed"));
}

#[test]
fn workspace_workspace_changed() {
    run_scenario(&scenario_dir("driver_events/workspace/workspace_changed"));
}

// SKIP workspace_session_restored_some — needs sqlite session-DB
// pre-seed (via `[[session_buffer]]` in setup.toml, flagged in the
// inventory).
// SKIP workspace_sync_result_sync_entries — needs cross-instance sync
// infra: pre-seeded `[[undo_entry]]` rows plus a notify-dir write
// script command.
// SKIP workspace_sync_result_external_save — same as sync_entries.
// SKIP workspace_sync_result_no_change — race-outcome null-op, not a
// user-visible event (dropped with None in sync_of.rs).
// SKIP workspace_notify_event — needs new mechanism: a script command
// that writes to `<config_dir>/notify/<hash>`.
// SKIP workspace_git_changed — needs new mechanism: script command
// `git-cmd <args...>` to run git mid-test.
// SKIP workspace_resuming / workspace_pr_settle / workspace_remote_changed
// — all flagged as needing session-DB / git-cmd / notify-file infra.

// === terminal_in ===

// NO new scenarios for terminal_in: every golden in the suite drives
// `TerminalInput::Key` via the PTY, so axis 1 and axis 2 cover the Key
// variant exhaustively.
// SKIP terminal_in_resize — needs PTY ioctl in the runner (flagged).
// SKIP terminal_in_focus_gained — inert: no consumer anywhere in the
// model. Dropped silently by actions_of.rs.
// SKIP terminal_in_focus_lost — same as focus_gained; no consumer.
