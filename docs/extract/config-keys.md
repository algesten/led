# Config keys inventory

Scope: every user-configurable knob on `led`. This document is written to feed a
golden-scenario generator — each row should map to a scenario that exercises a
non-default value and snapshots the visible effect.

---

## Config files

led has exactly two config files plus a SQLite DB. All live under the
config directory, which defaults to `~/.config/led/` and can be overridden with
`--config-dir <DIR>`.

| File | Purpose | Optional | Fallback when missing |
|---|---|---|---|
| `keys.toml` | keybindings + context-specific bindings | yes | bundled `default_keys.toml` (crates/config-file/src/default_keys.toml) |
| `theme.toml` | colors and text styles | yes | bundled `default_theme.toml` (crates/config-file/src/default_theme.toml) |
| `db.sqlite` | session DB (tabs, cursors, undo). Not user-edited; listed for completeness. | yes | created on demand by `workspace::db::open_db` (crates/workspace/src/db.rs) |

The loader is generic: `ConfigFile<File: TomlFile>` in
`/Users/martin/dev/led/crates/config-file/src/lib.rs`. The `TomlFile` trait has
two associated items: `file_name()` (relative file name) and `default_toml()`
(bundled `include_str!`). `Theme` and `Keys` are the only two `TomlFile`
implementors today.

### Load / reload behaviour

- `read_file` calls `std::fs::read_to_string(<config_dir>/<file_name>)` and
  falls back to `default_toml()` on any error — a missing file is not an
  error, it silently uses defaults.
- A TOML parse error surfaces as an `Alert` (info severity, no hard-fail).
- `ConfigFileOut::Persist` exists in the enum but its handler is empty (no-op)
  — there is no write-back path today.
- **Hot-reload: no.** The driver re-reads only when `ConfigFileOut::ConfigDir`
  is pushed into it. In `derived.rs` this is derived from
  `workspace.config` / `startup.config_dir` and deduped, so it fires:
  - once on `WorkspaceState::Loading → Standalone`
  - once on `WorkspaceState::Loading → Loaded`
  - again if primary/secondary flips (`read_only` flag toggles)
  Editing `keys.toml`/`theme.toml` at runtime does not trigger a reload. A
  restart (or a workspace state transition) is required.
- The `read_only` flag on `ConfigDir` is plumbed but not currently consulted
  by the reader (it is set on standalone + secondary instances).

### Goldens hint

The runner (`goldens/src/lib.rs`) already creates a per-scenario `config_dir`
tempdir and passes `--config-dir <that dir>` to `led`. To exercise a non-default
`keys.toml` or `theme.toml` today:

1. Extend `setup.toml` with a `[config]` section (or a pair of fields like
   `keys_toml = "..."` / `theme_toml = "..."`).
2. In `GoldenRunnerBuilder::spawn`, write those contents to
   `config_dir.join("keys.toml")` / `config_dir.join("theme.toml")` before
   spawning — exactly analogous to how `.fake-lsp.json` is materialised into
   the workspace dir.
3. No code change needed to `led` itself — the driver already reads these
   paths.

---

### `<config-dir>/keys.toml`

- Loader: `ConfigFile<Keys>` driver (`crates/config-file/src/lib.rs`, impl at
  line 104).
- Schema type: `struct Keys` at `/Users/martin/dev/led/crates/core/src/keys.rs:134`.
- Into runtime: `Keys::into_keymap()` at `crates/core/src/keys.rs:149`.
- Default: bundled `default_keys.toml` (crates/config-file/src/default_keys.toml).
- Hot-reload: no (see above).

#### Schema shape

Three top-level tables, all optional (empty `HashMap`s if absent):

```toml
[keys]            # global bindings: "<chord>" = "<action>"
[browser]         # context overrides active when the file browser is focused
[file_search]     # context overrides active when the project-search panel is focused

# Chord prefix: a sub-table, not a direct action. The prefix binding itself
# must NOT appear under [keys] as a scalar — it's inferred from the sub-table.
[keys."<prefix-chord>"]
"<second-chord>" = "<action>"
```

Value forms (`enum KeyBinding` in keys.rs):

- Scalar string → direct action binding. E.g. `"ctrl+a" = "line_start"`.
- Inline/sub table (`HashMap<String, String>`) → chord-prefix. The prefix key
  emits `KeymapLookup::ChordPrefix` on first press; the second key is looked
  up in the sub-table. E.g. `[keys."ctrl+x"]` → `"ctrl+c" = "quit"`.

#### Chord syntax (`parse_key_combo`, keys.rs:201)

`modifier+modifier+...+key`, lowercase, `+`-delimited. Order is irrelevant —
the parser just splits on `+`, walks all but the last token as modifiers.

- Modifiers: `ctrl`, `alt`, `shift`.
- Named keys: `up`, `down`, `left`, `right`, `home`, `end`, `pageup`,
  `pagedown`, `enter`, `backspace`, `delete`, `tab`, `esc`/`escape`, `space`.
- Any single character: `a`, `/`, `_`, `<`, `>`, `(`, `)`, `1`, etc.
- F-keys are unsupported in TOML today (the parser has no entry, and
  `format_key_combo` returns `None` for them).

`KeyCombo::from_key_event` strips SHIFT on `KeyCode::Char(_)` — the capital
letter is already the payload, so `shift+a` would never be observed. Shift is
only meaningful on non-char keys (`shift+tab`, `shift+enter`, etc.).

#### Action identifiers

Actions are the `Action` enum at `/Users/martin/dev/led/crates/core/src/lib.rs:223`,
deserialized with `serde(rename_all = "snake_case")`. Examples:
`line_start`, `line_end`, `move_up`, `move_down`, `page_up`, `page_down`,
`file_start`, `file_end`, `insert_newline`, `delete_backward`, `delete_forward`,
`insert_tab`, `kill_line`, `save`, `save_as`, `save_force`, `save_no_format`,
`save_all`, `kill_buffer`, `prev_tab`, `next_tab`, `jump_back`,
`jump_forward`, `outline`, `match_bracket`, `in_buffer_search`,
`open_file_search`, `close_file_search`, `toggle_search_case`,
`toggle_search_regex`, `toggle_search_replace`, `replace_all`, `find_file`,
`undo`, `redo`, `set_mark`, `kill_region`, `yank`, `sort_imports`,
`reflow_paragraph`, `lsp_goto_definition`, `lsp_rename`, `lsp_code_action`,
`lsp_format`, `next_issue`, `prev_issue`, `lsp_toggle_inlay_hints`,
`toggle_focus`, `toggle_side_panel`, `expand_dir`, `collapse_dir`,
`collapse_all`, `open_selected`, `open_selected_bg`, `open_messages`,
`open_pr_url`, `abort`, `kbd_macro_start`, `kbd_macro_end`,
`kbd_macro_execute`, `quit`, `suspend`.

Actions with payloads (`InsertChar(char)`, `Wait(u64)`, `Resize(u16, u16)`)
exist in the enum but are not user-bindable from TOML (they require data, not
a plain identifier).

#### Resolution order

`Keymap::lookup`:

1. If a context is active (`browser` or `file_search`), try that context table.
2. Otherwise / on miss, try the global `[keys]` direct map.
3. Otherwise, if the chord is a known prefix, return `ChordPrefix` (await
   second chord).
4. Otherwise `Unbound`.

#### Goldens hint

Writing a user `keys.toml` completely replaces the bundled defaults — the
loader does not merge. To cover e.g. "rebind `ctrl+a` to something other than
`line_start`" the scenario must include every default binding it still needs,
or only test key paths that happen not to depend on the removed defaults.
Consider a minimal `keys.toml` + a single rebound key as the cleanest shape.

---

### `<config-dir>/theme.toml`

- Loader: `ConfigFile<Theme>` driver (impl in `config-file/src/lib.rs:94`).
- Schema type: `struct Theme` at `/Users/martin/dev/led/crates/core/src/theme.rs:107`.
- Default: bundled `default_theme.toml` (crates/config-file/src/default_theme.toml).
- Hot-reload: no.

#### Value grammar (`enum StyleValue`)

Every field is either:

- A scalar string (fg color), e.g. `"$accent"`, `"ansi_red"`, `"#ff8800"`,
  `"term_reset"`.
- An inline table: `{ fg = "...", bg = "...", bold = true, italic = true }`.
  All four fields optional; `bold`/`italic` default to `false`.

String color grammar (resolved in `crates/ui/src/style.rs`):

- Named ANSI: `ansi_black`, `ansi_red`, `ansi_green`, `ansi_yellow`,
  `ansi_blue`, `ansi_magenta`, `ansi_cyan`, `ansi_white`,
  `ansi_bright_black` (alias `ansi_gray`), `ansi_bright_red`, …,
  `ansi_bright_white`.
- `term_reset` — inherit terminal default fg/bg.
- Hex: `#rgb` or `#rrggbb`.
- `$name` — reference into the `[COLORS]` table below. References may chain
  (e.g. `theme_dark = "$x024"`, `accent = "$theme_bright"`).

Truecolor hex is downgraded to the 256-cube when `COLORTERM` is not
`truecolor`/`24bit` (`supports_truecolor` in style.rs:184 — an environment
variable, see below).

#### `[COLORS]` (required)

Free-form palette: `HashMap<String, StyleValue>`. Any name here can be
referenced as `$name` in every other section. The bundled default defines:

- Named ANSI aliases (`black`, `red`, `green`, `yellow`, `blue`, `magenta`,
  `cyan`, `white`, and `bright_*` variants).
- The full xterm 256-cube `x016` through `x255` (mapping to hex).
- Semantic aliases: `normal`, `accent`, `muted`, `bold`, `inverse_bold`,
  `inverse_active`, `inverse_inactive`, `inverse_inactive2`, `selected`,
  `selected_2nd`, `neutral`.
- Syntax helpers: `syntax_keyword`, `syntax_type`, `syntax_string`,
  `syntax_number`, `syntax_comment`, `syntax_attribute`, `syntax_tag`,
  `syntax_label`.

These names aren't schema-fixed — they're just what the default theme happens
to define. A custom `theme.toml` may define any names as long as every `$ref`
resolves.

#### Required top-level sections

The `Theme` struct has required sections (`serde` without `#[serde(default)]`).
Omitting any of these will fail deserialization.

#### `[tabs]` (TabsTheme)

| Key | Type | Default | Effect |
|---|---|---|---|
| `active` | StyleValue | `$inverse_active` | Active regular tab cell |
| `inactive` | StyleValue | `$inverse_inactive` | Inactive regular tab cell |
| `preview_active` | StyleValue | `$inverse_active` | Active preview tab |
| `preview_inactive` | StyleValue | `$inverse_inactive` | Inactive preview tab |

#### `[status_bar]` (StatusBarTheme)

| Key | Type | Default | Effect |
|---|---|---|---|
| `style` | StyleValue | `{ fg = "$bold", bg = "$muted" }` | Bottom status bar |

#### `[editor]` (EditorTheme)

| Key | Type | Default | Effect |
|---|---|---|---|
| `text` | StyleValue | `$normal` | Base text cells |
| `gutter` | StyleValue | `$muted` | Line-number/gutter cells |
| `selection` | StyleValue | `$selected` | Mark-region highlight |
| `search_match` | StyleValue | `$selected_2nd` | In-buffer search non-current matches |
| `search_current` | StyleValue | `$inverse_bold` | Current in-buffer match |
| `file_search_match` | StyleValue | `$inverse_bold` | Editor-side highlight for project-search preview |
| `inlay_hint` | StyleValue? | `$bright_black` | LSP inlay hint style (optional; falls back to gutter style if omitted) |
| `ruler` | StyleValue? | `$x236` | Ruler column background (optional; `Dimensions::ruler_column` hardcoded to 110) |

#### `[browser]` (BrowserTheme)

| Key | Type | Default | Effect |
|---|---|---|---|
| `directory` | StyleValue | `$neutral` | Directory rows |
| `file` | StyleValue | `$normal` | File rows |
| `selected` | StyleValue | `$inverse_active` | Selected row (panel focused) |
| `selected_unfocused` | StyleValue | `$inverse_inactive2` | Selected row when panel unfocused |
| `border` | StyleValue | `$muted` | Vertical border between sidebar and editor |

#### `[file_search]` (FileSearchTheme — project search panel)

| Key | Type | Default | Effect |
|---|---|---|---|
| `border` | StyleValue | `$muted` | Panel border |
| `input` | StyleValue | `{ bg = "$bright_black" }` | Text-input field (focused) |
| `input_unfocused` | StyleValue | `{ bg = "$bright_black" }` | Text-input field (unfocused) |
| `toggle_on` | StyleValue | `$inverse_active` | Case/regex/replace toggle active |
| `toggle_off` | StyleValue | `$inverse_inactive` | Toggle inactive |
| `file_header` | StyleValue | `$accent` | File-group header row |
| `hit` | StyleValue | `$normal` | Result line base |
| `match` | StyleValue (TOML key `match`) | `{ fg = "$bright_yellow", bold = true }` | Matched span inside result |
| `selected` | StyleValue | `$inverse_active` | Currently selected result (focused) |
| `selected_unfocused` | StyleValue | `$inverse_inactive` | Selected result (unfocused) |
| `search_current` | StyleValue | `$inverse_bold` | Current match in preview |

#### `[diagnostics]` (DiagnosticsTheme)

| Key | Type | Default | Effect |
|---|---|---|---|
| `error` | StyleValue | `$x196` | Error squiggle + gutter |
| `warning` | StyleValue | `$x178` | Warning squiggle + gutter |
| `info` | StyleValue | `$x033` | Info squiggle + gutter |
| `hint` | StyleValue | `$x245` | Hint squiggle + gutter |

#### `[git]` (GitTheme)

| Key | Type | Default | Effect |
|---|---|---|---|
| `modified` | StyleValue | `$x172` | Modified-file badge in browser |
| `added` | StyleValue | `$x034` | Added-file badge |
| `untracked` | StyleValue | `$x022` | Untracked-file badge |
| `gutter_added` | StyleValue | `$x034` | Added-line gutter marker |
| `gutter_modified` | StyleValue | `$x172` | Modified-line gutter marker |

#### `[brackets]` (BracketsTheme)

| Key | Type | Default | Effect |
|---|---|---|---|
| `match` (TOML key `match`) | StyleValue | `{ fg = "$bright_yellow", bold = true }` | Highlighted matching bracket pair |
| `rainbow_0` | StyleValue | `$x033` | Rainbow depth 0 |
| `rainbow_1` | StyleValue | `$x170` | Rainbow depth 1 |
| `rainbow_2` | StyleValue | `$x034` | Rainbow depth 2 |
| `rainbow_3` | StyleValue | `$x172` | Rainbow depth 3 |
| `rainbow_4` | StyleValue | `$x069` | Rainbow depth 4 |
| `rainbow_5` | StyleValue | `$x135` | Rainbow depth 5 (wraps back via modulo) |

#### `[pr]` (PrTheme — optional)

Entirely optional — if the `[pr]` table is omitted the UI falls back to
diagnostic-free styling for PR views.

| Key | Type | Default | Effect |
|---|---|---|---|
| `diff` | StyleValue | `$x245` | PR diff line style |
| `comment` | StyleValue | `$x205` | PR review comment style |
| `gutter_diff` | StyleValue | `$x245` | PR diff gutter |
| `gutter_comment` | StyleValue | `$x205` | PR comment gutter |

#### `[syntax]`

Free-form `HashMap<String, StyleValue>` keyed by tree-sitter highlight
capture name. The default covers the common Helix/Zed-style captures:

- `keyword`, `function`, `module`, `conditional`, `include`, `repeat`,
  `exception` → `$syntax_keyword`
- `type`, `type.builtin`, `constructor` → `$syntax_type`
- `string`, `string.regex`, `text.literal` → `$syntax_string`
- `number`, `boolean`, `constant`, `constant.builtin`, `escape`,
  `string.special` → `$syntax_number`
- `comment` → `$syntax_comment`
- `operator`, `punctuation`, `variable` → `$normal`
- `variable.builtin`, `variable.parameter`, `variable.member`, `property`,
  `attribute`, `text.reference` → `$syntax_attribute`
- `tag` → `$syntax_tag`
- `label`, `embedded`, `text.title`, `text.strong`, `text.emphasis`,
  `text.uri` → label/bold variants

Goldens hint: a custom palette is a one-line change per capture — useful for
tests that verify the syntax pipeline wires through to the final cell color.

#### Goldens hint (theme)

A user `theme.toml` must provide **every required section** (`tabs`,
`status_bar`, `editor`, `browser`, `file_search`, `diagnostics`, `git`,
`brackets`, plus `COLORS` and `syntax`). Missing a required section fails
deserialization and you fall through to defaults. The simplest non-default
scenario is: copy the bundled default and change a handful of specific keys.

---

## CLI flags (`led/src/main.rs`)

| Flag | Arg | Default | Effect | Source |
|---|---|---|---|---|
| positional | `<paths...>` | (empty) | Files/dirs to open. A single dir opens the browser rooted there; a file list opens those files. | main.rs:20 |
| `--log-file` | `FILE` | off | Writes debug log to a file (`led::logging::init_file_logger`). | main.rs:24 |
| `--reset-config` | (bool) | false | Overwrites `keys.toml`/`theme.toml` with defaults, deletes `db.sqlite`, then exits. Intended for users in a corrupted state. | main.rs:28 |
| `--no-workspace` | (bool) | false | Standalone mode: no workspace/git/LSP/session/watchers. Browser rooted at CWD. For `EDITOR=` use. Passed into `Startup.no_workspace`. | main.rs:33 |
| `--keys-file` | `FILE` | off | Replays a list of chord strings (one per line, plus optional `goto <line>`) into terminal input. For profiling/benchmark; starts after a 3s sleep to let startup settle. | main.rs:37 |
| `--keys-record` | `FILE` | off | Appends every real key press (via `format_key_combo`) to a file. Format round-trips with `--keys-file`. | main.rs:41 |
| `--golden-trace` | `FILE` | off | Appends the normalized dispatch trace to a file. Used exclusively by the goldens runner. | main.rs:45 |
| `--config-dir` | `DIR` | `~/.config/led` (via `dirs::home_dir()`) | Override the config/state dir. Used by tests to isolate `db.sqlite` and config files. Goldens runner already sets this. | main.rs:51 |
| `--test-lsp-server` | `PATH` | off (hidden flag) | Replace the LSP server binary for ALL languages with this one. Test-only; used with `fake-lsp`. | main.rs:55 |
| `--test-gh-binary` | `PATH` | off (hidden flag) | Replace the `gh` CLI with this binary. Test-only; used with `fake-gh`. | main.rs:59 |

Goldens hint: `--config-dir`, `--no-workspace`, `--golden-trace`,
`--test-lsp-server`, `--test-gh-binary` are already wired in
`goldens/src/lib.rs`. To exercise a non-default CLI-flag-level setting in a
scenario, add a `Setup` field in `scenario.rs` and flip the corresponding
builder method.

Planned / referenced but not yet implemented:

- `--test-clock` — referenced in `docs/rewrite/GOLDENS-PLAN.md:279` to enable
  a virtual clock that would make time-dependent traces deterministic. Not in
  the Cli struct today; the goldens runner compensates with real `sleep` +
  quiescence detection (`settle()` in goldens/src/lib.rs:301).

---

## Environment variables

| Variable | Read at | Default | Effect |
|---|---|---|---|
| `HOME` (via `dirs::home_dir()`) | main.rs:169 | — | Resolves `~/.config/led/` when `--config-dir` is absent. Override for isolation. |
| `COLORTERM` | crates/ui/src/style.rs:188 | unset | When set to `truecolor` or `24bit`, hex colors are emitted as 24-bit RGB escapes; otherwise all colors are approximated into the 256-color cube. Read once (cached in `OnceLock`). Affects terminal output bytes, so every themed scenario implicitly depends on this being a specific value. |
| `UPDATE_GOLDENS` | goldens/src/lib.rs:375 | unset | When set, snapshot-diff rewrites the golden file instead of asserting. Test-harness-only; not a `led` config. |
| `TERM` | goldens/src/lib.rs:183 | `xterm-256color` | Set by the goldens runner on `led`'s child env — pins terminal capabilities. |
| (none) | — | — | led itself reads no other environment variables. No `LED_*` prefix exists (despite `docs/rewrite/SPEC-PLAN.md:106` mentioning `$LED_CONFIG_DIR`; that was a design note — only `--config-dir` is implemented). |

Goldens hint for `COLORTERM`: all current goldens run without it set (256-color
path). If a scenario wants to test truecolor output, spawn with
`cmd.env("COLORTERM", "truecolor")`. Today the runner's child inherits
whatever the test harness exports.

---

## Built-in defaults (compiled-in, not configurable via TOML today)

Source: `Dimensions::new` at `/Users/martin/dev/led/crates/state/src/lib.rs:234`.

| Setting | Default | Source | Notes |
|---|---|---|---|
| `side_panel_width` | 25 cols | Dimensions::new | Width reserved for the file browser when shown. |
| `min_editor_width` | 25 cols | Dimensions::new | Side panel auto-hides below this. |
| `status_bar_height` | 1 | Dimensions::new | |
| `tab_bar_height` | 1 | Dimensions::new | |
| `gutter_width` | 2 cols | Dimensions::new | Gutter between line-number margin and text. |
| `scroll_margin` | 3 rows | Dimensions::new | Vertical padding when cursor moves near viewport edge. |
| `tab_stop` | 4 spaces | Dimensions::new | Soft-tab expansion width (`edit::insert_soft_tab`). |
| `ruler_column` | Some(110) | Dimensions::new | Column where the subtle vertical ruler is drawn (themeable via `editor.ruler`, but position is hardcoded). |
| Initial viewport | 80×24 for goldens | `GoldenRunnerBuilder::new` viewport field | `Setup::TerminalSetup` allows override via `[terminal] cols=.. rows=..`; scenario default in `parse_script` test is 80×24 but `TerminalSetup::default` now returns 120×40 (goldens/src/scenario.rs:49). |
| Standalone browser root | process CWD | main.rs:132 | Only relevant under `--no-workspace`. |
| Auto-close buffer limit | (README mentions "auto-close buffers to prevent resource exhaustion"; value not surfaced) | — | No TOML key. |
| Language detection priority | extension → filename → modeline (first 5 lines) | README.md:84 | Not configurable. |
| Modeline recognized modes | `rust`, `python`, `javascript`/`js`, `typescript`/`ts`, `tsx`, `json`, `toml`, `markdown`/`md`, `bash`/`sh`, `c`, `cpp`/`c++`, `swift`, `make`, `ruby` | README.md:97 | Hardcoded in syntax crate. |
| LSP server commands per language | rust-analyzer, typescript-language-server, pyright, clangd, sourcekit-lsp, taplo, vscode-json-language-server, bash-language-server | README.md:22 | Hardcoded. The only override is `--test-lsp-server` (global, test-only). |
| Git scan debounce | 500 ms | docs/rewrite/SPEC-PLAN.md:197 | Fixed; not surfaced in any TOML. |
| Settle timeout (goldens only) | 120 ms quiet / 40 ms min / 15 s max | goldens/src/lib.rs:302 | Runner knob, not a `led` config. |
| Undo flush timer name | `"undo_flush"` | model/mod.rs:386 | Timer-level detail; not user-visible. |
| `--keys-file` initial sleep | 3 s | main.rs:257 | Hardcoded warm-up delay before replaying. |

Goldens hint: none of these are TOML-addressable today. If a scenario needs a
non-default (say `tab_stop = 2`), the options are:
- Land a patch that moves the field into a new `[editor]` TOML section plus a
  scenario-time override; or
- Set it via a future CLI flag; or
- Accept that this axis isn't testable until such infra exists.

---

## Summary: what the goldens runner can exercise today

Already wired end-to-end:

- Viewport size (`[terminal] cols/rows` in setup.toml).
- Workspace on/off (`no_workspace`).
- Git root presence (`git_init`).
- Fake LSP / fake gh (`fake_lsp`, `fake_gh`).
- Initial files (`[[file]]`).

Trivially addable with a small builder + `setup.toml` extension — just write
the string into `<config_dir>/<file_name>` before spawn:

- Custom `keys.toml` (full or minimal) — exercises `--config-dir` wire-up that
  is already present.
- Custom `theme.toml` — same pattern.

Requires new led-side infra:

- Any of the `Dimensions::new` defaults (tab_stop, side_panel_width,
  scroll_margin, ruler_column, gutter_width, …).
- Hot-reload of keys/theme while running.
- LSP server command per language (today only a global `--test-lsp-server`).
- Soft-wrap toggle, auto-format toggle, and similar "obvious editor
  preferences" — currently always on.
- `COLORTERM` handling per scenario (the OnceLock caches the first read).

---

## Findings

### Not documented anywhere user-facing

- `--keys-file` and `--keys-record` — undocumented in README; only reachable
  via `--help`. They are user-visible (not `hide = true`), so they count as
  configurable surface area.
- `--log-file` accepts any path; no max size, rotation, or structured format
  guarantee documented.
- `theme.toml`'s `[pr]` section is optional; the README doesn't mention it at
  all.
- `[browser]` and `[file_search]` context sections in `keys.toml` are
  mentioned in README, but the full action list specific to those contexts
  (`expand_dir`, `collapse_dir`, `collapse_all`, `open_selected`,
  `open_selected_bg`, `toggle_search_case`, `toggle_search_regex`,
  `toggle_search_replace`, `replace_all`, `close_file_search`) is only in the
  Action enum.
- `Dimensions::ruler_column = Some(110)` is a hardcoded vertical ruler at
  column 110. The theme lets you style it but not move or disable it from
  config.

### Looks configurable but is hardcoded

- `tab_stop` (soft-tab expansion) is a `Dimensions` field but initialized from
  `Dimensions::new` — no TOML/flag surface. Real editors almost always expose
  this.
- `scroll_margin` (3 rows of cursor-to-edge padding) — hardcoded.
- `side_panel_width` (25 cols) — hardcoded; the README features a sidebar
  but the width isn't a setting.
- LSP server binaries per language are hardcoded; there's no
  `[lsp.rust] cmd = "..."` table.
- Language detection heuristics (extension list, modeline modes) are
  hardcoded in the syntax crate.
- Git scan debounce (500 ms) is fixed.
- Auto-close-old-buffers threshold is fixed (and not even surfaced in any
  visible struct I could find from the README claim).

### Surprising defaults

- `~/.config/led` is not overridable via env var today, only via `--config-dir`.
  The spec plan references `$LED_CONFIG_DIR` but that was never implemented.
- `ConfigDir.read_only` is threaded through but ignored by the reader — it is
  *not* enforced, only set. A future writeback path (`Persist`) would need to
  consult it.
- `ConfigFileOut::Persist` exists but its handler is an empty match arm —
  nothing persists config, ever. Reset is the only write path
  (`--reset-config`, writes full defaults over the user's file).
- The `Keys` TOML loader completely overwrites the defaults — there is no
  merge layer. A user who defines two bindings in `keys.toml` loses *every
  other* default binding. This is a usability footgun worth flagging when
  generating goldens ("custom keys → also includes the bindings we rely on").
- Truecolor output depends on `COLORTERM` being set at process start and is
  cached in a `OnceLock`. Goldens that want to pin this to 256-color mode
  should either unset it in the runner or set it explicitly — today the
  child inherits the test process env, which can drift between CI and dev.
- `Keys::from_key_event` strips SHIFT on character keys. Any attempt to bind
  `shift+a` in `keys.toml` will parse and be stored in the keymap but will
  never match, because the lookup event always has `shift = false` when the
  code is `KeyCode::Char(_)`. Worth surfacing as a lint/warning.
- F-keys (`F1`, `F2`, …) are present in `crossterm::KeyCode::F(_)` but
  unsupported by both `parse_key_combo` and `format_key_combo`. Binding
  `f5 = "..."` in `keys.toml` fails to parse (alert-logged, not fatal).

### Blocked on missing infra for goldens today

- No way to set `keys.toml`/`theme.toml` contents from `setup.toml`. Builder
  methods exist only for files, fake-lsp, fake-gh. Add one `[config]` table
  with `keys`/`theme` string fields plus matching builder methods — this is
  the minimum work needed to unlock both entire axes (keybindings and theme)
  in a data-driven way.
- No virtual clock (`--test-clock`). Any scenario whose dispatch depends on
  debounced async work (git scan, LSP pull diagnostics, undo flush timer)
  currently snapshots real wall-clock behaviour via the `settle()` quiescence
  detector. This limits how deterministically the per-config-key axis can
  cover timing-related settings.
- No runtime reload. A golden can't exercise "user edits keys.toml mid-run
  and new binding takes effect" — the driver does not re-read in place.
