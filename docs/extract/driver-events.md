# Driver events inventory

One entry per `*In` variant produced by every driver. Goldens test-injection field tells the downstream generator exactly how to cause each event.

Drivers and their `*In` types:

- `workspace` → `WorkspaceIn` (+ inner `SyncResultKind`)
- `docstore` → `DocStoreIn` (wrapped in `Result<_, Alert>`)
- `lsp` → `LspIn`
- `syntax` → `SyntaxIn` (struct, not enum — one "event kind")
- `git` → `GitIn`
- `gh-pr` → `GhPrIn`
- `fs` → `FsIn`
- `file-search` → `FileSearchIn`
- `timers` → `TimersIn` (one shape, discriminated by `name: &'static str`)
- `clipboard` → `ClipboardIn`
- `config-file` → `Result<ConfigFile<Keys>, Alert>` and `Result<ConfigFile<Theme>, Alert>`
- `terminal-in` → `TerminalInput`

---

## workspace

Driver: `crates/workspace/`. The biggest driver in the tree: git-root detection, primary-instance flock, sqlite session/undo DB, `.git/` sentinel watcher, and cross-instance notify watcher. Init is one-shot (fires a fixed sequence right after `WorkspaceOut::Init`). The rest are event-driven responses or watcher signals.

### `WorkspaceIn::Workspace { workspace }`
- Cause: workspace driver resolves git root and acquires primary-instance flock after `WorkspaceOut::Init`
- Frequency: once at startup (skipped entirely in `--no-workspace` mode)
- Consumed in: `led/src/model/mod.rs:64-79` (`workspace_s`) → `Mut::Workspace` (sets browser root, seeds `pending_lists`, triggers first `pending_file_scan`)
- Test injection: natural — any scenario with `git_init = true` and `no_workspace = false` produces it; the runner sets up `.git/` and derives `start_dir` from the workspace temp dir

### `WorkspaceIn::SessionRestored { session }`
- Cause: after `Workspace`, the driver loads the session row from sqlite and attaches per-buffer undo state; in `--no-workspace` mode it is sent with `session: None`
- Frequency: once at startup
- Consumed in: `led/src/model/session_of.rs:104-253` — fan-out to ~12 child streams (active_tab_order, show_side_panel, positions, browser state, jump list, pending_lists, resume tabs/entries/phase, no-resume tabs/focus/reveal, standalone dir listing)
- Test injection:
  - `None` branch: natural — fresh workspace or `no_workspace = true`
  - `Some(session)` branch: needs new mechanism — the runner has no way to pre-seed the sqlite session DB. Would need either (a) setup.toml `[[session_buffer]]` entries that the runner inserts via `db::save_session()` before spawn, or (b) a two-phase test (scenario quits led to save, second scenario reopens). (a) is simpler.

### `WorkspaceIn::SessionSaved`
- Cause: response to `WorkspaceOut::SaveSession` (primary only)
- Frequency: once on quit (primary only)
- Consumed in: `led/src/model/mod.rs:56-62` (`workspace_misc_s`) → `Mut::SessionSaved` (sets `session.saved = true`); `led/src/lib.rs:339-350` gates actual quit on this flag
- Test injection: natural — quit via `C-x C-c` in a git workspace; trace shows `WorkspaceSaveSession` dispatch

### `WorkspaceIn::UndoFlushed { file_path, chain_id, persisted_undo_len, last_seen_seq }`
- Cause: response to `WorkspaceOut::FlushUndo` after sqlite write succeeds
- Frequency: per `undo_flush` timer fire (default 500ms after edits) per buffer
- Consumed in: `led/src/model/mod.rs:117-143` → `Mut::UndoFlushed` (calls `buf.undo_flush_confirmed`, updates `last_seen_seq`)
- Test injection: natural — type in a buffer, wait >500ms, scenario sees `WorkspaceFlushUndo` trace and this event is absorbed silently into buffer state. No visible frame change except the absence of "unsynced" indicators.

### `WorkspaceIn::SyncResult { result }` (inner `SyncResultKind`)
Covers three sub-kinds from `WorkspaceOut::CheckSync`:

#### `SyncResultKind::SyncEntries { ... }`
- Cause: another led instance wrote undo entries to sqlite; CheckSync returns entries newer than our `last_seen_seq`
- Frequency: per cross-instance write per open buffer
- Consumed in: `led/src/model/sync_of.rs:35` — applies entries via `BufferState::try_apply_sync`
- Test injection: needs new mechanism — requires a second led process writing to the same DB. Achievable via a setup.toml `[[sqlite_entry]]` section that the runner pre-inserts before spawn; then a script command like `touch-notify <path>` (or a natural NotifyEvent fired by a second mid-test fs write to the notify dir) triggers CheckSync.

#### `SyncResultKind::ExternalSave { file_path }`
- Cause: sqlite has no undo-after rows but the file's mtime moved (another instance saved without undo changes)
- Frequency: rare, per external save via another led
- Consumed in: `led/src/model/sync_of.rs:28-34` — invalidates buffer
- Test injection: needs new mechanism — same as SyncEntries but with an empty entry set post-save

#### `SyncResultKind::NoChange { file_path }`
- Cause: CheckSync race — NotifyEvent fires but the entries are our own, or chain_id matches and no entries
- Frequency: per spurious notify event
- Consumed in: `led/src/model/sync_of.rs:26` — dropped with `None`
- Test injection: unlikely target; natural fallout of other sync scenarios

### `WorkspaceIn::NotifyEvent { file_path_hash }`
- Cause: a file in `$config/notify/` was modified (touch from another instance after FlushUndo or ClearUndo). Debounced 100ms by the driver.
- Frequency: per cross-instance flush/clear
- Consumed in: `led/src/model/mod.rs:147-158` → `Mut::NotifyEvent` → `s.pending_sync_check.set(path)` → derived layer issues `WorkspaceOut::CheckSync` → produces `SyncResult`
- Test injection: needs new mechanism — runner must write to `<config_dir>/notify/<hash>` mid-test. Script command `notify <path>` that computes `path_hash()` and writes an empty file would work.

### `WorkspaceIn::WorkspaceChanged { paths }`
- Cause: recursive `.git`-less watcher on workspace root fires for Create/Remove events (Modify ignored)
- Frequency: per external file create/delete inside the workspace
- Consumed in: `led/src/model/mod.rs:81-107` → `Mut::WorkspaceChanged` (refreshes dir listings for visible parent dirs)
- Test injection: needs new mechanism — requires mid-test fs mutation. Plannable as script command `fs-create <path>` / `fs-remove <path>`. FSEvents is fast (<3ms on darwin per MEMORY) so waits would be short.

### `WorkspaceIn::GitChanged`
- Cause: root watcher sees `.git/index`, `.git/HEAD`, or `.git/refs/*` modified
- Frequency: per external `git` command (commit, checkout, add -u)
- Consumed in: `led/src/model/mod.rs:109-113` → `Mut::GitChanged`; also forked via `lib.rs:294-298` to `git_activity` stream, which triggers the `pr_settle` timer
- Test injection: needs new mechanism — runner must run `git add`/`git commit` mid-test. Script command `git-cmd <args...>` that runs in the workspace dir.

### `WorkspaceIn::WatchersReady`
- Cause: final step of Init, after both watchers are registered with the shared `FileWatcher`
- Frequency: once at startup
- Consumed in: `led/src/model/mod.rs:56-62` → `Mut::WatchersReady` (sets `session.watchers_ready = true` — gates cross-instance sync machinery)
- Test injection: natural — every scenario produces it during startup settle

---

## docstore

Driver: `crates/docstore/`. Both a resource driver (open/save/save-as) and an external-change watcher via per-parent-dir registrations on the shared `FileWatcher`. All variants wrapped in `Result<DocStoreIn, Alert>` — the Err side is used for save I/O failures.

### `DocStoreIn::Opening { path }`
- Cause: immediate ack at the top of the async open handler, before the disk read
- Frequency: per `DocStoreOut::Open` request
- Consumed in: `led/src/model/buffers_of.rs:164` — explicitly dropped with `None`
- Test injection: not testable in isolation — it never causes a state change. Deliberately inert; exists so the model can tell "open request accepted" from "open request lost", but currently no consumer uses it.

### `DocStoreIn::Opened { path, doc }`
- Cause: disk read completed for `DocStoreOut::Open`
- Frequency: per file open (from CLI arg, find-file, session restore, jump-to-def, preview)
- Consumed in: `led/src/model/buffers_of.rs:18-91` → `Mut::BufferOpen` (materializes buffer, applies session cursor/scroll, attaches undo history, activates tab)
- Test injection: natural — any `setup.toml [[file]]` that is opened either via CLI arg, find-file dialog, or session restore. The smoke `open_empty_file` scenario covers the CLI-arg path.

### `DocStoreIn::Saved { path, doc }`
- Cause: disk write completed for `DocStoreOut::Save`
- Frequency: per save (C-x C-s)
- Consumed in: `led/src/model/buffers_of.rs:92-106` → `Mut::BufferSaved`
- Test injection: natural — `type ...\npress Ctrl-s`. Trace shows `FileSave`, frame shows "Saved X" alert. Covered by smoke `type_and_save`.

### `DocStoreIn::SavedAs { path, doc }`
- Cause: disk write completed for `DocStoreOut::SaveAs` (C-x C-w)
- Frequency: per save-as
- Consumed in: `led/src/model/buffers_of.rs:107-126` → `Mut::BufferSavedAs`
- Test injection: natural — needs a find-file/save-as dialog scenario. `press Ctrl-x Ctrl-w`, then type new path, Enter.

### `DocStoreIn::ExternalChange { path, doc }`
- Cause: parent-dir watcher fired Create/Modify for a file we have open; driver re-reads from disk and compares content hash
- Frequency: per external edit of an open file
- Consumed in: `led/src/model/buffers_of.rs:127-163` → `Mut::BufferUpdate` (reloads buffer) or mark-as-externally-saved
- Test injection: needs new mechanism — mid-test fs write. Script command `fs-write <path> <content>` would trigger this. Alternatively, a sibling process spawned by the runner.

### `DocStoreIn::ExternalRemove { path }`
- Cause: parent-dir watcher fired Remove for a file we have open
- Frequency: per external delete of an open file
- Consumed in: `led/src/model/buffers_of.rs:165` — dropped with `None`
- Test injection: **unused/inert handler.** Event fires but model ignores it. Can still be demonstrated by mid-test `fs-remove <path>`, but frame/trace would only show upstream effects (e.g. later WorkspaceChanged).

### `DocStoreIn::OpenFailed { path }`
- Cause: disk read failed and `create_if_missing` was false (session restore for a deleted file, or an Opened path we can't read)
- Frequency: per failed open
- Consumed in: `led/src/model/buffers_of.rs:166` → `Mut::SessionOpenFailed` (removes tab + buffer, marks resume entry Failed)
- Test injection: needs new mechanism — simulate a deleted session-file. Either (a) pre-seed session DB with a path that doesn't exist on disk at spawn time, or (b) add setup.toml `session_missing_file = "..."`. The "natural" path via jump-to-def never takes this branch because jump uses `create_if_missing = true`.

### `Err(Alert)` wrapper
- Cause: save failures (tmpfile write, rename, dir creation). Never on open (opens always succeed via fallback or become `OpenFailed`).
- Frequency: rare in tests
- Consumed in: `led/src/model/buffers_of.rs:167` → `Mut::alert(a)`
- Test injection: needs new mechanism — make the save target unwritable. Could be done via a setup.toml `readonly_dir = "..."` or a script command `chmod -w <path>` before a save attempt.

---

## lsp

Driver: `crates/lsp/`. Manages per-language server processes and translates JSON-RPC protocol into domain types. Backed by `fake-lsp` in tests.

### `LspIn::Diagnostics { path, diagnostics, content_hash }`
- Cause: response to `RequestDiagnostics` pull (or the post-didOpen publish that fake-lsp sends)
- Frequency: per diagnostic cycle (triggered by timer after buffer changes settle) per opened file
- Consumed in: `led/src/model/lsp_of.rs:131-145` → `Mut::LspDiagnostics`
- Test injection: **fake-lsp config** — set `diagnostics."src/main.rs" = [{ range=..., severity=..., message=... }]` in setup.toml `[fake_lsp.diagnostics]`. Covered by smoke `lsp_diagnostic`.

### `LspIn::Completion { items, prefix_start_col }`
- Cause: response to `LspOut::Complete`
- Frequency: per completion trigger (C-SPC or trigger char)
- Consumed in: `led/src/model/lsp_of.rs:109-121` → `Mut::LspCompletion`
- Test injection: **fake-lsp config** — set `completions = [{ label, insertText, kind, ... }]` in `[fake_lsp]`, then a script that triggers completion (type a char, press C-SPC). Frame shows popup.

### `LspIn::CodeActions { actions }`
- Cause: response to `LspOut::CodeAction`
- Frequency: per code-action request (M-RET)
- Consumed in: `led/src/model/lsp_of.rs:123-129` → `Mut::LspCodeActions`
- Test injection: **fake-lsp config** — set `code_actions = [{ title, kind, edit/command }]` in `[fake_lsp]`. Trigger via keybind.

### `LspIn::Edits { edits }`
- Cause: response to rename, code-action-apply, or formatting
- Frequency: per rename / per code-action-select / per format
- Consumed in: `led/src/model/lsp_of.rs:75-107` → `Mut::LspEdits` + special-cased "empty edits after format" → `Mut::LspFormatDone` + cleanup
- Test injection:
  - Rename: **fake-lsp** supports `textDocument/rename` natively, no config needed — returns whole-word edits for the identifier at the cursor. Script: place cursor on a word, bind rename, type new name.
  - Format: **fake-lsp config** — set `formatting."src/main.rs" = "formatted content"`.
  - Code action: **fake-lsp config** code_actions with inline edits.

### `LspIn::Navigate { path, row, col }`
- Cause: response to `LspOut::GotoDefinition` — a `Location`
- Frequency: per goto-def (M-.)
- Consumed in: `led/src/model/lsp_of.rs:13-71` — five child streams (JumpRecord, BufferUpdate/RequestOpen, SetTabPendingCursor, ActivateBuffer)
- Test injection: **fake-lsp natural** — fake-lsp's `definition` handler searches for `fn <word>` in the document and returns the matching location. Requires a file containing `fn foo() { foo() }` and cursor on the call site.

### `LspIn::InlayHints { path, hints }`
- Cause: response to `LspOut::InlayHints` (viewport refresh)
- Frequency: per viewport change while inlay hints are enabled
- Consumed in: `led/src/model/lsp_of.rs:147-153` → `Mut::LspInlayHints`
- Test injection: **needs fake-lsp extension** — fake-lsp currently returns `[]` for all inlay-hint requests. Add a config field `inlay_hints: HashMap<String, Vec<Value>>` parallel to `diagnostics`.

### `LspIn::TriggerChars { extensions, triggers }`
- Cause: parsed from the `initialize` response's completion capabilities
- Frequency: once per server startup
- Consumed in: `led/src/model/lsp_of.rs:182-194` → `Mut::LspTriggerChars`
- Test injection: **fake-lsp config** — set `trigger_characters = [".", ":", ">"]` in `[fake_lsp]`.

### `LspIn::Progress { server_name, busy, detail }`
- Cause: `$/progress` notifications from server (begin → report → end)
- Frequency: per server work-done token
- Consumed in: `led/src/model/lsp_of.rs:155-169` → `Mut::LspProgress` (drives status bar spinner + server name)
- Test injection: **fake-lsp natural** — fake-lsp automatically sends begin+end on every didOpen. So any LSP-enabled scenario produces at least one Progress cycle during startup settle. For mid-test progress, fake-lsp would need an extension to emit progress on demand (e.g. trigger characters or a config-driven delayed progress).

### `LspIn::Error { message }`
- Cause: server crash, spawn failure, transport error, or explicit `window/showMessage` with Error severity
- Frequency: rare
- Consumed in: `led/src/model/lsp_of.rs:171-180` → `Mut::Warn { key: "lsp", message }` (shown as an alert)
- Test injection: needs new mechanism — no config path currently triggers it. Options: (a) use `--test-lsp-server` pointing to `/bin/false` (spawn succeeds, exits immediately — transport error), (b) extend fake-lsp with a config flag `simulate_error = "..."` that writes a `window/showMessage` notification, (c) extend fake-lsp to exit mid-session on a trigger.

---

## syntax

Driver: `crates/syntax/`. Tree-sitter based highlighting + bracket matching + auto-indent. Single `SyntaxIn` struct (no variants) — every event carries a full snapshot. The driver itself coalesces multiple `BufferChanged` commands per buffer.

### `SyntaxIn { ... }`
- Cause: response to `SyntaxOut::BufferChanged` (after coalescing) for a buffer whose extension/modeline matched a known language
- Frequency: per buffer change + per viewport change, coalesced within the driver's channel drain loop (bursts collapse to 1)
- Consumed in: `led/src/model/mod.rs:481-517` — three child streams keyed on `syntax_will_indent` / `syntax_can_apply_indent` (`Mut::SyntaxHighlights`, `Mut::ApplyIndent`, `Mut::SetReindentChars`)
- Test injection: natural — open any file with a known extension (`.rs`, `.js`, `.go`, etc.). Tree-sitter runs in-process, no fake needed. Frame shows colored text. Buffers with no language extension receive no SyntaxIn.

Note: there's no "closed" event on the In side — `SyntaxOut::BufferClosed` is fire-and-forget, the driver never acks.

---

## git

Driver: `crates/git/`. libgit2-based file/line status scanner. Triggered via `GitOut::ScanFiles`.

### `GitIn::FileStatuses { statuses, branch }`
- Cause: response to `GitOut::ScanFiles` — repo-wide file-level status scan
- Frequency: per scan (triggered by `git_file_scan` timer 50ms after any signal that bumps `pending_file_scan`, e.g. save, GitChanged, Workspace)
- Consumed in: `led/src/model/mod.rs:519-528` → `Mut::GitFileStatuses` (drives sidebar decorations + branch in status bar)
- Test injection: natural — every `git_init = true` scenario produces one at startup. Extra scans happen on save or external git command.

### `GitIn::LineStatuses { path, statuses }`
- Cause: per-file line-level diff scan, computed as part of the same `ScanFiles` batch for each file with a dirty status. Also sent with empty statuses for files that **were** dirty but are no longer, so the gutter clears.
- Frequency: one per dirty file per scan, plus one "clear" per formerly-dirty file
- Consumed in: `led/src/model/mod.rs:530-539` → `Mut::GitLineStatuses` (drives the left gutter +/−/~ marks)
- Test injection: natural — any `git_init = true` scenario with an edited file produces them. For the "clear" variant, a scenario that edits then reverts (or stages) a file.

---

## gh-pr

Driver: `crates/gh-pr/`. Wraps the `gh` CLI for PR metadata, diff, and review comments. `fake-gh` replaces the real binary in tests. Etag-based conditional polling.

### `GhPrIn::PrLoaded { number, state, url, api_endpoint, etag, diff_lines, comments, file_hashes }`
- Cause: response to `GhPrOut::LoadPr` (initial) or `GhPrOut::PollPr` (returned 200)
- Frequency: once on branch-with-PR detection; thereafter per 15s `pr_poll` timer fire that returns changes
- Consumed in: `led/src/model/gh_pr_of.rs:66-70` → `Mut::SetPrInfo(Some(...))`
- Test injection: **fake-gh config** — set `[fake_gh.pr_view] = { number, state, url, reviews, headRefOid }`, `[fake_gh.pr_diff] = "..."`, and optionally `[fake_gh.graphql] = { data: { repository: { pullRequest: { reviewThreads: ... } } } }`. Scenario must `git_init = true` and create a branch matching the gh view. See `gh-pr/src/lib.rs:101-160`.

### `GhPrIn::PrUnchanged`
- Cause: `PollPr` request got HTTP 304 (etag matched)
- Frequency: per 15s poll while PR is unchanged
- Consumed in: `led/src/model/gh_pr_of.rs:67` — filtered out (no state change)
- Test injection: **needs fake-gh extension** — current fake-gh always serves the same `pr_view`, so the `api --include` path never produces 304. Add support for `[fake_gh.api_endpoint_304 = true]` or inspect the `If-None-Match` header.

### `GhPrIn::NoPr`
- Cause: `gh pr view` exited non-zero (no PR for the current branch), or the JSON couldn't be parsed
- Frequency: once per LoadPr on a branch without a PR
- Consumed in: `led/src/model/gh_pr_of.rs:67-70` → `Mut::SetPrInfo(None)` (via `to_pr_info` returning None)
- Test injection: **fake-gh config** — omit `pr_view` from the config so fake-gh exits with code 1.

### `GhPrIn::GhUnavailable`
- Cause: `gh` binary not on PATH (spawn failure)
- Frequency: once at startup if gh is missing
- Consumed in: `led/src/model/gh_pr_of.rs:67-70` → `Mut::SetPrInfo(None)`
- Test injection: needs new mechanism — the runner always passes `--test-gh-binary <path>` pointing at fake-gh, so this path never fires. Either (a) pass a non-existent path like `/nonexistent/gh` via a new setup flag, or (b) omit the override entirely in some scenarios to let the real gh be searched.

---

## fs

Driver: `crates/fs/`. Synchronous-style directory listings for browser panel and find-file completion.

### `FsIn::DirListed { path, entries }`
- Cause: response to `FsOut::ListDir` (browser expand, workspace init, `pending_lists`)
- Frequency: per browser dir expand; bursts at startup (workspace root + session-restored expanded dirs) and on `WorkspaceChanged` refreshes
- Consumed in: `led/src/model/mod.rs:423-429` → `Mut::DirListed`
- Test injection: natural — every scenario with a workspace produces at least one (the root listing). Deeper dirs via `press Enter` on a sidebar item.

### `FsIn::FindFileListed { dir, entries }`
- Cause: response to `FsOut::FindFileList` (find-file dialog prefix-completion)
- Frequency: per find-file prefix change (debounced by the dialog itself before reaching the driver)
- Consumed in: `led/src/model/mod.rs:431-445` → `Mut::FindFileListed`
- Test injection: natural — `press Ctrl-x Ctrl-f`, type a partial path prefix. Frame shows completion menu.

---

## file-search

Driver: `crates/file-search/`. ripgrep-style workspace-wide search and replace. Coalesces bursts to latest.

### `FileSearchIn::Results { results }`
- Cause: response to `FileSearchOut::Search`
- Frequency: per search query (coalesced — typing a long query collapses to one run)
- Consumed in: `led/src/model/mod.rs:541-555` → `Mut::FileSearchResults`
- Test injection: natural — `press Ctrl-x s` (or whatever the binding is for workspace search), type query, wait for results.

### `FileSearchIn::ReplaceComplete { results, replaced_count }`
- Cause: response to `FileSearchOut::Replace`
- Frequency: per replace-all confirmation
- Consumed in: `led/src/model/mod.rs:557-574` → `Mut::FileSearchReplaceComplete`
- Test injection: natural — workspace search + replace confirmation script. Produces "Replaced N" alert.

---

## timers

Driver: `crates/timers/`. Named timers with four schedule modes (Replace, KeepExisting, Independent, Repeated). Single `TimersIn { name }` event — discriminated by the string.

All timer names are string constants. The model splits them: `undo_flush` goes through a state-sampling chain (`model/mod.rs:380-421`), all others route through `handle_timer()` in `model/mod.rs:1513-1543`.

### `TimersIn { name: "alert_clear" }`
- Cause: 5s timer after an alert is set; `TimersOut::Set { schedule: Replace }`
- Frequency: per alert
- Consumed in: `handle_timer` → clears `state.alerts`
- Test injection: needs new mechanism — requires waiting >5s wall-clock, which slows every test. Add virtual clock / `--test-clock` (mentioned in scenario.rs:68 as not yet landed). Alternatively, a script command `advance-clock 5s` once the clock is faked.

### `TimersIn { name: "undo_flush" }`
- Cause: 500ms timer after edit; `KeepExisting`
- Frequency: per edit burst per buffer
- Consumed in: `model/mod.rs:386-421` — samples state, produces `UndoFlushReady` per dirty buffer; derived then dispatches `WorkspaceOut::FlushUndo`
- Test injection: natural — `type ...\nwait 600ms` produces it. Trace shows `WorkspaceFlushUndo`.

### `TimersIn { name: "spinner" }`
- Cause: 80ms Repeated timer while LSP is busy
- Frequency: every 80ms during an LSP busy span
- Consumed in: `handle_timer` → increments `spinner_tick` (drives status-bar animation)
- Test injection: needs fake-lsp extension — fake-lsp's progress is begin→end in the same message, so the busy span is instantaneous and spinner rarely ticks. Need a config flag to hold the "begin" state for a configurable duration.

### `TimersIn { name: "tab_linger" }`
- Cause: 3s timer reset on every active-tab change; `Replace`
- Frequency: per tab-active-for-3s
- Consumed in: `handle_timer` → calls `buf.touch()` (updates last-used for LRU tab eviction)
- Test injection: virtual clock needed (see `alert_clear`).

### `TimersIn { name: "git_file_scan" }`
- Cause: 50ms coalescing timer after `pending_file_scan` is set; `Replace`
- Frequency: per scan request burst
- Consumed in: `handle_timer` → bumps `scan_seq`; derived emits `GitOut::ScanFiles`
- Test injection: natural — save a file, wait briefly. Trace shows `GitScan`.

### `TimersIn { name: "pr_settle" }`
- Cause: timer started on `git_activity` (GitChanged events); `Replace`
- Frequency: per quiescence after external git command
- Consumed in: `handle_timer` → bumps `pr_settle_seq` (triggers PR reload)
- Test injection: natural fallout from a `git-cmd` script command (if added) + fake-gh config.

### `TimersIn { name: "pr_poll" }`
- Cause: 15s Repeated timer started when a PR is loaded
- Frequency: every 15s
- Consumed in: `handle_timer` → bumps `pr_poll_seq`; derived emits `GhPrOut::PollPr`
- Test injection: virtual clock needed. Short of that, can set the timer to a small value via an override (not currently supported).

---

## clipboard

Driver: `crates/clipboard/`. arboard-based system clipboard. Headless variant uses an in-memory buffer (goldens use headless).

### `ClipboardIn::Text(text)`
- Cause: response to `ClipboardOut::Read`
- Frequency: per yank (C-y) — only the Read path produces In events; Write is fire-and-forget
- Consumed in: `led/src/model/mod.rs:447-477` — applies as yank at cursor, falls back to kill-ring when system clipboard is empty
- Test injection: natural — `press Ctrl-y`. In headless mode, the in-memory buffer returns whatever was last `Write`n (or empty → kill-ring fallback). A scenario that kill-rings first (`C-k`) then yanks covers both paths.

---

## config-file

Driver: `crates/config-file/`. Reads `keys.toml` and `theme.toml` from the config dir. Two driver instances share the same `ConfigFileOut` stream.

### `Ok(ConfigFile<Keys>)`
- Cause: response to `ConfigFileOut::ConfigDir { config, read_only: false }`
- Frequency: once at startup (no hot-reload currently)
- Consumed in: `led/src/model/mod.rs:367-372` → `Mut::ConfigKeys` → state → `keymap_s` builds the keymap
- Test injection: natural — every scenario produces it. The runner has to point `config_dir` at a valid config dir (default keys used when file missing, via `ConfigFile::default_toml`).

### `Ok(ConfigFile<Theme>)`
- Cause: same as above for theme
- Frequency: once
- Consumed in: `led/src/model/mod.rs:373-376` → `Mut::ConfigTheme`
- Test injection: natural

### `Err(Alert)` (either channel)
- Cause: TOML parse error — file exists but is malformed
- Frequency: rare
- Consumed in: → `Mut::alert(a)`
- Test injection: needs new mechanism — runner would need to write a malformed `keys.toml` or `theme.toml` into the scenario's config dir before spawn. Plannable as setup.toml `[[config_file]] path = "keys.toml" contents = "garbage"`.

---

## terminal-in

Driver: `crates/terminal-in/`. Crossterm event loop. Goldens inject synthetic events via the PTY rather than through this driver's channel (the runner sends key bytes into the PTY's master end, which crossterm reads through its normal pipeline).

### `TerminalInput::Key(KeyCombo)`
- Cause: user keypress
- Frequency: per keystroke
- Consumed in: `led/src/model/actions_of.rs:33-50` — resolves to an `Action` via the keymap (chord prefixes, literal char insert)
- Test injection: natural — every scenario driven by `press` or `type` produces these.

### `TerminalInput::Resize(w, h)`
- Cause: initial size probe at driver startup + SIGWINCH / PTY resize
- Frequency: once at startup + per terminal resize
- Consumed in: `led/src/model/actions_of.rs:22-27` → `Mut::Resize` → updates `AppState::dims`
- Test injection: natural — runner specifies cols/rows in `setup.toml [terminal]`. For mid-test resize, needs a script command `resize <cols> <rows>` that calls PTY ioctl (not currently supported).

### `TerminalInput::FocusGained`
- Cause: terminal focus-in escape sequence (only if the terminal supports and is configured to send focus events)
- Frequency: per window focus change (if enabled)
- Consumed in: **unused** — no filter_map matches this variant in `actions_of.rs` or elsewhere. Dropped silently.
- Test injection: **unused handler.** Not worth a golden. Flag for review: is this intentional (e.g. "we reserve this for future"), or dead code?

### `TerminalInput::FocusLost`
- Same as FocusGained: **unused**.

---

## Findings

### Events with no test path today (require new mechanism)

1. **Mid-test filesystem mutation** — blocks coverage of `WorkspaceChanged`, `DocStoreIn::ExternalChange`, `ExternalRemove`, cross-instance sync scenarios, and the save-error Err branch of docstore. A script command like `fs-write <path> <content>`, `fs-remove <path>`, `chmod -w <path>` is the highest-value single addition. FSEvents on darwin is fast (<3ms per MEMORY) so waits can be short.

2. **Virtual / faked clock** — required for `alert_clear` (5s), `tab_linger` (3s), `pr_poll` (15s) and to make `spinner` tick visibly. `scenario.rs:68` already acknowledges this as pending (`--test-clock`). Without it these timers only fire via wall-clock waits that slow the test suite.

3. **Cross-instance / second-process simulation** — blocks `WorkspaceIn::NotifyEvent`, `SyncResultKind::SyncEntries`, `SyncResultKind::ExternalSave`. Either (a) pre-seed sqlite via `[[session_buffer]]` and `[[undo_entry]]` sections in setup.toml, or (b) a script command that writes the notify-dir file directly (simpler, doesn't require a second led to be a binary). Plus a `touch-notify <path>` script command.

4. **External git command** — blocks `WorkspaceIn::GitChanged` and the `pr_settle` timer. A script command `git-cmd <args...>` running in the workspace dir is simple and composes with existing git_init.

5. **Pre-seeded session DB** — blocks the non-empty branch of `WorkspaceIn::SessionRestored` (most of the Resuming phase is untested). Needs `[[session_buffer]]` entries in setup.toml that the runner inserts via `db::save_session()` before spawn. This unlocks the whole session-restore golden family.

6. **Malformed config files** — blocks the `Err(Alert)` branches of both config-file drivers. Easy: `[[config_file]] path = "keys.toml" contents = "{{not toml"` in setup.toml.

7. **Mid-test resize** — blocks dynamic Resize events. Needs PTY ioctl in the runner.

8. **Non-existent gh binary** — blocks `GhPrIn::GhUnavailable`. Just a setup flag `gh_binary_override = "/nonexistent"` bypassing the default fake-gh.

### Variants that look unused (no effective handler)

- **`DocStoreIn::Opening`** — explicitly returns `None` in `buffers_of.rs:164`. Keeping it documents the driver-side state machine but the model has nothing to do with it. Not worth a golden.
- **`DocStoreIn::ExternalRemove`** — returns `None` in `buffers_of.rs:165`. Potentially a bug: led leaves the buffer open with stale content and no visible indicator after external deletion. Worth flagging, not a golden target.
- **`TerminalInput::FocusGained` / `FocusLost`** — no consumer anywhere. Either dead code or pending feature (auto-save on focus loss is common in other editors).
- **`SyncResultKind::NoChange`** — dropped with `None` in `sync_of.rs:26`. Expected; a race/null-op outcome, not a user-visible event.

### Fake-lsp extensions that would unlock goldens

- **`inlay_hints` config field** (HashMap<path, Vec<Value>>) — unlocks `LspIn::InlayHints`.
- **`progress_hold_ms` or `progress_steps`** — unlocks meaningful `LspIn::Progress` + `spinner` timer ticks. Currently progress is begin→end in one message.
- **`simulate_error`** or an error-on-method map — unlocks `LspIn::Error`.
- **Delayed response flag** — would help test request cancellation / superseded requests, not strictly for any one variant.

### Fake-gh extensions that would unlock goldens

- **Conditional 304 support** — check the `If-None-Match` header passed via `--include` args; when it matches a configured etag, respond with `HTTP/1.1 304 Not Modified\r\n...\r\n\r\n` and no body. Unlocks `GhPrIn::PrUnchanged`.
- **`api` subcommand for non-graphql endpoints** — current fake-gh only handles `graphql`. The driver's `PollPr` and `fetch_etag` both call `gh api repos/.../pulls/N --include`, which today exits 1 and produces `GhPrIn::PrUnchanged` (not the intended path). Add support for arbitrary `api <endpoint>` that returns a configured JSON response with optional etag headers.

### Event families well covered today

- docstore Opened/Saved — covered by smoke scenarios `open_empty_file` and `type_and_save`.
- LSP Diagnostics — covered by `lsp_diagnostic`.
- Movement — covered by `move_cursor_down_right`.
- Workspace init path, git initial scan, syntax highlighting — all natural side effects of any `git_init = true` scenario with a file.
