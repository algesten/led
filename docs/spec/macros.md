# macros

## Summary

`led` supports Emacs-style keyboard macros: a user presses `Ctrl-x (` to
begin recording, performs any sequence of actions, presses `Ctrl-x )` to end
recording, and then `Ctrl-x e` to replay the most recent macro. A digit
prefix typed between `Ctrl-x` and the final `e` sets a repeat count. After
the first `Ctrl-x e`, a transient "repeat mode" lets a bare `e` fire the
macro again, until any other key resets the mode. Macros live entirely in
`AppState.kbd_macro` — they are per-process state and are **not persisted
across restarts**.

## Behavior

State lives in `KbdMacroState` (`crates/state/src/lib.rs:1691-1698`):

```rust
pub struct KbdMacroState {
    pub recording: bool,
    pub current: Vec<led_core::Action>,
    pub last: Option<Vec<led_core::Action>>,
    pub playback_depth: usize,
    pub execute_count: Option<usize>,
}
```

Three actions drive the feature, all handled imperatively by
`handle_action` (`led/src/model/action/mod.rs:22-311`, the `Mut::Action`
mega-dispatcher — explicitly flagged as pending refactor by CLAUDE.md
Principle 9):

### Recording: `Action::KbdMacroStart`

Default binding: `Ctrl-x (`. On dispatch (`action/mod.rs:269-273`):

- `state.kbd_macro.recording = true`
- `state.kbd_macro.current.clear()`
- Alert: `"Defining kbd macro..."`

If the user presses `Ctrl-x (` again while already recording, the inner
branch at `action/mod.rs:32-35` fires instead: `current.clear()`, stay in
recording mode, no alert. This is the "restart recording without ending"
path.

### Appending actions to the recording

While `recording == true` every `Action` received by `handle_action` passes
through the pre-match guard at `action/mod.rs:23-43`. For any action that
is not `KbdMacroEnd`, `KbdMacroStart`, or filtered by `should_record(&action)`,
the action is pushed onto `current` before normal execution. `should_record`
(`action/helpers.rs:100-106`) excludes:

- `Action::Quit`
- `Action::Suspend`
- `Action::Resize(..)`
- `Action::Wait(..)`

These four are the "environment" actions that the user never means to
replay. Everything else — cursor moves, edits, saves, LSP requests,
browser navigation, search — is recorded. The recorded sequence is purely
`Action` values; raw keys, chord prefixes, and digit counts are not
preserved.

### End recording: `Action::KbdMacroEnd`

Default binding: `Ctrl-x )`. On dispatch while recording
(`action/mod.rs:26-31`):

- `state.kbd_macro.recording = false`
- `state.kbd_macro.last = Some(std::mem::take(&mut state.kbd_macro.current))`
- Alert: `"Keyboard macro defined"`

If dispatched while *not* recording (`action/mod.rs:274-276`):

- Alert: `"Not defining kbd macro"`
- No other state change.

This means `last` is overwritten on every successful end. There is exactly
one macro slot — no macro register ring or naming.

### Playback: `Action::KbdMacroExecute`

Default binding: `Ctrl-x e`. On dispatch (`action/mod.rs:277-305`):

1. If `playback_depth >= 100`: alert `"Keyboard macro recursion limit"`,
   return `false` (aborts further playback up the stack).
2. If `last.is_none()`: alert `"No kbd macro defined"`, return `false`.
3. Let `count = state.kbd_macro.execute_count.take().unwrap_or(1)`.
4. `iterations = if count == 0 { usize::MAX } else { count }`. A zero count
   means "run forever until the macro fails"; this is reachable by typing
   `Ctrl-x 0 e`.
5. `playback_depth += 1`.
6. Clone `last.as_ref().unwrap()` into a local `actions` vector.
7. For `_ in 0..iterations`: for each action `a` in `actions`: recursively
   call `handle_action(state, a.clone())`. If any call returns `false`,
   break out of both loops.
8. `playback_depth -= 1`.
9. Return `false` if any inner call failed, else `true`.

Because playback re-enters `handle_action`, a macro can invoke *another*
macro execute — hence the `playback_depth` guard at 100 to protect the
stack.

### Count prefix and repeat mode

Count and repeat mode are transient state in `actions_of.rs`, held in
`Cell`s that live for the duration of a keymap session (not inside
`AppState`):

- `chord_count: Cell<Option<usize>>` (`actions_of.rs:83-90`) — while a
  chord prefix is active (e.g. after `Ctrl-x`), bare digits `0..=9` are
  accumulated into a decimal count. The chord prefix remains active;
  digits do not consume it.
- When the terminal chord key resolves to `Action::KbdMacroExecute`
  (`actions_of.rs:96-105`), the code emits **two** Muts in order:
  `Mut::KbdMacroSetCount(n)` (if count is non-empty) followed by
  `Mut::Action(Action::KbdMacroExecute)`. The reducer sets
  `state.kbd_macro.execute_count = Some(n)` (`model/mod.rs:655-657`), and
  the subsequent `handle_action` call reads and clears it via `.take()`.
- `macro_repeat: Cell<bool>` (`actions_of.rs:64, 97`) — set to `true` as
  soon as a `KbdMacroExecute` is dispatched. While true, the chord
  resolver short-circuits (`actions_of.rs:66-74`): a bare `e` (no Ctrl, no
  Alt, `KeyCode::Char('e')`) directly emits another `Mut::Action(KbdMacroExecute)`
  without going through the keymap. Any other key clears the flag and
  falls through to normal chord handling.

So `Ctrl-x 3 e` plays three times; `Ctrl-x e e e` plays four times total
(first Ctrl-x e, then three more via repeat mode).

### Interaction with other state

- **Alerts**: every state transition surfaces an info alert
  (`"Defining kbd macro..."`, `"Keyboard macro defined"`, `"Not defining
  kbd macro"`, `"No kbd macro defined"`, `"Keyboard macro recursion limit"`).
  Per `docs/extract/driver-events.md` § timers, alerts auto-clear via the
  3s `alert_clear` timer.
- **Modals**: macro playback is recursive `handle_action`, which respects
  the same pre-match guards as any key-originated action. Completion
  popup, code actions, rename, file-search, find-file, confirm-kill
  prompts, isearch, and blocking overlays all intercept actions normally
  during playback. A macro recorded with a completion popup open will not
  reproduce the popup on replay unless the same identifier char triggers
  it again.
- **Pending indent**: if a buffer has `pending_indent_row` set,
  `is_editing_action` guard at `action/mod.rs:60-66` drops mutating
  actions silently. During macro replay this could drop recorded edits —
  effectively a race between the async indent round-trip and replay
  speed.
- **Undo**: each recorded action goes through the normal edit path and
  interacts with the undo system; a long macro replay can produce dozens
  of undo entries (grouped per `EditKind` transition).
- **Session persistence**: `KbdMacroState` is **not** serialized to
  `session_kv` or anywhere else (see `persistence.md`). Macros do not
  survive a quit. This is consistent with Emacs' default behavior.
- **Recording `KbdMacroExecute`**: if the user records a macro that
  *itself* invokes `KbdMacroExecute`, then replays it, the nested execute
  will play whatever `last` held at replay time — not what it held at
  record time. There's only one slot.

### Limits

- `playback_depth` hard cap: **100** (`action/mod.rs:278`). Exceeding it
  aborts playback with an alert.
- `execute_count == 0` → `usize::MAX` iterations (approx. 1.8e19 on
  64-bit). Real use relies on inner failure (e.g. hitting buffer end)
  to stop. There is no wall-clock cap and no keyboard interrupt during
  playback; the model is single-threaded so a runaway macro blocks the
  UI. `[unclear — is this intentional, or a rewrite should add an `Esc`
  interrupt?]`
- Macro buffer length: unbounded `Vec<Action>`. In practice, tens to low
  hundreds of entries from a typical recording session.

## User flow

Typical session:

1. `Ctrl-x (` — status bar shows `Defining kbd macro...`. User edits.
2. Type some text, move cursor, execute actions.
3. `Ctrl-x )` — status bar shows `Keyboard macro defined`.
4. `Ctrl-x e` — macro plays once.
5. `e` (bare) — plays again (repeat mode still active).
6. Any other key (e.g. cursor move) — repeat mode cleared; next `e` types
   a literal `e`.
7. `Ctrl-x 5 e` — plays five times, resetting the digit count after.

Error path: `Ctrl-x e` with no macro yet → alert `"No kbd macro defined"`.

## State touched

- `AppState.kbd_macro: KbdMacroState` — the only field owned by this
  feature.
- `AppState.alerts.info` — written for every user-facing status line.
- Every other field can be touched *by* a playing macro (since playback
  is just recursive action dispatch) but only indirectly.
- `actions_of.rs` local `Cell`s: `chord`, `chord_count`, `macro_repeat`
  — transient, outside `AppState`.

## Extract index

- Actions: `KbdMacroStart`, `KbdMacroEnd`, `KbdMacroExecute` →
  `docs/extract/actions.md` § Macros.
- Keybindings: `Ctrl-x (`, `Ctrl-x )`, `Ctrl-x e`; bare `e` (in repeat
  mode); digits `0..9` (in chord prefix) → `docs/extract/keybindings.md`.
- Config keys: `kbd/kbd_macro_start`, `kbd/kbd_macro_end`,
  `kbd/kbd_macro_execute` → `docs/extract/config-keys.md`.
- Muts: `Mut::Action(KbdMacro*)`, `Mut::KbdMacroRecord(Action)` (record
  path from `mod.rs:265-268`), `Mut::KbdMacroSetCount(usize)`.
- Timers: `alert_clear` — indirectly, via the alert on each transition.
- No driver events: macros never cross the driver boundary.

## Edge cases

- **End without start**: `Ctrl-x )` → alert `"Not defining kbd macro"`.
- **Execute without end**: `Ctrl-x e` while still recording. Per
  `action/mod.rs:23-43`, the pre-match guard doesn't special-case
  `KbdMacroExecute`; `should_record(KbdMacroExecute)` is `true`, so
  it is appended to `current` *and* falls through to the match arm that
  plays `last` (the previously defined macro, if any). So recording a
  macro that invokes the previous macro is possible.
- **Start while recording**: clears `current`, stays recording, no alert
  — see `action/mod.rs:32-35`.
- **Nested execute**: legal up to depth 100. `playback_depth` counter
  protects the stack.
- **Zero count**: `Ctrl-x 0 e` → `iterations = usize::MAX`. Runs until
  an inner action fails (returns `false` from `handle_action`) or until
  the heat death of the universe.
- **Count without execute**: `Ctrl-x 3` followed by any non-`e`
  chord-terminating key (e.g. `Ctrl-x 3 s`) — `chord_count` is consumed
  when the chord resolves; for most actions it is discarded
  (`keybindings.md:301-305`).
- **Repeat mode after a failed execute**: `macro_repeat` is set at
  chord-resolve time, before the Mut is dispatched. If the execute fails
  (no macro defined, recursion limit), the flag is still set — next bare
  `e` will retry. `[unclear — intentional? Feels like a bug.]`
- **Recording LSP-driven edits**: a macro that records `Action::LspFormat`
  will re-trigger format on replay, potentially producing different
  output if the server's state has changed.
- **Recording over a modal**: recording during a completion popup
  records the individual actions (`LspCompletionAction`s, cursor moves)
  but not the popup itself. Replay will not reopen the popup unless the
  same characters re-trigger it.

## Error paths

- **Recursion limit**: depth 100 → alert, playback aborts, `false`
  returned up the stack.
- **No macro defined at execute time**: alert, `false` returned.
- **Inner action fails**: e.g. saving to a read-only file, or a move
  action reaches buffer end. `handle_action` returns `false`; the outer
  loop breaks. Remaining iterations are skipped.

## Gaps

- `[unclear — runaway-playback interrupt]`: no user-initiated stop
  mechanism (no `Esc` handler during playback, since the thread is busy
  dispatching). In practice users avoid `Ctrl-x 0 e` for this reason.
- `[unclear — persisted macros]`: not implemented. Other editors persist
  named macros; `led` has one unnamed slot.
- `[unclear — `macro_repeat` latching after failure]`: see Edge cases.
  May want to gate on successful execution.
- `[unclear — recording of `Action::InsertChar` during completion]`:
  char insertions made while a popup is open drive the popup's filter,
  not the buffer. On replay without a popup, the same chars reach the
  buffer. This is a semantic gap that may confuse users.
