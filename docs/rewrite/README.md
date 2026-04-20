# led rewrite — handover docs

## What this is

`led` is being rewritten from its current **FRP (functional reactive programming)** architecture to a **query-driven** architecture (the rust-analyzer / salsa pattern, plus some extensions). These docs capture the decisions and plan so the work can continue in a fresh context without losing details.

**Status (2026-04-19):** Phases 0–2 (PTY harness + ~280 goldens + narrative spec + per-driver inventory) completed on `main` and merged into `rewrite`. Phase 3 (clean-slate skeleton) also complete — Milestone 1 (tabs + buffer-loads + basic render) runs end-to-end. Phase 4 (domain-by-domain port driven by goldens) is the next ~multi-milestone effort.

## The decision in one paragraph

The current led architecture has a single `model()` function with a large combinator graph of `Stream`s that produces fine-grained `Mut` variants applied by a trivial reducer to a single `AppState`, plus a `derived()` layer that eagerly pushes state through to driver output streams. This is being replaced by: multiple domain-owned **sources** (plain structs — one per driver concern: `BufferStore`, `Terminal`, later `GitState`/`LspState`/…), each owned by a strictly-isolated driver crate; a **sync non-blocking event handler** in the runtime that mutates those sources; a **query layer** built on the [`drv`](../../../drv/README.md) crate producing derived views on demand with memoization; **versioned data + rebase queries** for position-sensitive data (diagnostics, hunks) that arrive async w.r.t. buffer edits; and **fire-and-forget dispatch** to async native workers for cache misses (the event loop never blocks on I/O).

## Read these in order

1. **[`../drv/README.md`](../../../drv/README.md)** — the memoization primitive. Atoms, lenses, memos, `drv::assemble!()`. Skim this first.
2. **[`../drv/EXAMPLE-ARCH.md`](../../../drv/EXAMPLE-ARCH.md)** — the authoritative guide to query-driven app architecture on drv. Driver split, core/native crate pattern, runtime integration crate, cross-crate input idiom, ABI-boundary mock point. This is the canonical "how to organise the code" reference and supersedes large parts of QUERY-ARCH.md.
3. **[QUERY-ARCH.md](QUERY-ARCH.md)** — led-specific application of the patterns: domain sources planned for led, rebase-query design for position-sensitive data, mapping from the current FRP principles.
4. **[MILESTONE-1.md](MILESTONE-1.md)** — the first vertical slice (tabs + file reads + render). Partly shipped; see the Status block at the top.
5. **[MILESTONE-2-SCOPE.md](MILESTONE-2-SCOPE.md)** — short scope-bookmark for M2 (cursor + arrow-key movement + viewport scrolling). Not a design; the first task of M2 is to write the full MILESTONE-2.md.
6. **[M1-arch.svg](M1-arch.svg)** — graphviz of the actual M1 shape (atoms, lenses, memos, drivers, the ABI boundary). [`.dot`](M1-arch.dot) and [`.png`](M1-arch.png) alongside.
7. **[REWRITE-PLAN.md](REWRITE-PLAN.md)** — phased execution plan. Phases 0–3 done; Phase 4 (domain port against goldens) pending.
8. **[SPEC-PLAN.md](SPEC-PLAN.md)** — methodology used to produce the spec; artefacts in `docs/spec/` and `docs/extract/`.
9. **[DRIVER-INVENTORY-PLAN.md](DRIVER-INVENTORY-PLAN.md)** — template used for per-driver docs; artefacts in `docs/drivers/`.
10. **[GOLDENS-PLAN.md](GOLDENS-PLAN.md)** — golden-test strategy. Harness lives in `goldens/` (excluded from the workspace; black-box subprocess tests). ~280 scenarios authored.
11. **[POST-REWRITE-REVIEW.md](POST-REWRITE-REVIEW.md)** — bugs and quirks in current led to consider during rewrite.

## Current crate layout (rewrite branch)

```
crates/
  core/                             shared primitives: UserPath, CanonPath, id_newtype!
  state-tabs/                       user-decision source (Tabs); self-contained, no driver
  driver-buffers/
    core/   → led-driver-buffers-core    BufferStore source + LoadState + LoadAction +
                                          FileReadDriver sync API + ReadCmd/ReadDone ABI types +
                                          Trace trait. No deps on other drivers or state-tabs.
    native/ → led-driver-buffers-native  Desktop async: thread + std::fs. spawn() convenience.
  driver-terminal/
    core/   → led-driver-terminal-core   Terminal source + mirrored key/event types +
                                          Frame / TabBarModel / BodyModel + TerminalInputDriver +
                                          Trace trait. No deps on other drivers or state-tabs.
    native/ → led-driver-terminal-native Desktop crossterm: input thread + paint() + RawModeGuard.
  runtime/                          Integration layer: `#[drv::input]` projections + cross-source
                                    memos (file_load_action, tab_bar_model, body_model,
                                    render_frame) + Event + dispatch + Trace + SharedTrace +
                                    run + spawn_drivers.
led/                                Thin main: CLI parse, RawModeGuard, run.
```

Principles: each driver is **strictly isolated** (no knowledge of other drivers or `state-tabs`). Drivers split **sync core** (portable) + **async native** (platform-specific). All cross-source composition lives in `runtime`. The mpsc between core and native is the mock point for tests.

## Execution order (high-level)

1. ✅ **Subprocess test runner + binary contract.** `goldens/` crate: PTY spawn, `vt100` parser, scripted fakes for LSP/gh. Current led honours `--golden-trace` + `--test-clock`; the rewrite binary currently only honours `--golden-trace` and must catch up.
2. ✅ **Spec goldens generated.** Mechanical axes (actions, keybindings, driver-events, config-keys) + narrative features + edge cases, ~280 scenarios in `goldens/scenarios/`. All captured against current led on `main`.
3. ✅ **Functional spec written.** `docs/extract/*.md` (mechanical extracts) + `docs/spec/*.md` (18 narrative files).
4. ✅ **Driver inventory written.** `docs/drivers/*.md` (14 files).
5. ✅ **Rewrite skeleton (Phase 3).** Current M1 state.
6. 🚧 **Domain-by-domain port (Phase 4).** The work ahead: make the rewrite binary pass the ~280 goldens, one domain at a time. Each milestone corresponds roughly to one domain coming online (cursor+movement, editing, saving, config, LSP, git, syntax, search, …). Progress is measurable as `% goldens green`.

## Key decisions already made

Do not re-litigate these without good reason:

- **Architecture**: query-driven, not FRP. Domain-owned sources, not a single `AppState`. Sync non-blocking handler, not a stream graph. See `QUERY-ARCH.md` and, for the general pattern, `../drv/EXAMPLE-ARCH.md`.
- **Mechanism**: the `drv` crate (0.3.1) provides the memoization primitive. Current API: sources are plain structs (no annotation), `#[drv::input]` on projection structs, `#[drv::memo(single)]` / `#[drv::memo(lru = N)]`. No `drv::assemble!()` — the input macro self-registers. Memo params use concrete types (`MyInput<'a>`, `&MyInput<'a>`, `T`, `&T`); `impl Into<...>` is not supported.
- **Strict driver isolation**: `driver-buffers/*` must not import `driver-terminal/*` or `state-tabs`. All cross-source composition is in the `runtime` crate. Enforced by `Cargo.toml`, not discipline.
- **Core / native split per driver**: the portable sync half (`*-core`) holds source + sync API + ABI types; the platform half (`*-native`) holds the async worker. The mpsc between them is the mock point. See `../drv/EXAMPLE-ARCH.md` § "Organizing the code: crate layout."
- **Runtime is the integration crate**: `#[drv::input]` projections (with hand-written `new(&source)` constructors) + cross-source memos + event dispatch + main loop all live in `crates/runtime/`. Inputs with reference fields `#[derive(Copy, Clone)]` so sibling memos can forward them.
- **User-decision sources don't need drivers**: `Tabs` has no async side; it sits in `state-tabs/` alone and is mutated directly by `dispatch`.
- **Fine-grained muts carry over, per-domain**: the "many small muts, trivial reducer" principle from the current root `CLAUDE.md` still holds, now per domain atom rather than globally.
- **No `rm -rf` on day one**: the current FRP code (on `main`) is the final arbiter of behavior; keep it one `grep` away until parity is verified. `rewrite` branch and `main` coexist; worktree at `../led-rewrite`.
- **Goldens capture the external boundary** (rendered frame + dispatched trace) at the raw-keypress input level. Not internal state — that shape is changing.
- **Goldens drive the compiled binary in a PTY.** Zero coupling to internal Rust types; the entire code-side contract is `--golden-trace` + `--test-clock` flags plus the trace-line format. Same golden files run unchanged against legacy and rewrite binaries.
- **Golden generation happens against current led, before the domain-by-domain port starts.** Still pending.
- **The rewrite happens on a `rewrite` branch as a full clean slate.** `main` continues receiving fixes and features; goldens, spec, and driver inventory authored there are merged into `rewrite`.
- **Allocation discipline: zero malloc on idle ticks.** The main loop runs at ~100 Hz; on true idle every memo must cache-hit, every `execute` must iterate an empty collection, and `paint` must be skipped. Memo outputs containing large owned data are `Arc`-wrapped (`Arc<Vec<String>>`, `Arc<str>`, `imbl::Vector<T>`) so cache-hit `Clone` is a refcount bump, not a deep copy. Render/paint code never materialises intermediate collections — no `Vec<&str>` to iterate, no `format!(" {x} ")` per cell. Filter before clone in action memos. Rope ops walk `RopeSlice` directly (`.char(i)`, `.len_chars()`) — never `.to_string()` to measure. Before adding hot-path code, ask "what does this allocate per idle tick / per keystroke / per paint?" and if the answer is non-zero for the idle case, rethink. An M2 audit found 10 wasteful sites in one pass; it's easy to reintroduce.

## Open questions that are still open

- **Edit log representation for rebase queries.** Exact shape of the per-buffer edit log, and the rebase function signature for diagnostics / hunks / other position-sensitive data. Blocked on M3 (editing) + M5+ (LSP/git).
- **Cross-platform story.** `../drv/EXAMPLE-ARCH.md` § "Multiple platforms: one native per platform, not cfg" settles the principle. Concrete iOS/Android ports aren't built yet.
- **Goldens harness + spec work.** Planned in Phases 0–2 but not started. The M1 skeleton emits `--golden-trace` lines, so the harness work can begin anytime.
- **Async runtime choice for native workers.** Current `*-native` uses `std::thread` + blocking I/O for simplicity. Tokio/mio/async-std are all viable; defer until a driver needs async-per-operation concurrency.
- **Dispatch shape.** Currently `dispatch(Event, &mut Tabs)` in runtime. As more atoms get mutated by dispatch (M2+ cursor, M3 edits), decide whether to keep one fat `dispatch` or split per-domain (`dispatch_tabs`, `dispatch_cursor`, …). Lean toward split.

## Open questions that are closed

- **~~Multi-crate organization~~**: settled — one crate per source (or crate pair for drivers), runtime is the integration layer. See `../drv/EXAMPLE-ARCH.md`.
- **~~How memos cross crates~~**: settled — `#[drv::input]` projection struct declared in the consumer crate with a hand-written `new(&foreign_source)` constructor. drv 0.3.1 uses per-memo thread-local caches keyed by value; the consumer declares whatever shape it wants.
- **~~How events are plumbed~~**: single `Event` enum in `runtime`, pushed onto `Terminal.pending` by the input driver, drained into dispatch each tick.
- **~~Where the render query lives~~**: `runtime/src/query.rs`. `render_frame` composes `tab_bar_model` + `body_model` as sub-memos (each independently cached). Future render work extends the same file.

## Directory layout of this doc set

```
docs/rewrite/
  README.md                   ← you are here
  MILESTONE-1.md              ← first vertical slice (partly shipped)
  M1-arch.dot / .svg / .png   ← architecture graph for the current code
  QUERY-ARCH.md               ← led-specific target architecture
  REWRITE-PLAN.md             ← phased execution plan
  SPEC-PLAN.md                ← how to document current led
  DRIVER-INVENTORY-PLAN.md    ← per-driver template + translation table
  GOLDENS-PLAN.md             ← golden-test strategy
  POST-REWRITE-REVIEW.md      ← current-led bugs/quirks to consider
```

Generated artefacts already in the repo:

```
docs/extract/                 ← Phase A mechanical extracts (4 files)
docs/spec/                    ← functional spec narrative (18 files)
docs/drivers/                 ← per-driver inventory (14 files)
goldens/scenarios/            ← ~280 golden scenarios; authored on main
                                against current led
goldens/src/, goldens/tests/  ← PTY harness (black-box; not in workspace)
```

## Context for a fresh Claude session

- The current project (on `main`) uses FRP (push-based, stream-graph) and has a detailed root `CLAUDE.md` describing the 10 FRP principles. **Those principles do not apply to the rewrite.** They apply to the current code, which stays untouched during the rewrite.
- The `rewrite` branch (this worktree) is a clean slate with the M1 skeleton: tabs + file-read driver + terminal driver + runtime. 35 unit tests passing.
- Read `../drv/README.md` first (the memoization primitive — drv 0.3.1, plain-struct sources, `#[drv::input]` projections, `#[drv::memo(single)]`), then `../drv/EXAMPLE-ARCH.md` (how to organise query-driven apps on drv). Those two together define the arch. `QUERY-ARCH.md` in this directory is the led-specific application of those patterns.
- When working on the rewrite, every new driver follows the `core/` + `native/` + `runtime-declares-inputs` pattern. No cross-driver imports. `Cargo.toml` is the enforcement.
