# Functional spec plan

How to produce a functional spec for current led that doesn't miss features.

The failure mode being guarded against: "I must prompt every single thing back into existence." The user has been burned by this once before. The spec is the insurance policy.

---

## Principle: extract, don't compose

The wrong way: "let me write down what led does" from memory. You will miss things.

The right way: **derive the spec from code.** Every class of thing in the current code is a completeness anchor. List every `Action`, every keybinding, every driver output, every config key — mechanically. Then layer prose on top and cross-check that every extract entry is covered.

---

## Four phases

```
Phase A — mechanical extraction     (agent-driven; high coverage, no semantics)
Phase B — narrative layer           (authored; semantics; reverse index to A)
Phase C — behavior capture          (goldens; ground-truth; see GOLDENS-PLAN.md)
Phase D — interactive exploration   (catches what A/B/C miss)
```

Phases A and B produce docs. Phase C produces tests (separate workstream, see `GOLDENS-PLAN.md`). Phase D fills residual gaps.

---

## Phase A — mechanical extraction

Produce one file per extraction target. Flat lists. Agents are excellent at this — run several in parallel.

Target output: `docs/extract/*.md`, one file per category below.

### A.1 Keybindings

Source: `crates/config-file/`, any default-keymap definition in `crates/core/` or similar, and any runtime keymap resolution in `crates/core/` action modules.

Format:
```
| Key          | Mode/Context     | Action                | Notes                     |
|--------------|------------------|-----------------------|---------------------------|
| Ctrl-s       | main             | Save                  |                           |
| Ctrl-x Ctrl-s| main             | SaveAll               |                           |
| Esc          | overlay          | Abort                 | dismisses any overlay     |
| ...          | ...              | ...                   | ...                       |
```

Include: default bindings + any mode-specific or context-specific bindings (main, sidebar, overlay, isearch, find-file, completion, etc.).

Exit criterion: every key-chord reachable by the user is listed.

### A.2 Actions

Source: `Action` enum in `crates/core/`.

Format:
```
## Action::MoveDown
- Purpose: move cursor one line down
- State read:  active buffer, cursor position, scroll, doc contents
- State written: cursor row/col, scroll offset (if necessary)
- Triggers: keymap "Down" in main mode, macro playback
- Dispatched requests: none
```

Every variant gets a stanza. Fields: purpose, state read, state written, triggers (what causes this action to fire), dispatched requests (async work spawned).

Exit criterion: every `Action` variant has a stanza.

### A.3 Muts

Source: `Mut` enum in current led's `model/` module.

Format:
```
| Variant                | State field(s) changed        | Produced by                      |
|------------------------|-------------------------------|----------------------------------|
| SetPhase(Phase)        | state.phase                   | actions_of::suspend, resume, quit|
| BufferOpen{...}        | state.buffers[path], tabs     | buffers_of::from docstore Opened |
| ...                    | ...                           | ...                              |
```

This is a *translation table* for the rewrite. Each variant maps to which domain atom owns that data in the new arch.

Exit criterion: every `Mut` variant is listed with its source and target.

### A.4 Driver inputs/outputs

See `DRIVER-INVENTORY-PLAN.md` — per-driver docs live there. Phase A is about the bulk listing.

Source: each driver's `*Out`/`*In` types and their handlers in `derived.rs` and `model/*_of.rs`.

Format: per-driver summary with I/O types enumerated. See `DRIVER-INVENTORY-PLAN.md`.

Exit criterion: every driver has an entry; every I/O variant is listed.

### A.5 Config keys

Source: keys config (keymap TOML), theme config, any other user-configurable settings.

Format:
```
## Config: keys (keys.toml)
- Path resolution: ~/.config/led/keys.toml (override with $LED_CONFIG_DIR)
- Schema: table of key = action mappings
- Default: baked-in defaults (see A.1)
- Hot-reload: yes (via config-file driver watcher)

| Key          | Action   | Default |
|--------------|----------|---------|
| ctrl+s       | Save     | yes     |
| ...          | ...      | ...     |
```

Exit criterion: every config key with its default and effect is listed.

### A.6 Persisted artifacts

Anything led writes to disk that's meaningful on next startup.

- Session database (location, schema, what's persisted).
- Undo database (location, schema, persistence trigger, retention).
- Config files (location, format, hot-reload).
- Cache files (if any).
- Log files (location, format).

Format: one stanza per artifact. Fields: path, format (sqlite schema / TOML / binary), write triggers, read triggers, growth behavior, GC.

Exit criterion: every file led opens for write is listed.

### A.7 CLI flags and invocation modes

Source: `bin/` main or arg parser.

Format:
```
| Flag                 | Purpose                           | Default              |
|----------------------|-----------------------------------|----------------------|
| --config-dir <path>  | override config directory         | $XDG_CONFIG_HOME/led |
| --keys-file <path>   | load keymap from specific file    | config-dir/keys.toml |
| --keys-record        | record keypresses to stdout       | off                  |
| --flamegraph         | emit profiling data (debug build) | off                  |
| <path> [path ...]    | open files at startup             | —                    |
```

Exit criterion: every CLI flag documented.

### A.8 Panels, overlays, pickers

Every distinct UI surface the editor can show.

Format:
```
## Completion popup (LSP)
- Trigger: typing after a completion-trigger char, or Ctrl-space
- Content: list of completion items from LSP, fuzzy-filtered by prefix
- Input: Up/Down to select, Enter to accept, Esc to abort
- Dismisses on: Esc, Enter, cursor move off line, buffer switch

## Find file
- Trigger: Action::FindFile
- Content: fuzzy-filtered list of files under workspace + recents
- Input: type to filter, Up/Down, Enter to open, Tab to expand path, Esc to abort
```

One stanza per overlay/panel.

Exit criterion: every modal/overlay/popup documented.

### A.9 Alerts and status messages

Every user-visible string led produces (informational, warnings, errors).

Format:
```
| Message                         | Level | Triggered by                        | Duration   |
|---------------------------------|-------|-------------------------------------|------------|
| "saved {path}"                  | info  | docstore Save success               | 3s         |
| "file changed externally"       | warn  | fs notify, file modified outside    | until ack  |
| "LSP server crashed: {name}"    | error | lsp driver detects server exit      | until ack  |
| ...                             | ...   | ...                                 | ...        |
```

Exit criterion: every string path in alert/warn producers is listed.

### A.10 Timers

Every named timer led sets (source: `derived.rs` timer output stream).

Format:
```
| Timer name          | Duration | Fired from                  | Effect                    |
|---------------------|----------|-----------------------------|---------------------------|
| "alert-clear"       | 3000ms   | on new alert                | clear alert               |
| "git-scan-debounce" | 500ms    | on git-affecting activity   | trigger GitOut::ScanFiles |
| "pr-poll"           | 15000ms  | always while branch has PR  | trigger GhPrOut::PollPr   |
| ...                 | ...      | ...                         | ...                       |
```

Exit criterion: every timer name led uses documented.

### A.11 Error paths

Every distinct failure mode and what happens.

Format:
```
## FS read fails
- Trigger: docstore tries to open a file, OS returns Err
- State effect: buffer is opened with empty doc? Error dialog? Alert?
- User recovery: dismiss alert, retry

## LSP server fails to start
- Trigger: lsp driver spawns server, process exits quickly
- State effect: LspState records server unavailable; alert shown
- User recovery: fix installation, restart led
```

Exit criterion: every error path in current code has an entry.

### A.12 Startup sequence

The ordered flow of what happens from process-start to first-paint.

Format: a numbered list describing each step, what state is initialized, what drivers start, what events fire.

Exit criterion: a new developer can follow startup top-to-bottom and understand what runs when.

---

## Phase B — narrative layer

Now the prose spec, organized by user-facing feature area. This is what a reviewer reads to answer "what does led do?"

Target output: `docs/spec/*.md`, one file per feature area.

### Suggested feature areas

Produce one `.md` per area. Start with a table of contents:

1. **`lifecycle.md`** — startup, session resume, suspend/resume (Ctrl-Z), quit, crash recovery.
2. **`buffers.md`** — open, close, save, save-as, tabs, preview tabs, active/inactive buffer, external changes, kill-buffer with confirm.
3. **`editing.md`** — insert, delete, newline, tab/indent, undo/redo, mark/region, kill/yank, kill ring, clipboard integration, auto-indent, bracket matching, paragraph reflow, import sort.
4. **`navigation.md`** — cursor movement (basic, word, line, file), page scroll, jump list, outline, match-bracket.
5. **`search.md`** — in-buffer isearch, file search across workspace (with regex/case/replace toggles), replace-all.
6. **`find-file.md`** — find-file picker, save-as picker (shares find-file), directory expansion logic.
7. **`file-browser.md`** — sidebar, expansion, reveal, navigation keys, open-selected (fg/bg).
8. **`lsp.md`** — diagnostics (inline and gutter), completions (trigger chars, fuzzy filter, auto-import), goto-definition, rename, code actions, format, inlay hints, progress indicator, server lifecycle.
9. **`git.md`** — file status badges, line status (change bars), branch display, gutter integration, debounced scanning.
10. **`gh-pr.md`** — PR metadata, URL open, polling.
11. **`syntax.md`** — tree-sitter parsing, highlighting, rainbow brackets, bracket matching, indent detection.
12. **`ui-chrome.md`** — status bar (content, layout, phase indicator, LSP progress), gutter, alerts (info/warn/error), confirm dialogs.
13. **`keymap.md`** — keymap loading, compilation, hot-reload, modes/contexts, chord handling.
14. **`persistence.md`** — session DB schema, undo DB, workspace detection, sync between instances.
15. **`macros.md`** — recording, playback, playback count.
16. **`cli.md`** — command-line arguments, headless / test modes, key recording.
17. **`config.md`** — config files, defaults, theme, hot-reload.

Each file has the following structure:

```markdown
# <feature area>

## Summary
One paragraph describing the feature area.

## Behavior
Prose description of what the feature does. Edge cases called out inline.

## User flow
Typical interaction sequence: "user opens X, types Y, sees Z".

## State touched
- `BufferState.cursors` — read/written on every move
- `LspState.diagnostics` — read (by render)
- ...

## Extract index
- Actions: [MoveUp, MoveDown, ...] → docs/extract/actions.md
- Keybindings: [Up, Down, ...] → docs/extract/keybindings.md
- Driver events: [terminal-in keys, lsp diagnostics] → docs/extract/drivers.md
- Timers: [...] → docs/extract/timers.md

## Edge cases
- Empty file
- Unicode characters
- Very long lines
- etc.

## Error paths
- LSP timeout during completion
- FS read fails during save (read-modify-write sanity check)
```

The **Extract index** is the mechanical completeness check. Every action/keybinding/event referenced here should exist in the extract files. Every extract entry should appear in at least one feature area's index.

---

## Phase C — behavior capture (goldens)

Runs in parallel with Phases A and B. See `GOLDENS-PLAN.md`.

The relationship: goldens are the *enforceable* spec, the narrative is the *readable* spec. Both exist; neither replaces the other.

Feedback loop between B and C: while writing a feature area's narrative, reviewer notices a scenario ("what happens if you save while LSP is formatting?"). Add it as a golden scenario. The scenario's existence in the golden suite means the rewrite will regress-check it.

---

## Phase D — interactive exploration

Even with A+B+C, some behaviors won't be captured — they emerge from interactions the code doesn't branch on explicitly, or they're documented nowhere and no test covers them.

Approach:

1. Use led for real work. Keep notes of anything surprising, anything you rely on.
2. Have an agent generate "exploration scripts" ("open two files, edit one with LSP active, press Ctrl-S, then delete the other, then quit"). Run them. Record outcomes.
3. For every behavior found: add a golden if novel; add a spec entry if uncovered.

This is open-ended; stop when the yield drops (days without new findings).

---

## Using agents

Phase A is highly parallel. Suggested splits:

- One agent for A.1 (keybindings).
- One agent for A.2 (actions) — scans the Action enum and every handler module.
- One agent for A.4 (driver I/O) — reads each driver crate.
- One agent for A.5 (config keys).
- One agent for A.6 (persisted artifacts) — traces every file-write.
- One agent for A.7 (CLI).
- One agent for A.8 (panels/overlays/pickers) — hardest to be exhaustive; requires reading render code.
- One agent for A.9 (alerts/status) — greps for alert/warn/error string producers.
- One agent for A.10 (timers) — traces timer output stream producers.
- One agent for A.11 (error paths) — scans `Result`/`Err` paths in drivers.
- One agent for A.12 (startup sequence).

Prompt pattern:

> Extract the complete list of <target> from the led codebase. Output format: <format>. Do not editorialize. Do not suggest improvements. If you find variants/cases that seem undocumented, include them. If you find dead code, include it with a [dead?] marker.

Phase B is mostly authored — agents can draft individual feature-area files from extracts + code reading, but careful human review is essential. Specific prompt: "Given the extracts in docs/extract/, draft the narrative for <feature area>. Cite extract entries. Flag anything unclear."

---

## Cross-check

When Phases A and B are both drafted, run the cross-check:

1. For each entry in each extract file: is it referenced by at least one Phase B narrative?
2. For each Phase B narrative: are all its extract-index entries valid (i.e., they exist in the extracts)?

Failures are actionable:

- Extract entry not referenced anywhere → either dead code (confirm, delete, leave a note) or missing feature coverage in Phase B.
- Narrative references unknown extract entry → typo or stale narrative.

This is mechanizable. A small script that parses both sides and reports mismatches.

Run the cross-check as CI once the docs exist: changes to extracts or narratives that break the cross-check fail CI.

---

## Scope and stopping criteria

**Scope: everything user-visible.** If a user can perceive it happening (via terminal output, filesystem change, network request), it goes in the spec.

**Out of scope (for the narrative):** internal representations, precise data structure choices, optimization tricks. These can still be documented if valuable, but they're not part of the rewrite contract. The rewrite may choose different internals.

**Stopping criteria:**

- Every extract file exists and is complete by its own criterion.
- Every feature area has a narrative.
- Cross-check passes.
- Phase D exploration for a week has surfaced no new findings.

At that point, the spec is "done" for Phase 2. Further polish during Phase 4 is expected as gaps surface during the rewrite.

---

## Format conventions

- **File paths**: repository-relative, backticked (`` `crates/state/src/lib.rs` ``).
- **Types and identifiers**: backticked (`` `BufferState` ``, `` `Action::MoveDown` ``).
- **File line references**: `crate/path/file.rs:123` so they're clickable.
- **Keybinding notation**: `Ctrl-s`, `Ctrl-Shift-p`, `Esc`, `Meta-x`. Chord-style: `Ctrl-x Ctrl-s`.
- **Tables for enumerations**, prose for explanations.
- **One-liners after colons**, longer descriptions below.

---

## Useful starting commands

```bash
# List all Action variants
rg --type rust 'pub enum Action' -A 200 crates/core/src/

# List all Mut variants
rg --type rust 'pub enum Mut' -A 500 led/src/model/

# Find every driver's *Out and *In types
rg --type rust 'pub enum \w+Out' crates/
rg --type rust 'pub enum \w+In'  crates/

# Find every timer name
rg 'TimersOut::Set' -A 3

# Find every alert/warn producer
rg 'Mut::Alert|Mut::Warn' -A 2

# Find all _of.rs files
ls led/src/model/*_of.rs
```

(Use the `Grep` tool in code, not shell `rg` — the above are hints for mental orientation.)
