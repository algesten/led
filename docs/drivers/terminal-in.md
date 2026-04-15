# Driver: terminal-in

## Purpose

Translates raw terminal events (keystrokes, resize, focus changes) from crossterm's blocking event loop into a reactive `Stream<TerminalInput>` that feeds the model. This is the user's sole input channel into the editor: every keybinding, chord, self-insert, and resize pulse originates here. The driver is push-only: it emits, it never consumes commands. A secondary responsibility (via sibling helpers in the same crate) is terminal setup and teardown — `enable_raw_mode`, `EnterAlternateScreen`, `EnableBracketedPaste` at start and their inverses (plus a panic hook) on drop of the returned `InputGuard`.

## Lifecycle

Starts from `led::run` (see `/Users/martin/dev/led/led/src/lib.rs:271-273`) when running the real binary: `setup_terminal()` returns an `InputGuard`, then `led_terminal_in::driver()` spawns the input thread and is forwarded into the caller-owned `terminal_in` stream. In profiling/replay mode (`led/src/main.rs:231-262`), `driver()` is not called at all — `main.rs` instead synthesizes `TerminalInput::Key` events from a trace file and pushes them directly into a `Stream<TerminalInput>`.

The driver owns two background tasks:

- A dedicated OS thread that calls `crossterm::event::read()` in a loop. This thread also emits one initial `TerminalInput::Resize(w, h)` probing `crossterm::terminal::size()` before the read loop begins.
- A `tokio::task::spawn_local` bridge that drains the mpsc receiver into the `Stream`.

There is no explicit shutdown handshake. Termination happens when the process exits; the OS thread dies with the process. The `InputGuard` drop runs `restore_terminal()` (leaves alternate screen, disables raw mode, disables bracketed paste) so the outer terminal is left in a sane state. A `set_hook` panic hook duplicates the cleanup so terminal state is restored on panic.

## Inputs (external → led)

- OS/terminal keystrokes, parsed by crossterm into `KeyEvent` values.
- SIGWINCH / terminal resize events, delivered by crossterm as `Event::Resize(w, h)`.
- Terminal focus-change escape sequences (`Event::FocusGained`, `Event::FocusLost`) — only emitted by terminals that support and are configured for focus reporting. Bracketed-paste is enabled in setup but paste events are filtered out in `driver()` via the `_ => continue` arm (only Key/Resize/Focus variants are forwarded).

Initial size probe: the OS thread reads `crossterm::terminal::size()` once before entering the event loop and emits a synthetic `Resize` so the model can lay out without waiting for a real SIGWINCH.

## Outputs from led (model → driver)

None. This driver has no `*Out` channel — it is purely push-upstream. Terminal setup/teardown is driven by the owner of `InputGuard` via Rust's drop semantics, not via a command stream.

## Inputs to led (driver → model)

| Variant                      | Cause                                                              | Frequency             |
|------------------------------|--------------------------------------------------------------------|-----------------------|
| `TerminalInput::Key(KeyCombo)` | Any `Event::Key` from crossterm, converted via `KeyCombo::from_key_event` | per keystroke         |
| `TerminalInput::Resize(w,h)` | Initial size probe on startup + every `Event::Resize`              | once + per SIGWINCH   |
| `TerminalInput::FocusGained` | Terminal focus-in escape sequence (if enabled by the terminal)     | per window focus-in   |
| `TerminalInput::FocusLost`   | Terminal focus-out escape sequence (if enabled by the terminal)    | per window focus-out  |

`Key` events are consumed in `led/src/model/actions_of.rs:33-50`. The key flow:

1. `filter_map` to extract `KeyCombo`s.
2. `sample_combine(state)` so the keymap is resolved against the current `AppState` (for chord prefixes, focus-gated actions, macro-repeat mode, chord count accumulation).
3. `map_key()` emits a `Vec<Mut>` — usually either `Mut::Action(a)` or nothing, plus edge cases like `Mut::KbdMacroSetCount`.
4. `flat_map` expands the vec and downstream filters split actions from non-action Muts.

`Resize` events bypass the keymap entirely (see `actions_of.rs:22-27`): a dedicated `filter_map` produces `Mut::Resize(w, h)` and forwards to the reducer, which updates `AppState::dims`.

`FocusGained` / `FocusLost` have **no downstream consumer**. They are produced by the driver but no `filter_map` in `actions_of.rs` or elsewhere matches them. Flagged as dead in `/Users/martin/dev/led/docs/rewrite/POST-REWRITE-REVIEW.md` and `/Users/martin/dev/led/docs/extract/driver-events.md:416-423`. The rewrite should either drop these variants or wire up a real handler (e.g. auto-save on focus loss, as other editors do).

## State owned by this driver

None. The driver is stateless: the OS thread holds an mpsc sender, the local task holds the receiver, and both are dropped when the outer `Stream` handle is dropped. There are no buffers, queues with semantics, or reconnection logic.

The channel capacity is 256 — a soft backpressure ceiling. If the reactive tree is starved (e.g. blocked on a slow render) for long enough to fill 256 events, `blocking_send` will block the OS read thread until space frees up. In practice this never happens: the model drains keystrokes in microseconds.

SHIFT-handling is stateful at the `KeyCombo` level, not here: `KeyCombo::from_key_event` in `/Users/martin/dev/led/crates/core/src/keys.rs:21-38` strips SHIFT from `KeyCode::Char(_)` events. This is a codebase-wide quirk (bindings like `shift+a` parse but never match — see POST-REWRITE-REVIEW.md § "SHIFT stripped on KeyCode::Char").

## External side effects

- Mutates terminal state on setup (`enable_raw_mode`, alternate screen, bracketed paste).
- Mutates terminal state on drop (restores all of the above).
- Installs a global panic hook.

No filesystem I/O, no network.

## Known async characteristics

- **Latency**: keystroke-to-event is on the order of microseconds (crossterm read + mpsc send + stream push).
- **Ordering**: strict. Single OS thread, single consumer, single mpsc channel.
- **Cancellation**: none. Events in-flight are always delivered.
- **Backpressure**: 256-event mpsc buffer; the OS thread blocks on `blocking_send` if the buffer fills. Events are never dropped.

## Translation to query arch

| Current behavior                              | New classification                           |
|-----------------------------------------------|----------------------------------------------|
| Emits `TerminalInput::Key`                    | Input driver → `Event::Key(KeyCombo)`        |
| Emits `TerminalInput::Resize`                 | Input driver → `Event::Resize(w, h)`         |
| Emits `TerminalInput::FocusGained` / `FocusLost` | Drop (dead code) — or optional `Event::FocusChange` if a use case is designed in |
| Terminal setup/teardown (`InputGuard`)        | Stays as a lifetime-tied resource in the runtime entrypoint; not part of the query arch |
| Initial size probe                            | First `Event::Resize` emission on startup    |

## State domain in new arch

Transient events, applied via reducer to `UiState`:
- `Event::Resize` → `UiState::dims` (and any dependent wrapping / layout caches).
- `Event::Key` → usually produces an `Action` via keymap lookup; the Action is the unit that reducers consume. The raw key does not land in any state atom.

No resource-result slots needed — this driver has no resource role.

## Versioned / position-sensitive data

None. Keystrokes are stateless; resizes are stateless (layout re-derivation is a pure function of new dims + existing buffers).

## Edge cases and gotchas

- **SHIFT-on-Char stripped at source.** The `KeyCombo::from_key_event` conversion happens inside this driver's OS thread. Anything downstream that wants to distinguish `a` from `shift+a` must change `KeyCombo`, not the driver. Flagged for explicit decision in the rewrite (POST-REWRITE-REVIEW.md).
- **F-keys, Insert, numpad keys** can arrive here as `KeyCode::F(_)`, `KeyCode::Insert`, etc. The driver forwards them normally, but `parse_key_combo` in `keys.rs` doesn't accept them in TOML bindings — so they're unbindable (POST-REWRITE-REVIEW.md § "F-keys, Insert, numpad keys unbindable"). Not a driver bug; a parser gap.
- **Bracketed paste is enabled in setup but dropped in the read loop.** `Event::Paste(_)` hits the `_ => continue` arm in `driver()`. Paste handling today relies on per-char `KeyCombo` events. Rewrite should decide whether paste should be a first-class `Event::Paste(String)` for atomic undo and large-payload efficiency.
- **`FocusGained`/`FocusLost` emitted but unused.** [unclear — intent]: reserved for a feature never landed, or pure dead code. Treat as dead until a use case is identified.
- **Replay path bypasses the driver entirely.** `/Users/martin/dev/led/led/src/main.rs:240-262` pushes `TerminalInput::Key` directly into a `Stream`. Any logic added to the real driver (debouncing, translation) must be mirrored there or the replay and real paths will diverge. Rewrite should unify: a single keymap-adapter consumes either a real event stream or a replay stream.
- **The goldens runner doesn't use this driver's channel.** It writes key bytes to the PTY master FD; crossterm's normal event loop then produces `KeyEvent`s. Consequence: golden coverage transitively covers this driver's read-loop behaviour without stubbing it.

## Goldens checklist

Minimum scenarios:
- `terminal-in/key-single` — single literal char press, verify `Action::InsertChar` fires.
- `terminal-in/key-ctrl-chord` — `C-x C-c` chord prefix sequence, verify two-step chord resolution.
- `terminal-in/resize-initial` — every startup scenario naturally covers this (initial size probe → `Mut::Resize` → `dims` populated).
- `terminal-in/resize-dynamic` — [gap — requires PTY ioctl support in the runner, currently unimplemented per driver-events.md:443].
- `terminal-in/focus-gained` — [gap — no consumer; document as dead rather than covering].
- `terminal-in/focus-lost` — [gap — same as above].
- `terminal-in/bracketed-paste` — [unclear — whether the rewrite keeps today's "drop paste events" behaviour or lifts paste to a first-class event; no test until that decision is made].
