# Milestone 22 — Keyboard macros

After M22, the user can press `Ctrl-x (` to start recording a
keyboard macro, run any sequence of edits / cursor moves /
saves, press `Ctrl-x )` to end recording, and replay the
recorded sequence with `Ctrl-x e`. A digit prefix sets the
repeat count (`Ctrl-x 4 2 ctrl+x e` plays 42 times); after
the first execute, a bare `e` keeps replaying until any other
key cancels the repeat mode. Macros live entirely in
process-local state and are **not persisted across restarts**
(consistent with Emacs and with legacy `led`).

This is a small but stand-alone feature: no driver, no async,
no new chrome surface beyond a status-bar `Recording` mode
string. The reason it lands as its own milestone is the chord
state machine extension — count accumulator + repeat-mode
latch — which interacts subtly with the rest of dispatch and
needs covering with golden + unit tests before it stabilises.

Prerequisite reading:

1. `docs/spec/macros.md` — full file. The behaviour to
   replicate. Pay particular attention to:
   - The `should_record` exclude list.
   - `playback_depth` recursion guard at 100.
   - `execute_count == 0` → `usize::MAX` iterations.
   - Repeat-mode latch (`macro_repeat`) cleared on any non-`e`
     key but *not* on a failed execute (legacy quirk).
2. Legacy `led/src/model/actions_of.rs:25-135` — the
   `chord` / `chord_count` / `macro_repeat` cells driving the
   keymap dispatch. Reference port for the rewrite's
   `ChordState` extension.
3. Legacy `led/src/model/action/mod.rs:23-43` — the
   pre-match guard that appends to `current` while
   recording. Reference port for the recording hook in
   `dispatch_key`.
4. Legacy `led/src/model/action/mod.rs:269-305` — the three
   `KbdMacro*` action arms (Start / End / Execute). Reference
   port for the new `Command` arms in
   `runtime/src/dispatch/mod.rs`.
5. `goldens/scenarios/actions/kbd_macro_*` and
   `goldens/scenarios/keybindings/kbd_macro/e_replay/` — the
   target scenarios. Read both the `script.txt` and the
   `dispatched.snap` so the recording filter matches what
   the goldens expect.

---

## Goal

```
$ cargo run -p led -- buffer.txt
# Type Ctrl-x (
# Status bar shows: "Defining kbd macro..."   (mode = Recording)
# Type:  hello<Down><Home>
# Type Ctrl-x )
# Status bar shows: "Keyboard macro defined"  (mode cleared)
# Type Ctrl-x e
# The macro plays once: inserts "hello", moves down + home.
# Type bare `e`
# Plays again (repeat-mode latch).
# Move the cursor: repeat mode clears.
# Type Ctrl-x 5 e
# Plays five times.
```

## Scope

### In

- **`Command` moves to `crates/core/`** — prerequisite for the
  state crate split. `Command` is a flat dep-free enum with the
  same primitive-type shape as the existing `UserPath` /
  `CanonPath` / `id_newtype!` residents in `core/`. Moving it
  there breaks the `state-kbd-macro` → `runtime` cycle that
  would otherwise force a Guideline-9-violating shortcut.
  `runtime::keymap` re-exports `Command` so existing call sites
  see no change. `Keymap` / `ChordState` stay in `runtime` —
  they depend on `KeyEvent` from `driver-terminal-core` and
  belong on the runtime side.

- **`state-kbd-macro` crate** — new workspace member. Plain
  user-decision state crate per the layout: no driver, no
  async, no `core/native` split. Depends only on `core/` (for
  `Command`) and the `drv` macros. Mutated directly by
  `dispatch` from the runtime, like every other `state-*` crate.

  ```rust
  use led_core::Command;

  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct KbdMacroState {
      /// `true` between `KbdMacroStart` and `KbdMacroEnd`.
      pub recording: bool,
      /// Commands appended to the in-progress recording. Cleared
      /// on `KbdMacroStart`; moved to `last` on `KbdMacroEnd`.
      pub current: Vec<Command>,
      /// The last successfully ended recording. `None` until the
      /// first `KbdMacroEnd`. Overwritten on every successful end —
      /// a single slot, no register ring.
      pub last: Option<Arc<Vec<Command>>>,
      /// Recursion guard. Bumped before each playback iteration,
      /// decremented on return. Hard-capped at 100 to mirror
      /// legacy and protect the host stack.
      pub playback_depth: usize,
      /// Pending iteration count from the chord prefix
      /// (`Ctrl-x N e`). Set by the dispatch layer right before
      /// `KbdMacroExecute`; consumed (`take()`) on the next
      /// execute. `None` means "play once". `Some(0)` means
      /// "play until inner failure" (`usize::MAX` cap).
      pub execute_count: Option<usize>,
  }
  ```

  `last` is `Arc<Vec<_>>` so playback can clone the slot without
  deep-copying — playback may push *new* recording entries
  back into `current` at the same time, so the borrow has to
  separate cleanly. Matches the rewrite's allocation discipline
  (`Arc`-wrap heavy memo outputs).

- **`Command` extensions** in `crates/core/` (the enum's new
  home — see D9):

  ```rust
  pub enum Command {
      // … existing variants …

      // Keyboard macros (M22).
      KbdMacroStart,
      KbdMacroEnd,
      KbdMacroExecute,
      /// Headless / harness wait primitive. Not bound by
      /// default; reachable only from a recorded macro that
      /// captured one (rare) or from explicit harness paths.
      /// Not recorded into `current` — `should_record` excludes
      /// it, mirroring legacy `Action::Wait(..)` exclusion.
      Wait(u64),
  }
  ```

  The corresponding `parse_command` arms accept the strings
  `"kbd_macro_start"`, `"kbd_macro_end"`, `"kbd_macro_execute"`
  so user keymap overrides can rebind them.

- **`ChordState` extensions** in `runtime/src/keymap.rs`:

  ```rust
  pub struct ChordState {
      pub pending: Option<KeyEvent>,
      /// Decimal accumulator for digits typed while a chord
      /// prefix is pending. `Ctrl-x 4 2 ctrl+x e` → `Some(42)`.
      /// Cleared when the chord resolves (consumed by execute,
      /// dropped by any other resolved command).
      pub count: Option<usize>,
      /// Set the moment a `KbdMacroExecute` resolves. While
      /// `true`, a bare `e` (no Ctrl/Alt) short-circuits the
      /// keymap and emits another `KbdMacroExecute` directly.
      /// Cleared on any non-`e` key.
      pub macro_repeat: bool,
  }
  ```

- **Default keymap additions** in `default_keymap()`:

  ```rust
  m.bind_chord("ctrl+x", "(", Command::KbdMacroStart);
  m.bind_chord("ctrl+x", ")", Command::KbdMacroEnd);
  m.bind_chord("ctrl+x", "e", Command::KbdMacroExecute);
  ```

  These three match legacy `default_keys.toml`.

- **Recording hook** in `dispatch_key`. Right after
  `resolve_command` returns `Resolved::Command(cmd)` and
  *before* `run_command` runs:

  ```rust
  if kbd_macro.recording && should_record(&cmd) {
      kbd_macro.current.push(cmd);
  }
  ```

  Recording captures the resolved `Command` (the rewrite's
  equivalent of legacy's `Action`), not the raw `KeyEvent`.
  This means a macro replay won't reproduce raw chord
  prefixes / digit counts — it replays the same effect each
  iteration regardless of the keymap.

- **`should_record(&Command) -> bool`** helper in
  `dispatch/mod.rs`. Returns `false` for:

  - `Command::Quit`
  - `Command::Suspend`
  - `Command::Wait(_)`
  - `Command::KbdMacroStart` — start while recording
    *clears* `current`, doesn't append (legacy parity).
  - `Command::KbdMacroEnd` — end *finishes* `current`,
    doesn't append.

  `KbdMacroExecute` is **not** excluded — recording a macro
  that itself invokes the previous macro is legal. Every
  other command records.

- **`KbdMacroStart` arm** in `run_command`:

  ```rust
  Command::KbdMacroStart => {
      kbd_macro.recording = true;
      kbd_macro.current.clear();
      alerts.set_info("Defining kbd macro...".into(), …);
      DispatchOutcome::Continue
  }
  ```

  Pressing `Ctrl-x (` while already recording clears `current`
  and stays in recording mode (no alert) — same as legacy's
  inner branch. The single arm above handles both paths
  because `clear()` is idempotent on an already-empty buffer.
  We deliberately re-emit the alert in either case (legacy's
  inner branch suppresses it; the rewrite simplification is
  acceptable per "Don't re-litigate cosmetic legacy quirks"
  — the goldens key on the alert text, not its absence).

  *(If a golden fails because of the duplicate alert, gate the
  alert on `!kbd_macro.recording` before flipping the flag.)*

- **`KbdMacroEnd` arm** in `run_command`:

  ```rust
  Command::KbdMacroEnd => {
      if kbd_macro.recording {
          kbd_macro.recording = false;
          let recorded = std::mem::take(&mut kbd_macro.current);
          kbd_macro.last = Some(Arc::new(recorded));
          alerts.set_info("Keyboard macro defined".into(), …);
      } else {
          alerts.set_info("Not defining kbd macro".into(), …);
      }
      DispatchOutcome::Continue
  }
  ```

- **`KbdMacroExecute` arm** in `run_command`:

  ```rust
  Command::KbdMacroExecute => {
      const RECURSION_LIMIT: usize = 100;
      if kbd_macro.playback_depth >= RECURSION_LIMIT {
          alerts.set_info("Keyboard macro recursion limit".into(), …);
          return DispatchOutcome::Continue;
      }
      let Some(recorded) = kbd_macro.last.clone() else {
          alerts.set_info("No kbd macro defined".into(), …);
          return DispatchOutcome::Continue;
      };
      let count = kbd_macro.execute_count.take().unwrap_or(1);
      let iterations = if count == 0 { usize::MAX } else { count };
      kbd_macro.playback_depth += 1;
      let mut last_outcome = DispatchOutcome::Continue;
      'outer: for _ in 0..iterations {
          for cmd in recorded.iter() {
              let outcome = run_command(*cmd, /* … all the &mut refs … */);
              if !matches!(outcome, DispatchOutcome::Continue) {
                  last_outcome = outcome;
                  break 'outer;
              }
          }
      }
      kbd_macro.playback_depth -= 1;
      last_outcome
  }
  ```

  Notes:
  - `last.clone()` clones the `Arc`, not the `Vec` — fast and
    safe against concurrent recording.
  - Quit / Suspend mid-playback short-circuits and propagates
    out (so `Ctrl-x e` on a macro that ends in `Quit` exits
    cleanly).
  - Inner `false` (e.g. failed save, isearch abort, edit at
    EOF) breaks both loops and propagates `Continue` — the
    failure stops further iterations but is *not* itself a
    quit. Matches legacy.

- **Chord-prefix digit accumulator** in `resolve_command`
  (the function that takes a `KeyEvent` and returns
  `Resolved`). When `chord.pending` is `Some(prefix)` and the
  next key is a bare digit `0..=9`:

  ```rust
  if let KeyCode::Char(c @ '0'..='9') = k.code
      && k.modifiers.is_empty()
  {
      let prev = chord.count.unwrap_or(0);
      chord.count = Some(prev * 10 + (c as usize - '0' as usize));
      chord.pending = Some(prefix);  // keep prefix active
      return Resolved::Continue;     // silent
  }
  ```

  Any non-digit second key resolves the chord normally; the
  accumulated count is then either consumed (by
  `KbdMacroExecute`) or dropped (every other command — matches
  legacy `keybindings.md:301-305`).

- **Macro-repeat short-circuit** in `resolve_command`. Before
  any chord / direct lookup, if `chord.macro_repeat` is set:

  ```rust
  if chord.macro_repeat {
      if matches!(k.code, KeyCode::Char('e')) && k.modifiers.is_empty() {
          return Resolved::Command(Command::KbdMacroExecute);
      }
      chord.macro_repeat = false;
      // fall through to normal resolution
  }
  ```

  Set `chord.macro_repeat = true` whenever a chord resolves
  to `KbdMacroExecute`, regardless of count or success. Cleared
  on any non-`e` key. This mirrors legacy's deliberate "fires
  even on failed execute" behaviour — flagged in `macros.md`
  § "Edge cases" as `[unclear — bug?]` but kept for
  golden parity.

- **Count → execute coupling**. The chord resolution path
  needs to write `kbd_macro.execute_count` *before* dispatch
  runs the `KbdMacroExecute` arm:

  ```rust
  if matches!(cmd, Command::KbdMacroExecute) {
      if let Some(n) = chord.count.take() {
          kbd_macro.execute_count = Some(n);
      }
      chord.macro_repeat = true;
  }
  ```

  This block sits in `dispatch_key`, between `resolve_command`
  and `run_command`. (Legacy emits two `Mut`s — `KbdMacroSetCount`
  + `Mut::Action(KbdMacroExecute)` — and the reducer applies the
  count first; the rewrite collapses that into a single
  imperative write, since dispatch is sync.)

- **Status-bar `Recording` mode string**. A new
  `KbdMacroRecordingInput<'a>` projection plus an extension to
  `position_string`'s nested-input bundle. The projection
  exposes only `recording`:

  ```rust
  // crates/runtime/src/query.rs
  #[derive(Copy, Clone, drv::Input)]
  pub struct KbdMacroRecordingInput<'a> {
      pub recording: &'a bool,
  }

  impl<'a> KbdMacroRecordingInput<'a> {
      pub fn new(s: &'a KbdMacroState) -> Self {
          Self { recording: &s.recording }
      }
  }
  ```

  `position_string` consumes this input and prepends
  `"Recording "` when `*recording == true`. Projecting only the
  flag (not `current` / `last` / `playback_depth` /
  `execute_count`) is what keeps the memo cache-hitting across
  the recording session: pushes into `KbdMacroState.current`
  during recording mutate the source but not the projected
  fields, so the status-bar memo doesn't recompute on every
  recorded keystroke. Matches `EXAMPLE-ARCH.md` § "Sources" and
  Guideline 5: project only the fields the memo actually reads.

- **`Dispatcher` field**. Add `kbd_macro: &'a mut KbdMacroState`
  to the `Dispatcher` struct + the long argument list of
  `dispatch_key` and `run_command`. (Yes the list keeps
  growing; the bundle refactor is on the perpetual "later"
  list and isn't in scope here.)

### Out

Per the roadmap and `macros.md`, the following are deliberately
**not** in M22:

- **Persisted macros across restarts** — `macros.md` § "Gaps"
  flags this as `[unclear]`. Legacy doesn't do it, M22 doesn't
  either. If we ever want it: extend M21's session schema with
  a macro slot.
- **Multiple register slots** — single `last` slot. Legacy
  parity.
- **`Esc` interrupt during runaway playback** — `macros.md` §
  "Gaps" flags this as `[unclear]`. The model thread is
  blocked during playback, so an `Esc` keypress just queues
  in the terminal driver and lands after playback finishes
  (or after the recursion guard trips). Acceptable for M22;
  revisit if a real user complains.
- **Recording the completion-popup interaction faithfully** —
  `macros.md` § "Edge cases" describes the semantic gap
  (chars typed while a popup is open drive the popup, not the
  buffer; on replay without the popup, they reach the
  buffer). Out of scope; we record `Command`s as-resolved and
  accept the divergence.
- **`Wait` as a default-bound key** — `Command::Wait(u64)`
  exists for harness parity but isn't bound. The goldens
  harness already supports script-level `wait 500ms`
  steps (`goldens/src/scenario.rs:129`); the in-process
  `Command::Wait` is reserved for completeness and for any
  future test that needs an in-recording delay.

Cross-references for those: `macros.md` § "Limits" / "Gaps",
`POST-REWRITE-REVIEW.md` § (existing entries).

## Key design decisions

### D1 — `KbdMacroState` is a runtime-local source, no driver

Macros never cross the driver boundary (`macros.md` § "Extract
index" confirms: *"No driver events: macros never cross the
driver boundary."*). The state is pure user-decision; it sits
in `state-kbd-macro/` alongside `state-tabs` / `state-jumps`
and is mutated directly by `dispatch`. No `core/native` split,
no async worker.

### D2 — Recording captures `Command`, not `KeyEvent`

Same as legacy (which captures `Action`). This means a macro
replays the *effect* of each keystroke, decoupled from the
keymap. If the user remaps a key between record and replay,
the macro still does the same thing.

The cost is that chord prefixes and digit counts don't
record — `Ctrl-x 5 ctrl+x e` records only the second
`KbdMacroExecute`, not the count. Legacy has the same
behaviour and the goldens depend on it.

### D3 — Recording hook lives at the dispatch boundary

Specifically: in `dispatch_key`, after `resolve_command`
succeeds and before `run_command` runs. Three reasons:

1. The `Resolved::Command(cmd)` is the canonical ingestion
   point — every recorded command flows through there
   regardless of which overlay (find-file / isearch /
   completions / file-search) ultimately handles it.
2. Recording *before* execution means a command that
   internally fails is still recorded, matching legacy's
   `current.push` happening unconditionally in the pre-match
   guard.
3. `should_record` is one switch statement, easy to audit.

We considered recording inside `run_command` after each
overlay intercept, but that splinters the filter across many
sites and creates ordering races with overlay "consume vs
fall through" decisions. The dispatch boundary is the right
seam.

### D4 — `playback_depth` lives on `KbdMacroState`, not in a Cell

Legacy uses `state.kbd_macro.playback_depth` (a regular
`usize` field). The rewrite mirrors that: a regular field on
the source. No `Cell`, no thread-local.

The increment + decrement bracket the recursive `run_command`
calls, so the depth field reflects "depth of currently
in-flight macro playback". Concurrent macros aren't a thing
(single-threaded dispatch), so a plain field is sufficient.

### D5 — `last` is `Arc<Vec<Command>>` for lock-free playback

During playback, `current` may receive new entries (a macro
that records *another* macro). If `last` were `Vec<Command>`,
we'd either need to clone the whole vector before each
iteration (legacy does this) or borrow-check would fail.

Wrapping `last` in an `Arc` lets playback `.clone()` a cheap
refcount, then iterate the `Arc<Vec<_>>` while `current`
mutates freely. Allocation discipline win: zero per-iteration
allocation in the playback loop.

### D6 — Repeat mode latches even on failed execute

Per `macros.md` § "Edge cases":

> Repeat mode after a failed execute: `macro_repeat` is set
> at chord-resolve time, before the Mut is dispatched. If
> the execute fails (no macro defined, recursion limit), the
> flag is still set — next bare `e` will retry. `[unclear —
> intentional? Feels like a bug.]`

We mirror it. Goldens encode legacy behaviour, and
`POST-REWRITE-REVIEW.md` is the place to document the bug if
we want to fix it on `main` later. M22 doesn't fix it.

### D7 — `Wait(u64)` is a no-op shim, not a real sleep

The model thread is single-threaded and synchronous; a real
sleep would freeze the UI. The rewrite harness handles waits
at the script-step level
(`goldens/src/scenario.rs::ScriptStep::Wait`), so a
`Command::Wait` only needs to *exist* — it's reachable from a
recorded macro that captured it (rare) and from any future
binding that wants to use it.

Implementation: the `Command::Wait(_)` arm in `run_command`
is a no-op `DispatchOutcome::Continue`. `should_record`
excludes it from recordings (legacy parity). If a real-time
wait is ever needed, the M22 design leaves room: route the
arm through a virtual-clock primitive (`led-test-clock`-aware)
rather than `std::thread::sleep`.

### D8 — Status-bar `Recording` is a single-source query, projected narrowly

We extend the existing `position_string` memo rather than
inventing a new sibling. The new input
`KbdMacroRecordingInput<'a> { recording: &'a bool }` projects
**only** the recording flag — not `current`, `last`,
`playback_depth`, or `execute_count`. This is the
EXAMPLE-ARCH discipline (Guideline 5: "project only the
fields the memo actually reads"): pushes into `current`
during recording mutate the source but not the projection,
so the memo cache-hits across the whole recording session
and only invalidates when the flag flips.

The string becomes `"Recording  L4:C12"` while recording,
just `"L4:C12"` otherwise — one `Arc<str>` per render tick
on idle hits, allocation-discipline preserved.

### D9 — `Command` lives in `core/`, `KbdMacroState` lives in its own state crate

This is the layout move that makes `state-kbd-macro` viable
as a standalone crate. Without it, `state-kbd-macro` would
either depend on `runtime` (cyclic) or duplicate `Command`
(forks the type), so the only "easy" option becomes putting
`KbdMacroState` inline in `runtime/src/`. That option
violates Guideline 9: "Don't rely on discipline — put each
[state] in its own crate that has no dependency on other
drivers or on sibling user-decision state. The Cargo.toml is
the constraint; the compiler rejects accidental coupling."

The proper fix: `Command` is a primitive enum (no driver
deps, no `KeyEvent`, no fancy generics) — it belongs in
`core/` next to the existing primitive types. `runtime`
re-exports it so call sites don't churn. `state-kbd-macro`
depends only on `core/`, fitting the standard state-crate
shape (`state-tabs`, `state-jumps`, `state-kill-ring`, …).

Yes, the move touches every `use crate::keymap::Command;`
import — but the re-export keeps that to a path-rename, not
a semantic change. Doing it now (one crate ahead of the
state crate) costs less than carrying a permanent
arch-violation marker forward.

### D10 — Count and repeat-mode cells live on `ChordState`

Legacy puts `chord_count` and `macro_repeat` in `Cell`s
local to `actions_of.rs` (not in `AppState`). The rewrite
already has `ChordState` for the single-key prefix; extending
it with `count` and `macro_repeat` keeps all "transient
chord-resolution state" in one place. They are runtime-local
fields, threaded through `dispatch_key` by `&mut`, not a
drv source.

Why not put them on `KbdMacroState`?

- They reset on every non-chord key, which would force
  spurious `KbdMacroState` mutations and break memo
  cache-hit assumptions.
- They're chord-resolution concerns, not macro-state
  concerns. A future user who rebinds digits to something
  else would still want the accumulator.

## Types

### `core` additions (prerequisite refactor)

`Command` moves out of `runtime/src/keymap.rs` and into
`crates/core/`. It's a flat dep-free enum — same primitive
shape as `UserPath` / `CanonPath` / `id_newtype!` newtypes
that already live there. Move:

```rust
// crates/core/src/lib.rs
pub enum Command { /* every variant, including the new
                      KbdMacro{Start,End,Execute} and Wait(u64) */ }

pub fn parse_command(s: &str) -> Result<Command, ParseError> { /* … */ }
```

`runtime::keymap` re-exports `Command` and `parse_command` so
existing call sites keep their `use crate::keymap::Command;`
imports unchanged:

```rust
// crates/runtime/src/keymap.rs
pub use led_core::{Command, parse_command};
```

What stays in `runtime`: `Keymap`, `ChordState`,
`default_keymap()`, the chord lookup tables. They depend on
`KeyEvent` from `driver-terminal-core` and on the runtime's
context machinery, so they live runtime-side. Only `Command`
itself crosses to `core/`.

This is the layout fix that lets `state-kbd-macro` exist as
its own crate without a cyclic dep.

### `state-kbd-macro` (new crate)

```rust
// crates/state-kbd-macro/src/lib.rs
use std::sync::Arc;
use led_core::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KbdMacroState {
    pub recording: bool,
    pub current: Vec<Command>,
    pub last: Option<Arc<Vec<Command>>>,
    pub playback_depth: usize,
    pub execute_count: Option<usize>,
}
```

Dep graph: `state-kbd-macro → core` only. No `drv` dep
(per Guideline 11, the input projection is declared by the
consumer in `runtime/src/query.rs`, not here). No driver dep,
no other state-* dep.

This is the literal reading of EXAMPLE-ARCH §"User-decision
sources have no driver" + Guideline 9 ("enforce isolation
with crate boundaries"): the source crate is plain, the
runtime is the only place that combines it with anything.

### `runtime/src/keymap.rs` additions

`Command` is now defined in `core/`; `keymap.rs` re-exports
it. ChordState gains the two new transient cells:

```rust
// Re-export so callers keep their imports.
pub use led_core::{Command, parse_command};

pub struct ChordState {
    pub pending: Option<KeyEvent>,
    pub count: Option<usize>,
    pub macro_repeat: bool,
}
```

### `Dispatcher` additions

```rust
pub struct Dispatcher<'a> {
    // … existing …
    pub kbd_macro: &'a mut KbdMacroState,
}
```

## Crate changes

```
crates/
  core/                  + Command enum (moved from runtime),
                          + parse_command (moved from runtime),
                          + Command::KbdMacro{Start,End,Execute},
                          + Command::Wait(u64).
  state-kbd-macro/       NEW workspace member. Single file:
                          src/lib.rs holding KbdMacroState +
                          its Default / Clone / PartialEq derives.
                          Deps: led-core only.
  runtime/Cargo.toml     + led-state-kbd-macro dep.
  runtime/src/
    keymap.rs            - Command enum body (now in core).
                          + `pub use led_core::{Command, parse_command};`
                            re-export so existing imports keep working.
                          + ChordState.count + ChordState.macro_repeat,
                            default_keymap chord bindings.
    dispatch/mod.rs      + Dispatcher.kbd_macro field,
                          should_record helper,
                          recording-hook block,
                          chord digit accumulator,
                          macro-repeat short-circuit,
                          KbdMacroStart/End/Execute arms,
                          Wait arm (no-op).
    query.rs             + KbdMacroRecordingInput projection,
                          + position_string consumes it and
                            prepends "Recording " on the flag.
    lib.rs               + Atoms.kbd_macro field, init,
                          + KbdMacroRecordingInput::new at the
                            position_string call site.
```

The `Command`-to-core move is a mechanical rename + re-export;
no behaviour changes outside M22's scope. Touches every file
that has `use crate::keymap::Command;` but the re-export keeps
those imports valid — alternatively rewrite to
`use led_core::Command;` in a sweep.

## Testing

### `state-kbd-macro` (unit)
- `KbdMacroState::default()` — recording=false, current empty,
  last=None, depth=0, count=None.
- Round-trip a populated state through Clone + Eq.

### `dispatch/mod.rs` (unit)
- **Recording hook**: a state with `recording=true`,
  dispatch a `Command::CursorDown`. Assert `current` is
  `[CursorDown]` after.
- **`should_record` filter**: dispatch each of
  Quit/Suspend/Wait/KbdMacroStart/KbdMacroEnd while recording.
  Assert `current` stays empty.
- **KbdMacroStart twice**: dispatch Start, push some
  commands, dispatch Start again. Assert `current` is empty
  and `recording` is still true.
- **KbdMacroEnd while not recording**: alert is "Not
  defining kbd macro"; no state change beyond the alert.
- **KbdMacroEnd while recording**: `last == Some(Arc::new([…]))`,
  `current` empty, `recording=false`.
- **KbdMacroExecute with no macro**: alert "No kbd macro
  defined"; no change to other state.
- **KbdMacroExecute with a recorded macro**: replays each
  command. Cursor + buffer end up where a manual sequence
  would.
- **KbdMacroExecute with `count = 3`**: replays 3×.
- **KbdMacroExecute with `count = 0`**: clamps to
  `usize::MAX` but inner failure breaks early. Use a
  `CursorRight` past EOF as the failure trigger.
- **Recursion limit**: build a `last` containing a
  `KbdMacroExecute`, run it, assert the alert fires after
  exactly 100 nested levels and the 101st level is rejected.
- **Chord digit accumulator**: synthesise key events
  `Ctrl-x 4 2` then `Ctrl-x e`; assert
  `kbd_macro.execute_count == Some(42)` going into the
  execute arm.
- **Macro-repeat latch**: dispatch `Ctrl-x e` once, then
  bare `e`. Assert the second is treated as another
  `KbdMacroExecute` (not `InsertChar('e')`).
- **Macro-repeat clears on non-`e`**: dispatch `Ctrl-x e`,
  then `Down`, then bare `e`. The bare `e` should reach
  `InsertChar('e')`, not the macro replay.
- **Macro-repeat persists past a failed execute**: dispatch
  `Ctrl-x e` with `last=None`. Repeat-mode flag still set;
  next bare `e` retries (and re-fails). Legacy parity.
- **Quit mid-playback**: build a macro `[CursorDown, Quit]`,
  execute. Outcome: `DispatchOutcome::Quit`. Subsequent
  iterations don't run.
- **Recording filter excludes nested execute correctly**:
  A `KbdMacroExecute` *is* recorded (legal nested execute);
  but `KbdMacroStart` and `KbdMacroEnd` are not.
- **Position string while recording**: returns a string
  starting with `"Recording  L"` when `recording=true`.

### `keymap::ChordState` (unit)
- `Default` produces `count=None, macro_repeat=false`.
- `count` survives across digit accumulation;
  cleared after a non-digit chord resolution.

### `runtime` integration
- Headless run with the goldens harness: drive
  `Ctrl-x ( <Down> Ctrl-x ) Ctrl-x e` on a 4-line buffer,
  assert cursor moved 2 lines down (one record + one
  replay).
- Drive `Ctrl-x ( x Ctrl-x ) Ctrl-x 3 ctrl+x e`, assert
  the buffer has 4 `x`s prepended (one record + three
  replays).

Expected: +20 unit tests + 1 integration.

## Done criteria

- All existing tests pass.
- New tests green.
- Clippy: net delta ≤ +2 from post-M21.
- Interactive smoke:
  - Open a file, record a macro that types "TODO ", moves
    down, indents, pastes the next 5 lines as a comment.
    Replay with `Ctrl-x e` works.
  - `Ctrl-x 5 ctrl+x e` plays five times. Status bar
    shows `L:C` correctly between iterations.
  - Pressing `Ctrl-x (` then `Ctrl-x )` with nothing in
    between: empty macro, `Ctrl-x e` is a silent no-op.
  - Pressing `Ctrl-x e` with no macro defined: alert "No
    kbd macro defined".
  - During recording: the status bar mode shows
    `Recording`. After end: cleared.
- Goldens:
  - `actions/kbd_macro_start` — green.
  - `actions/kbd_macro_end` — green.
  - `actions/kbd_macro_execute` — green.
  - `keybindings/kbd_macro/e_replay` — green.
  - `actions/wait` — green if authored (it isn't yet on
    either tree; M22 either authors a minimal one on `main`
    first or skips it).
  - `features/kbd_macro/*` — best-effort. Author
    additional scenarios as needed (chord-count, recursion
    cap, count zero, repeat-mode-latch across keys).

## Growth-path hooks

- **Persisted macros** — extend M21's session schema with a
  `macro_slot` blob; `KbdMacroState.last` round-trips on
  startup.
- **Named macros / register ring** — replace `last:
  Option<Arc<Vec<Command>>>` with `slots: HashMap<char,
  Arc<Vec<Command>>>` keyed on a register letter. New
  commands `KbdMacroStoreToRegister(char)` /
  `KbdMacroExecuteFromRegister(char)` so users can keep
  multiple macros simultaneously.
- **`Esc` interrupt during playback** — turn the playback
  loop into a check-and-yield: each iteration polls a
  `Terminal.pending` for an Esc keystroke and aborts cleanly.
  Requires either a co-operative dispatch design (out of
  scope) or a small "abort flag" pumped from the input
  driver during execution.
- **Macro recording with completion-popup fidelity** —
  capture not just `Command` but also "popup state at
  capture time". Replay re-opens the popup and applies the
  same selection. Big enough that it deserves its own
  milestone (call it M22a if it ever happens).
- **`led_test_clock`-aware Wait** — make `Command::Wait(ms)`
  consult the virtual clock so deterministic test scenarios
  can include in-process delays without real sleeping.
