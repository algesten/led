# Driver: timers

> **Out-of-contract for the rewrite.** Per
> `docs/rewrite/DRIVER-INVENTORY-PLAN.md` and the broader rewrite-scope
> discussion, the timers driver's *scheduling surface* (when timers
> fire, how they're coalesced, which schedule mode to use) is not part
> of the frozen contract. The rewrite is free to replace it with a
> dispatcher-internal scheduler, a different API shape, or virtual-
> clock integration that current led doesn't have. **This document
> captures current behavior for reference only** — the goldens that
> observe timer traces remain authoritative for the specific names and
> durations, but how they get emitted is negotiable.

## Purpose

The timers driver owns named deadlines: the model dispatches
`TimersOut::Set { name, duration, schedule }` and receives a
`TimersIn { name }` when the deadline fires. Seven timer names are in
use (see the table below). Each name has an agreed-upon meaning
(`undo_flush`, `alert_clear`, etc.) and a consistent schedule mode
across all dispatch sites. Timers are the sole mechanism in current
led for debouncing, rate-limiting, and coalescing.

## Lifecycle

One driver instance, started at startup. An internal tokio task owns
the `HashMap<&'static str, Vec<JoinHandle<()>>>` that tracks active
timers. When a `Set` or `Cancel` comes in, the task creates or aborts
`JoinHandle`s. No explicit shutdown: task drops with the local set at
process exit, and any in-flight `JoinHandle`s are aborted via Drop.

## Inputs (external → led)

- `tokio::time::sleep` and `tokio::time::interval` — the host
  timekeeping.
- Nothing else.

## Outputs from led (model → driver)

| Variant                                                                       | What it causes                                                                                   | Async? | Returns via           |
|-------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------|--------|-----------------------|
| `TimersOut::Set { name, duration, schedule: Schedule::Replace }`              | Cancel any existing timers with `name`; spawn a one-shot after `duration`                        | yes    | `TimersIn { name }` once |
| `TimersOut::Set { name, duration, schedule: Schedule::KeepExisting }`         | If a timer with `name` is already running, do nothing. Otherwise spawn a one-shot                | yes    | `TimersIn { name }` once (possibly from the original scheduling) |
| `TimersOut::Set { name, duration, schedule: Schedule::Independent }`          | Always spawn a new one-shot; keep all existing ones. `Cancel` aborts all of them                 | yes    | `TimersIn { name }` once per spawn |
| `TimersOut::Set { name, duration, schedule: Schedule::Repeated }`             | Cancel existing, spawn a `tokio::time::interval(duration)` ticker that fires `TimersIn { name }` every `duration` until cancelled; first tick (which fires immediately) is skipped | yes | `TimersIn { name }` every `duration` |
| `TimersOut::Cancel { name }`                                                  | Abort and remove all `JoinHandle`s for `name`                                                    | sync   | (none)                |

See `crates/timers/src/lib.rs:37-100`. `name` is `&'static str` — it
must be a string literal or otherwise static (the seven names below
all are).

## Inputs to led (driver → model)

| Variant                            | Cause                                                           | Frequency                    |
|------------------------------------|-----------------------------------------------------------------|------------------------------|
| `TimersIn { name: "alert_clear" }`   | 3 s after an info/warn alert is displayed; `Replace`              | Once per alert               |
| `TimersIn { name: "undo_flush" }`    | 200 ms after any dirty buffer's version bumps; `KeepExisting`   | Once per edit burst per buffer |
| `TimersIn { name: "spinner" }`       | 80 ms `Repeated` while `s.lsp.busy`; cancelled when idle          | Every 80 ms during busy span |
| `TimersIn { name: "tab_linger" }`    | 3 s after active tab changes; `Replace`                           | Once per tab-active-for-3s   |
| `TimersIn { name: "git_file_scan" }` | 50 ms after `pending_file_scan` bumps; `Replace`                  | Once per scan-request burst  |
| `TimersIn { name: "pr_settle" }`     | 2 s after any `git_activity` push; `Replace`                      | Once per git-activity quiesce |
| `TimersIn { name: "pr_poll" }`       | 15 s `Repeated` while a PR is loaded; cancelled when PR clears    | Every 15 s while PR present  |

See the SPEC-PLAN.md "Timers" section (A.10) for the originating
derived.rs locations. Consumption is split in
`led/src/model/mod.rs:378-421` (undo_flush gets its own state-sampling
chain) and `mod.rs:1513-1543` (`handle_timer`, a match on the name
string for the other six).

## State owned by this driver

- `timers: HashMap<&'static str, Vec<JoinHandle<()>>>` — live timers
  keyed by name. `Vec<JoinHandle<()>>` is a vec to accommodate
  `Schedule::Independent` which stacks. For every other schedule mode
  the vec is length 0 or 1.
- `KeepExisting` also prunes finished handles before checking
  `is_empty`, so a one-shot that already fired but hasn't been
  cleaned up doesn't count as "still running."

No other state.

## External side effects

None beyond spawning tokio tasks that eventually wake on
`tokio::time::sleep`. No files, no network.

## Known async characteristics

- **Latency**: tokio's default resolution is 1 ms; actual fire time
  can drift under load by a few ms. For sub-second durations this is
  acceptable; for the 15 s PR poll it's invisible.
- **Ordering**: commands drain via a single mpsc consumer — `Set` and
  `Cancel` for the same name serialize. If `Set(Replace)` and
  `Cancel` arrive back-to-back, the order of events is `drain` order,
  which in practice matches dispatch order.
- **Cancellation**: `JoinHandle::abort()` — cooperative, cancels
  `tokio::time::sleep` cleanly.
- **Backpressure**: mpsc bounded at 64 on both command and result
  channels. Under sustained churn (e.g. `pr_poll` with a slow
  consumer) `try_send` can drop a fire; the result is a missed poll
  tick, which is harmless because the next interval tick comes around.

## Translation to query arch

The rewrite has three options; none need to match the current shape
exactly:

1. **Keep a driver, simplify the contract.** `Request::SetTimer` and
   `Request::CancelTimer` on the dispatcher side, the driver task
   owns the timer map. Virtual clock integration becomes an injected
   `Clock` trait that the driver uses instead of `tokio::time`.
2. **Inline into the dispatcher.** The dispatcher holds the timer
   map directly. `Event::TimerFired(name)` lands like any other
   event. No separate driver crate.
3. **Drop named timers entirely.** Replace each use-case with an
   inline future / saga that holds the delay locally. `undo_flush`,
   for instance, becomes "the undo-saga awaits 200 ms then flushes."
   This is the most radical and makes virtual-clock testing harder.

Given the rewrite's test strategy depends on `--test-clock` being
able to jump the clock to the next scheduled deadline, option 1 or 2
is strongly preferred over 3. The `Schedule` enum's four modes can
all be expressed as combinations of a one-shot deadline with a
dedupe/coalesce policy; the rewrite may collapse the enum into just
"one-shot" and "repeated" and push Replace/KeepExisting/Independent
semantics up to the call sites.

## State domain in new arch

None. Timer state is driver-internal (or dispatcher-internal) and
never lands in a domain atom. `spinner_tick: u32` in `LspState` is
state derived from timer fires, but the timer itself is not.

## Versioned / position-sensitive data

None. Timer names are opaque tokens; there's no content to rebase.
The `undo_flush` chain does sample buffer `version()` at fire time
and rebuilds `UndoFlush` entries from the buffer's current state —
that's standard state-sampling, not rebase.

## Edge cases and gotchas

- **`Independent` schedule is used in exactly zero places today.**
  `[unclear — verify in `rg 'Schedule::Independent'`]` The mode exists
  for completeness; the rewrite can drop it without migration cost.
  Grep-confirmed: not used in current `derived.rs`.
- **`Repeated` timers skip the first tick.** `tokio::time::interval`
  fires immediately on the first `.tick().await`; the driver
  discards that and only emits from the second tick onwards. For
  `spinner` (80 ms) this means there's an 80 ms gap between
  "spinner starts" and "first tick" — intentional to avoid a
  spinner flash on fast LSP responses. For `pr_poll` (15 s) it
  means the first poll happens 15 s after the PR loads, not
  immediately.
- **`KeepExisting` prunes finished handles before checking
  `active`.** Without the prune, a one-shot that already fired but
  whose `JoinHandle` is still in the vec would falsely count as
  "still running" and the new `Set` would no-op. The prune fixes
  this.
- **`&'static str` names prevent dynamic timer creation.** A "timer
  per open PR" would need string interning, which led doesn't do.
  All seven names are fixed. The rewrite should keep this
  restriction — it makes the name space enumerable for goldens.
- **`result_tx.send(...)` is fallible.** If the model side has torn
  down its receiver, the repeated ticker notices and breaks its
  loop (`lib.rs:137`). One-shots don't care because they spawn and
  forget.
- **No persistence across restarts.** Every timer is wall-clock-
  relative; a crash mid-`pr_poll` cycle resets the 15 s countdown
  on the next startup. Acceptable for every named timer today.
- **Virtual clock is not currently implemented.** `--test-clock` is
  a planned flag (`GOLDENS-PLAN.md`); goldens that need sub-15 s
  `pr_poll` or 3 s `alert_clear` cadences are currently blocked on
  wall-clock waits. The rewrite is the natural place to land this.

## Goldens checklist

Scenarios under `tests/golden/drivers/timers/`:

1. `undo_flush_after_edit/` — type, wait 200 ms (or advance virtual
   clock), assert `WorkspaceFlushUndo` appears in the trace.
2. `undo_flush_coalesce_bursts/` — type rapidly; assert only one
   `undo_flush` fire per burst (validates `KeepExisting`).
3. `alert_clear/` — set an alert, advance 3 s, assert the alert
   clears (`state.alerts` empty on the next frame). **Needs virtual
   clock.**
4. `spinner_during_lsp_busy/` — trigger an LSP busy span, assert
   multiple `spinner` fires land (status-bar animation). **Needs
   virtual clock + fake-lsp extension to hold busy.**
5. `tab_linger_touches_buffer/` — activate a tab, wait 3 s, assert
   buffer's `last_used` updates (observable indirectly via LRU
   eviction ordering). **Needs virtual clock.**
6. `git_scan_coalesce/` — save twice rapidly, assert exactly one
   `GitScan` dispatch (50 ms `Replace`).
7. `pr_poll_cadence/` — load a PR, advance 15 s repeatedly, assert
   a `GhPrPoll` fires per tick. **Needs virtual clock.**
8. `pr_poll_cancelled_on_pr_clear/` — load, then clear (branch
   switch), assert no further `GhPrPoll` dispatches.
9. `timer_name_enumeration/` — cross-cutting: confirm the set of
   names produced by the trace across all drivers goldens equals
   the seven documented here. Guards against new untracked timers.
