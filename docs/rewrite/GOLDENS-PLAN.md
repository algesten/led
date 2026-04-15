# Golden-tests plan

The enforceable spec. Generated against **current led** before any rewrite work. The rewrite's job is to make the whole suite green.

Prerequisite reading: `README.md`, and ideally `REWRITE-PLAN.md` § Phase 0/1.

---

## Approach: binary black box

Goldens drive the **compiled `led` binary** in a pseudoterminal. Nothing in the test suite imports from `led-*` crates; nothing references internal types (`Action`, `AppState`, `Mut`, driver `*In`/`*Out`). The spec contract is exactly:

- Keystrokes and resize events sent to the PTY.
- ANSI output coming back from the PTY (parsed into a text grid).
- A small, stable set of CLI flags the binary must honor.
- A line-based trace file the binary writes when asked.
- Scripted fake-server binaries for external resources (LSP, `gh`, optionally `git`).

This is strictly more durable than an in-process harness. Every internal refactor is a no-op for the goldens; the rewrite just has to re-implement the same CLI flags and trace format. The same golden files run unchanged against legacy led on `main` and new led on the `rewrite` branch.

---

## What a golden is (quick refresher)

A golden test captures output to a checked-in file; on every subsequent run, output is compared byte-for-byte to the committed reference. Unlike a normal assertion, you don't have to know in advance what to check — the whole captured output is the contract.

In Rust, `insta` is the common library; `expect-test` is a lighter alternative. Pick one; reference: `https://docs.rs/insta`.

---

## What we snapshot, and why only these

Two capture types per golden:

1. **Rendered terminal grid** — the PTY output run through a terminal emulator (e.g. `vt100`) and serialized as a plain-text grid. Byte-stable, diffable, implementation-agnostic. The rewrite must produce the same grid.
2. **Dispatched trace** — a normalized, virtual-time-stamped log of every externally-observable action the binary takes: resource requests, file writes, spawned subprocesses, clipboard operations, timer sets. Written by led itself when `--golden-trace <path>` is set.

### What we deliberately do NOT snapshot

- **Internal state.** Not accessible from outside the binary; shape is changing in the rewrite anyway.
- **Raw ANSI bytes.** Too implementation-sensitive (order of cursor-moves, color-set sequences). We snapshot the *rendered grid*, which is stable across equivalent rendering strategies.
- **Logs / debug traces.** Not user-visible.
- **Timing / latency.** Handled by benchmarks, not goldens.

---

## Input layer

The test runner spawns `${LED_BIN} <args>` in a PTY and drives it with raw keystrokes:

```rust
let mut g = GoldenRunner::new("scenarios/my_test")?;
g.press("Ctrl-s")?;
g.press_seq(&["Ctrl-x", "Ctrl-s"])?;
g.type_text("hello")?;
g.resize(80, 24)?;
g.settle()?;          // wait for PTY output + trace file to stabilize
g.assert_frame()?;    // snapshot against frame.snap
g.assert_trace()?;    // snapshot against dispatched.snap
```

- `press` translates a key-chord string into the correct raw byte sequence (including modifiers, function keys, arrow keys).
- `settle` waits until the PTY output stream has been quiet for N ms *and* the trace file has not grown for M ms. All snapshots go through `settle` first.
- There is no `inject(Event)` and no `complete_request(id, response)`. External events (LSP diagnostics, fs changes, git results) enter the binary through the same mechanism production uses: responses from the server on the other side. In tests, that "server" is a scripted fake binary. See "Scripted fake resources" below.

---

## Six axes of coverage

The list of goldens is derived along six orthogonal axes. Axes 1–4 are mechanical (generated from code). Axes 5–6 are authored. Together they exhaust the coverage space.

### Axis 1 — per-`Action`

Every variant of the `Action` enum. One scenario per variant, in a minimal setup, exercising just that action.

- Generated: the *list* of Action variants is read from the code (mechanically, once); for each, we author a script of keystrokes that triggers it (usually the default keybinding).
- Location: `tests/golden/actions/<action_name>/`.

### Axis 2 — per-keybinding

Every key-chord in every mode. Tests that the key actually resolves to the right action in the right context.

- Generated: iterate the keymap table; produce a scenario per binding.
- Location: `tests/golden/keybindings/<mode>/<key>/`.

### Axis 3 — per-driver-event

Every kind of event a driver can produce (e.g., `LspIn::Diagnostics`, `FsIn::DirListed`, `GitIn::FileStatuses`). One scenario per event, triggering it via the corresponding scripted fake server and capturing the consequence.

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
        fakes/                ← optional scripted fake-server responses
          lsp.script
          gh.script
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
  runner/                   ← the subprocess test runner
    mod.rs
    pty.rs
    grid.rs                 ← vt100 wrapper
    trace.rs                ← trace-file reader + normalizer
    fakes.rs                ← fake-server process spawners
    keys.rs                 ← chord → raw-bytes translation
```

Each golden scenario directory contains:

```
setup.toml       — initial conditions (files, config, terminal size, fakes)
script.txt       — sequence of keystrokes
fakes/*.script   — scripted responses for fake servers, optional
frame.snap       — captured rendered terminal grid
dispatched.snap  — captured virtual-time-stamped trace
```

`.snap` files are plain text. `insta` supports this natively.

---

## The binary contract

The entire code-side surface of the golden system. Both legacy led and the rewrite must honor these:

### CLI flags

| Flag                         | Effect                                                                 |
|------------------------------|------------------------------------------------------------------------|
| `--golden-trace <path>`      | Append one line per dispatched external action to `<path>`.           |
| `--test-clock`               | Enable virtual clock: idle-advance to the next scheduled timer.       |
| `--config-dir <path>`        | (already exists) override config directory.                            |
| `--no-workspace`             | (already exists) skip workspace/session/git/LSP.                       |
| `--test-lsp-server <path>`   | (already exists) use the given binary as the LSP server.               |
| `--test-gh-binary <path>`    | (already exists) use the given binary as `gh`.                         |

The rewrite may add flags; it must not remove these.

### Trace format

One line per dispatched action. Format is deliberately loose — stable string representations, not a typed protocol:

```
t=0ms        FsRead            src/foo.rs
t=12ms       LspRequest        rust-analyzer goto_definition path=src/foo.rs line=3 col=10
t=20ms       DocStoreSave      src/foo.rs bytes=420
t=3020ms     TimerSet          alert-clear duration_ms=3000
t=3020ms     Render            frame_id=5 cells_changed=12
```

- `t=` is virtual-time-millisecond offset from process start (under `--test-clock`) or wall-clock ms (otherwise; not used by goldens).
- The category token (`FsRead`, `LspRequest`, ...) is stable. Fields after it are key-ordered and canonicalized (paths repo-relative, IDs masked).
- Ordering reflects actual dispatch order.
- The `Render` line is the hook the runner uses for "settle"; when a Render line appears and no new traces follow for M ms, the frame is ready to snapshot.

The exact token set is documented in `docs/drivers/*.md` as part of Phase 2 (each driver's translation table lists the trace tokens it emits).

### Virtual clock (`--test-clock`)

- All `Instant::now()` / `tokio::time::sleep` / timer scheduling in led goes through an injectable clock.
- When the event loop has no pending external I/O *and* a timer is scheduled for the future, the clock jumps forward to that timer's fire-time and the timer fires.
- When fake servers are in use, they also operate on the virtual clock: a scripted response timed for `t=3000ms` fires when the clock reaches 3000ms.
- Wall-clock does not appear in any output under `--test-clock`.

### Scripted fake resources

For any external process led talks to (LSP server, `gh`), a companion fake binary reads a script file and replays it. Script format (one command per line):

```
# lsp.script for scenario "diagnostic arrives after edit"
on initialize          reply { "capabilities": { ... } }
on didOpen src/foo.rs  at +500ms publish { path: src/foo.rs, version: 0, diagnostics: [{ line: 5, msg: "unused" }] }
on didChange           at +200ms publish { path: src/foo.rs, version: $version, diagnostics: [] }
```

- `on <trigger>` matches an incoming request/notification from led.
- `at +<N>ms` schedules a response at virtual-time offset from the trigger.
- `reply` responds synchronously; `publish` pushes an unsolicited server notification.
- Variables like `$version` are interpolated from the triggering message.

The fake server binary lives in `crates/fake-lsp/`, `crates/fake-gh/`, etc., built once and referenced from tests by path. Current led already has `--test-lsp-server`/`--test-gh-binary` wired — the shift is from "custom fake binary per test" to "one fake binary + a scripted response file per test."

---

## Test runner responsibilities

The `tests/golden/runner/` code is the entire test-side surface.

### Spawning

1. Parse `setup.toml`: build a scratch workspace with files, config, symlinks.
2. Build the `led` command line: `${LED_BIN} --test-clock --golden-trace <tmp>/trace.log --config-dir <tmp>/config ...`.
3. If `setup.toml` declares fakes: spawn each fake binary first (they open a Unix socket or named pipe led connects to), and pass their paths via `--test-lsp-server` / `--test-gh-binary`.
4. Spawn led in a PTY at the declared `width × height`.

### Driving

1. Read `script.txt`. For each line:
   - `press <chord>` — send chord bytes to PTY.
   - `type <text>` — send literal text bytes.
   - `resize <w> <h>` — send SIGWINCH after updating PTY dimensions.
   - `wait <ms>` — advance the settle horizon by N ms (virtual).
   - `quit` — press the configured quit chord and wait for process exit.
2. After each line (except `wait`), call `settle()`: block until PTY output is quiet for ≥50ms wall-clock *and* the trace file has not grown for the same window. `wait` explicitly allows virtual time to advance.

### Capturing

1. **Grid**: feed every byte received from the PTY into a `vt100::Parser`. At snapshot time, serialize the current `vt100::Screen` as a text grid (one line per row, trailing spaces trimmed, with a small header for cursor position + relevant attributes).
2. **Trace**: read the trace file, normalize (strip absolute paths to repo-relative, mask non-deterministic IDs), emit the canonicalized form.
3. Feed both into `insta::assert_snapshot!(@"frame.snap")` and `insta::assert_snapshot!(@"dispatched.snap")`.

### Determinism sources

- **Time**: `--test-clock` makes it virtual. Runner never calls `sleep`; settle uses wall-clock only to detect quiescence.
- **Thread/task interleaving**: led is already single-threaded for the event loop; keep it that way. Trace order reflects loop order.
- **Iteration order**: anything `HashMap`-derived that reaches the trace or frame must be sorted before emission. The binary is responsible; tests just verify.
- **Random IDs**: trace normalizer rewrites request IDs to monotonic sequences; file hashes to `<hash-N>`.
- **Absolute paths**: stripped to repo-relative / tempdir-relative by the trace normalizer and by led itself (it should already emit repo-relative paths in user-visible output).
- **Timestamps in status messages**: virtual clock makes them deterministic; screenshots of "saved at 15:02" become "saved at 00:00:03".

---

## Mechanical generation

Axes 1–4 should be generated, not hand-written. A build-time script generates stub goldens on first run:

```rust
#[test]
fn generate_action_goldens() {
    if std::env::var("GENERATE_GOLDENS").is_err() { return; }
    for action in list_actions_from_source() {
        let dir = format!("tests/golden/actions/{}", action.snake_case());
        if Path::new(&dir).exists() { continue; } // idempotent
        write_stub_scenario(&dir, &default_setup(), &default_script_for(&action));
    }
}
```

`list_actions_from_source()` parses the Action enum from source (syn crate) rather than importing it; keeps the test crate free of code dependencies.

Run once (`GENERATE_GOLDENS=1 cargo test generate_action_goldens`), review the hundreds of stubs, commit. From there, goldens are maintained manually.

Same pattern for axes 2–4.

### Default setup for mechanical axes

Each axis needs a sensible minimal setup:

- **actions/**: a buffer with a known file ("foo.rs" containing 10 lines of known content), cursor at (0,0), terminal 80×24.
- **keybindings/**: same as actions/, plus mode context.
- **drivers/<driver>/**: same setup, with a scripted fake firing the event in question as the only script step.
- **config/<key>/**: config override, then a minimal scenario that exercises whatever the key affects.

These baselines mean differences in the snapshots reflect *the thing being tested*, not ambient setup.

---

## Coverage enforcement (CI)

Completeness is mechanically checkable. Add tests:

```rust
#[test]
fn every_action_has_a_golden() {
    let expected: HashSet<_> = list_actions_from_source().map(snake_case).collect();
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

1. **Phase 0 (runner + binary contract)**: build the subprocess test runner (`tests/golden/runner/`), add `--golden-trace` and `--test-clock` to led, build the scripted fake-server binaries. A handful of example goldens committed.

2. **Phase 1a (mechanical generation)**: bulk-generate stubs for axes 1–4. Review. Commit the baselines. Coverage-enforcement tests pass.

3. **Phase 1b (narrative + edge cases)**: author axes 5–6. Aim for ~30–80 narrative goldens and ~20–50 edge-case goldens.

4. **(Later, on the rewrite branch)**: run the whole suite against the new `led` binary. Iterate on diffs. The runner doesn't change; only the binary on the other end of the PTY does.

---

## Special considerations for the rewrite

### Version-stamped data goldens

A specific set of goldens must exercise the **versioned data + rebase** pattern (§ `QUERY-ARCH.md` Rebase queries). These are the highest-value goldens for the rewrite because they codify the behavior difference between "naive keeping-in-sync" (current) and "rebase-on-read" (target).

Canonical scenario (expressed as a script + a scripted fake-LSP response):

```
# script.txt
type fn main() {\n
press Enter
press Enter
press Enter
press Enter
press Enter                       # buffer now has 5 blank lines after `fn main() {`
wait 100ms                        # let settle
press Ctrl-n                      # move down (just for a render)
```

```
# fakes/lsp.script
on initialize          reply { "capabilities": { ... } }
on didOpen             at +50ms  publish { path: $path, version: 0, diagnostics: [{ line: 4, msg: "unused" }] }
on didChange version=5 at +50ms  publish { path: $path, version: 5, diagnostics: [{ line: 4, msg: "unused" }] }
```

The frame snapshot at the end must show the diagnostic on line 4 *as currently located* after edits — whether by imperative shift (current led) or rebase query (new led). Both produce the same `frame.snap`. This is the exact scenario the rewrite architecture was designed for.

Equivalent scenarios for git line status, inlay hints, and any other position-sensitive data. These are the "Rebase Reference Goldens" and should be authored in Phase 1b with special care.

### Dispatch goldens

Another high-value set for the rewrite. `dispatched.snap` captures what work led issues. The new arch's resource-driver pattern must issue the same external actions in the same order. For example:

```
# scenario: open_file_first_time
# script.txt
press Ctrl-x
press Ctrl-f
type src/foo.rs
press Enter

# dispatched.snap
t=0ms       FsListDir        .
t=5ms       FsListDir        src
t=20ms      DocStoreOpen     src/foo.rs
t=25ms      SyntaxParse      src/foo.rs version=0
t=30ms      LspDidOpen       src/foo.rs version=0
t=40ms      GitScan          src/foo.rs
```

The new arch's `Request::*` dispatches must produce an equivalent trace (possibly in a slightly different shape within the tolerance of the normalizer, but covering the same external actions in the same order). The rewrite's correctness is verified by these dispatch-goldens more than almost anything else.

---

## What to do when a golden fails during the rewrite

Categorize the diff:

- **Frame diff** (`frame.snap`): check whether the rendered grid differs. If yes, new arch is producing wrong visual output. Investigate.
- **Trace diff** (`dispatched.snap`): check the delta. Missing trace lines = feature not implemented yet or logic bug. Extra lines = overeager. Reordered lines = ordering bug in the new runtime.
- **Both**: compound bug; fix the logic before re-running.

The golden is "what current led does." The rewrite matches it. If mid-rewrite you genuinely need to change current-led behavior (fix a bug), update the golden on `main` first, then merge into the `rewrite` branch.

---

## Format of setup.toml

```toml
[terminal]
width = 80
height = 24

[[file]]
path = "src/foo.rs"
contents = """
fn main() {
    println!("hello");
}
"""

[config]
keys = "~/.config/led/keys.toml"
theme = "dark"

[[workspace]]
root = "."

[startup]
args = ["src/foo.rs"]

[fakes]
# Optional; declares scripted fake binaries for this scenario.
lsp = "fakes/lsp.script"
gh  = "fakes/gh.script"
```

## Format of script.txt

One command per line:

```
press Ctrl-s
type hello
press Enter
wait 3000ms
press Ctrl-x Ctrl-c
```

Commands:

| Command             | Meaning                                                             |
|---------------------|---------------------------------------------------------------------|
| `press <chord>`     | Send the key-chord bytes. Supports `Ctrl-`, `Alt-`, `Shift-`, `Meta-`, named keys (`Up`, `Enter`, `Esc`, `F1`...). |
| `press <c1> <c2>`   | Sequential chord (e.g. `Ctrl-x Ctrl-s`).                            |
| `type <text>`       | Send literal characters.                                            |
| `resize <w> <h>`    | Update PTY size and send SIGWINCH.                                  |
| `wait <N>ms`        | Advance the settle horizon by N virtual-ms.                         |
| `quit`              | Press the configured quit chord and wait for process exit.          |

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
press Down
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
(no external dispatches)
```

This tiny golden verifies: opening a file, initial state, key → action mapping, cursor visual, and the absence of spurious dispatches. That's a lot of coverage in ~10 lines of input.
