# Milestone 20 — Lifecycle: phases, quit, suspend

Twentieth vertical slice. Adds the small state machine (`Phase`)
legacy uses to sequence startup, shutdown, and SIGTSTP suspend,
plus the actual suspend plumbing (leave alt-screen → `SIGTSTP` →
re-enter). Today the rewrite quits on `Ctrl-X Ctrl-C` cleanly but
without a phase transition, and has no suspend at all. M20 closes
that gap and lays a seat for M21 session persistence to slot in
("session flush on Exiting" becomes a one-line addition once the
session driver exists).

Prerequisite reading:

1. `docs/spec/lifecycle.md` — full file. Authoritative reference
   for the state machine, startup sequence, suspend/resume, and
   quit flow.
2. Legacy `led/src/model/process_of.rs` and
   `crates/state/src/lib.rs:33` — where legacy defines
   `Phase` and the Suspended↔Running dance.
3. `crates/driver-terminal/native/src/lib.rs` §`RawModeGuard` —
   the existing acquire/drop RAII. M20 factors out a
   `suspend_and_resume()` helper that shares the same
   enter/leave logic but parks the process between.
4. `ROADMAP.md` § M20 — scope line items and target goldens.

---

## Goal

```
$ cargo run -p led
# Editor running.

# Ctrl-Z (default binding)            →
#   * leaves alt-screen, restores cooked mode
#   * raise(SIGTSTP) — shell prompt returns
#   * `fg` → editor re-enters alt-screen + raw mode, full repaint

# Ctrl-x Ctrl-c (default binding)     →
#   * sets phase = Exiting
#   * main loop exits (for M20, session flush is a no-op; M21
#     adds the real SaveSession dispatch under this flag)
```

## Scope

### In

- **`state-lifecycle` crate** — new workspace member.

  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
  pub enum Phase {
      /// Initial boot: workspace + session load, first paint
      /// hasn't landed yet. In the M20 rewrite this means "no
      /// tick has drained the first frame".
      #[default]
      Starting,
      /// Fully operational — every subsequent tick until
      /// quit / suspend.
      Running,
      /// Suspended by SIGTSTP. The suspend handler parks the
      /// process in place; on `fg` the phase flips back to
      /// Running.
      Suspended,
      /// Quit requested. The main loop breaks on the next
      /// iteration; M21 adds session-flush gating before the
      /// break.
      Exiting,
  }

  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct LifecycleState {
      pub phase: Phase,
      /// Monotonic counter bumped whenever a full repaint is
      /// required (suspend→resume edge, external redraw
      /// requests). The painter diffs against `last_frame`; a
      /// `force_redraw` bump clears `last_frame` so the next
      /// paint emits every cell.
      pub force_redraw: u64,
  }
  ```

- **`Command::Suspend`** — new keymap command; `"suspend"` parse
  case. Default binding: `ctrl+z`.

- **`DispatchOutcome::Suspend`** — new variant alongside
  `Continue` / `Quit`. `Command::Quit` → `DispatchOutcome::Quit`
  (unchanged). `Command::Suspend` → `DispatchOutcome::Suspend`.

- **`suspend_and_resume()`** in `driver-terminal/native`. Free
  function; takes a `&mut dyn Write` for flushing before hand-
  off. Body:

  ```rust
  pub fn suspend_and_resume(out: &mut dyn Write) -> io::Result<()> {
      crossterm::execute!(
          out,
          crossterm::cursor::Show,
          crossterm::terminal::EnableLineWrap,
          crossterm::terminal::LeaveAlternateScreen,
      )?;
      let _ = crossterm::terminal::disable_raw_mode();
      out.flush()?;

      // POSIX-stop this process. Returns when shell's `fg`
      // (SIGCONT) resumes us.
      #[cfg(unix)]
      unsafe { libc::raise(libc::SIGTSTP); }

      crossterm::terminal::enable_raw_mode()?;
      crossterm::execute!(
          out,
          crossterm::terminal::EnterAlternateScreen,
          crossterm::terminal::DisableLineWrap,
      )?;
      Ok(())
  }
  ```

  On non-Unix targets the `libc::raise` path is stubbed out
  (no-op); suspend silently does nothing. Matches legacy
  behaviour.

- **Main-loop integration** — dispatch returns
  `DispatchOutcome::Suspend` → main loop invokes
  `suspend_and_resume()`, bumps `lifecycle.force_redraw`, clears
  its local `last_frame` so the next paint walks every cell.

  Quit path adjusted to set `phase = Exiting` before the
  `break`; M21 adds a save-session dispatch + "wait for
  `SessionSaved`" gate between the set and the break.

- **Starting → Running transition** — in the M20 rewrite this
  happens on the first successful frame emission. No session
  restore, no materialisation wait — if the painter ran, we're
  Running. Lives at the end of the first tick's Render block
  in `run()`.

- **Default binding** — `m.bind("ctrl+z", Command::Suspend)` in
  `default_keymap`.

- **Trace hook** — no new dispatched-intent lines. Suspend is
  an in-process side effect with no driver dispatch, so
  `dispatched.snap` stays unchanged for the scenario. (A
  future iteration could add a `Suspend` trace line for
  debug-log symmetry, but none of the goldens under
  `actions/suspend` expect one.)

### Out

Per the roadmap's M20 scope and the spec:

- **Session flush on Quit** — deferred to **M21** (session /
  persistence). For M20 the `Exiting` phase transition is a
  marker; the actual save-session dispatch slots in once the
  workspace driver exists. The spec's "hold until
  session.saved == true" gate is authored as a placeholder
  (always-true for non-primary / standalone) that M21 tightens.
- **Dirty-buffer confirm on Quit** — explicitly out per spec
  `lifecycle.md` § Quit: "Ctrl-x Ctrl-c on a workspace with
  dirty buffers saves session and exits without prompting."
  The confirm-kill prompt is a separate flow, already
  implemented in M9 (`alerts.confirm_kill` on `KillBuffer` of
  a dirty tab).
- **Full 14-step startup sequence** — most of the steps
  (session restore, DB open, primary flock, watchers) are
  driver-level work landing with M21 (session/persistence)
  and M26 (file watcher). M20 wires in the phase marker only;
  `Starting` is strictly "first tick hasn't painted yet" in
  this branch.
- **`Resuming` phase** — legacy splits startup into
  `Init → Resuming → Running`. The `Resuming` sub-phase is
  meaningful only when there's a session to restore (it gates
  rendering until buffer materialisation catches up). M20
  collapses it into `Starting`; M21 re-introduces `Resuming`
  when it wires session replay.
- **`force_redraw` consumers beyond suspend** — legacy bumps
  this on multiple edges (resize, external signal, sidebar
  toggle). M20 wires only the suspend→resume bump. Other
  edges stay as they are; adding them later is a one-liner at
  each bump site.
- **`Cmd::Abort` global wiring** — legacy dispatches `Abort`
  via the command machinery to close overlays. Our overlays
  already dispatch Esc locally; M20 does not introduce a
  cross-overlay Abort command.
- **Window-close signal (SIGHUP)** — not in legacy, not
  scheduled.

## Key design decisions

### D1 — Phase lives in its own crate (`state-lifecycle`), not on `state-alerts`

Alerts are UI transient state with a short TTL; phase is a
whole-process lifecycle marker. Bundling them invites future
cross-contamination (a phase change invalidating the alert
memo for no reason, or vice versa). A tiny crate keeps the
invalidation surface minimal — `LifecycleStateInput` has two
fields, `phase` and `force_redraw`, and neither moves except
on the specific events that warrant it.

### D2 — `force_redraw` is a counter, not a bool

A bool invites the same bug as "did I forget to clear it?". A
monotonic counter has no "reset" state — a consumer that
caches `last_seen` detects any bump by `current != last_seen`.
Matches legacy's `state.force_redraw: u64` exactly.

### D3 — `DispatchOutcome::Suspend` is a distinct variant

Suspend could have been folded into `Continue` with a side
channel ("also, please suspend"), but that splits the
"what should the loop do next?" answer across two return
values. A third variant keeps the decision single-sourced and
the main loop's match exhaustive.

### D4 — Suspend helper is a free fn, not a method on
RawModeGuard

The guard's `Drop` already runs the "leave alt, disable raw"
sequence on scope-end; factoring that out into an associated
method would cross-contaminate drop order assumptions. A
free function that knows the same sequence keeps the guard
simple. We do NOT drop and re-acquire the guard across
suspend — the guard's presence survives the suspend window,
and the free fn does the minimal reversible transitions in
the middle.

### D5 — Starting → Running on first successful paint

The spec's full 14-step sequence is M21 territory. For M20 we
model the simpler reality: as long as the first `render_frame`
memo produced `Some(frame)`, we're Running. That's a useful
signal even without the rest of the sequence — status-bar
consumers that want "render while Running, suppress while
Starting" already have the data.

### D6 — Quit no-session path: Exiting → break

M20 sets `phase = Exiting`, breaks the loop, and lets the
`RawModeGuard::drop` restore the terminal. Legacy adds a
"hold until session.saved" gate; without a session driver
that gate is trivially true. The main-loop shape under M20
is:

```rust
Command::Quit => {
    lifecycle.phase = Phase::Exiting;
    break;
}
```

M21 changes the break to a gate:

```rust
Command::Quit => {
    lifecycle.phase = Phase::Exiting;
    // fall through — next iteration checks session.saved
}
// ...
if matches!(lifecycle.phase, Phase::Exiting)
    && (session.saved || !workspace.needs_save())
{
    break;
}
```

### D7 — `libc` dependency for SIGTSTP, Unix-only

`crossterm` doesn't expose a SIGTSTP primitive. `libc::raise`
is a 4-line addition and already in the workspace dep set
(`libc = "0.2"`). Gated on `#[cfg(unix)]` so non-Unix targets
(unsupported today) compile cleanly with suspend as a no-op.

### D8 — Suspend doesn't drain the event queue first

The user hit Ctrl-Z *at* this moment. Draining any queued
keys before suspending would blur that timing (keys from
before the suspend might get processed as if they happened
after). Suspend is the one command that takes effect
immediately — right after dispatch returns, before anything
else.

## Types

### `state-lifecycle` (new crate)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Starting,
    Running,
    Suspended,
    Exiting,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifecycleState {
    pub phase: Phase,
    pub force_redraw: u64,
}
```

### Runtime additions

- `Atoms.lifecycle: LifecycleState`
- `DispatchOutcome::Suspend` (new variant)
- Main-loop dispatch match grows a `Suspend` arm
- `Keymap` default gains `ctrl+z → Command::Suspend`
- `parse_command` gains `"suspend"` → `Command::Suspend`

### driver-terminal/native

- `pub fn suspend_and_resume(out: &mut dyn Write) -> io::Result<()>`
  — does the mid-session leave/park/re-enter dance.

## Crate changes

```
crates/
  state-lifecycle/            NEW — Phase, LifecycleState
  driver-terminal/native/     + suspend_and_resume()
  runtime/src/
    lib.rs                    + Atoms.lifecycle; Suspend arm in
                              the main-loop dispatch match;
                              Starting→Running transition after
                              first paint; Exiting on Quit
    dispatch/mod.rs           DispatchOutcome::Suspend;
                              Command::Suspend routes there
    keymap.rs                 Command::Suspend; parse + default
  led/src/main.rs             (unchanged)
```

New workspace member: `led-state-lifecycle`.

## Testing

### `state-lifecycle`
- `Phase::default() == Starting`.
- `LifecycleState::default()` is `{ Starting, 0 }`.

### `runtime::dispatch`
- `Command::Quit` → `DispatchOutcome::Quit` (existing).
- `Command::Suspend` → `DispatchOutcome::Suspend` (new).

### `runtime::run`
- Starting → Running on first tick that produces a frame.
- Quit arm sets `phase = Exiting` before breaking.
- Suspend arm bumps `force_redraw` and sets phase to Running
  (Suspended transient is unobservable from tests unless we
  block inside the helper).
- `last_frame` cleared on force_redraw bump.

### Non-test: interactive smoke
- `cargo run -p led` → Ctrl-Z → shell prompt returns → `fg` →
  editor repaints intact.
- Ctrl-X Ctrl-C → process exits cleanly, terminal restored.

Expected: +6 tests.

## Done criteria

- All existing tests pass.
- New tests green.
- Clippy: net delta 0 from post-M19.
- Interactive smoke per the list above.
- Goldens:
  - `actions/quit` — already green on `rewrite` (scenario just
    presses Ctrl-X Ctrl-C and captures the frame right
    after; the `Exiting` transition doesn't change the trace
    shape). Re-confirmed post-M20.
  - `actions/suspend` — not in `goldens/scenarios/` on this
    branch (scaffold-TBA by the goldens author). When it
    arrives the expected contract is: dispatched.snap has no
    new line (suspend is not a driver dispatch); frame.snap
    is whatever the editor painted last before the SIGTSTP.
  - `keybindings/confirm_kill/*` — stay red pending M21 work
    (`WorkspaceFlushUndo` after accepted force-kill).
    Documented dependency.
  - `features/lifecycle/*` — similarly deferred to M21
    (session-save scenarios).

## Growth-path hooks

- **M21 session save** — `Exiting` phase becomes the trigger
  for `WorkspaceOut::SaveSession` dispatch; the main-loop
  break gates on `session.saved`.
- **M21 Resuming phase** — reintroduced between Starting and
  Running once session-restore materialisation lands.
- **M26 file watcher** — drops `WorkspaceChanged` events that
  might bump `force_redraw` (external file modifications);
  the consumer (main loop's `last_frame` clear) already
  exists.
- **Resize handling** — currently the terminal driver repaints
  on resize via its own path; once `force_redraw` is the
  single source of truth, resize becomes a one-line bump
  without special-casing.
- **Dual binding for Ctrl-X Ctrl-Z** — legacy's escape-hatch
  chord. Trivial alias (`bind_chord("ctrl+x", "ctrl+z",
  Command::Suspend)`) when / if needed.
