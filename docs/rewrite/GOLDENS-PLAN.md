# Golden-tests plan

The enforceable spec. Generated against **current led** before any rewrite work. The rewrite's job is to make the whole suite green.

Prerequisite reading: `README.md`, and ideally `REWRITE-PLAN.md` § Phase 0/1.

---

## What a golden is (quick refresher)

A golden test captures output to a checked-in file; on every subsequent run, output is compared byte-for-byte to the committed reference. Unlike a normal assertion, you don't have to know in advance what to check — the whole captured output is the contract.

In Rust, `insta` is the common library; `expect-test` is a lighter alternative. Pick one; reference: `https://docs.rs/insta`.

---

## What we snapshot, and why only these

Three capture types per golden:

1. **Rendered terminal frame** — the user-visible output. Byte-stable across implementations by design. The rewrite must produce the same pixels.
2. **Dispatched events** — every driver command issued in response to the scenario (LSP requests, FS reads, timer sets, clipboard operations). Externally observable; part of the spec contract.
3. **State snapshot (optional, internal-only)** — serialized `AppState`. Useful for current-led regression testing. **Not part of the rewrite contract** because the new arch changes state shape.

### What we deliberately do NOT snapshot

- **Internal state shape** as the contract. It changes.
- **Logs / debug traces**. Not user-visible.
- **Timing / latency**. Handled by benchmarks, not goldens.
- **Performance**. Separate benchmark suite.

---

## Input layer for goldens

Scripts drive led via **raw keypresses**, not `Action` variants. This tests the whole pipeline: key → keymap → action → state change → render. Action-level inputs remain useful for the existing integration tests, but goldens want the widest coverage.

Minimum required input primitives:

```rust
h.press("Ctrl-s");                          // key chord
h.press_seq(&["Ctrl-x", "Ctrl-s"]);         // sequential chord (save-all)
h.type_text("hello");                       // literal chars
h.inject(Event::FsChanged(path));           // simulate driver input
h.complete_request(id, response);           // respond to a resource-driver request
h.advance_time(Duration::from_secs(3));     // virtual clock
h.resize(80, 24);
```

`inject` and `complete_request` are the mechanisms for testing scenarios that involve async driver events — without those, you can't write a golden for "LSP diagnostic arrives while buffer is being edited."

---

## Six axes of coverage

The list of goldens is derived along six orthogonal axes. Axes 1–4 are mechanical (generated from code). Axes 5–6 are authored. Together they exhaust the coverage space.

### Axis 1 — per-`Action`

Every variant of the `Action` enum. One scenario per variant, in a minimal setup, exercising just that action.

- Generated: iterate `Action::variants()` (add `strum` or macro if needed); produce a scenario file per variant.
- Location: `tests/golden/actions/<action_name>/`.

### Axis 2 — per-keybinding

Every key-chord in every mode. Tests that the key actually resolves to the right action in the right context.

- Generated: iterate the keymap table; produce a scenario per binding.
- Location: `tests/golden/keybindings/<mode>/<key>/`.

### Axis 3 — per-driver-event

Every kind of event a driver can produce (e.g., `LspIn::Diagnostics`, `FsIn::DirListed`, `GitIn::FileStatuses`). One scenario per event, injecting it and capturing the consequence.

- Generated: iterate each driver's `*In` enum variants.
- Location: `tests/golden/drivers/<driver>/<event>/`.

### Axis 4 — per-config-key

Every config key at a meaningfully-different value. Demonstrates the effect of each config.

- Generated: iterate config schema; produce a scenario per key with a non-default value.
- Location: `tests/golden/config/<key>/`.

### Axis 5 — per-feature narrative

One scenario per feature area from `SPEC-PLAN.md`'s feature list, walking the happy path.

- Authored, one per feature area (~20 files).
- Location: `tests/golden/features/<feature>/`.

### Axis 6 — edge cases and combinations

Unusual conditions and feature interactions. Authored as they come to mind or are surfaced by Phase D exploration.

Examples:
- Unicode buffer content (emoji, RTL, CJK).
- Empty file, very long line (10k chars), very large file (truncated display).
- Symlinks.
- CRLF vs LF.
- Tab-vs-space indentation.
- LSP server unavailable.
- LSP diagnostic arrives after buffer has been edited beyond it (must rebase — this is THE scenario for the rewrite).
- Save fails (permission denied).
- External fs-change during edit.
- Quit with unsaved changes (confirm dialog).
- Session restore with missing files.
- Multiple buffers with diagnostic conflicts.

- Location: `tests/golden/scenarios/<name>/`.

---

## Directory layout

```
tests/
  golden/
    actions/
      move_down/
        setup.toml
        script.txt
        frame.snap
        dispatched.snap
      move_up/
      insert_char/
      ...
    keybindings/
      main/
        ctrl_s/
        ctrl_x_ctrl_s/
        esc/
      overlay/
        esc/
      ...
    drivers/
      lsp/
        diagnostics_published/
        completion_response/
        goto_def_response/
        server_crashed/
      fs/
        dir_listed/
        find_file_list/
        external_change/
      git/
        file_statuses/
        line_statuses/
      ...
    config/
      tab_width_2/
      theme_dark/
      keys_custom_save/
      ...
    features/
      buffers/
      editing/
      navigation/
      search/
      ...
    scenarios/
      edit_during_lsp_load/
      unicode_line/
      save_permission_denied/
      session_restore_missing_files/
      ...
  golden_coverage.rs        ← CI test: enforce axis completeness
  harness/                  ← the TestHarness extensions for goldens
    mod.rs
    capture.rs
    script.rs
    time.rs
```

Each golden scenario directory contains:

```
setup.toml       — initial conditions (files, config, terminal size, atoms)
script.txt       — sequence of inputs
frame.snap       — captured rendered terminal frame
dispatched.snap  — captured driver commands issued
state.snap       — (optional, internal) serialized AppState
```

`.snap` files use plain text for diffability. `insta` supports this natively.

---

## The harness contract

The new led must implement the same harness interface. This is what lets goldens generated against old led validate new led.

```rust
pub trait GoldenHarness {
    fn new() -> Self;

    // Setup
    fn with_file(&mut self, path: &str, contents: &str) -> &mut Self;
    fn with_config(&mut self, path: &str, contents: &str) -> &mut Self;
    fn with_terminal_size(&mut self, w: u16, h: u16) -> &mut Self;

    // Drive
    fn press(&mut self, chord: &str);
    fn type_text(&mut self, s: &str);
    fn inject(&mut self, event: Event);
    fn complete_request(&mut self, id: RequestId, response: Response);
    fn advance_time(&mut self, d: Duration);
    fn resize(&mut self, w: u16, h: u16);
    fn quit_and_wait(&mut self);

    // Capture
    fn render_frame(&self) -> String;
    fn dispatched(&self) -> Vec<Dispatched>;
    fn state_snapshot(&self) -> String;  // current-led only
}
```

The implementation on current led wraps the existing `TestHarness`. The implementation on new led wraps its `Runtime`.

`Dispatched` is a normalized representation of driver commands — string-form for diffability:

```
LspOut::GotoDefinition { path: "src/foo.rs", line: 3, col: 10 }
DocStoreOut::Save { path: "src/foo.rs" }
TimersOut::Set { name: "alert-clear", duration_ms: 3000 }
```

Order is preserved (dispatches are observable in order) but fields are canonicalized to avoid spurious diffs (absolute paths stripped to repo-relative, PIDs/IDs masked).

---

## Harness responsibilities

### Determinism

The golden suite must be reproducible. Sources of non-determinism and how to handle:

- **Time**: virtual clock. Every driver that uses time gets its clock injected. Tests advance time explicitly via `advance_time`. No `Instant::now()` anywhere.
- **Thread interleaving**: goldens run single-threaded. The runtime's `select!` is replaced by an explicit "next event" pull in tests.
- **Iteration order**: `HashMap` iteration order must not appear in snapshots. Either use `BTreeMap` in output-producing code paths or sort before snapshotting.
- **Random IDs**: any IDs led generates (request IDs, file hashes, …) are normalized. Either pass a fake RNG or rewrite IDs to monotonic sequences before snapshotting.
- **Absolute paths**: always strip to repo/tempdir-relative.
- **Timestamps in status messages**: either virtual-clock or regex-masked.

### Spawning and dispatch

The harness intercepts `Dispatch::spawn`: instead of actually spawning async work, it records the request and returns. The test then calls `complete_request(id, response)` to simulate completion, which injects the appropriate `Event` into the runtime.

This means goldens don't need a real tokio runtime or thread pool. Tests are pure functions from inputs to outputs.

### Rendering

Rendering must produce a string (or grid) rather than writing to a real terminal. The existing `crates/ui/` either (a) abstracts over a "terminal" trait that can be implemented with a `String` buffer, or (b) gets extended to expose a "render-to-string" entry point for tests. Prefer (a) — it's cleaner and might uncover latent coupling.

---

## Mechanical generation

Axes 1–4 should be generated, not hand-written. A build-time script or test that generates stub goldens on first run:

```rust
#[test]
fn generate_action_goldens() {
    if std::env::var("GENERATE_GOLDENS").is_err() { return; }
    for action in Action::variants() {
        let dir = format!("tests/golden/actions/{}", action.snake_case());
        if Path::new(&dir).exists() { continue; } // idempotent
        write_stub_scenario(&dir, &default_setup(), &[format!("action {action:?}")]);
    }
}
```

Run once (`GENERATE_GOLDENS=1 cargo test generate_action_goldens`), review the hundreds of stubs, commit. From there, goldens are maintained manually.

Same pattern for axes 2–4.

### Default setup for mechanical axes

Each axis needs a sensible minimal setup:

- **actions/**: a buffer with a known file ("foo.rs" containing 10 lines of known content), cursor at (0,0), terminal 80×24.
- **keybindings/**: same as actions/, plus mode context.
- **drivers/<driver>/**: same setup, with the injected event as the only script step (plus a step to allow the event to settle).
- **config/<key>/**: config override, then a minimal scenario that exercises whatever the key affects.

These baselines mean differences in the snapshots reflect *the thing being tested*, not ambient setup.

---

## Coverage enforcement (CI)

Completeness is mechanically checkable. Add tests:

```rust
#[test]
fn every_action_has_a_golden() {
    let expected: HashSet<_> = Action::variants().map(|a| a.snake_case()).collect();
    let actual = list_dirs("tests/golden/actions/");
    let missing: Vec<_> = expected.difference(&actual).collect();
    assert!(missing.is_empty(), "missing action goldens: {:?}", missing);
}

#[test]
fn every_keybinding_has_a_golden() { /* similar */ }

#[test]
fn every_driver_event_has_a_golden() { /* similar */ }

#[test]
fn every_config_key_has_a_golden() { /* similar */ }
```

These fail CI if someone adds (or the rewrite introduces) a new action/event/config without a corresponding golden. Impossible to forget.

---

## Running and reviewing

- `cargo test --test golden` runs the whole suite.
- `cargo insta review` shows a TUI for reviewing and accepting diffs.
- `cargo insta accept` accepts all pending.

**Golden-review discipline** (critical):

- **Never** auto-accept without reading diffs. Rubber-stamping regressions is the main failure mode of snapshot testing.
- When accepting, commit message explains why (behavior change, bug fix, new feature).
- If a diff looks suspicious but correct on inspection: include a comment in the scenario explaining the subtlety, so future-you remembers why.

---

## Phase ordering recap

1. **Phase 0 (harness)**: build the harness on old led. Capture frames, dispatched events, state. Virtual clock. Event injection. `insta` integrated. A handful of example goldens committed.

2. **Phase 1a (mechanical generation)**: bulk-generate stubs for axes 1–4. Review. Commit the baselines. Coverage-enforcement tests pass.

3. **Phase 1b (narrative + edge cases)**: author axes 5–6. Aim for ~30–80 narrative goldens and ~20–50 edge-case goldens.

4. **(Later, during Phase 4/5)**: run the whole suite against `led-next`. Iterate on diffs.

---

## Special considerations for the rewrite

### Version-stamped data goldens

A specific set of goldens must exercise the **versioned data + rebase** pattern (§ `QUERY-ARCH.md` Rebase queries). These are the highest-value goldens for the rewrite because they codify the behavior difference between "naive keeping-in-sync" (current) and "rebase-on-read" (target).

Canonical scenario:

```
1. Open a file with 10 lines.
2. Inject: LSP diagnostic at line 5.
3. Render frame — should show diagnostic at line 5.
4. Script: insert newline at line 2.
5. Render frame — diagnostic should now display at line 6.
6. Inject: LSP diagnostic at line 5 (now stale, buffer has moved).
7. Render frame — diagnostic should display at line 6 (the rebased position).
```

Both current led (via imperative buffer-side shifting) and new led (via rebase query) must produce the same frame at step 7. If they don't, either the current logic is wrong (fix it before committing the golden) or the rewrite plan needs adjustment.

Equivalent scenarios for git line status, inlay hints, and any other position-sensitive data. These are the "Rebase Reference Goldens" and should be authored in Phase 1b with special care.

### Dispatch goldens

Another high-value set for the rewrite. `dispatched.snap` captures what work led issues. The new arch's resource-driver pattern must issue the same requests in the same order. For example:

```
scenario: open_file_first_time
inputs: press Ctrl-x Ctrl-f, type "src/foo.rs", press Enter
dispatched:
  FsOut::ListDir { path: "." }
  FsOut::ListDir { path: "src" }
  DocStoreOut::Open { path: "src/foo.rs" }
  SyntaxOut::BufferChanged { path: "src/foo.rs", ... }
  LspOut::BufferOpened { path: "src/foo.rs", ... }
  GitOut::ScanFiles { paths: [...] }
```

The new arch's `Request::*` dispatches must produce an equivalent list (possibly in a slightly different shape, but covering the same external actions). The rewrite's correctness is verified by these dispatch-goldens more than almost anything else.

---

## What to do when a golden fails during the rewrite

Categorize the diff:

- **Rendering diff** (frame.snap): check whether the pixels differ. If yes, new arch is producing wrong visual output. Investigate.
- **Dispatch diff** (dispatched.snap): check the delta. Missing dispatches = feature not implemented yet or logic bug. Extra dispatches = overeager. Reordered dispatches = ordering bug in the new runtime.
- **Both**: compound bug; fix the logic before re-running.

The golden is "what current led does." The rewrite matches it. If mid-rewrite you genuinely need to change current-led behavior (fix a bug), update the golden as a deliberate commit, not mid-sprint.

---

## Format of setup.toml

Example:

```toml
[terminal]
width = 80
height = 24

[[file]]
path = "src/foo.rs"
contents = """
fn main() {
    println!(\"hello\");
}
"""

[config]
keys = "~/.config/led/keys.toml"
theme = "dark"

[[workspace]]
root = "."

[startup]
args = ["src/foo.rs"]
```

## Format of script.txt

One command per line:

```
press Ctrl-s
type hello
press Enter
inject LspIn::Diagnostics { path: "src/foo.rs", version: 0, diagnostics: [...] }
advance 3000ms
press Ctrl-x Ctrl-c
```

Simple parser; easy to author; diffable.

---

## Worked example

A complete golden for `actions/move_down`:

`tests/golden/actions/move_down/setup.toml`:
```toml
[terminal]
width = 80
height = 24

[[file]]
path = "test.txt"
contents = "line 1\nline 2\nline 3\n"

[startup]
args = ["test.txt"]
```

`tests/golden/actions/move_down/script.txt`:
```
action MoveDown
```

`tests/golden/actions/move_down/frame.snap`:
```
…
  1 line 1
▶ 2 line 2
  3 line 3
…
```

`tests/golden/actions/move_down/dispatched.snap`:
```
(empty — no new driver work)
```

This tiny golden verifies: opening a file, initial state, action dispatch, cursor visual, and the absence of spurious dispatches. That's a lot of coverage in ~10 lines of input.
