# Rewrite execution plan

How to actually carry out the rewrite from FRP → query-driven.

Prerequisite reading: `README.md`, `QUERY-ARCH.md`.

---

## The cardinal rule

**Do not `rm -rf` the existing crates on day one.** The current implementation is the final arbiter of behavior — thousands of details live in code and nowhere else. The previous rewrite (which this plan is explicitly guarding against repeating) had the failure mode of "must prompt every detail back into existence."

Two acceptable strategies:

### Strategy A — sibling path (recommended)

Keep everything under `crates/` untouched. Build new code under a new path that doesn't collide:

```
led/
  crates/           ← current FRP implementation, untouched
  crates-next/      ← new query-driven implementation, grows here
  led-next/         ← new binary
  Cargo.toml        ← workspace includes both
  docs/rewrite/     ← these docs
  docs/extract/     ← Phase A extraction output
  docs/spec/        ← narrative spec
  tests/golden/     ← goldens generated against current led (spec contract)
```

Both binaries (`led` and `led-next`) build from the same workspace. Tests for the old code continue to run. The new code can depend on common utilities (e.g., `CanonPath` types) by either copying them or referencing them via the workspace until a natural split emerges.

When `led-next` reaches parity:
- Rename `crates/` → `crates-legacy/` (keep briefly as reference)
- Rename `crates-next/` → `crates/`
- Eventually delete `crates-legacy/`

### Strategy B — branch with reference tree

Alternative: work on a `rewrite` branch where `crates/` is moved to `reference/` (read-only; no code depends on it). New code goes in a fresh `crates/`.

Strategy A is preferred because both implementations compile in the same tree, and you can `grep` across both simultaneously.

### What not to do

- **Do not** start a new branch and delete `crates/` before Phase 2 is complete. If the goldens are in place and the old code is pushed, you can recover — but the cost/risk is enormous and unnecessary.
- **Do not** try to incrementally rewrite *within* the existing FRP structure. The two architectures don't mix; partial migrations would be worse than either.

---

## Phases

```
Phase 0  Harness bootstrap            (prereq; on old code)
Phase 1  Goldens generation           (on old code)
Phase 2  Functional spec + driver inv (on old code, docs only)
Phase 3  Skeleton of new arch         (new code)
Phase 4  Domain-by-domain porting     (new code; goldens as target)
Phase 5  Parity verification          (all goldens green)
Phase 6  Swap                         (rename; delete legacy)
```

Phases 0–2 are **pre-rewrite**. Phases 3–6 are the rewrite proper.

### Phase 0 — harness bootstrap

**Goal:** a test harness capable of generating a golden for any scenario.

Work happens on the current code. See `GOLDENS-PLAN.md` for details; in brief:

- Extend the existing `TestHarness` (`led/tests/harness/mod.rs`) to capture:
  - **Rendered frame** (run the ui driver in-process to a `String` buffer instead of the real terminal).
  - **Dispatched events** (record every driver `*Out` stream emission).
  - **State snapshots** (serialize `AppState` via serde for internal goldens only — not part of spec contract).
- Add `insta` (or equivalent snapshot crate).
- Accept inputs at the raw-keypress layer (`press("Ctrl-s")`) in addition to the existing `Action`-level API. Raw keys test the keymap layer; `Action`-level tests stay useful too.
- Make time deterministic (virtual clock injected into drivers that use time).
- Normalize non-determinism (iteration order, random IDs, absolute paths) before snapshotting.

**Exit criteria:** can write a test that scripts keypresses and produces diffable golden files for frame + dispatched events. A handful of examples checked in.

### Phase 1 — goldens generation

**Goal:** the complete spec contract for the rewrite, frozen in the repo.

See `GOLDENS-PLAN.md` for the six axes. In order of priority:

1. **Mechanical axes** (per-Action, per-keybinding, per-driver-event, per-config-key). Each is enumerated from code; goldens are generated in bulk via scripts that iterate the enum and fire minimal scenarios. Hundreds of goldens produced this way.
2. **Narrative axes** (per-feature). Author a scenario per feature area (see `SPEC-PLAN.md` for the feature list). Dozens to low hundreds.
3. **Edge and combination cases**. Unicode, empty files, long lines, error paths, feature interactions (edit during LSP load, save fails, etc.). Authored.
4. **CI completeness enforcement**. Tests that fail if any Action/keybinding/driver-event exists without a corresponding golden.

**Exit criteria:** `cargo test --test golden` passes; coverage across all mechanical axes is 100% (enforced); narrative scenarios cover all major features; goldens are committed.

### Phase 2 — functional spec + driver inventory

**Goal:** human-readable reference for the rewrite.

In parallel with Phase 1 (they share the Phase A extraction step):

- Do the Phase A mechanical extraction (see `SPEC-PLAN.md`). Output: `docs/extract/*.md`.
- Write per-driver inventory docs (see `DRIVER-INVENTORY-PLAN.md`). Output: `docs/drivers/*.md`.
- Write the narrative functional spec (see `SPEC-PLAN.md`). Output: `docs/spec/*.md`, with reverse indices into the extracts.
- Cross-check: every extract entry is referenced by some narrative section (dead entries = either dead code or missing docs).

**Exit criteria:** spec covers every feature area; driver inventory covers every driver; cross-check passes.

### Phase 3 — skeleton of new arch

**Goal:** the query-driven skeleton compiles and produces a (blank or minimal) frame.

- Set up `crates-next/` workspace members.
- Define the initial domain atoms (`BufferState`, `UiState` at minimum; others as they come online).
- Define the `Event` enum (coarse inputs + resource completions).
- Write `apply_event` skeleton (match arms that panic with `todo!()` initially).
- Write a minimal `render_frame` query that returns a blank frame for empty state.
- Wire a minimal `Runtime` with `tick()` over a channel.
- Keyboard input driver → produces `Event::Key`.
- Terminal driver → calls `terminal.draw(&frame)`.

**Exit criteria:** `cargo run -p led-next` opens a blank terminal UI that responds to Ctrl-C to quit. All architectural layers exist in skeletal form.

### Phase 4 — domain-by-domain porting

**Goal:** `led-next` passes all Phase 1 goldens.

Port one domain at a time. For each domain:

1. Read the relevant driver's inventory doc (`docs/drivers/<name>.md`).
2. Read the relevant extract entries for that domain's actions and muts.
3. Define the domain atom fully (fields, lenses).
4. Define the domain's `Mut` enum and reducer.
5. Implement the dispatch logic for relevant events.
6. Implement queries that read from this domain.
7. Run the subset of goldens for this domain. Fix diffs.

Suggested order (roughly dependency order):

1. `UiState` — phase, focus, dims, alerts (enables rendering).
2. `BufferState` — the editing core.
3. `ConfigState` — keybindings, theme (blocked on file-load dispatch).
4. `WorkspaceState` / `SessionState` — startup and persistence.
5. `LspState` — diagnostics, completions, etc.
6. `GitState` — file status, line status, PR.
7. `SyntaxState` (or merge into BufferState) — highlights, brackets.
8. `SearchState` — file search + replace.
9. Remaining (clipboard, kill ring, macros, jump list, ...).

As each domain comes online, more goldens pass. Progress is measurable in "% goldens green."

**Exit criteria:** 100% of goldens green (including mechanical + narrative + edge cases).

### Phase 5 — parity verification

**Goal:** confidence that `led-next` behaves like `led`.

Beyond goldens:

- **Interactive exploration**: use `led-next` for real work for a period. Log every bug / behavior mismatch as an issue. Add goldens for each.
- **Benchmarks**: compare startup time, keypress latency, memory use. Not required to be identical; must be reasonable.
- **Coverage**: run goldens under `cargo llvm-cov`. Uncovered branches in new code are either dead or need new goldens.
- **Mutation testing**: `cargo-mutants` (optional but valuable). Surviving mutants indicate behaviors not exercised by goldens.

**Exit criteria:** goldens green + no known behavior regressions + benchmarks acceptable.

### Phase 6 — swap

**Goal:** `led-next` becomes `led`.

1. Rename workspace:
   - `crates/` → `crates-legacy/`
   - `crates-next/` → `crates/`
   - `led/` → `led-legacy/`
   - `led-next/` → `led/`
2. Update root `Cargo.toml` workspace members.
3. Update root `README.md` (replace "vibe coded FRP" framing with new arch description).
4. Update root `CLAUDE.md` (replace/supersede FRP principles with query-arch principles; see `QUERY-ARCH.md` § "What happens to the current principles").
5. Tag a release or commit marking the cutover.
6. Keep `crates-legacy/` / `led-legacy/` for at least a release cycle as a reference. Delete when you're sure nothing's missing.

**Exit criteria:** `cargo run` runs the new led; legacy is opt-in via path.

---

## Work breakdown guidelines

### What can parallelize

- Phase 1 + Phase 2: goldens and docs share Phase A extracts. Different people (or agents) can work on narrative spec and goldens generation simultaneously after extracts exist.
- Phase 4 domains can parallelize to some extent if their dispatch logic doesn't interleave. In practice, start sequential (UI → Buffers) then branch once the core is stable.

### What doesn't parallelize

- Phase 0 must complete before Phase 1 starts (no harness → no goldens).
- Phase 3 skeleton must compile before Phase 4 begins domain work.

### Agent use

Most of Phase A extraction (keybindings, actions, config keys, driver outputs) is well-suited to parallel agents. See `SPEC-PLAN.md` § "Using agents."

Mechanical golden generation (Phase 1 step 1) is also agent-friendly — give an agent a list of all `Action` variants and a harness template, ask it to produce scenario files for each.

Narrative scenarios and careful review of golden diffs should be done by a human or under close supervision.

---

## Things to watch out for

### Don't trust the new code until goldens run

The discipline during Phase 4 is: **no behavior is correct until a golden proves it.** It's tempting to say "yeah that looks right" after manually checking — but the whole point of the harness is not needing to rely on that. Every domain port ends with "run the golden subset, investigate every diff."

### Don't auto-accept golden diffs

When a golden fails during the rewrite, the prompt is strong to run `cargo insta accept` and move on. **Read every diff.** If the new behavior is intentional (fixing a bug in the old code), accept with a commit message explaining. If it's a regression, fix the new code.

### Don't skip Phase 2

The narrative spec seems optional next to the goldens — goldens are enforceable, docs aren't. But the spec is what catches *what you didn't think to test*. A feature area missing from the spec is a blind spot in the golden suite. The cross-check (every extract entry referenced by narrative) is the completeness check.

### Don't rewrite the test harness during Phase 4

The harness is the contract between old and new. If the harness changes, goldens change shape, and the spec is no longer frozen. Extensions are fine (new capture types, new input sources). Rewrites are not — except under explicit plan in Phase 6.

### Expect the edit log / rebase design to iterate

The first draft of `BufferState.edits` and `rebase_diagnostics` will probably need revision when the second position-sensitive data kind (hunks, then inlay hints) comes online. That's expected. Keep the design small, modify as needed.

---

## Milestones and how to know you're on track

- **After Phase 0**: you can write a new golden test in ~5 minutes and commit its baseline.
- **After Phase 1**: the CI "coverage" tests pass; if you add an `Action::Foo` without a golden, CI catches it.
- **After Phase 2**: you can answer "what does led do when X?" by reading docs, without running the binary.
- **After Phase 3**: `led-next` runs, shows a blank screen, accepts Ctrl-C.
- **During Phase 4**: the percentage of green goldens grows monotonically. A regression in that number means a port broke something already-ported. Investigate before moving on.
- **After Phase 4**: all goldens green.
- **After Phase 5**: goldens green + you've used `led-next` for real work for days without issue.
- **After Phase 6**: `led` is the new arch; legacy is a subdirectory.

---

## When to stop and ask

Bring the user back in before any of:

- A golden diff that looks like a bug fix in the old code (confirm intent before accepting).
- A situation where a Phase 4 port requires changing the `Event` enum or dispatch contract fundamentally (indicates `QUERY-ARCH.md` needs revision).
- Discovery of a major feature in current led that has no entry in any extract (indicates spec coverage gap).
- Benchmark regressions >2x in hot paths.
- Consideration of any destructive action on `crates/` before Phase 6.
