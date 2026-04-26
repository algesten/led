# Milestone 11 — Architecture course correction

Produced from an end-of-M11 architecture review. The rewrite is
genuinely query-driven and the execute-pattern discipline is
honored on most paths; the problems are boundary leaks, one
asymmetric driver, and a handful of patterns that will scale
badly as M12+ lands.

Nine concrete course corrections below, ranked by leverage.
Priority ordering at the bottom of this doc.

Prerequisite reading:

1. `../../../drv/EXAMPLE-ARCH.md` — the canonical architecture
   guide. The review is calibrated against its rules (sources,
   drivers, queries, execute pattern, crate layout).
2. `README.md` — rewrite handover doc; the "Key decisions already
   made" block is the contract each correction either restores
   or renegotiates.
3. `QUERY-ARCH.md` — led-specific target architecture.

---

## What's going well — don't change

- Input projections in `crates/runtime/src/query.rs` are
  textbook narrow (e.g. `AlertsInput` omits the ticking
  `info_expires_at`; `PendingSavesInput` omits ropes). Good
  discipline, visible payoff.
- The execute-pattern sync-write is rigorously applied on the
  file-save path (`runtime/src/lib.rs:275-278`) and the
  file-load path (`driver-buffers/core/src/lib.rs:156-165`).
- Driver cores have standalone mpsc-boundary tests that don't
  spawn threads — the documented mock point actually works.
- No `join()` in any native `Drop`; workers self-exit on channel
  hangup exactly as EXAMPLE-ARCH § "Drop-order" prescribes.
- Dispatch is genuinely sync, genuinely non-blocking, split
  cleanly across nine per-domain modules. No hidden I/O, no
  shared mutex.
- Wake-on-event loop (commit `cb59d16`) is correctly wired; no
  busy-spin, no fixed cadence.

---

## 1. Split `BrowserState` into user-decision and external-fact sources

`crates/state-browser/src/lib.rs:52-65` mixes both categories in
one struct:

- **External fact** (FS-discovered, written by the fs-list
  driver): `root`, `dir_contents`, and the derived `entries`.
- **User decision** (chosen): `expanded_dirs`, `selected`,
  `scroll_offset`, `visible`, `focus`.

EXAMPLE-ARCH § "Sources: two kinds of ground truth" makes this a
hard rule: _"these have different lifecycles, different update
paths, and different owners. They should live in separate
sources."_

The browser is the first place it's violated, and M11 is actively
being built on it — easy to fix now, painful to fix after more
memos depend on the combined struct.

**Fix:** Split into `BrowserUi` (user-decision, keeps
`expanded_dirs`, `selected`, `scroll_offset`, `visible`, `focus`)
and `FsTree` (driver-fed, keeps `root`, `dir_contents`). A
runtime memo combines both to produce the flattened `entries`
vector the painter walks.

## 2. Flip the `driver-fs-list` ↔ `state-browser` dependency direction

`crates/driver-fs-list/core/Cargo.toml:13` and `src/lib.rs:12`:

```rust
pub use led_state_browser::{DirEntry, DirEntryKind};
```

This is the only claim-4 violation in the codebase — "drivers
must not know about each other or about `state-*`." The comment
says it's done so "the worker can emit them directly," but the
types are structurally owned by the driver (they're the shape of
a FS listing). `state-browser/src/lib.rs:31-36` duplicates them.

**Fix:** Move `DirEntry` and `DirEntryKind` into
`driver-fs-list/core`; let `state-browser` (or the split in
correction 1) depend on the driver for ABI types. Eliminates the
duplication and restores dependency direction.

## 3. Make the terminal driver symmetric

Every other driver has `execute(actions) + process() →
completions`. `TerminalInputDriver` has only `process()`. The
output half — `paint()` — lives as a free function inside
`crates/driver-terminal/native/src/lib.rs` with no sync
counterpart, no `Cmd` type, no frame-in-flight state, and no
unified trace entry. The `render_tick` trace in
`runtime/src/lib.rs:298` is emitted by the runtime itself, never
by the driver.

Consequences:

- No mock point for paint: goldens are the only test that
  exercises the output path.
- Future frame-change throttling (batch a burst of keystrokes,
  coalesce to one paint) has no natural seat.
- The async-vs-sync execute discipline doesn't apply to output,
  so a reader can't reason about it the same way as other
  drivers.

**Fix:** Introduce a `FrameBuffer` source + `PaintCmd` /
`PaintDone` so paint becomes `execute(Frame)` on a
terminal-output driver. Small amount of code, big consistency
payoff.

## 4. Introduce a `World` struct for the main-loop state atoms

`crates/runtime/src/lib.rs:123` has
`#[allow(clippy::too_many_arguments)]` defending 11 parameters;
`crates/runtime/src/dispatch/mod.rs:146` has the same defense for
10. Today's atoms: `tabs`, `edits`, `store`, `terminal`,
`kill_ring`, `alerts`, `jumps`, plus drivers, keymap, config,
trace. The roadmap adds browser (M11), diagnostics (M12), palette
(M13), search (M14+), LSP (M18+), git, syntax. By M18 this is
15+ parameters and the attribute will propagate to every dispatch
helper.

The defending comment (_"packaging them into a struct would hide
the relationships"_) is wrong in the direction of abstraction:
which atoms each branch mutates is visible _inside_ each
function, not at the call site.

**Fix:** `World { atoms: Atoms, drivers: Drivers, cfg: &Config,
trace: &dyn Trace }` split. Keeps the relationships explicit AND
stable. Do it before M12.

## 5. Clipboard — extract state from `KillRing`, express intent as a memo

The clipboard is the one place where the rewrite drifts back
toward transition-handler style (`runtime/src/lib.rs:285-293`):

```rust
let mut clip_actions: Vec<ClipboardAction> = Vec::new();
if kill_ring.pending_yank.is_some() && !kill_ring.read_in_flight {
    kill_ring.read_in_flight = true;
    clip_actions.push(ClipboardAction::Read);
}
```

Three smells in one block:

1. `read_in_flight` (an async-work flag) lives on `KillRing`, a
   user-decision source. That's driver state on a state crate.
2. The "desired action" is computed imperatively inline, not in
   a memo.
3. `Vec::new()` allocates every idle tick, violating the "zero
   malloc on idle" discipline the README calls out.

**Fix:** Add a `ClipboardState` source owned alongside the
clipboard driver (state: `Idle` / `ReadInFlight` / `WritePending`),
write a `clipboard_action(kill_ring, clip)` memo returning
`ClipboardAction` or `Noop`, and let the execute phase write
intent into `ClipboardState` synchronously the same way file
saves do.

## 6. Make `BufferEdits` vs `BufferStore` contract a compile-time invariant

Two ropes per buffer: `BufferStore::LoadState::Ready(rope)`
(driver-owned, pristine disk snapshot) and
`BufferEdits.buffers[path].rope` (user-edited view). The
relationship is prose-documented in
`crates/state-buffer-edits/src/lib.rs:2-7` and
`crates/runtime/src/query.rs:262-298`, but nothing enforces it.
`runtime/src/lib.rs:151-157` relies on `or_insert_with` to avoid
clobbering, but the fallback logic in `body_model` (prefers
edits, falls back to store) means a bug that swaps the two ropes
would render correctly in steady state and only show up on
specific race paths.

**Fix:** Tighten with either (a) a typed wrapper
`EditedBuffer { seed: Arc<Rope>, current: Arc<Rope> }` where
`seed` is immutable post-seed and the invariant is checkable, or
(b) a unit test that simulates a load-after-edit race and asserts
the discard behavior. Cheapest first.

## 7. Kill the idle-tick allocations in render

Two known allocation sources per tick, cache-hit notwithstanding:

- `crates/runtime/src/query.rs:221-244` (`tab_bar_model`) builds
  a `Vec<String>` of labels then `Arc`-wraps. Cache miss on any
  buffer dirty flip allocates N strings.
- `crates/runtime/src/lib.rs:285` allocates a fresh
  `Vec<ClipboardAction>` every tick even when empty.

Both trip the README's own stated discipline (_"before adding
hot-path code, ask what does this allocate per idle tick"_).

**Fix:** Tab labels → `imbl::Vector<Arc<str>>` that the memo
rebuilds structurally (imbl keeps unchanged entries), clipboard
actions → small-vec / stack-backed. These will rot back in
M12+ unless there's a periodic audit; add one to the ROADMAP.

## 8. Introduce a deadline source for the main loop

`crates/runtime/src/lib.rs:313-316` hardcodes the wait timeout to
alert TTL-or-60s:

```rust
let timeout = alerts
    .info_expires_at
    .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
    .unwrap_or(Duration::from_secs(60));
```

M12 (diagnostics debouncing), M13 (command palette animation),
M18 (LSP completion timeouts), M-anywhere (file watch), search
throttling — each will need a deadline to plug in.

**Fix:** A `nearest_deadline(atoms) -> Option<Instant>` memo that
min-folds all currently-registered timers. Keeps the main loop
immune to future additions. One-line drop-in now; a painful
refactor once three features are fighting over the timeout.

## 9. Close the testability gaps that will bite during M12+

The test footprint is strong (~270 in-workspace unit + ~280
goldens) but has two concrete holes that will get worse:

- **No dispatch → driver plumbing tests.** Unit tests mock the
  driver out; goldens test end-to-end. Nothing verifies
  "keystroke X causes `SaveAction Y` to land on the file
  driver." Add a thin test harness that captures driver
  `execute()` calls.
- **Trace emission sites aren't verified.** Trace format is
  unit-tested; emission points aren't. The golden contract
  depends on emissions — a silent misfire shows up as a single
  mysterious golden diff weeks later. Capture-trace assertions
  at the dispatch and ingest level, M11 or M12.

Also worth promoting: `crates/runtime/src/dispatch/testutil.rs`
is `pub(super)` but the fixture pattern will be needed by driver
integration tests and future state crates. Extract to a
workspace-level `led-testutil` crate.

---

## Priority ordering

If budget allows only three, do **#1 (browser split)**, **#2
(fs-list dep flip)**, **#4 (World struct)** — all get cheaper to
do now and rapidly more expensive with each new milestone.

**#3 (terminal symmetry)** and **#5 (clipboard memo)** are
design-purity fixes you can defer, but both will look out of
place forever if left.

**#6–#9** are discipline-maintenance items that can ride along
with the milestone they're most relevant to:

- #6 (BufferEdits/BufferStore invariant) with M3-style editing
  work or on its own.
- #7 (zero-alloc audit) periodic — schedule against the ROADMAP.
- #8 (deadline source) before M12 (diagnostics debouncing is the
  first new timer).
- #9 (testability gaps) M11 or M12, before the test surface
  calcifies.
