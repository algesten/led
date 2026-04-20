# Milestone 6 — chord bindings + richer keymap

Sixth vertical slice. M5 gave us a single-key keymap; M6 catches it
up to legacy's two-level direct + chord scheme. After M6 the default
keymap matches legacy's `default_keys.toml` for every command we've
implemented so far, and scenarios that drive `ctrl+x ctrl+s` /
`ctrl+x k` / `alt+f` start hitting the right dispatch path.

Prerequisite reading:

1. `docs/spec/keymap.md` § "Compilation", "Chord key format",
   "Chord-prefix state" — legacy's keymap shape, which this matches.
2. `MILESTONE-5.md` — the flat keymap this builds on.
3. `docs/extract/actions.md` — action vocabulary + default bindings.

---

## Goal

```
$ cargo run -p led -- Cargo.toml
# C-x C-s  → save (chord)
# C-x k    → kill active tab
# C-x C-c  → quit
# M-f / M-b → word forward / back
# C-left / C-right → prev / next tab
# Esc, C-g → abort (a no-op on plain editor state today;
#                    hooks in later modal milestones)
```

Plain `ctrl+s` / `ctrl+c` are **dropped** by default — legacy
doesn't bind them. Users who want them back add two lines to
`keys.toml`.

## Scope

### In
- `Keymap` becomes a two-level structure: a `direct` table of
  key → command and a `chords` table of prefix-key → (key → command).
  Matches legacy's shape exactly; contexts (`[browser]` /
  `[file_search]`) stay parked until M11 / M14.
- Dispatch gains a `ChordState` value threaded through the main
  loop. After a prefix key lands, the next key consults the nested
  map; unrecognized-second-key cancels silently (legacy behaviour).
- TOML loader accepts nested sub-tables: `[keys."ctrl+x"] "ctrl+s" =
  "save"`. Values inside `[keys]` are now either strings (direct) or
  tables (chord).
- New `Command` variants: `Abort`, `SaveAll`, `SaveNoFormat`,
  `KillBuffer`, `FileStart`, `FileEnd`, `WordLeft`, `WordRight`.
  (Quit / TabPrev / TabNext already exist; they keep their snake-case
  names `quit` / `prev_tab` / `next_tab`.)
- Default keymap reorganised to match legacy `default_keys.toml` for
  every command the rewrite implements today. The remaining legacy
  defaults (`ctrl+f` open_file_search, `ctrl+r` lsp_rename, etc.) stay
  unbound until their feature milestones land.

### Out

Per `ROADMAP.md`:

- **Context overlays** (`[browser]`, `[file_search]`) → M11 / M14.
- **Timeout on pending chord** → not scheduled. Legacy has none; we
  match.
- **Chord-count accumulator** (`ctrl+x 4 2 ctrl+x e` for repeat) →
  M22 (macros). Only `kbd_macro_execute` consumes it in legacy.
- **`macro_repeat` state** (bare `e` after execute) → M22.
- **Post-lookup action interceptors** (isearch/completion/rename
  absorb the action stream) → land with the feature that needs them
  (M13 / M17 / M18 respectively).

## Key design decisions

### D1 — Keymap is flat + one-level chords (matches legacy)

```rust
pub struct Keymap {
    direct: HashMap<KeyEvent, Command>,
    chords: HashMap<KeyEvent, HashMap<KeyEvent, Command>>,
}
```

Not a deep trie. Legacy's TOML deserialiser treats nested
`[keys."ctrl+x"]` as a flat sub-table; nesting any deeper is not
supported. We match.

`direct` and `chords` are **disjoint** — a single key can't be both
a direct binding and a chord prefix. If the user configures both,
`direct` wins and the chord table for that key is unreachable. This
is legacy's behaviour (confirmed via `keymap.md` § "Chord prefix
also bound as a direct key"); we emit no warning, also matching
legacy.

### D2 — Dispatch state lives in a new `ChordState`

```rust
#[derive(Default, Debug, Clone, Copy)]
pub struct ChordState {
    pub pending: Option<KeyEvent>,
}
```

Threaded into `dispatch_key` as `&mut ChordState`. Lives in `main.rs`
alongside `tabs`, `edits`, `store`, `terminal`.

Legacy carries the same state in a `Cell` inside `actions_of` —
explicitly not in `AppState`, so snapshots never see it. Our design
matches: `ChordState` is not a source, has no drv input, doesn't
participate in memoization. A pending prefix between two dispatch
ticks lives in the runtime frame, not in any atom.

### D3 — Dispatch algorithm

```text
dispatch_key(k):
    if chord.pending is Some(prefix):
        chord.pending = None                           # clear unconditionally
        if cmd = keymap.lookup_chord(prefix, k):
            run_command(cmd)
        # else: silent cancel — legacy behaviour
        return

    if cmd = keymap.lookup_direct(k):
        run_command(cmd)
        return

    if keymap.is_prefix(k):
        chord.pending = Some(k)
        return

    if cmd = implicit_insert(k):                       # printable fallback (M5)
        run_command(cmd)
        return

    # unbound → no-op
```

Clearing `pending` before the lookup ensures a failed chord resets
state. Matching the second key via `lookup_chord` means chord
behaviour can't leak into direct. The printable-char fallback only
fires at the root — pressing `a` inside a `ctrl+x` prefix that
doesn't bind `a` is the "unrecognized second key" case and silently
cancels.

### D4 — TOML loader: string vs table per entry

Inside `[keys]`, each value is either a string (direct binding) or
a TOML table (chord prefix). The loader branches on the value type:

```toml
[keys]
"ctrl+s" = "save"              # direct
"tab"    = "next_tab"

[keys."ctrl+x"]                # prefix ctrl+x → sub-table
"ctrl+s" = "save"
"ctrl+c" = "quit"
"k"      = "kill_buffer"
```

Errors surfaced:

- Unknown modifier / key in the outer `[keys]` key — the existing
  `ConfigError::UnknownKey` (covers both direct and prefix keys).
- Unknown command string — existing `ConfigError::UnknownCommand`.
- Unknown key inside a chord sub-table — new
  `ConfigError::UnknownChordKey { prefix, key, message }`.
- Unknown command inside a chord sub-table — new
  `ConfigError::UnknownChordCommand { prefix, key, command, message }`.
- Value is neither string nor table — new
  `ConfigError::InvalidBindingShape { key, message }`.

(Consolidating these into one `InvalidBinding { path, key, kind }`
would read nicer but legacy surfaces the errors individually in
its `Alert::Warn` messages; matching that helps when users migrate
configs from legacy.)

### D5 — New commands

All new variants of `Command`:

| Variant | Snake-case name | Behaviour |
|---|---|---|
| `Abort` | `abort` | No-op at the root keymap level today. Future milestones (isearch M13, LSP rename M18, confirm-kill M9) override behaviour via their own absorption logic. |
| `SaveAll` | `save_all` | Enqueue every dirty buffer's path into `pending_saves`. Per-buffer save flow is unchanged. |
| `SaveNoFormat` | `save_no_format` | Alias of `Save` in M6. M18 (LSP format) will differentiate: `save` runs format first, `save_no_format` skips it. |
| `KillBuffer` | `kill_buffer` | Close the active tab. For M6 we no-op on dirty buffers (no confirm-kill prompt yet — M9 adds that). Switch to the next remaining tab; if the last tab closed, leave `Tabs` empty and `active` `None`. |
| `FileStart` | `file_start` | Move cursor to line 0, col 0; scroll to top. |
| `FileEnd` | `file_end` | Move cursor to last line, col = line length; scroll so cursor is visible. |
| `WordLeft` | `word_left` | Move cursor backward to the start of the previous word. "Word" = run of `is_alphanumeric() || == '_'`. |
| `WordRight` | `word_right` | Move cursor forward to the start of the next word (past any trailing non-word chars). |

Word-move semantics follow the bread-and-butter Emacs convention
(skip over non-word chars, then skip over word chars). Legacy uses
a tree-sitter-aware variant; that's a later optimisation.

### D6 — Default keymap matches legacy for implemented commands

Starting from legacy `default_keys.toml`, we keep only the bindings
whose command the rewrite already implements. Specifically:

**Direct:**
```
tab       = insert_tab  -> NOT YET (M23); leave as M5 default next_tab
up        = move_up
down      = move_down
left      = move_left
right     = move_right
home      = line_start
end       = line_end
pageup    = page_up
pagedown  = page_down
enter     = insert_newline
backspace = delete_backward
delete    = delete_forward

ctrl+home  = file_start      [NEW — M6]
ctrl+end   = file_end        [NEW — M6]
ctrl+left  = prev_tab        [NEW — M6]
ctrl+right = next_tab        [NEW — M6]
alt+b      = word_left       [NEW — M6]
alt+f      = word_right      [NEW — M6]
alt+v      = page_up         [NEW — M6, legacy extra]
ctrl+v     = page_down       [NEW — M6, legacy extra]
alt+<      = file_start      [NEW — M6, Emacs alias]
alt+>      = file_end        [NEW — M6, Emacs alias]
ctrl+g     = abort           [NEW — M6]
esc        = abort           [NEW — M6]
```

**Chord prefix `ctrl+x`:**
```
ctrl+s = save
ctrl+c = quit
ctrl+a = save_all
ctrl+d = save_no_format
k      = kill_buffer
```

**Removed from M5's defaults:**
```
ctrl+s = save        (moved to ctrl+x ctrl+s chord)
ctrl+c = quit        (moved to ctrl+x ctrl+c chord)
```

Users who want the plain forms back add two lines to their user
`keys.toml`. This is legacy-faithful and matches the golden-expected
trace output.

Note: `tab` stays bound to `next_tab` in M6 rather than
`insert_tab` (legacy's default) because our `InsertTab` command
doesn't exist yet — auto-indent lands in M23. M6 does not regress
tab-switching.

## Types

```rust
// keymap.rs
pub struct Keymap {
    direct: HashMap<KeyEvent, Command>,
    chords: HashMap<KeyEvent, HashMap<KeyEvent, Command>>,
}

impl Keymap {
    pub fn empty() -> Self;
    pub fn bind(&mut self, key: &str, cmd: Command);
    pub fn bind_chord(&mut self, prefix: &str, second: &str, cmd: Command);
    pub fn lookup_direct(&self, key: &KeyEvent) -> Option<Command>;
    pub fn lookup_chord(&self, prefix: &KeyEvent, second: &KeyEvent) -> Option<Command>;
    pub fn is_prefix(&self, key: &KeyEvent) -> bool;
}

#[derive(Default, Debug, Clone, Copy)]
pub struct ChordState {
    pub pending: Option<KeyEvent>,
}
```

`dispatch_key` signature:

```rust
pub fn dispatch_key(
    k:        KeyEvent,
    tabs:     &mut Tabs,
    edits:    &mut BufferEdits,
    store:    &BufferStore,
    terminal: &Terminal,
    keymap:   &Keymap,
    chord:    &mut ChordState,
) -> DispatchOutcome;
```

Main loop owns `ChordState` alongside the four atoms.

## Crate changes

Unchanged layout:

```
crates/runtime/src/
  keymap.rs    Keymap restructured; ChordState; new Command variants
  config.rs    Loader branches on string-vs-table per entry
  dispatch.rs  chord state machine + new run_command arms
  lib.rs       run() threads ChordState; exports
led/src/main.rs  constructs ChordState::default() and passes in
```

No new workspace members.

## Testing

- `keymap` — bind + lookup_direct + lookup_chord + is_prefix;
  direct-wins-over-chord edge; unbind chord prefix by replacing with
  direct.
- `config` — nested TOML round-trip; `[keys."ctrl+x"] "k" =
  "kill_buffer"` parses; string-vs-table error surfaces as
  `InvalidBindingShape`; unknown chord key / command errors.
- `dispatch` — chord state machine: first press sets pending; matching
  second press fires command and clears; unmatched second press
  silently cancels; pending survives a no-op tick (none at this layer
  — tested at the dispatch level); direct binding still fires while
  pending is None.
- `dispatch` — new commands: `file_start` / `file_end` / `word_left`
  / `word_right` mutate cursor correctly; `kill_buffer` closes active
  tab; `save_all` enqueues every dirty path; `abort` is a no-op.

Expected: +20 unit tests.

Baseline goldens expected to stay at 0 / 257 green; every scenario
still fails on frame diff because we don't have chrome yet. Trace
diffs may change for scenarios whose expected input uses chord keys
— those move closer to right but remain wrong.

## Done criteria

- All existing tests pass; new chord tests pass.
- `cargo clippy --all-targets` warning count at M5 baseline (6).
- Interactive sanity: `cargo run -p led -- file.txt`, try
  `ctrl+x ctrl+c` → exits; `ctrl+x ctrl+s` → saves; `alt+f` /
  `alt+b` → word move; `ctrl+home` → top; plain `ctrl+c` → no-op
  (legacy-faithful).
- `goldens/` baseline unchanged (still 0 / 257 — frame diffs), but
  ready to turn green when M9 lands.

## Growth-path hooks

- **Context overlays** (M11 browser, M14 file-search): add a
  `contexts: HashMap<String, HashMap<KeyEvent, Command>>` field to
  `Keymap`, a `current_context: Option<&'static str>` param to
  `lookup_*`, and a dispatch-time context-picker mirroring legacy's
  `actions_of.rs:147-157`.
- **Chord count accumulator** (M22): grow `ChordState` with
  `count: Option<usize>` tracking digits between prefix and second
  key.
- **Post-lookup absorption** (M9 confirm-kill, M13 isearch, M17 LSP
  completion, M18 LSP rename): a dispatch-time filter after
  `run_command` returns, overriding behaviour when a modal is
  active. Legacy does this in `model/mod.rs:190-283`.
