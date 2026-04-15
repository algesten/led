# led rewrite — handover docs

## What this is

`led` is being rewritten from its current **FRP (functional reactive programming)** architecture to a **query-driven** architecture (the rust-analyzer / salsa pattern, plus some extensions). These docs capture the decisions and plan so the work can continue in a fresh context without losing details.

No rewrite work has started. These docs are the plan and the pre-rewrite checklist.

## The decision in one paragraph

The current led architecture has a single `model()` function with a large combinator graph of `Stream`s that produces fine-grained `Mut` variants applied by a trivial reducer to a single `AppState`, plus a `derived()` layer that eagerly pushes state through to driver output streams. This is being replaced by: multiple domain-owned atoms (one per driver concern: `BufferState`, `GitState`, `LspState`, etc.), each with its own small mut enum and reducer; a **sync non-blocking event handler** that applies events to the relevant domain; a **query layer** built on the [`drv`](../../../drv/README.md) crate that produces derived views on demand with memoization; **versioned data + rebase queries** for position-sensitive data (diagnostics, hunks) that arrive async w.r.t. buffer edits; and **fire-and-forget dispatch** to async resource drivers for cache misses (the event loop never blocks on I/O).

## Read these in order

1. **[QUERY-ARCH.md](QUERY-ARCH.md)** — the target architecture. The "what." Read this first; everything else is how to get there.
2. **[REWRITE-PLAN.md](REWRITE-PLAN.md)** — phased execution plan, including the critical "do not `rm -rf` on day one" rule.
3. **[SPEC-PLAN.md](SPEC-PLAN.md)** — methodology for documenting what current led does, so the rewrite doesn't lose features.
4. **[DRIVER-INVENTORY-PLAN.md](DRIVER-INVENTORY-PLAN.md)** — template for per-driver documentation, plus a current-driver starter list and translation-to-new-arch table.
5. **[GOLDENS-PLAN.md](GOLDENS-PLAN.md)** — golden-test strategy. The enforceable spec. Six axes of coverage, harness contract, how to guarantee exhaustiveness.

## Execution order (high-level)

Nothing destructive happens until step 5. Each step produces an artifact committed to the repo.

1. **Build the test harness.** Extend the existing `TestHarness` (in `led/tests/harness/mod.rs`) to capture rendered frames, dispatched driver events, and state snapshots. Add `insta` (or similar). Input scripts at the keypress layer, not the `Action` layer. See `GOLDENS-PLAN.md`.
2. **Generate spec goldens.** Mechanical axes (per-Action, per-keybinding, per-driver-event, per-config-key) first — these are enumerations over code and produce hundreds of stub goldens automatically. Then author narrative goldens per feature area. All generated against current led and committed. See `GOLDENS-PLAN.md`.
3. **Write the functional spec.** Phase A extraction, Phase B narrative (with reverse index into Phase A extracts). See `SPEC-PLAN.md`.
4. **Write the driver inventory.** One file per driver, plus the translation table. See `DRIVER-INVENTORY-PLAN.md`.
5. **Start the rewrite.** Sibling path or worktree; existing `crates/` stays untouched as reference until parity. See `REWRITE-PLAN.md`.

Steps 1–4 must be substantially complete before step 5 starts. Steps 1–4 can be done in parallel where they don't depend on each other, but the golden harness (step 1) gates everything else.

## Key decisions already made

Do not re-litigate these without good reason:

- **Architecture**: query-driven, not FRP. Domain-owned atoms, not a single `AppState`. Sync non-blocking handler, not a stream graph. See `QUERY-ARCH.md`.
- **Mechanism**: the `drv` crate provides the memoization primitive. It already supports atoms, lenses, chained memos, and multi-atom queries. It lives in a sibling directory (`../drv`).
- **Fine-grained Muts**: the "many small muts, trivial reducer" principle from the current `CLAUDE.md` carries forward — but per-domain, not global.
- **No `rm -rf` on day one**: the current code is the final arbiter of behavior; keep it one `grep` away until parity is verified.
- **Goldens capture the external boundary** (rendered frame + dispatched events) at the raw-keypress input level. Not internal `AppState` — that shape is changing.
- **Golden generation happens against current led, before the rewrite starts.**

## Key open questions

These should be resolved early in the execution:

- **Edit log representation for rebase queries.** Exact shape of the per-buffer edit log, and the rebase function signature for diagnostics / hunks / other position-sensitive data. See `QUERY-ARCH.md` § "Versioned data + rebase queries."
- **How events are plumbed.** Single `Event` enum with nested domain variants, or per-domain channels. See `QUERY-ARCH.md` § "The event handler."
- **Where the render query lives.** One top-level query or a tree of queries? Probably a tree, with `drv` handling the caching per-node.
- **Config/keybinding hot-reload.** Currently works via FRP; need to decide whether it's a domain of its own or part of input-handling.
- **Async runtime choice.** Current led uses tokio; new arch can be tokio, mio, single-thread, or mixed. Not urgent to decide — the handler contract is runtime-agnostic.

## Directory layout of this doc set

```
docs/rewrite/
  README.md                   ← you are here
  QUERY-ARCH.md               ← target architecture
  REWRITE-PLAN.md             ← phased execution plan
  SPEC-PLAN.md                ← how to document current led
  DRIVER-INVENTORY-PLAN.md    ← per-driver template + translation table
  GOLDENS-PLAN.md             ← golden-test strategy
```

Any generated artifacts (extracts, goldens, inventory docs) should live in:

```
docs/extract/                 ← Phase A mechanical extracts
docs/spec/                    ← functional spec narrative
docs/drivers/                 ← one .md per driver
tests/golden/                 ← golden snapshots
```

## Context for a fresh Claude session

The current project uses FRP (push-based, stream-graph) and has a detailed `CLAUDE.md` at the repo root describing the 10 FRP principles. **Those principles do not apply to the rewrite.** They apply to the current code, which stays untouched during the rewrite. When working on the new code, refer to `QUERY-ARCH.md` for principles. The repo-root `CLAUDE.md` will need a section added (or be replaced) once the rewrite has progressed.

The `drv` crate's README is at `../drv/README.md` relative to the led repo root. Read it before `QUERY-ARCH.md` — the architecture depends on understanding atoms, lenses, and memos.
