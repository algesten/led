//! Axis 4: per-config-key goldens.
//!
//! One scenario per meaningfully-testable user-configurable setting. The
//! configurable surface is small: two TOML files (`keys.toml`,
//! `theme.toml`) plus a handful of CLI flags. Hot-reload is a no-op in
//! the current runtime, so only startup-time effects are observable.
//!
//! Seeding: `setup.toml`'s `[config]` section writes literal TOML to
//! `<config_dir>/{keys,theme}.toml` before led spawns. led REPLACES
//! bundled defaults wholesale (no merge), so every custom `keys.toml`
//! must include every default binding the scenario still depends on.
//!
//! Skipped categories (with reasons):
//!
//! * CLI flags — `--config-dir`, `--no-workspace`, `--golden-trace`,
//!   `--test-lsp-server`, `--test-gh-binary` are already wired by the
//!   runner for every scenario and don't need a dedicated axis-4 test.
//!   The remaining flags are infra-only and not user-facing behaviour:
//!     - `--reset-config`: exits after writing defaults; the normal
//!       run_scenario flow can't observe the post-reset state because
//!       led terminates before the PTY reader sees a steady frame.
//!     - `--keys-file`, `--keys-record`: replay/record harnesses for
//!       profiling and macro dumps. Not user-facing.
//!     - `--log-file`: diagnostic-only; writes to disk, no visible
//!       effect on the rendered frame.
//!
//! * Hardcoded "should be configurable" defaults — `tab_stop`,
//!   `side_panel_width`, `scroll_margin`, `gutter_width`,
//!   `ruler_column`, per-language LSP commands, git scan debounce, and
//!   the soft-wrap / auto-format toggles are all baked into
//!   `Dimensions::new` or the relevant crate. No TOML/CLI surface exists
//!   today, so they can't be varied from a scenario. See
//!   `docs/rewrite/POST-REWRITE-REVIEW.md` when one of them is promoted
//!   to a config knob.
//!
//! * Most theme keys produce no observable diff in
//!   `vt100::Screen::contents()` (which drops ANSI styling). One
//!   `theme_minimal_override` scenario exists as a smoke test that a
//!   full custom `theme.toml` deserialises and led keeps running.
//!   Per-key theme coverage is left to unit tests on the Theme loader.
//!
//! * `COLORTERM` env-var switching (truecolor vs 256-colour) would need
//!   a per-scenario env override plus a new builder method; the
//!   `OnceLock` cache in `ui::style` further complicates scoping.
//!   Skipped until the runner supports per-scenario env.

use led_goldens::{run_scenario, scenario_dir};

// === keys.toml ===

/// Rebind Ctrl-s directly to `save` (replacing its default
/// `in_buffer_search`), demonstrating that a scalar binding takes
/// precedence and that chord-prefix ordering isn't required.
#[test]
fn keys_remap_save() {
    run_scenario(&scenario_dir("config_keys/keys_remap_save"));
}

/// Rebind plain `j` / `k` to `move_down` / `move_up` (vi-style). Proves
/// that scalar printable-char bindings override the default
/// InsertChar path.
#[test]
fn keys_remap_movement() {
    run_scenario(&scenario_dir("config_keys/keys_remap_movement"));
}

/// Minimal `keys.toml` with an empty `[keys]` table. Locks down the
/// "no bindings → no chord-driven behaviour" semantic: Ctrl-a and
/// Ctrl-x Ctrl-s both become no-ops.
#[test]
fn keys_minimal() {
    run_scenario(&scenario_dir("config_keys/keys_minimal"));
}

/// Move the save chord prefix from Ctrl-x to Ctrl-y by placing the
/// sub-table at `[keys."ctrl+y"]`. Exercises chord-prefix dispatch on a
/// non-default root.
#[test]
fn keys_chord_prefix_remap() {
    run_scenario(&scenario_dir("config_keys/keys_chord_prefix_remap"));
}

/// Rebind browser-context movement to vi-style j/k. Demonstrates that
/// the `[browser]` section overrides `[keys]` only while the sidebar
/// is focused; in the main editor j still inserts the character.
#[test]
fn keys_browser_context() {
    run_scenario(&scenario_dir("config_keys/keys_browser_context"));
}

/// Rebind the case-sensitivity toggle in the project-search panel from
/// Alt-1 to Alt-c. Exercises the `[file_search]` context table.
#[test]
fn keys_file_search_context() {
    run_scenario(&scenario_dir("config_keys/keys_file_search_context"));
}

// === theme.toml ===

/// Load a fully-custom `theme.toml` covering every required section
/// with a simplified ANSI-only palette. Sanity check that the theme
/// pipeline accepts a user file and doesn't fall back to defaults via
/// an Alert. vt100 strips colour, so this is a smoke test of the load
/// path rather than per-key colour coverage.
#[test]
fn theme_minimal_override() {
    run_scenario(&scenario_dir("config_keys/theme_minimal_override"));
}
