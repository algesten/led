# Rewrite roadmap

A concrete schedule for porting every feature of legacy `led` into the
query-driven rewrite. This is the answer to "when does X land?" — if a
feature exists in legacy and isn't on this list, the list has a bug.

The surface area comes from:

- `docs/spec/*.md` — 18 narrative feature docs.
- `docs/drivers/*.md` — 14 driver inventories.
- `docs/extract/actions.md` — 59 actions, with defaults and dead-variant
  notes.
- `docs/extract/keybindings.md` — the full binding table.
- **`goldens/scenarios/*` on `main`** — ~280 black-box PTY scenarios
  authored against the reference implementation. These are the
  enforceable spec: the rewrite is done when 100 % of them are green
  against the new binary.

Every `[Out]` bucket in a milestone doc should reference an entry
here. Milestones can renumber as scope shifts — what must not happen is
a feature being deferred with no scheduled home.

## Progress metric: `% goldens green`

Per `REWRITE-PLAN.md` § Phase 4, the single quantitative progress
signal is the percentage of the ~280 scenarios in
`goldens/scenarios/` that pass when run against the rewrite binary.
Each milestone below notes which golden *subdirectories* it is
expected to move from red → green. If a milestone doesn't move its
subset, the implementation isn't done.

Golden layout (on `main`):

```
goldens/scenarios/
  smoke/              open_empty_file, move_cursor_down_right,
                      type_and_save, external_change, lsp_diagnostic
  actions/            per-Action scenario (59 subdirs)
  keybindings/        per-binding + per-context
                      (main, ctrl_x, browser, file_search,
                       find_file, confirm_kill, kbd_macro,
                       lsp_completion, lsp_rename, lsp_code_actions)
  driver_events/      per-driver completions + errors
                      (workspace, docstore, fs, clipboard, config_file,
                       file_search, gh_pr, git, lsp, syntax, terminal_in)
  config_keys/        per-config-key
  features/           narrative scenarios grouped by feature area
  edge/               unicode, empty files, long lines, error paths
```

Harness status: the `goldens/` crate (a `portable-pty` + `vt100`
driver, excluded from the workspace) is authored on `main` but not
yet wired to run against the `rewrite` branch's binary. That wiring
is **Mα** below, done before M6.

### Golden-review discipline

Goldens encode **what legacy led does**, not **what led should do**.
They are largely correct because legacy is largely correct, but they
also encode legacy's known bugs:

- Silent no-op on `shift+a` bindings (`keymap.md` § "SHIFT
  stripped").
- Silent swallow of unrecognized second chord (`keymap.md` §
  "Chord prefix with no second chord").
- Four `[dead?]` action variants (`docs/extract/actions.md` §
  "Findings" 1).
- `Action::Suspend` actually calls `SIGTSTP` in a test harness
  (`docs/extract/actions.md` § "Findings" 3).
- Rename overlay drops `InsertChar` due to focus gate (`keymap.md` §
  "Editor-focus gating").

**Before accepting a green run as a milestone done, every new-green
scenario gets reviewed for sanity.** A scenario is suspect when any
of these patterns show up in its `dispatched.snap`:

- The same file path appears in two successive `file_load_start`
  lines without an intervening failure or reload (double read).
- A `file_save_start` / `file_save_done` pair repeats on consecutive
  ticks for the same `version` (double save).
- A driver receives a `*_cancel` command it was never told to start.
- A key input produces a trace line in a later tick than expected
  (state not sync-written).
- A render tick emits an empty frame where the previous tick had
  content, and the next tick restores it (flicker).
- Actions fire during `Phase::Starting` or `Phase::Exiting` that the
  model's migration table should have absorbed.

When a scenario is *legitimately* a bug in legacy: fix it on `main`,
regenerate the golden there, merge into `rewrite`. The rewrite
should never silently carry a legacy bug forward just because "the
golden is green." This aligns with `REWRITE-PLAN.md` § "Don't
auto-accept golden diffs."

The known-bugs list in `POST-REWRITE-REVIEW.md` is the starting
point; expect it to grow as the porting surfaces more.

---

## Shipped (milestones 1–5)

| M | Ships | Spec reference |
|---|---|---|
| M1 | Tabs + file-read driver + terminal-input driver + render skeleton | `buffers.md` (open/load), `ui-chrome.md` (minimal tab bar) |
| M2 | Visible cursor, arrow / Home / End / PageUp / PageDown movement, viewport scroll (with preferred-column preservation) | `navigation.md` (char/line/page movement), `editing.md` (cursor state) |
| M3 | Printable-char insert, `Enter`, `Backspace`, `Delete`, dirty indicator (`*`) | `editing.md` (basic insert/delete) |
| M4 | `Ctrl-S` save with atomic tmp-file + rename; `saved_version` tracking | `buffers.md` (save), `docstore.md` (write path) |
| M5 | Keymap (`Command` enum), TOML `[keys]` at `~/.config/led/config.toml`, printable-char fallback | `keymap.md` (layer 1 only — flat table, no chords/contexts) |

---

## Planned (milestones 6+)

The order below is *dependency order* where dependencies exist, and
*visibility order* where they don't — i.e. ship features that unlock
daily use before specialties.

### Mα — Goldens harness integration (prerequisite)

Not a feature; a quality gate. Pull the `goldens/` crate from `main`
into the rewrite worktree and wire it to invoke the new binary.

- Merge `goldens/scenarios/` + `goldens/src/` + `goldens/tests/` from
  `main` into `rewrite` (authored one-way per the branch strategy).
- Ensure the runner spawns the rewrite `target/debug/led`, feeds
  keystrokes, snapshots `frame.snap` + `dispatched.snap` via
  `vt100::Parser`.
- Wire `--test-clock` in the rewrite binary (reserved in M1 CLI,
  parse-only). Virtual time lets scenarios with timers run
  deterministically.
- Wire `--test-lsp-server` and `--test-gh-binary` CLI flags as
  parse-only stubs; they resolve to real behaviour at M16 / M20.
- Initial baseline: running against M5's binary, expect ≈ the
  smoke subset passing (5 scenarios) plus a handful of the
  `actions/move_*` tree. All others fail — that's the starting
  percentage, and each milestone moves it forward.

**Goldens moved to green:** `smoke/*` (all), `actions/move_*`,
`actions/insert_char`, `actions/delete_backward`,
`actions/delete_forward`, `actions/insert_newline`,
`actions/save`, `actions/quit`, `keybindings/main/<basic keys>`.

### M6 — Chord bindings + richer keymap

Catches the keymap up to legacy's two-layer scheme.

- Chord prefix: `ctrl+x ctrl+s = "save"` syntax in TOML; a trie-shaped
  `Keymap` (current flat map becomes leaves).
- Dispatch state machine: after a prefix key, the next keystroke
  consults the nested map; unrecognized key in prefix state cancels
  silently.
- No timeout (matches legacy; Emacs-tradition).
- Add `Cmd::SaveAll`, `Cmd::KillBuffer`, `Cmd::PrevTab`, `Cmd::NextTab`,
  `Cmd::FileStart`, `Cmd::FileEnd`, `Cmd::WordLeft`, `Cmd::WordRight`,
  `Cmd::Abort`, `Cmd::SaveNoFormat`, `Cmd::Quit` so the default keymap
  can rebind to legacy chords:
  - `ctrl+x ctrl+s` → save
  - `ctrl+x ctrl+a` → save-all
  - `ctrl+x ctrl+d` → save-no-format
  - `ctrl+x ctrl+c` → quit
  - `ctrl+x k` → kill-buffer
  - `ctrl+left` / `ctrl+right` → prev/next tab
  - `ctrl+home` / `ctrl+end` → file-start/end
  - `alt+f` / `alt+b` → word-right/left (word move implemented in M10)
  - `esc` / `ctrl+g` → abort
- Move `Ctrl-S` to `ctrl+x ctrl+s` as default; keep `Ctrl-S` as alias
  for easy access (dual binding).

**Spec reference:** `keymap.md` § "Compilation", "Chord key format",
"Chord-prefix state". Context overlays (browser / file_search) stay
parked until those features land (M11, M14).

**Goldens moved to green:** `keybindings/ctrl_x/*`,
`keybindings/main/<all remaining non-modal>`,
`actions/save_all`, `actions/save_no_format`,
`actions/kill_buffer` (without the confirm-kill branch — that waits
until M9), `actions/prev_tab`, `actions/next_tab`, `actions/abort`,
`actions/file_start`, `actions/file_end`.

### M7 — Mark, region, kill ring, clipboard

- `Mark` field on `Tab.cursor` state (the second anchor).
- `SetMark` command (`ctrl+space`).
- `KillRegion` (`ctrl+w`) removes mark..cursor into a kill-ring entry.
- `KillLine` (`ctrl+k`) — from cursor to EOL; consecutive kills
  accumulate into one entry.
- `Yank` (`ctrl+y`) — paste from system clipboard (`arboard`) with
  fallback to kill ring when clipboard is empty.
- `ClipboardDriver` (new `driver-clipboard/` pair) handles async
  clipboard reads/writes; kill-ring lives in state.

**Spec reference:** `editing.md` § "Mark and region", `editing.md` §
"Kill ring", `docs/drivers/clipboard.md`.

**Goldens moved to green:** `actions/set_mark`, `actions/kill_region`,
`actions/kill_line` (including accumulate), `actions/yank`,
`driver_events/clipboard/*`.

### M8 — Undo / redo

Closes out the long-deferred **edit log**. Same milestone introduces
it because undo is the first consumer.

- Per-buffer `history: VecDeque<EditOp>` and a cursor into it
  (`next_undo_index`).
- `EditOp` variants: `Insert { at, text }`, `Delete { range, text }`,
  split on cursor boundary for line ops.
- Edit coalescing: consecutive single-char inserts within the same
  word become one undoable group (matches legacy).
- `Undo` (`ctrl+/`, `ctrl+_`, `ctrl+7`) / `Redo` commands.
- Dispatch bumps `version` on every group; the history survives
  tab switch / save.

The op log format chosen here is the one LSP / git / PR rebase queries
will consume in later milestones, so the shape matters. Expect it to
evolve when the first async consumer lands (M16).

**Spec reference:** `editing.md` § "Undo groups", `POST-REWRITE-REVIEW.md`.

**Goldens moved to green:** `actions/undo`, `actions/redo`,
`edge/*edit*` scenarios that previously relied on undo.

### M9 — UI chrome: status bar, gutter, alerts

- Three-region layout: tab bar (row 0) + body + status bar (last row).
- 2-column gutter for future git/lsp marks (starts empty; reserved).
- `AlertState` source with `Info` and `Warn` levels and a TTL.
- Status bar shows: dirty marker + filename + mode string + cursor
  position (line:col) + pending alert.
- Mode string updates with `Recording` (for macros), `Chord` (when
  inside a chord prefix, echoed until the chord resolves).

**Spec reference:** `ui-chrome.md` (whole file).

**Goldens moved to green:** most `features/*` scenarios that include
status-bar snapshots, `actions/open_messages`,
`keybindings/confirm_kill/*` (M9 introduces the confirm-kill
infrastructure via the alert system).

### M10 — Extended navigation

- Word move (`alt+f` / `alt+b`, or `ctrl+left/right` if config prefers).
- `PrevTab` / `NextTab` via keymap commands.
- `MatchBracket` (`alt+]`).
- Jump list: back/forward with `alt+b/f` / `alt+left/right`. Entries
  captured on any cursor move that crosses a "significant" boundary
  (line jump ≥ 5 lines, or buffer switch).
- `JumpState` source.

**Spec reference:** `navigation.md` (full movement grid + jump list).

**Goldens moved to green:** `actions/jump_back`, `actions/jump_forward`,
`actions/match_bracket`, remaining `actions/move_*` (word / bracket
variants), `keybindings/main/alt_*` for `alt+b`/`alt+f`.

### M11 — File browser sidebar

- `BrowserState` user-decision + `driver-fs-list/` for directory
  enumeration.
- Context keymap overlay: `[browser]` in config TOML, activated when
  side-panel focused.
- `ExpandDir` / `CollapseDir` / `CollapseAll` / `OpenSelected`.
- `ToggleSidePanel` (`ctrl+b`) / `ToggleFocus` (`alt+tab`).
- Preview tab (open-on-nav without committing).

**Spec reference:** `file-browser.md`, `docs/drivers/fs.md`.

**Goldens moved to green:** `keybindings/browser/*`,
`actions/expand_dir`, `actions/collapse_dir`, `actions/collapse_all`,
`actions/open_selected`, `actions/open_selected_bg`,
`actions/toggle_focus`, `actions/toggle_side_panel`,
`driver_events/fs/list_dir*`, `features/browser/*`.

### M12 — Find-file picker (+ Save-as)

- `FindFileState` overlay.
- `FindFile` (`ctrl+x ctrl+f`) / `SaveAs` (`ctrl+x ctrl+w`).
- Prefix completion, `Tab` longest-common-prefix, `~` expansion.
- Context: overlay absorbs all actions into the modal.

**Spec reference:** `find-file.md`.

**Goldens moved to green:** `keybindings/find_file/*`,
`actions/find_file`, `actions/save_as`, `features/find_file/*`.

### M13 — Incremental in-buffer search

- `IsearchState` on Tab.
- `InBufferSearch` (`ctrl+s`) starts search; repeat advances.
- Query builds via `InsertChar` / `DeleteBackward` while isearch active.
- Non-consumed actions (arrow keys etc.) accept the current match and
  fall through — the legacy-specific "SearchAccept then run" behavior.
- `Abort` (`esc`, `ctrl+g`) closes.

**Spec reference:** `search.md` § "In-buffer isearch".

**Goldens moved to green:** `actions/in_buffer_search`,
`features/isearch/*`.

### M14 — Project-wide file search

- `FileSearchState` overlay; driven by `driver-file-search/` (ripgrep).
- Case/regex/replace toggles (`alt+1/2/3`).
- `ReplaceAll` with buffer-mediated preview flow.
- Context overlay `[file_search]`.

**Spec reference:** `search.md` § "File-search overlay",
`docs/drivers/file-search.md`.

**Goldens moved to green:** `keybindings/file_search/*`,
`actions/open_file_search`, `actions/close_file_search`,
`actions/toggle_search_case`, `actions/toggle_search_regex`,
`actions/toggle_search_replace`, `actions/replace_all`,
`driver_events/file_search/*`, `features/file_search/*`.

### M14b — Chrome theming

Introduces the theme pipeline and replaces the hard-coded chrome
styles currently emitted by `crates/driver-terminal/native/src/lib.rs`
(tab-bar `Reverse`, status-bar `Red`/`White`/`Bold`, side-panel
selection `Reverse`, side-panel border `│` in default fg) with
theme-driven styles. Landing this before M15 means syntax
highlighting plugs into an already-wired theme rather than
reinventing the loader, and lets reviewers verify every earlier
milestone's chrome at the *correct* colors.

Scope:

- `--theme` CLI flag + `theme.toml` parser (minimal: named + hex
  24-bit colors, bold/reverse/underline attrs, no cascading yet).
- `[chrome]` section covering:
  - `tab.active`, `tab.inactive`, `tab.preview`, `tab.dirty_marker`.
  - `status.normal`, `status.warn` (replacing the hard-coded
    red/white/bold at lines 265–267 of the native painter).
  - `browser.selected_focused`, `browser.selected_unfocused`,
    `browser.chevron`, `browser.border`.
  - `cursor_line` background (implicit today — the terminal default).
- Render view-models gain a `Style` field (or an equivalent
  per-region enum) so the painter selects colors rather than hard-
  coding `Color::Red` etc.
- Browser focus split: the painter currently draws the selected row
  with `Reverse` regardless of focus — M14b takes `SidePanelModel.
  focused` into account to pick focused vs unfocused selection
  styles (the cursor-hide fix in M11 handles the *cursor* side; the
  *selection color* is chrome theming).
- Ruler column (typically col 110 at 120-col terminals) —
  introduced here rather than earlier because it's a theme-owned
  overlay, not a layout primitive.

Out of scope for M14b:

- Syntax token classes in the theme file (those land in M15, which
  extends the same `theme.toml` with a `[syntax]` section).
- Per-language chrome variations.
- Nerd-font chrome glyphs — the existing `▷ ▽ │` unicode is fine.

**Spec reference:** new `docs/rewrite/specs/theming.md` (to be
authored as part of this milestone).

**Goldens moved to green:** `features/theming/*` (to be authored).
Chrome-theme assertions currently ride inside individual feature
goldens; M14b adds dedicated theme-switching coverage.

### M15 — Syntax highlighting (tree-sitter)

- `SyntaxState` per buffer: `Arc<Tree>` + `language` + version.
- `driver-syntax/` core + native: tree-sitter parse in a worker.
- Incremental reparse on edits (`edit` on tree, `parse` with
  `prev_tree`).
- Language detection: extension + shebang + modeline override.
- Highlight query → tokens → render as styled cells.
- `Frame.body` gains per-cell style (foreground, background, attrs).
- Extends the `theme.toml` introduced in M14b with a `[syntax]`
  section mapping token classes to foreground colors.

**Spec reference:** `syntax.md`, `docs/drivers/syntax.md`.

**Goldens moved to green:** `driver_events/syntax/*`,
`features/syntax/*`.

### M16 — LSP core: client bootstrap + pull diagnostics

- `LspState` per language server + `driver-lsp/` (stdio child process,
  JSON-RPC framing).
- Initialize, `textDocument/didOpen`, `didChange`, `didClose`.
- **Pull** `textDocument/diagnostic` (not publish — per user preference
  memo `feedback_pull_diagnostics.md`).
- Freeze mechanism: diagnostics are accepted only when the buffer's
  version matches the request's version; mid-edit diagnostics are
  dropped.
- First rebase-query consumer: diagnostic ranges rebased through the
  edit log introduced in M8.
- Diagnostics shown in gutter (M9) + inline underline.

**Spec reference:** `lsp.md` § "Diagnostics", `docs/drivers/lsp.md`.

**Goldens moved to green:** `smoke/lsp_diagnostic`,
`driver_events/lsp/diagnostic*`, `features/lsp/diagnostics*`,
`actions/next_issue` + `actions/prev_issue` (diagnostics portion).

### M17 — LSP completions

- `CompletionState` overlay.
- Trigger-char + identifier-prefix triggers.
- Popup rendering below cursor.
- `Tab` / `Enter` accept; `Esc` cancel; `Up` / `Down` navigate.
- Filter on continued typing.

**Spec reference:** `lsp.md` § "Completions".

**Goldens moved to green:** `keybindings/lsp_completion/*`,
`features/lsp/completion*`,
`driver_events/lsp/completion*`.

### M18 — LSP extras

- `LspGotoDefinition` (`alt+enter`).
- `LspRename` (`ctrl+r`) with overlay.
- `LspCodeAction` (`alt+i`) + picker.
- `LspFormat` + format-on-save integration (replaces the M4 save flow:
  if an LSP is attached, `format` → apply edits → write).
- `LspToggleInlayHints` (`ctrl+t`) + inlay rendering.

**Spec reference:** `lsp.md` § "Goto definition / Rename / Code
actions / Format / Inlay hints".

**Goldens moved to green:** `keybindings/lsp_rename/*`,
`keybindings/lsp_code_actions/*`, `actions/lsp_goto_definition`,
`actions/lsp_rename`, `actions/lsp_code_action`, `actions/lsp_format`,
`actions/lsp_toggle_inlay_hints`, `actions/outline`,
`features/lsp/goto*`, `features/lsp/rename*`,
`features/lsp/code_actions*`, `features/lsp/format*`,
`features/lsp/inlay_hints*`.

### M19 — Git integration

- `GitState` + `driver-git/` (libgit2 via `git2` crate).
- Per-file status (untracked / modified / staged).
- Per-line status (gutter marks: add / del / modify).
- Current branch displayed in status bar.
- Debounced scan; rebased through the edit log.
- `NextIssue` / `PrevIssue` (`alt+.`/`alt+,`) now includes git hunks.

**Spec reference:** `git.md`, `docs/drivers/git.md`.

**Goldens moved to green:** `driver_events/git/*`,
`features/git/*`, `actions/next_issue` / `actions/prev_issue`
(git-hunks portion).

### M20 — GitHub PR

- `GhPrState` + `driver-gh-pr/` (spawns `gh` CLI).
- ETag-driven polling.
- PR comments rendered alongside git gutter marks.
- `OpenPrUrl` (`ctrl+x ctrl+p`).
- `NextIssue` / `PrevIssue` now also includes PR comments.

**Spec reference:** `gh-pr.md`, `docs/drivers/gh-pr.md`.

**Goldens moved to green:** `driver_events/gh_pr/*`,
`features/gh_pr/*`, `actions/open_pr_url`.

### M21 — Persistence (session + undo)

- SQLite-backed `SessionState` (`driver-session/` + `rusqlite`).
- Primary-flock per workspace (one editor process owns a workspace
  root).
- On quit: save open tabs + cursors + scroll + workspace.
- On startup: reopen the saved session.
- Optional undo DB (history persisted across restarts).
- Cross-instance sync via notify files for multi-pane scenarios.

**Spec reference:** `persistence.md`, `docs/drivers/docstore.md`.

**Goldens moved to green:** `driver_events/docstore/*`,
`driver_events/workspace/*`, `features/persistence/*`,
`smoke/external_change` (needs this milestone + M26).

### M22 — Keyboard macros

- `MacroState`: recording flag + current record + last slot.
- `KbdMacroStart` (`ctrl+x (`) / `KbdMacroEnd` (`ctrl+x )`) /
  `KbdMacroExecute` (`ctrl+x e`).
- Chord-count accumulator (`ctrl+x 4 2 ctrl+x e` replays 42 times).
- Recursion cap + `Wait(ms)` primitive for the harness.
- `e`-repeat mode after execute.

**Spec reference:** `macros.md`.

**Goldens moved to green:** `keybindings/kbd_macro/*`,
`actions/kbd_macro_start`, `actions/kbd_macro_end`,
`actions/kbd_macro_execute`, `actions/wait`,
`features/kbd_macro/*`.

### M23 — Auto-indent, reflow, sort-imports

All three rely on syntax (M15) for language-aware logic.

- `InsertNewline` in a language with an indent query → insert newline
  then inject whitespace per the query.
- `ReflowParagraph` (`ctrl+q`): dprint-driven reflow of markdown
  paragraph / doc comment at cursor.
- `SortImports` (`ctrl+x i`): tree-sitter helpers detect the import
  block, sort its lines.

**Spec reference:** `editing.md` § "Auto-indent", "Reflow",
"Sort imports".

**Goldens moved to green:** `actions/insert_tab`,
`actions/reflow_paragraph`, `actions/sort_imports`,
`features/auto_indent/*`.

### M24 — Lifecycle: phases, quit, suspend

Most of this has been glossed over so far because we quit on Ctrl-C
with no grace. M24 brings lifecycle discipline.

- `Phase` enum: `Starting`, `Running`, `Exiting`, `Suspended`.
- `Quit` (`ctrl+x ctrl+c`): enter `Exiting`, flush session, then
  break loop.
- Dirty-buffer confirm: `confirm_kill` prompt on close of dirty tab.
- `Suspend` (`ctrl+z`): restore cooked mode, `SIGTSTP`, on resume
  re-enter raw mode + redraw.
- Startup ordering matches legacy's 14-step sequence.

**Spec reference:** `lifecycle.md`.

**Goldens moved to green:** `actions/quit`, `actions/suspend`,
`features/lifecycle/*`, `keybindings/confirm_kill/*` (dirty-tab
dismissal flow).

### M25 — Grapheme-aware column math

Promotes the M2/M3 deferral. Column arithmetic becomes
grapheme-cluster-indexed; rendering consults `unicode-width` for cell
occupancy.

- Cursor `col` semantics shift: the units are grapheme clusters, not
  chars. Requires rewriting `apply_move` / `body_model` to use
  `unicode-segmentation` iteration over the rope.
- Wide chars (CJK, emoji) render as two cells.
- Combining marks / ZWJ sequences collapse into one cluster.

**Spec reference:** none explicit — legacy mostly ignores this, we
improve it. Golden tests must add wide-char + combining cases.

**Goldens moved to green:** relevant `edge/unicode*` scenarios. New
scenarios authored here also land on `main` first (the branch rule
in `REWRITE-PLAN.md`), since M25 is a behaviour improvement over
legacy, not a regression fix.

### M26 — External file change detection

- `driver-file-watch/` (FSEvents on macOS, inotify on Linux, etc. via
  `notify`).
- On disk change for an open buffer: compare disk hash vs
  `saved_version` baseline. If user hasn't edited → reload silently.
  If dirty → prompt to discard or reload.

**Spec reference:** `buffers.md` § "External change",
`docs/drivers/fs.md`.

**Goldens moved to green:** `smoke/external_change`,
`features/external_change/*`.

---

## Orphan items worth a concrete home

From the extract, four actions are `[dead?]` in legacy. The rewrite
can either ship them properly or explicitly drop them:

- `Action::Outline` — bound to `alt+o`, no handler. Intent: jump
  between outline entries (functions / sections). **Schedule: M18**
  (after LSP + syntax so we have data).
- `Action::OpenMessages` — bound to `ctrl+h e`, no handler. Intent:
  show the alert log / recent messages. **Schedule: M9**, as part of
  the alert system.
- `Action::OpenSelectedBg` — bound to `alt+enter` in browser, no
  handler. Intent: open file in background tab. **Schedule: M11**.
- `Action::SaveForce` — no handler, not bound. **Drop** — the new
  `SaveNoFormat` (M6) covers the documented intent.

---

## How to use this doc

**When planning a milestone design doc:** its `Out` section must cite
specific future milestone numbers from this file (not "deferred until
a user wants it").

**When a new need surfaces that isn't on this list:** add an entry to
the orphan section or retarget an existing milestone; don't just drop
the work into a nebulous "later" bucket.

**When renumbering happens:** update the M-references in earlier
milestone docs. The per-milestone `Out` lists are a distributed index;
this file is the authoritative schedule.

## Continuous-discipline items (not milestones)

These ride along whenever the relevant surface is under the
keyboard; they don't get their own milestone.

- **Idle-tick allocation audit.** Before each new milestone that
  touches a hot path (render, dispatch, query memos), re-check the
  `Vec::new()` / `String::new()` sites and confirm they stay
  allocation-free on cache hits. Post-course-correct #7 the known
  sinks are gone; stay vigilant.
- **Dispatch → driver plumbing tests.** As new dispatch → execute
  paths land (save, list, clipboard, paint, LSP), add a capture-
  driver helper + assertions that `execute()` was called with the
  expected shape. Course-correct #9 left this as a per-milestone
  discipline.
- **Trace-emission verification.** Every new trace site (per
  feature) earns at least one test that asserts the emission
  fires. Catches silent misfires before a golden diff two weeks
  later exposes them.
- **`led-testutil` extraction.** When the third consumer of
  `dispatch/testutil.rs` shows up (first will be fs-list driver
  integration tests), extract to a workspace crate.

## Things deliberately not scheduled

- **`led_macros` / Lua scripting.** Legacy doesn't have it; no need
  for the rewrite.
- **Plugin system.** Same.
- **Remote / SSH editing.** Not in legacy.
- **Non-editor commands** (open mail, file manager integration).
  Not in legacy.

If a future feature ask shows up that isn't in legacy, it lands
after the rewrite is complete — not woven into the roadmap.
