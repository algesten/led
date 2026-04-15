# keymap

## Summary

led's keymap is a two-layer system:

1. **Config-layer lookup** — a `Keymap` compiled from `keys.toml` that
   matches a `KeyCombo` against a direct table, a chord sub-table, or a
   named context overlay (`[browser]` / `[file_search]`), returning an
   `Action` or `ChordPrefix` sentinel.
2. **Post-lookup action interceptors** in `led/src/model/mod.rs` that
   rewrite or absorb actions based on transient state (isearch active,
   find-file overlay open, LSP completion popup, LSP rename overlay, LSP
   code-action picker, confirm-kill prompt, kbd-macro repeat mode). These
   are **not** expressible in TOML — they are hand-coded guards on the
   action stream.

Anything a golden generator that only reads TOML can see is layer 1. Layer 2
is discovered by reading the model. Both must be documented for a faithful
port.

Display conventions: TOML chord strings are lowercase `ctrl+`/`alt+`/`shift+`,
`+`-delimited. The display form (`KeyCombo::display_name`) uses `Ctrl-`,
`Alt-`, `Shift-`, arrows as unicode glyphs.

## Behavior

### Compilation — `Keys::into_keymap` (`crates/core/src/keys.rs:149`)

The `Keys` struct from TOML has three `HashMap`s: `keys`, `browser`, and
`file_search`. Compilation:

1. For every entry in `keys`:
   - Scalar string value → parse the chord (`parse_key_combo`), parse the
     action (`parse_action`), insert into the `direct: HashMap<KeyCombo,
     Action>` table.
   - Sub-table value (from `[keys."ctrl+x"]`) → parse the prefix, build a
     flat `HashMap<KeyCombo, Action>` from the sub-table, insert into
     `chords: HashMap<KeyCombo, HashMap<KeyCombo, Action>>`.
2. For `browser` and `file_search`: each becomes a flat table registered in
   the `contexts: HashMap<String, HashMap<KeyCombo, Action>>` under the
   table name. These tables only exist if the TOML section was non-empty.
3. Compilation failures (`unknown action`, `unknown key`, `unknown
   modifier`) bubble up as `Err(String)`. In `model/mod.rs` that becomes
   `Alert::Warn` in the status bar; the previous keymap (or defaults)
   stays in place.

No defaults are merged. A user `keys.toml` completely replaces the bundled
one.

### Chord key format — `parse_key_combo` (`keys.rs:201`)

`modifier+modifier+...+key`, `+`-delimited, case-insensitive.

Modifiers: `ctrl`, `alt`, `shift`.

Named keys accepted today:

| Token | KeyCode |
|---|---|
| `up`, `down`, `left`, `right` | `Up`, `Down`, `Left`, `Right` |
| `home`, `end` | `Home`, `End` |
| `pageup`, `pagedown` | `PageUp`, `PageDown` |
| `enter` | `Enter` |
| `backspace`, `delete` | `Backspace`, `Delete` |
| `tab` | `Tab` |
| `esc` / `escape` | `Esc` |
| `space` | `Char(' ')` |
| any single character | `Char(c)` — `a`, `/`, `_`, `<`, `>`, `(`, `)`, `1`, etc. |

Everything else fails to parse. Notably **unsupported today**:

- F-keys (`f1`..`f24`). `parse_key_combo` has no entry, and
  `format_key_combo` returns `None` — so `--keys-record` silently drops
  them too.
- `Insert`.
- Numpad-specific keys.

The `Action` string is deserialized with `serde(rename_all = "snake_case")`
over the `Action` enum. Only payload-free variants are bindable from TOML;
`InsertChar(char)`, `Wait(u64)`, and `Resize(u16,u16)` exist in the enum but
cannot be written in `keys.toml`.

### Runtime event normalization — `KeyCombo::from_key_event` (`keys.rs:21`)

Every `crossterm::KeyEvent` is normalized into a `KeyCombo`:

```rust
let (code, shift) = match code {
    KeyCode::Char(c) => (KeyCode::Char(c), false),  // SHIFT dropped
    _                => (code, modifiers.contains(SHIFT)),
};
```

**SHIFT is stripped on `KeyCode::Char`** because the uppercase letter is
already the payload. Consequence: a binding `shift+a = "..."` parses OK,
lives in the keymap, but never matches at runtime — the looked-up combo
always has `shift = false` when the code is `Char`. This is a silent
footgun, not a lint/error.

SHIFT on non-char keys (`shift+tab`, `shift+enter`, `shift+left`) works
normally.

### Top-level lookup — `Keymap::lookup` (`keys.rs:99`)

```text
If context active (e.g. "browser" / "file_search"):
    if context table has the combo → Action(...)        // context wins
Global direct table:
    if present                      → Action(...)
Chords:
    if combo matches a prefix       → ChordPrefix
Otherwise                           → Unbound
```

The key point: **a context hit short-circuits**. Everything *not* in the
context table falls through to the global `[keys]` table. Both context
tables behave identically at this layer — they differ only in when they're
active.

### Context selection — `model/actions_of.rs:147-157`

Exactly one context can be active at a time. The selection is a linear
`if/else` over state:

1. `state.file_search.is_some()` → `"file_search"`.
2. `state.focus == PanelSlot::Side` → `"browser"`.
3. Otherwise → no context.

If both conditions hold, `"file_search"` wins. `PanelSlot::Overlay` returns
no context — overlays inherit the main keymap at this layer; their
per-action filtering lives in `model/mod.rs` (see next section).

### Overlay context resolution — table

The full story in one table, merging layer 1 (TOML) and layer 2 (model
interceptors). "Resolves via" is the source of the binding; "absorbs
action" is the post-lookup gate in `model/mod.rs` that routes actions into
the overlay handler instead of the normal `handle_action`.

| Modal/context | Active when | Resolves keys via | Post-lookup: absorbs action | Pass-through (never absorbed) |
|---|---|---|---|---|
| Main | default | `[keys]` + chord tables | — | — |
| Side panel / browser | `focus == Side` | `[browser]` (context overlay) then `[keys]` | `mov::*` re-routed to browser nav in `action/browser.rs`; editor actions filtered out by `requires_editor_focus` | `Resize`, `Quit`, `Suspend` |
| File search | `file_search.is_some()` | `[file_search]` (context overlay) then `[keys]` | yes, all actions → `Mut::FileSearchAction` (`mod.rs:217-223`) | `Resize`, `Quit`, `Suspend` |
| Find-file / Save-as | `find_file.is_some()` | `[keys]` only (no context table) | yes, all actions → `Mut::FindFileAction` (`mod.rs:226-232`) | `Resize`, `Quit`, `Suspend` |
| LSP completion popup | `lsp.completion.is_some()` | `[keys]` only | yes, all actions → `Mut::LspCompletionAction` (`mod.rs:207-213`); `InsertChar` / `DeleteBackward` re-applied to buffer so popup re-filters | `Resize`, `Quit`, `Suspend` |
| LSP rename overlay | `lsp.rename.is_some() && focus == Overlay` | `[keys]` only | yes, all actions → `Mut::LspRenameAction` (`mod.rs:199-204`) | `Resize`, `Quit`, `Suspend` |
| LSP code-action picker | `lsp.code_actions.is_some()` | `[keys]` only | yes, all actions → `Mut::LspCodeActionPickerAction` (`mod.rs:193-197`) | `Resize`, `Quit`, `Suspend` |
| Isearch | `active_buffer.isearch.is_some()` | `[keys]` only | `is_consumed_by_isearch` in `isearch_of.rs:25-31`: `InsertChar`, `DeleteBackward`, `Abort`, `InsertNewline`. Any *other* action emits `Mut::SearchAccept` then runs normally (e.g. `Up` accepts match then moves up). | `Resize`, `Quit`, `Suspend`, `InBufferSearch` (→ `search_next`) |
| Confirm-kill | `confirm_kill == true` | `[keys]` only | `InsertChar('y'/'Y')` → `Mut::ForceKillBuffer`. Any other migrated action → `Mut::DismissConfirmKill` and runs normally. (`mod.rs:270-282`) | `Resize`, `Quit`, `Suspend` |
| Kbd-macro repeat | transient `Cell` set by `Ctrl-x e` | `[keys]` (prefix `ctrl+x` + bare `e`) | bare `e` (no modifiers) → replay macro; any other key clears repeat. | — |

**Precedence at the model layer (when multiple could absorb)**: the
`unmigrated_actions_s` filter in `mod.rs:235-252` bypasses `handle_action`
if any overlay is active, so the action is routed to whichever overlay
handler fires first. The overlay streams themselves don't check each
other — in practice only one should be active at a time, and the
combination checks are in the absorption predicates.

### Chord-prefix state (`actions_of.rs:30-32`)

Three per-stream `Cell`s carry transient state:

- `chord: Option<KeyCombo>` — holds the first chord of a pending prefix.
- `chord_count: Option<usize>` — digits `0-9` pressed between chord start
  and second chord accumulate here as a repeat count. Currently only
  `kbd_macro_execute` consumes the count (emitted as
  `Mut::KbdMacroSetCount(n)` before the action); for any other chord
  target, the count is discarded.
- `macro_repeat: bool` — set after `Ctrl-x e`. While true, bare `e`
  replays the macro; any other key clears the flag and falls through to
  normal processing.

Because these live in `Cell`s, not `AppState`, they are **not** reflected
in snapshots. PTY-based goldens must always send the full chord in one go
and reset between scenarios.

### Editor-focus gating — `requires_editor_focus` / `has_input_dialog`

`actions_of.rs:159-178` blocks a set of editing actions (`InsertChar`,
`InsertNewline`, `InsertTab`, `DeleteBackward`, `DeleteForward`,
`KillLine`, `KillRegion`, `Yank`, `Undo`, `Redo`, `SortImports`) unless:

- `focus == PanelSlot::Main`, or
- `has_input_dialog(state) == true`, which checks `file_search` and
  `find_file` *only* — **not** `lsp.rename`, `lsp.completion`, or
  `lsp.code_actions`.

This is the "rename overlay drops `InsertChar`" hole flagged in
`POST-REWRITE-REVIEW.md`: the rename overlay sets `focus = Overlay` but
isn't in `has_input_dialog`, so `InsertChar` is filtered at the keymap
layer before it can reach the rename handler. Needs a golden to confirm,
and a fix (or explicit documented gate) in the rewrite.

## User flow

Common paths a user's keystroke takes, end to end:

1. **Typing a letter in the editor**: `A` → `KeyEvent{Char('A'), SHIFT}` →
   `KeyCombo{Char('A'), shift=false}` → `Keymap::lookup`: no context
   active, not in `direct`, not a chord prefix → `Unbound`. `actions_of`
   sees Unbound on a `Char` with `allow_char_insert == true` → emits
   `Action::InsertChar('A')` → `editing_of` applies the edit.
2. **`Ctrl-x Ctrl-s`**: `Ctrl-x` looked up → `ChordPrefix`. `chord` cell
   set to `ctrl+x`. Next key `Ctrl-s` → `lookup_chord(ctrl+x, ctrl+s)` →
   `Save`. `chord` cleared. `Action::Save` dispatched.
3. **`Ctrl-f` in main**: resolves to `open_file_search`, model sets
   `state.file_search = Some(..)`. Next key: context is now
   `"file_search"` so `Enter` resolves via `[file_search] enter =
   "open_selected"` (not `insert_newline`), and the action is absorbed by
   `Mut::FileSearchAction`.
4. **`Ctrl-r` (rename)**: resolves to `lsp_rename`, model opens rename
   overlay and sets `focus = Overlay`. User types `f` →
   `KeyCombo{Char('f')}` → no context (`focus == Overlay` returns no
   context at `actions_of.rs`) → resolves via `[keys]` → `Unbound` →
   would become `InsertChar('f')` — **but** `requires_editor_focus`
   blocks `InsertChar` because `focus != Main` and `has_input_dialog` is
   false. Suspected bug.
5. **Isearch active, press `Up`**: keymap returns `move_up`. `Up` is not
   in `isearch_consumes`, so isearch emits `Mut::SearchAccept` (keep
   cursor, record jump), then the action also runs normally — cursor
   moves up.

## State touched

- `AppState.keymap: Option<Rc<Keymap>>` — compiled keymap; rebuilt from
  `state.config_keys` via the `keymap_s` stream in `mod.rs:163-177`.
- `AppState.focus: PanelSlot` — drives context selection.
- `AppState.file_search`, `find_file`, `lsp.completion`, `lsp.code_actions`,
  `lsp.rename`, `confirm_kill`, `active_buffer.isearch` — each toggles a
  post-lookup interceptor.
- `actions_of` transient cells: `chord`, `chord_count`, `macro_repeat` —
  not in `AppState`.

## Extract index

- Default keymap: `crates/config-file/src/default_keys.toml`.
- Full enumeration of main + context + overlay bindings: `docs/extract/keybindings.md`.
- Parser: `crates/core/src/keys.rs:201` (`parse_key_combo`),
  `crates/core/src/keys.rs:196` (`parse_action`).
- Compilation: `crates/core/src/keys.rs:149` (`Keys::into_keymap`).
- Runtime lookup: `crates/core/src/keys.rs:99` (`Keymap::lookup`,
  `lookup_chord`).
- Action interceptors: `led/src/model/mod.rs:190-283`; `actions_of.rs:65-178`;
  `isearch_of.rs:25-31`.
- Action enum: `crates/core/src/lib.rs:223`.

## Edge cases

- **Same chord in `[keys]` and in a context table**: context wins while
  active, global fires otherwise. Intentional and exercised by several
  defaults (`Left`, `Right`, `Enter`, `Alt-Enter`, `Ctrl-q`).
- **Chord prefix with accumulator**: pressing `Ctrl-x 1 2 Ctrl-s`:
  `chord_count` grows to `12`, then `Ctrl-s` fires `save`; the count is
  discarded because `save` doesn't consume it. Only `kbd_macro_execute`
  consumes the count today.
- **Chord prefix with no second chord** (timeout / unbound second chord):
  state is held in a `Cell` with no timeout. The next unrelated press
  will try to match `(prefix, that press)` in `chords`; if no match, the
  press is silently swallowed. No status-bar feedback.
- **`shift+a` binding**: parses, stored, never matches. No warning.
- **F-keys in `keys.toml`**: fail to parse, surface as `Alert::Warn`,
  keymap falls back to defaults (at startup) or whatever was previously
  loaded.
- **Unknown action string (`seve` instead of `save`)**: same as F-keys —
  `parse_action` error → alert → previous map retained.
- **Empty `keys.toml`** (file exists but is `""` or `[keys]` alone):
  parses OK with empty `HashMap`s. Every key is `Unbound` — typing a
  letter inserts it; everything else does nothing.
- **Key press during `Phase::Resuming`**: actions flow through the same
  streams; most are absorbed because the editor has no active tab yet.
- **Context flip mid-chord**: if `Ctrl-x` sets the chord cell, then a
  state change activates `file_search` before the second press — the
  second press is still looked up under *current* context
  (file_search), and the chord-prefix cell is consulted by
  `lookup_chord(ctrl+x, second)` which is a global-only table. [unclear
  — exact interaction; worth a golden.]
- **`Ctrl-g` and `Esc`** both bind to `abort`; `Esc` is conventional,
  `Ctrl-g` is the Emacs analogue. Used heavily by overlays.
- **Terminal-specific aliases**: `Ctrl-/`, `Ctrl-_`, and `Ctrl-7` all
  map to `undo`. Some terminals emit `Ctrl-7` for `Ctrl-/`.

## Error paths

- **Unknown modifier / key / action in `keys.toml`**: `Keys::into_keymap`
  returns `Err(String)`; `model/mod.rs:163-177` turns it into
  `Alert::Warn`. The previously-loaded `Keymap` stays in place — at
  startup, that's the defaults.
- **TOML structurally invalid** (e.g. a scalar where a sub-table was
  expected, or vice versa): the config-file driver's `toml::from_str`
  error surfaces as `Alert::Info` before `into_keymap` is called. No
  keymap update happens.
- **`shift+a` bound**: no error. Silent mismatch.
- **F-key bound**: parse error → `Alert::Warn`.
- **Duplicate bindings in the same context/section**: the `HashMap`
  overwrites silently — last write wins in iteration order, which is
  unspecified. No warning.
- **Chord prefix also bound as a direct key**: because `direct` and
  `chords` are separate `HashMap`s, binding `ctrl+x = "save"` as a
  scalar while ALSO having `[keys."ctrl+x"]` sub-table — [unclear
  whether TOML deserialization allows both; if both survive,
  `Keymap::lookup` checks `direct` first and returns `Action(Save)`,
  making the chord table unreachable.]
