# cli

## Summary

led's CLI is defined in `led/src/main.rs` via `clap`. Positional arguments
are zero-or-more paths (files, or a single directory); everything else is a
flag. Three flags are test-only (`hide = true` in `clap`, or documented here
with a "test-only" marker). One flag — `--reset-config` — is an **early-exit
path**: it does its work and returns before the event loop starts.

All flags are long-form only (`--xxx`); there are no short aliases. Default
values are the clap defaults (mostly `None`/off) unless noted.

## Behavior

### Flag table

| Flag | Arg | Default | Kind | Effect |
|---|---|---|---|---|
| *(positional)* | `<paths...>` | empty | normal | Zero or more paths. A single directory opens the file browser rooted there with no open buffers. A list of files opens each (directories in the list are filtered out). |
| `--log-file` | `FILE` | off | normal | Initialize a file-based tracing logger writing to `FILE`. No rotation, no max size, format determined by `led::logging::init_file_logger`. |
| `--reset-config` | — | false | **early-exit** | Create `<config-dir>` if missing, overwrite `keys.toml` and `theme.toml` with bundled defaults, remove `db.sqlite`, print one status line per action to stderr, then `return` from `main` before any terminal/UI setup. |
| `--no-workspace` | — | false | normal | Standalone mode. Disables workspace, git, LSP, session DB and file watchers. Intended for `EDITOR="led --no-workspace"` single-file use (commit messages, temp files). Browser always rooted at process CWD. Passed through as `Startup.no_workspace`. |
| `--keys-file` | `FILE` | off | normal | After a 3-second warm-up sleep, replay a list of key-combo strings (one per line, plus optional `goto <line>` jumps) into terminal input. Used for profiling/benchmarking. See "Keys script format" below. |
| `--keys-record` | `FILE` | off | normal | Append each real key press (formatted via `format_key_combo`, one per line) to `FILE` as it arrives. The output format round-trips with `--keys-file`. |
| `--golden-trace` | `FILE` | off | **test-only** | Append the normalized dispatch trace to `FILE`. Used exclusively by the goldens runner. Not hidden in `--help` but only useful for test infra. |
| `--config-dir` | `DIR` | `~/.config/led` | normal | Override config/state directory. Goldens runner sets this per scenario to isolate `db.sqlite`, `keys.toml`, and `theme.toml`. Also useful for users who want a non-default location. |
| `--test-lsp-server` | `PATH` | off | **test-only, hidden** | Override the LSP server binary for *all* languages with a single path (used with `fake-lsp`). Hidden from `--help` (`hide = true`). |
| `--test-gh-binary` | `PATH` | off | **test-only, hidden** | Override the `gh` CLI binary path (used with `fake-gh`). Hidden from `--help`. |

### Positional paths

`main.rs:100-166` does the following to the positional paths:

1. Each raw string is resolved: the parent is canonicalized, then joined with
   the original filename. Non-existent targets fall back to the original path.
2. If `--no-workspace`: directories are filtered out, everything else becomes
   `arg_paths` (canonical) + `arg_user_paths` (user-typed, used for symlink
   chain walking to detect language). `arg_dir` is `None`.
3. Else if exactly one path was given and it is a directory: `arg_dir =
   Some(dir)`, no file args, browser rooted at `dir`.
4. Else: directories are filtered out, remaining files become `arg_paths` /
   `arg_user_paths`; `start_dir` is the first file's parent or CWD if none.

`user_start_dir` (separate from `start_dir`) is the non-canonicalized dir
shown in the UI. Under `--no-workspace` it is always process CWD — the file
arg is typically something like `.git/COMMIT_EDITMSG`, and rooting the
browser at that parent would show a hidden/temp dir.

### `--reset-config` sequence

Runs entirely synchronously before `Startup` is constructed:

1. `std::fs::create_dir_all(config_dir)` — ignore errors.
2. `fs::write(<dir>/keys.toml, Keys::default_toml())` → print `"Config reset
   to defaults."` or `"Failed to reset config: {e}"` to stderr.
3. Same for `theme.toml`.
4. `fs::remove_file(<dir>/db.sqlite)` → print `"Session database reset."`.
   `NotFound` is treated as success.
5. `return` from `main`.

No terminal setup (raw mode, alt screen, bracketed paste) runs, so the flag
is safe to invoke from a normal shell.

### `--keys-file` / `--keys-record` format

Line-oriented. Lines starting with `#`, or that are empty/whitespace-only,
are comments. A line `goto <N>` jumps playback to the first entry whose
source-file line number is ≥ `N` (a primitive loop/macro mechanism).
Every other line is a chord string in the same format as `keys.toml`
(`ctrl+a`, `alt+left`, `esc`, a single character, etc.) parsed by
`parse_key_combo`. The writer (`--keys-record`) uses `format_key_combo`, which
returns `None` for unrepresentable keys (F-keys, Insert, numpad) — those key
presses are silently dropped from the record file.

Playback sleeps 3 seconds before the first key to let startup (session
restore, LSP warm-up) settle, then `yield_now` between keys. There is no
synchronization on application state — throughput is determined by how
fast tokio's current-thread runtime drains the action stream.

### Environment variables

led itself reads almost no env vars. Notable:

| Var | Effect |
|---|---|
| `HOME` | resolves `~/.config/led/` when `--config-dir` is absent (via `dirs::home_dir()`). |
| `COLORTERM` | `truecolor`/`24bit` → 24-bit RGB escapes; otherwise 256-color approximation. Read once into a `OnceLock`. |
| `TERM` | not read by led; set to `xterm-256color` by the goldens runner for reproducibility. |
| `UPDATE_GOLDENS` | not read by led; used by the goldens test harness. |

No `LED_*` variables exist. `--config-dir` is the only config-path override.

## State touched

Startup-only. Every flag value ends up either:

- Captured in `Startup` (`led_core::Startup`) before the event loop — `arg_paths`,
  `arg_user_paths`, `arg_dir`, `start_dir`, `user_start_dir`, `config_dir`,
  `test_lsp_server`, `test_gh_binary`, `golden_trace`, `no_workspace`.
- Consumed immediately and returned (`--reset-config`, `--log-file`).
- Attached as a side-channel to the terminal-in stream (`--keys-file`,
  `--keys-record`).

After `main` hands off to `led::run`, CLI values are read-only.

## Extract index

- Full flag table with source-line refs: `docs/extract/config-keys.md` §
  "CLI flags".
- `Startup` struct: `crates/core/src/lib.rs` (see `led_core::Startup`).
- Parser: `led/src/main.rs:17-61`.
- Script parser (`parse_keys_script`): `led/src/main.rs:69-90`.

## Edge cases

- **No arguments at all**: empty `arg_paths`, `arg_dir = None`, `start_dir =
  CWD`. led opens with no buffer, browser at CWD.
- **Single directory arg**: `arg_dir = Some(canonical)`, browser rooted at
  that directory, no buffer opened. Session restore can still reopen the tabs
  that were last open in this workspace.
- **Multiple directory args**: every directory is filtered out; only files
  are opened. `start_dir` is the first file's parent.
- **Non-existent file path**: `resolve_path` keeps the path as-is. The file
  is opened as a "new" buffer (will be created on first save) —
  `BufferState::with_create_if_missing(true)`.
- **`--no-workspace` with a directory arg**: the directory is silently
  filtered out; nothing opens. Browser stays at CWD.
- **`--keys-file` with a missing path**: `panic!` with `"read keys file ...:
  ..."`. Not caught.
- **`--keys-file` with a bad line**: `panic!("keys file line N: parse '...':
  ...")`. Not caught.
- **`--keys-record` path unwritable**: `panic!("create keys record file
  ...")`. The record is best-effort per line after that —
  `writeln!`/`flush` errors are ignored.
- **`--reset-config` plus other flags**: the other flags are parsed but
  ignored — reset happens before anything else consults them.
- **`--log-file` with a path in a missing directory**: [unclear — depends
  on `init_file_logger` behavior; confirm in Phase D.]
- **`--test-lsp-server` + real workspace**: the test binary replaces the
  real LSP binary for *every* language, not just one. There is no
  per-language override.

## Error paths

- **clap parse error** (unknown flag, missing value): clap prints usage to
  stderr and exits non-zero. Normal clap behavior.
- **`--reset-config` write errors**: printed to stderr with `"Failed to
  reset {config|theme|session database}: {e}"` and the sequence continues
  (each step is independent).
- **`--keys-file` read/parse errors**: panic.
- **`--keys-record` initial open**: panic. Per-line write errors after open:
  silently ignored.
- **`HOME` unavailable**: `dirs::home_dir()` returns `None` →
  `.unwrap_or_default()` → `""`. `~/.config/led/` becomes `.config/led/`
  relative to CWD. [unclear — may want explicit error here in the rewrite.]
- **Terminal teardown on exit**: after the event loop returns, `main`
  executes `disable_raw_mode`, shows the cursor, `LeaveAlternateScreen`, and
  `DisableBracketedPaste`, then `std::process::exit(0)`. Failures from these
  are ignored (`.ok()`). The explicit `process::exit` skips polite shutdown
  of background `spawn_blocking` work (git scans, gh CLI, LSP shutdown
  handshakes, native file-watcher thread) — otherwise the tokio
  current-thread runtime would stall waiting for them, especially on
  quit-mid-startup.
