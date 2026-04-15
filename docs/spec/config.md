# config

## Summary

led reads at most two user configuration files — `keys.toml` and `theme.toml` —
from a single config directory that defaults to `~/.config/led/`. Both files
are **optional**: if either is missing (or fails to parse), led silently falls
back to the compiled-in defaults bundled inside the `led-config-file` crate
(`crates/config-file/src/default_keys.toml` and `default_theme.toml`). A third
artifact, `db.sqlite`, lives in the same directory but is not user-edited — it
is the session/undo database, owned by the workspace driver.

A handful of behaviour-shaping values (tab stop, side-panel width, scroll
margin, ruler column, gutter width, LSP server binaries, git-scan debounce) are
*hardcoded today* and not surfaced through any TOML. They are listed below so
the rewrite can make a deliberate decision about which to expose.

Hot-reload is **not implemented**. `ConfigFileOut::Persist` exists in the driver
enum but its handler is a no-op; the file watcher infrastructure is wired but
the round-trip is never completed (see `docs/rewrite/POST-REWRITE-REVIEW.md`).
Editing `keys.toml` or `theme.toml` while led is running has zero effect —
you must restart, or cause a workspace state transition that re-triggers the
`ConfigDir` command.

## Behavior

### Config directory resolution

The directory is picked in this order:

1. `--config-dir <DIR>` CLI flag (explicit override, used by tests and power
   users).
2. `dirs::home_dir()/.config/led` — i.e. `~/.config/led/` on Unix.

There is no `LED_CONFIG_DIR` environment variable today despite an early design
note mentioning one (`docs/rewrite/SPEC-PLAN.md:106`). Only `--config-dir` is
wired (`led/src/main.rs:168-173`). The directory is created lazily — nothing
writes into it except `--reset-config` and the workspace driver (which creates
`db.sqlite` on demand).

### Load path — `ConfigFile<File: TomlFile>`

The loader is generic over a `TomlFile` trait (`crates/config-file/src/lib.rs:32`)
with two associated fns: `file_name()` (relative filename) and `default_toml()`
(bundled `include_str!` fallback). `Keys` and `Theme` are the only implementors.

For each config file the driver:

1. Joins `<config-dir>/<file_name>`.
2. `std::fs::read_to_string` the path.
3. **On any read error** (missing, permission denied, anything) it substitutes
   `default_toml()` as if the file had contained the defaults.
4. Parses the resulting string with `toml::from_str`. A parse failure
   surfaces as `Alert::Info` (status-bar level, *not* a hard error) and the
   previously-loaded config remains in effect.
5. Wraps the parsed value in `Arc` and pushes it into the result stream, where
   `model::mod.rs` stores it in `AppState.config_keys` / `config_theme`.

The driver listens for `ConfigFileOut::ConfigDir(ConfigDir)` on its input
stream and re-runs the load only when that command arrives. Since
`derived.rs` derives the emitted `ConfigDir` from the workspace state and
dedupes it, re-reads happen only when:

- The workspace transitions `Loading → Standalone` (initial load).
- The workspace transitions `Loading → Loaded`.
- The `read_only` flag toggles (primary/secondary instance flip).

File-system changes to `keys.toml` / `theme.toml` do **not** push a new
`ConfigDir`; there is no watch on those specific files. The `read_only` flag
is plumbed through but the reader ignores it today — nothing would enforce
"don't write" because nothing writes.

### `keys.toml` — schema high level

The full keymap schema lives in `keymap.md`; only the shape is repeated here.
`keys.toml` has up to three top-level tables, all optional:

```toml
[keys]            # global bindings + chord-prefix sub-tables
[browser]         # context overrides active when the side panel is focused
[file_search]     # context overrides active when file-search is open
```

Values under `[keys]` are either:

- A scalar string — direct action binding, e.g. `"ctrl+s" = "save"`.
- A sub-table — chord prefix, e.g. `[keys."ctrl+x"] "ctrl+c" = "quit"`.

The runtime representation is a `Keymap` (`crates/core/src/keys.rs:91`) built
by `Keys::into_keymap`. Compilation rules and chord/context semantics are in
`keymap.md`. Action identifiers are the `snake_case` serde-renamed variants of
the `Action` enum (`crates/core/src/lib.rs:223`).

**No merging with defaults.** If a user writes `keys.toml` with only two
bindings, they get those two bindings and nothing else — every default is
dropped. This is a usability footgun worth flagging; scenarios that exercise a
rebound key must either include every other binding they rely on, or confine
themselves to actions the test doesn't touch.

### `theme.toml` — schema high level

The full theme schema is in `docs/extract/config-keys.md`. At a glance:

- A required `[COLORS]` free-form palette mapping names to colors. Any name
  defined here can be referenced from any other section as `$name`. References
  may chain.
- Required sections: `[tabs]`, `[status_bar]`, `[editor]`, `[browser]`,
  `[file_search]`, `[diagnostics]`, `[git]`, `[brackets]`, `[syntax]`.
- Optional section: `[pr]` (PR review styling). Missing `[pr]` falls back to
  neutral styles.

Each value is either a color string (`"$accent"`, `"ansi_red"`, `"#ff8800"`,
`"term_reset"`) or an inline table `{ fg = "...", bg = "...", bold = true,
italic = true }` (all fields optional).

Because no section has `#[serde(default)]`, **a custom `theme.toml` must
provide every required section** or deserialization fails and the bundled
defaults take over entirely. There is no partial theming.

Truecolor output depends on `COLORTERM` being set to `truecolor` or `24bit` at
process start (`crates/ui/src/style.rs:184`); otherwise all hex colors are
approximated to the 256-color cube. The read is cached in a `OnceLock`, so
changing the env mid-run has no effect.

### `--reset-config` (early-exit CLI path)

`--reset-config` is handled before the event loop starts. It:

1. Creates the config dir if missing.
2. Overwrites `<dir>/keys.toml` with the bundled `Keys::default_toml()`.
3. Overwrites `<dir>/theme.toml` with the bundled `Theme::default_toml()`.
4. Removes `<dir>/db.sqlite` (ignores `NotFound`).
5. Prints one status line per action to stderr.
6. Exits.

It does not read or parse either file first, so it also acts as a recovery
path when a hand-edited config is unparseable.

### What is **not** configurable today (hardcoded)

From `Dimensions::new` (`crates/state/src/lib.rs:234`):

| Setting | Value | Notes |
|---|---|---|
| `tab_stop` | 4 spaces | soft-tab expansion width |
| `side_panel_width` | 25 cols | sidebar width when shown |
| `min_editor_width` | 25 cols | sidebar auto-hides below this |
| `scroll_margin` | 3 rows | cursor-to-edge padding |
| `gutter_width` | 2 cols | change + diagnostic columns |
| `ruler_column` | 110 | column for vertical ruler (style is themeable, position is not) |
| `status_bar_height` | 1 | |
| `tab_bar_height` | 1 | |

Other hardcoded values:

- LSP server binaries per language — overridable only via `--test-lsp-server`
  (global, test-only).
- Git scan debounce — 500 ms, fixed.
- Language detection (extensions + modeline mode names) — hardcoded in the
  syntax crate.
- `--keys-file` initial sleep — 3 s warm-up.
- Undo flush timer name `"undo_flush"` — not user-visible.
- Auto-close buffer limit — README mentions it but the value isn't surfaced.

## User flow

1. First launch: no config dir exists. led reads nothing, uses compiled-in
   defaults, and runs. The config dir is **not** auto-created for the user;
   only `--reset-config` or `db.sqlite` creation writes into it.
2. User runs `led --reset-config` (or copies the bundled defaults by hand) to
   materialize `keys.toml` / `theme.toml`. They edit one value.
3. User restarts led. The changed value takes effect. Editing at runtime does
   nothing.
4. If the user writes invalid TOML, led shows a status-bar `Alert::Info` with
   the parser error on next launch or next workspace transition and continues
   using whatever config was successfully loaded last.

## State touched

- `AppState.config_keys: Option<ConfigFile<Keys>>` — raw parsed TOML, reset on
  each `ConfigDir` load. Compiled into `AppState.keymap` downstream.
- `AppState.config_theme: Option<ConfigFile<Theme>>` — raw parsed theme, used
  by `derived.rs` to produce styled output.
- `AppState.keymap: Option<Rc<Keymap>>` — compiled runtime keymap, rebuilt
  when `config_keys` changes.
- `AppState.alerts.info` — populated with TOML parse errors (not fatal).
- `AppState.startup.config_dir: UserPath` — the resolved directory, never
  mutated after startup.

## Extract index

- Config files: `keys.toml`, `theme.toml`, `db.sqlite` — see
  `docs/extract/config-keys.md`.
- CLI flag `--config-dir`, `--reset-config` — see `docs/extract/config-keys.md`
  and this doc's sibling `cli.md`.
- Driver: `ConfigFileOut::{ConfigDir, Persist}`, `ConfigFile<File>` —
  `crates/config-file/src/lib.rs`.
- Schema types: `Keys` (`crates/core/src/keys.rs:134`), `Theme`
  (`crates/core/src/theme.rs:107`).
- Hardcoded defaults: `Dimensions::new` at `crates/state/src/lib.rs:234`.

## Edge cases

- **Both files missing**: completely normal. led uses bundled defaults and
  emits no alert. The config dir is not created.
- **Config dir missing entirely**: same as above. `--reset-config` is the only
  path that `mkdir`s it.
- **TOML parse error**: `Alert::Info` with the error text. Previously-loaded
  config remains in effect (at startup, this means the defaults). Not fatal.
- **`keys.toml` defines only a subset of bindings**: everything *else* is
  unbound. Defaults are not merged. This is the main usability footgun.
- **`theme.toml` omits a required section** (e.g. `[git]`): deserialization
  fails, the entire theme falls back to defaults, one `Alert::Info` surfaces.
- **`keys.toml` binds `shift+a`**: parses OK but never matches at runtime —
  `KeyCombo::from_key_event` strips SHIFT on `KeyCode::Char(_)`. See
  `keymap.md`.
- **`keys.toml` binds `f5`**: fails to parse (F-keys unsupported by
  `parse_key_combo`). See `keymap.md`.
- **`keys.toml` binds the same chord in both `[keys]` and `[browser]`**: the
  `[browser]` entry wins when the sidebar is focused; global wins otherwise.
- **`theme.toml` references an undefined `$name`**: style resolution returns
  a fallback; [unclear — need to confirm whether this is a hard error or a
  silent default]. The color resolver lives in `crates/ui/src/style.rs`.
- **Secondary led instance** on the same workspace: `ConfigDir.read_only =
  true` is set but ignored by the reader; configs load identically.
- **`COLORTERM` unset vs set mid-run**: first read is cached. Only the
  process-start value matters.
- **User edits `keys.toml` while led runs**: no reload. Changes visible only
  after restart.

## Error paths

- **TOML parse failure (`keys.toml` or `theme.toml`)**: `Alert::Info` (via
  `AlertExt::as_info`), not `Warn`. Surfaced in the status bar; transient
  (3s). The loaded-config slot is not overwritten, so whatever was parsed
  successfully previously remains in effect. At startup this means the
  compiled-in defaults.
- **`--reset-config` write failure**: stderr message (`"Failed to reset
  config: {e}"` etc.) and process continues through the reset sequence
  before exiting. Session DB removal failure with `NotFound` is treated as
  success.
- **Stale `ConfigFileOut::Persist` emissions**: silently dropped by the
  driver (empty match arm). This is the "hot-reload doesn't work" path —
  see `POST-REWRITE-REVIEW.md`.
- [unclear — behavior when `$name` references a non-existent color in a
  `StyleValue`; confirm by reading `crates/ui/src/style.rs` in Phase D.]
- [unclear — behavior when a `keys.toml` action string is misspelled
  (`"seve"` instead of `"save"`); `parse_action` returns `Err(String)`
  but the caller path in `into_keymap` returns the error, which in
  `model/mod.rs:163-177` becomes `Alert::Warn` — worth confirming level
  ("warn" here, but TOML parse is "info"; inconsistency noted).]
