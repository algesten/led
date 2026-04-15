//! Auto-authored per-keybinding goldens. One test per chord per context.
//! See goldens/scenarios/keybindings/<context>/<chord>/setup.toml + script.txt.
//!
//! Skipped chords (with reasons):
//!
//!  * Dead actions (parsed but no handler):
//!      - main:    Alt-o   (outline)
//!      - ctrl_h:  Ctrl-h e (open_messages)  → entire context has no live chords
//!      - browser: Alt-Enter (open_selected_bg)
//!
//!  * Unsupported by the runner's chord → bytes encoder (see
//!    goldens/src/keys.rs): Ctrl-on-named and Ctrl-on-non-letter
//!    chords are rejected. This excludes:
//!      - Ctrl-Home, Ctrl-End, Ctrl-Left, Ctrl-Right
//!      - Ctrl-Space
//!      - Ctrl-/, Ctrl-_, Ctrl-7 (undo aliases)
//!    Their bindings live in the inventory but cannot be sent through
//!    the PTY at this layer. Covered indirectly by aliased chords
//!    (Home/End/Alt-</> for navigation; there is no alternate chord for
//!    undo at main level — Undo is currently only reachable via these
//!    skipped aliases in default_keys.toml).
//!
//!  * kbd_macro digit chords Ctrl-x 0..9 are skipped: they only
//!    accumulate a count consumed by the next macro execute; per-chord
//!    coverage adds no signal at this axis.
//!
//!  * main Ctrl-z (suspend) is skipped: raises SIGTSTP which hangs the
//!    led child inside the PTY.

use led_goldens::{run_scenario, scenario_dir};

// === main context ===

#[test]
fn main_ctrl_a() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_a"));
}
#[test]
fn main_ctrl_e() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_e"));
}
#[test]
fn main_ctrl_d() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_d"));
}
#[test]
fn main_ctrl_k() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_k"));
}
#[test]
fn main_up() {
    run_scenario(&scenario_dir("keybindings/main/up"));
}
#[test]
fn main_down() {
    run_scenario(&scenario_dir("keybindings/main/down"));
}
#[test]
fn main_left() {
    run_scenario(&scenario_dir("keybindings/main/left"));
}
#[test]
fn main_right() {
    run_scenario(&scenario_dir("keybindings/main/right"));
}
#[test]
fn main_home() {
    run_scenario(&scenario_dir("keybindings/main/home"));
}
#[test]
fn main_end() {
    run_scenario(&scenario_dir("keybindings/main/end"));
}
#[test]
fn main_page_up() {
    run_scenario(&scenario_dir("keybindings/main/page_up"));
}
#[test]
fn main_page_down() {
    run_scenario(&scenario_dir("keybindings/main/page_down"));
}
#[test]
fn main_ctrl_home() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_home"));
}
#[test]
fn main_ctrl_end() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_end"));
}
#[test]
fn main_enter() {
    run_scenario(&scenario_dir("keybindings/main/enter"));
}
#[test]
fn main_backspace() {
    run_scenario(&scenario_dir("keybindings/main/backspace"));
}
#[test]
fn main_delete() {
    run_scenario(&scenario_dir("keybindings/main/delete"));
}
#[test]
fn main_tab() {
    run_scenario(&scenario_dir("keybindings/main/tab"));
}
#[test]
fn main_ctrl_f() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_f"));
}
#[test]
fn main_ctrl_v() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_v"));
}
#[test]
fn main_alt_v() {
    run_scenario(&scenario_dir("keybindings/main/alt_v"));
}
#[test]
fn main_alt_tab() {
    run_scenario(&scenario_dir("keybindings/main/alt_tab"));
}
#[test]
fn main_ctrl_b() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_b"));
}
#[test]
fn main_ctrl_left() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_left"));
}
#[test]
fn main_ctrl_right() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_right"));
}
#[test]
fn main_ctrl_slash() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_slash"));
}
#[test]
fn main_ctrl_underscore() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_underscore"));
}
#[test]
fn main_ctrl_7() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_7"));
}
#[test]
fn main_ctrl_g() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_g"));
}
#[test]
fn main_esc() {
    run_scenario(&scenario_dir("keybindings/main/esc"));
}
// Ctrl-z (suspend) — skipped: raises SIGTSTP on the led process, which
// hangs the test since the PTY child stops and cannot resume itself
// without a SIGCONT from outside. Action covered at unit-test level.
#[test]
fn main_alt_lt() {
    run_scenario(&scenario_dir("keybindings/main/alt_lt"));
}
#[test]
fn main_alt_gt() {
    run_scenario(&scenario_dir("keybindings/main/alt_gt"));
}
#[test]
fn main_ctrl_space() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_space"));
}
#[test]
fn main_ctrl_w() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_w"));
}
#[test]
fn main_ctrl_y() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_y"));
}
#[test]
fn main_ctrl_s() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_s"));
}
#[test]
fn main_alt_dot() {
    run_scenario(&scenario_dir("keybindings/main/alt_dot"));
}
#[test]
fn main_alt_comma() {
    run_scenario(&scenario_dir("keybindings/main/alt_comma"));
}
#[test]
fn main_alt_enter() {
    run_scenario(&scenario_dir("keybindings/main/alt_enter"));
}
#[test]
fn main_ctrl_r() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_r"));
}
#[test]
fn main_alt_i() {
    run_scenario(&scenario_dir("keybindings/main/alt_i"));
}
#[test]
fn main_ctrl_t() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_t"));
}
#[test]
fn main_alt_b() {
    run_scenario(&scenario_dir("keybindings/main/alt_b"));
}
#[test]
fn main_alt_left() {
    run_scenario(&scenario_dir("keybindings/main/alt_left"));
}
#[test]
fn main_alt_f() {
    run_scenario(&scenario_dir("keybindings/main/alt_f"));
}
#[test]
fn main_alt_right() {
    run_scenario(&scenario_dir("keybindings/main/alt_right"));
}
// Alt-o (outline) — skipped, dead action.
#[test]
fn main_alt_rbracket() {
    run_scenario(&scenario_dir("keybindings/main/alt_rbracket"));
}
#[test]
fn main_ctrl_q() {
    run_scenario(&scenario_dir("keybindings/main/ctrl_q"));
}

// === ctrl_x prefix ===

#[test]
fn ctrl_x_ctrl_c() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/ctrl_c"));
}
#[test]
fn ctrl_x_ctrl_s() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/ctrl_s"));
}
#[test]
fn ctrl_x_ctrl_a() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/ctrl_a"));
}
#[test]
fn ctrl_x_ctrl_d() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/ctrl_d"));
}
#[test]
fn ctrl_x_ctrl_w() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/ctrl_w"));
}
#[test]
fn ctrl_x_k() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/k"));
}
#[test]
fn ctrl_x_ctrl_f() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/ctrl_f"));
}
#[test]
fn ctrl_x_i() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/i"));
}
#[test]
fn ctrl_x_lparen() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/lparen"));
}
#[test]
fn ctrl_x_rparen() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/rparen"));
}
#[test]
fn ctrl_x_e() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/e"));
}
#[test]
fn ctrl_x_ctrl_p() {
    run_scenario(&scenario_dir("keybindings/ctrl_x/ctrl_p"));
}
// Ctrl-x 0..9 (count accumulators) — skipped; covered by macro execute.

// === ctrl_h prefix ===
// Ctrl-h e (open_messages) — skipped, dead action. No other bindings in this
// context, so the entire ctrl_h context has zero scenarios.

// === browser (sidebar focus) context ===

#[test]
fn browser_left() {
    run_scenario(&scenario_dir("keybindings/browser/left"));
}
#[test]
fn browser_right() {
    run_scenario(&scenario_dir("keybindings/browser/right"));
}
#[test]
fn browser_enter() {
    run_scenario(&scenario_dir("keybindings/browser/enter"));
}
// Alt-Enter (open_selected_bg) — skipped, dead action.
#[test]
fn browser_ctrl_q() {
    run_scenario(&scenario_dir("keybindings/browser/ctrl_q"));
}

// === file_search overlay ===

#[test]
fn file_search_alt_1() {
    run_scenario(&scenario_dir("keybindings/file_search/alt_1"));
}
#[test]
fn file_search_alt_2() {
    run_scenario(&scenario_dir("keybindings/file_search/alt_2"));
}
#[test]
fn file_search_alt_3() {
    run_scenario(&scenario_dir("keybindings/file_search/alt_3"));
}
#[test]
fn file_search_enter() {
    run_scenario(&scenario_dir("keybindings/file_search/enter"));
}
#[test]
fn file_search_alt_enter() {
    run_scenario(&scenario_dir("keybindings/file_search/alt_enter"));
}

// === find_file overlay (Ctrl-x Ctrl-f) ===

#[test]
fn find_file_char_a() {
    run_scenario(&scenario_dir("keybindings/find_file/char_a"));
}
#[test]
fn find_file_backspace() {
    run_scenario(&scenario_dir("keybindings/find_file/backspace"));
}
#[test]
fn find_file_delete() {
    run_scenario(&scenario_dir("keybindings/find_file/delete"));
}
#[test]
fn find_file_tab() {
    run_scenario(&scenario_dir("keybindings/find_file/tab"));
}
#[test]
fn find_file_enter() {
    run_scenario(&scenario_dir("keybindings/find_file/enter"));
}
#[test]
fn find_file_up() {
    run_scenario(&scenario_dir("keybindings/find_file/up"));
}
#[test]
fn find_file_down() {
    run_scenario(&scenario_dir("keybindings/find_file/down"));
}
#[test]
fn find_file_left() {
    run_scenario(&scenario_dir("keybindings/find_file/left"));
}
#[test]
fn find_file_right() {
    run_scenario(&scenario_dir("keybindings/find_file/right"));
}
#[test]
fn find_file_ctrl_a() {
    run_scenario(&scenario_dir("keybindings/find_file/ctrl_a"));
}
#[test]
fn find_file_home() {
    run_scenario(&scenario_dir("keybindings/find_file/home"));
}
#[test]
fn find_file_ctrl_e() {
    run_scenario(&scenario_dir("keybindings/find_file/ctrl_e"));
}
#[test]
fn find_file_end() {
    run_scenario(&scenario_dir("keybindings/find_file/end"));
}
#[test]
fn find_file_ctrl_k() {
    run_scenario(&scenario_dir("keybindings/find_file/ctrl_k"));
}
#[test]
fn find_file_esc() {
    run_scenario(&scenario_dir("keybindings/find_file/esc"));
}
#[test]
fn find_file_ctrl_g() {
    run_scenario(&scenario_dir("keybindings/find_file/ctrl_g"));
}

// === isearch overlay (Ctrl-s) ===

#[test]
fn isearch_char_a() {
    run_scenario(&scenario_dir("keybindings/isearch/char_a"));
}
#[test]
fn isearch_backspace() {
    run_scenario(&scenario_dir("keybindings/isearch/backspace"));
}
#[test]
fn isearch_enter() {
    run_scenario(&scenario_dir("keybindings/isearch/enter"));
}
#[test]
fn isearch_esc() {
    run_scenario(&scenario_dir("keybindings/isearch/esc"));
}
#[test]
fn isearch_ctrl_s() {
    run_scenario(&scenario_dir("keybindings/isearch/ctrl_s"));
}
#[test]
fn isearch_ctrl_g() {
    run_scenario(&scenario_dir("keybindings/isearch/ctrl_g"));
}
#[test]
fn isearch_up() {
    run_scenario(&scenario_dir("keybindings/isearch/up"));
}

// === lsp_rename overlay (Ctrl-r) ===

#[test]
fn lsp_rename_char_a() {
    run_scenario(&scenario_dir("keybindings/lsp_rename/char_a"));
}
#[test]
fn lsp_rename_backspace() {
    run_scenario(&scenario_dir("keybindings/lsp_rename/backspace"));
}
#[test]
fn lsp_rename_enter() {
    run_scenario(&scenario_dir("keybindings/lsp_rename/enter"));
}
#[test]
fn lsp_rename_esc() {
    run_scenario(&scenario_dir("keybindings/lsp_rename/esc"));
}

// === lsp_code_actions overlay (Alt-i) ===

#[test]
fn lsp_code_actions_up() {
    run_scenario(&scenario_dir("keybindings/lsp_code_actions/up"));
}
#[test]
fn lsp_code_actions_down() {
    run_scenario(&scenario_dir("keybindings/lsp_code_actions/down"));
}
#[test]
fn lsp_code_actions_enter() {
    run_scenario(&scenario_dir("keybindings/lsp_code_actions/enter"));
}
#[test]
fn lsp_code_actions_esc() {
    run_scenario(&scenario_dir("keybindings/lsp_code_actions/esc"));
}

// === lsp_completion popup ===

#[test]
fn lsp_completion_up() {
    run_scenario(&scenario_dir("keybindings/lsp_completion/up"));
}
#[test]
fn lsp_completion_down() {
    run_scenario(&scenario_dir("keybindings/lsp_completion/down"));
}
#[test]
fn lsp_completion_enter() {
    run_scenario(&scenario_dir("keybindings/lsp_completion/enter"));
}
#[test]
fn lsp_completion_tab() {
    run_scenario(&scenario_dir("keybindings/lsp_completion/tab"));
}
#[test]
fn lsp_completion_esc() {
    run_scenario(&scenario_dir("keybindings/lsp_completion/esc"));
}
#[test]
fn lsp_completion_char_a() {
    run_scenario(&scenario_dir("keybindings/lsp_completion/char_a"));
}
#[test]
fn lsp_completion_backspace() {
    run_scenario(&scenario_dir("keybindings/lsp_completion/backspace"));
}

// === confirm_kill prompt ===

#[test]
fn confirm_kill_char_y() {
    run_scenario(&scenario_dir("keybindings/confirm_kill/char_y"));
}
#[test]
fn confirm_kill_char_y_upper() {
    run_scenario(&scenario_dir("keybindings/confirm_kill/char_y_upper"));
}
#[test]
fn confirm_kill_char_n() {
    run_scenario(&scenario_dir("keybindings/confirm_kill/char_n"));
}

// === kbd_macro repeat mode ===

#[test]
fn kbd_macro_e_replay() {
    run_scenario(&scenario_dir("keybindings/kbd_macro/e_replay"));
}
