# Milestone 5 — config / keybindings

Fifth vertical slice. After M4 all key handling is hardcoded inside
`dispatch_key`; after M5 keypresses go through a `Keymap` that maps
key events to named `Command`s, with an optional TOML file overriding
the defaults.

Prerequisite reading:

1. `MILESTONE-3.md` and `MILESTONE-4.md` for the current dispatch
   shape — M5 refactors the function body while keeping the behaviour.
2. `README.md` § "Key decisions already made" — the allocation
   discipline still applies; the keymap lookup path is on every
   keystroke.

---

## Goal

```
$ cargo run -p led -- Cargo.toml
# defaults behave exactly as M4 did (arrows, Ctrl-S, Ctrl-C, tab cycle)

$ mkdir -p ~/.config/led
$ cat > ~/.config/led/config.toml <<'TOML'
[keys]
"ctrl-q" = "quit"
"ctrl-w" = "tab.next"
TOML
$ cargo run -p led -- Cargo.toml
# Ctrl-Q now quits, Ctrl-W advances the tab;
# Ctrl-C falls through to default unless rebound.
```

## Scope

### In
- New `Command` enum listing every dispatch-level action.
- New `Keymap` struct (immutable at runtime) mapping `KeyEvent` to
  `Command`.
- Built-in `default_keymap()` that reproduces M1–M4 behaviour.
- `dispatch_key` refactor: keymap lookup first; if no binding and the
  key is a printable char, fall back to `Command::InsertChar(c)`.
- Key-string parser — `"ctrl-s"`, `"shift-tab"`, `"pageup"`, `"up"`,
  `"a"` — bidirectional (parse + display) for config loading and
  diagnostics.
- `--config-dir <PATH>` CLI flag (already reserved) points at a
  directory; `<dir>/config.toml` is read if present. Absent
  `--config-dir` falls back to `dirs::config_dir()/led`, also optional.
- TOML `[keys]` section merges onto the default map (later entries
  override earlier ones; no "unbind" yet).
- Parse errors at startup print to stderr and exit non-zero **before**
  raw mode is acquired so the message lands on a usable terminal.

### Out
- Hot reload. Config is read once at startup; the resulting `Keymap`
  is immutable for the process lifetime. Hot reload needs a watch
  driver + a `ConfigState` atom; deferred.
- Unbind / `"none"` / fallthrough. You can only override, not remove.
- Per-mode keymaps (insert vs normal etc.). No modal design yet.
- Chord / prefix bindings (`ctrl-x ctrl-s` style). Single keypress →
  single command.
- Non-keymap config: theming, tab width, line endings, session,
  plugins. All separate concerns, future milestones.
- Command arguments beyond the `InsertChar(char)` built-in. Users
  bind key → named command; no parameterised bindings in config.
- Lua / DSL config. TOML only.

## Key design decisions

### D1 — `Command` enum is the dispatch vocabulary

Every path through `dispatch_key` becomes a named command:

```rust
pub enum Command {
    Quit,
    TabNext,
    TabPrev,
    Save,
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    CursorLineStart,
    CursorLineEnd,
    CursorPageUp,
    CursorPageDown,
    InsertNewline,
    DeleteBack,
    DeleteForward,
    /// Fallback injected by dispatch when no binding matches and
    /// the key is a printable character. Not config-bindable — it
    /// carries the char the user actually pressed.
    InsertChar(char),
}
```

The enum is the *contract* between config (strings) and dispatch
(behaviour). Future features add variants here; no need to touch
the parser except to teach it new strings.

### D2 — Keymap is immutable for the run

A runtime-mutable keymap would be a source (atom) and we'd have a
`ConfigReadDriver` + watcher. M5 avoids both: the keymap is built
once in `main.rs`, passed to `run()` by reference, and read by
dispatch. No memo, no atom.

If hot reload ever lands: promote `Keymap` to a user-decision
source, add a file-watch driver, and re-insert one level of
indirection in dispatch (take `KeymapInput<'a>` instead of
`&Keymap`). Straightforward refactor; not worth the ceremony now.

### D3 — Keymap is a plain `HashMap<KeyEvent, Command>`

The set of bindings is bounded (hundreds at most) so `HashMap`'s
constant-factor overhead is irrelevant; `get(&k)` on every keystroke
is still sub-microsecond. The hash is over the existing
`{ code, modifiers }` tuple — no custom derivation needed.

### D4 — Printable-char fallback happens outside the keymap

The default keymap does NOT contain an entry for every ASCII char.
Instead, dispatch does:

```rust
fn dispatch_key(k, tabs, edits, store, terminal, keymap) {
    let cmd = match keymap.lookup(&k) {
        Some(c) => c,
        None => match k.code {
            KeyCode::Char(c)
                if !k.modifiers.contains(KeyModifiers::CONTROL)
                    && !k.modifiers.contains(KeyModifiers::ALT)
                    && !c.is_control() =>
            {
                Command::InsertChar(c)
            }
            _ => return DispatchOutcome::Continue,
        },
    };
    run_command(cmd, ...)
}
```

This keeps the keymap finite and lets users bind specific chars
(e.g. `"a" = "quit"`) without having to enumerate every other
printable char to restore default behaviour.

### D5 — Config is loaded synchronously in `main.rs`, before raw mode

Parse errors (bad key string, unknown command name, malformed TOML)
print `error: ...` to stderr and exit 2 before `RawModeGuard` is
acquired. Messages land on a cooked terminal where they're readable.

Absent config file is **not** an error — silently fall through to
defaults. Missing `~/.config/led` is the common case.

### D6 — Key-string parser — lowercase, dash-separated

```
tab           → Tab
shift-tab     → Shift-BackTab
ctrl-s        → Ctrl-s
ctrl-shift-k  → Ctrl-Shift-K
up            → Up
pageup        → PageUp
a             → 'a'
A             → Shift-'a'  (uppercase form of the char)
space         → ' '
enter         → Enter
```

Modifier order in parsing is flexible (`shift-ctrl-x` == `ctrl-shift-x`);
emitted canonical order is `ctrl-alt-shift-<code>`.

Reason this matters: error messages from the config parser use the
canonical string ("unknown key `ctrl-shift-tab`"), which must round-
trip with what users wrote.

### D7 — Command-string vocabulary

Dot-separated, lowercase:

```
quit                     → Command::Quit
save                     → Command::Save
tab.next                 → Command::TabNext
tab.prev                 → Command::TabPrev
cursor.up                → Command::CursorUp
cursor.down              → Command::CursorDown
cursor.left              → Command::CursorLeft
cursor.right             → Command::CursorRight
cursor.line-start        → Command::CursorLineStart
cursor.line-end          → Command::CursorLineEnd
cursor.page-up           → Command::CursorPageUp
cursor.page-down         → Command::CursorPageDown
edit.insert-newline      → Command::InsertNewline
edit.delete-back         → Command::DeleteBack
edit.delete-forward      → Command::DeleteForward
```

`Command::InsertChar` is not bindable by name — the fallback path
produces it exclusively.

Unknown command strings are a parse error at load time.

### D8 — Default keymap reproduces M1–M4 exactly

```rust
pub fn default_keymap() -> Keymap {
    let mut m = Keymap::empty();
    m.bind("ctrl-c",     Command::Quit);
    m.bind("ctrl-s",     Command::Save);
    m.bind("tab",        Command::TabNext);
    m.bind("shift-tab",  Command::TabPrev);
    m.bind("up",         Command::CursorUp);
    m.bind("down",       Command::CursorDown);
    m.bind("left",       Command::CursorLeft);
    m.bind("right",      Command::CursorRight);
    m.bind("home",       Command::CursorLineStart);
    m.bind("end",        Command::CursorLineEnd);
    m.bind("pageup",     Command::CursorPageUp);
    m.bind("pagedown",   Command::CursorPageDown);
    m.bind("enter",      Command::InsertNewline);
    m.bind("backspace",  Command::DeleteBack);
    m.bind("delete",     Command::DeleteForward);
    m
}
```

User config merges on top. For the golden path — user has no
config — the default map produces identical behaviour to M4.

## Crate layout

```
crates/
  runtime/
    src/
      keymap.rs            NEW: Command, Keymap, key_string
      config.rs            NEW: TOML loader, --config-dir
      dispatch.rs          refactored to consume Keymap
      lib.rs               exports Keymap/Command; run() takes &Keymap
led/
  src/main.rs              builds Keymap via load_config() ± CLI
```

No new workspace members. `toml` and `dirs` are already workspace
deps.

## Runtime integration

```rust
// runtime/src/lib.rs
pub use keymap::{default_keymap, Command, Keymap};
pub use config::load_keymap;  // reads disk + merges

pub fn run(
    tabs:     &mut Tabs,
    edits:    &mut BufferEdits,
    store:    &mut BufferStore,
    terminal: &mut Terminal,
    drivers:  &Drivers,
    keymap:   &Keymap,
    stdout:   &mut impl Write,
    trace:    &SharedTrace,
) -> io::Result<()> { ... }
```

```rust
// led/src/main.rs
let keymap = led_runtime::load_keymap(cli.config_dir.as_deref())
    .unwrap_or_else(|e| {
        eprintln!("led: config error: {e}");
        std::process::exit(2);
    });
// ... raw mode ... run(..., &keymap, ...)
```

## Testing

- `runtime::keymap::key_string` — parse + display round-trip; error
  messages for unknown tokens.
- `runtime::keymap::Keymap` — `default_keymap()` contains the right
  bindings; `bind` + `lookup` work; overrides replace.
- `runtime::keymap::Command` — command-string parse/display round-
  trip; unknown commands rejected with a helpful message.
- `runtime::config` — loading missing file returns default; loading a
  TOML file merges overrides; malformed TOML / unknown key / unknown
  command each surface as distinct errors.
- `runtime::dispatch` — every existing test continues to pass against
  the default keymap; add one test that drives a custom keymap and
  asserts a non-default command fires.

Expected: +15 tests, total ≥ 99.

## Done criteria

- All M1–M4 tests still pass without change to behaviour.
- New keymap / config tests pass.
- Running `led <file>` with no config behaves identically to M4.
- Running `led <file>` with a user config overrides the intended keys.
- Malformed config prints a legible error and exits non-zero without
  mangling the terminal.
- Clippy warning count unchanged from baseline.
- Allocation discipline: keymap lookup is a single `HashMap::get`
  per keystroke; no per-tick work.

## Growth-path hooks

- **Hot reload.** Wrap `Keymap` in a `ConfigState` user-decision
  source; add a `ConfigWatchDriver` (FSEvents / inotify); on change,
  re-parse and swap. Dispatch's `&Keymap` becomes a `KeymapInput`.
- **Chord / prefix bindings.** Introduce a small state machine in
  dispatch: after a prefix key fires (e.g. `ctrl-x`), subsequent
  keys consult a nested map until one lands or a timeout fires.
- **Modal keymaps.** Add a `Mode` enum (Insert, Normal, …) to the
  tab or to a separate source; keymap lookup becomes
  `keymap.lookup(mode, key)`.
- **More commands.** Every later milestone lands here first: search,
  LSP actions, git, jump-to-definition, etc. All variants added to
  `Command` + entries in the command-string parser.
- **Non-keymap config.** Add further TOML sections — `[editor]`,
  `[ui]`, `[theme]` — as those features arrive. The loader grows
  symmetrically.
