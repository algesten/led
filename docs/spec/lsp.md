# LSP

## Summary

led integrates with per-language LSP servers to provide diagnostics, completions, goto-definition, rename, code actions, formatting, and inlay hints. The driver (`crates/lsp/`) owns per-language server processes, translates JSON-RPC into domain types, and normalises two non-trivial design choices: a **pull-only diagnostic policy** (the model never accepts `publishDiagnostics` pushes directly — every diagnostic cycle is initiated by led and governed by a **freeze mechanism** that holds edits back until the server has reconciled), and a content-hashed rebase path on the buffer side so diagnostics that arrive after the user has kept typing are re-anchored to the right rows instead of being dropped or shown at stale positions. Servers are started on demand the first time a matching buffer opens; the rest of the chrome (status bar spinner, progress strings, completion popup, code-action picker, rename overlay, inlay hints, alerts) is derived from `LspState` on `AppState`.

## Behavior

### Server lifecycle per language

The driver keeps a `LspRegistry` of hardcoded server commands: `rust-analyzer`, `typescript-language-server`, `pyright-langserver`, `clangd`, `sourcekit-lsp`, `taplo lsp stdio`, `vscode-json-language-server`, `bash-language-server`. See `crates/lsp/src/registry.rs`. The mapping is language → command; there is no `[lsp.<lang>]` config table — the only global override is the hidden test flag `--test-lsp-server` (`docs/extract/config-keys.md` CLI section) which replaces the binary for all languages and is used with `fake-lsp` in tests.

`LspOut::Init { root }` is emitted once, when the workspace becomes `Loaded` (derived.rs:613). In **standalone mode** (`--no-workspace`) the workspace never leaves `Loading`/`Standalone`, so no server is ever started — LSP is intentionally off for `EDITOR=led` one-shot editing. When the first buffer of a given language materialises, the manager spawns that language's server asynchronously; opens for that language queue in `pending_opens` until `ServerStarted` arrives, then flush as a batch of `textDocument/didOpen`. On `Phase::Exiting` derived emits `LspOut::Shutdown` and all servers are terminated.

If the binary is not installed, the manager records a `ServerError { not_found: true }` and logs at info (no alert — the absence of, say, `clangd` in a Rust project is expected). Other spawn or transport errors produce `LspIn::Error { message }`, which `lsp_of.rs:171` converts into `Mut::Warn { key: "lsp", message: "LSP: ..." }` — a user-visible alert keyed so subsequent LSP errors replace rather than stack.

### Pull-diagnostics-only policy and the freeze mechanism

This is the single non-obvious design choice in the LSP layer. The model **never** consumes a `publishDiagnostics` push directly. The manager runs a `DiagnosticSource` state machine (`crates/lsp/src/manager.rs:42-367`) that decides on server startup whether it is in **Pull** mode (server advertised `diagnosticProvider`) or **Push** mode (it did not). In either case, diagnostics reach the model only through **propagation windows** opened by `LspOut::RequestDiagnostics`.

A propagation window is opened on two signals from derived (`derived.rs:706-729`):

1. **Content hash changed** while `Phase::Running` — any buffer's persisted content hash transitioning to a new value. In practice this fires on save and on external file changes.
2. **Transition to Running** — one-shot at the end of startup resume.

When the window opens:

- **Pull mode**: the manager snapshots `content_hash` for every open doc, sets `frozen = true` with a 5-second deadline, and issues `textDocument/diagnostic` for every open path. While frozen, `cmd_rx` is **not read** — the entire command channel (BufferChanged, new requests, everything) is paused (`manager.rs:533-543`). This is the "freeze": the manager refuses to accept new edits until it has either collected all pull responses or hit the timeout. Once all pulls arrive, `frozen` clears and normal command processing resumes. If the timeout fires first, the freeze is cancelled with a warning and the in-flight pulls are dropped.
- **Push mode**: the window opens non-frozen; cached push results (from notifications the server sent earlier) are forwarded tagged with the hash snapshot. If the server also has pull capability, the manager issues targeted pulls for paths that have no cached push result. A push arriving mid-window is forwarded immediately with the snapshot hash.

Each `LspIn::Diagnostics { path, diagnostics, content_hash }` carries the **snapshot hash at window-open time**, not the current doc hash. This is the ground-truth the buffer-side acceptance policy needs. The buffer calls `offer_diagnostics(diags, hash)` (`crates/state/src/lib.rs:1205`) which decides:

- Hash equals current → **fast path**, accept as-is.
- Hash equals a historical save point whose edits are known → **replay path**, transform each diagnostic row through recorded edits (structural shifts + content-edit drops) and accept.
- Hash unknown → **reject** — the diagnostic is from a buffer state we can neither match nor reconstruct.

This is the "diagnostic arriving after buffer has moved" path — the canonical regression scenario for the rewrite, captured by `goldens/scenarios/edge/lsp_rebase_after_insert/`: a diagnostic on line 2 must re-anchor to line 3 after the user inserts a new line at the top, *not* snap to a stale visual location.

If a `publishDiagnostics` arrives while in Pull mode, the manager **switches to Push mode permanently** (`manager.rs:233-245`), because the server is telling us push is authoritative. The current window is discarded and re-opened in Push mode. Once switched, the manager never switches back — push-with-pull-fallback is the stable state.

### Diagnostics display and freeze-aware acceptance

Accepted diagnostics are stored on the per-buffer `status.diagnostics` (`crates/state/src/lib.rs:127`). Render consumes them two ways:

- **Inline**: squiggle/highlight at the diagnostic range; tooltip with `message` (and `source`/`code` where available).
- **Gutter**: a per-row severity marker.
- **Status line**: the current error count (via annotations).

When the buffer is edited after a window opens, the manager compares the current doc hash against the snapshot hash; if they diverge, the window is closed and the push cache for that path is invalidated (`manager.rs:588-592`). This means a single edit burst doesn't spam stale diagnostics — the manager waits for the next quiet save to re-pull.

The edit-shift logic in `BufferStatus::shift_annotations` (`crates/state/src/lib.rs:1453`) keeps already-accepted diagnostics aligned in real time as the user types: a newline above a diagnostic shifts it down by one, a delete on the diagnostic row clears it. This is separate from the replay path — it is how already-displayed diagnostics move with the text before the next pull replaces them.

### Completions

Typing a trigger character causes the driver to issue `completionProvider` for that position. Trigger characters are discovered from server capabilities on `ServerStarted` and delivered to the model as `LspIn::TriggerChars { extensions, triggers }` (e.g. `[".", ":", ">"]` for Rust). `apply_trigger_chars` (`led/src/model/mod.rs:1368`) stores them on every buffer whose extension matches. There are two ways a completion request fires:

1. **Trigger char** — the manager checks `edit_ops` on `LspOut::BufferChanged` and if the last inserted char is in `trigger_characters`, it starts a fresh completion session (`manager.rs:598-612`).
2. **Identifier char** — in the model, `editing_of.rs:69-75` watches `InsertChar(ch)` where `ch.is_alphanumeric() || ch == '_'`, and emits `Mut::LspRequestPending(Complete)` if there is no active popup and the buffer has trigger characters at all (proxy for "this language has LSP completion"). Derived picks up the pending request and issues `LspOut::Complete`.

The driver fuzzy-filters results against the prefix using `nucleo_matcher` (`manager.rs:2057`), sorted by match score then `sort_text`/`label`. `filter_text` overrides the label for matching purposes. Empty prefix returns all items unfiltered. On subsequent `BufferChanged` in the same path, the driver **re-filters in place** without re-requesting from the server (`manager.rs:1735`): it finds the identifier at `prefix_start_col` in the current line, re-runs the fuzzy match, and pushes a new `LspIn::Completion`. If the user edits outside the identifier range, the completion is **dismissed** as stale. An empty filtered set also dismisses.

The popup absorbs up to 10 items visible at a time (scroll offset maintained in `CompletionState`). `MoveUp`/`MoveDown` re-scroll. `Abort` dismisses. **`InsertNewline` and `InsertTab` both accept** the selected item (`action/lsp.rs:30`): the selected item's `text_edit` (or fallback `insert_text`) replaces the range from `prefix_start_col` to the current cursor, the cursor is placed at the end of the insertion, and if `additional_edits` is non-empty (auto-imports) those are applied too. After accept, `LspRequest::CompleteAccept { index }` is sent for servers that resolve additional edits lazily.

Trigger-char behaviour does *not* depend on whether LSP is "ready" in any user-visible sense — as soon as `TriggerChars` arrives, typing `.` will request completion.

### Goto-definition

`Alt-Enter` in main mode (`lsp_goto_definition`) emits `Action::LspGotoDefinition` → `Mut::LspRequestPending(GotoDefinition)`. Derived samples the current cursor position and issues `LspOut::GotoDefinition { path, row, col }`. The response arrives as `LspIn::Navigate { path, row, col }`. `lsp_of.rs:13-71` branches this single event into five child streams per Principle 2 of the FRP architecture:

1. `Mut::JumpRecord` — remember where we came from (for jump-back).
2. `Mut::BufferUpdate` with cursor + centered scroll — if the target is already in a loaded buffer.
3. `Mut::RequestOpen(path)` — if the target buffer does not exist.
4. `Mut::SetTabPendingCursor` — if the buffer will open later, remember where to put the cursor.
5. `Mut::ActivateBuffer(path)` — always, so the target becomes the active tab.

This means goto-def works uniformly across "cursor jumps in same file", "switch to already-open file", and "open new file and jump". Jump-back (`M-,` → `JumpBack`) undoes the jump by popping the jump list.

### Rename

`Ctrl-r` (`lsp_rename`) opens an overlay seeded with the word under the cursor (`Mut::SetLspRename(RenameState) + Mut::SetFocus(Overlay)`). The overlay is rendered by ui-chrome and absorbs *every* action until dismissed (`action/lsp.rs:169`, and `action/mod.rs` routes everything through `handle_rename_action` while `state.lsp.rename.is_some() && focus == Overlay`). `InsertChar` and `DeleteBackward` edit the input; `InsertNewline` submits; `Abort` dismisses.

On submit: `LspRequest::Rename { new_name }` → `LspOut::Rename` → manager calls `textDocument/rename` → returns a `WorkspaceEdit` flattened to `Vec<FileEdit>` → `LspIn::Edits` → `Mut::LspEdits` → reducer applies text edits into each affected buffer (opening buffers as needed). Since fake-lsp supports rename natively (returns whole-word edits for the cursor identifier) this round-trips without extra config; see `goldens/scenarios/features/lsp/rename_round_trip/`.

### Code actions

`Alt-i` (`lsp_code_action`) emits `Mut::LspRequestPending(CodeAction)`. Derived samples cursor + mark (so the range covers a selection when one exists) and issues `LspOut::CodeAction`. The response arrives as `LspIn::CodeActions { actions: Vec<String> }` — only the titles survive the driver boundary; the actions themselves stay in `pending_code_actions` inside the manager, indexed by position. The model opens a picker overlay (`Mut::LspCodeActions`, reducer sets `focus = PanelSlot::Overlay`).

`MoveUp`/`MoveDown` move selection; `InsertNewline` accepts by emitting `LspRequest::CodeActionSelect { index }`; `Abort` dismisses. Selection causes the manager to resolve the action (including any lazy resolve round-trip for edit-less commands) and returns its edits as `LspIn::Edits`. Commands with server-side only effects (not edits to buffers) are executed by the manager; led does not currently surface their results to the user as anything other than "nothing happened".

### Formatting

Two paths:

- **`Action::LspFormat`** (no default binding; dispatched internally): `Mut::LspRequestPending(Format)` → `LspOut::Format` → edits return via `LspIn::Edits` → `Mut::LspEdits` → reducer applies them. No save.
- **`Action::Save` with active LSP** (`save_of.rs:19-41`): begin-save + touch the buffer, emit `Mut::SetPendingSaveAfterFormat`, then `Mut::LspRequestPending(Format)`, then show an alert `"Formatting..."`. The reducer sets `lsp.pending_save_after_format = true`. When the format edits come back:
  - `lsp_of.rs:88-107` splits the `Edits` parent into two child streams. The format-done children fire **only when** all returned file-edits are empty **and** `pending_save_after_format` is set — i.e. "format arrived and produced no new edits, so we're at the formatted state".
  - Child 1: apply `buf.apply_save_cleanup()` + `record_diag_save_point()` (so diagnostics offered after this save are accepted as the fast path).
  - Child 2: emit `Mut::LspFormatDone` → reducer clears the flag and calls `s.save_request.set(())`, triggering `WorkspaceOut::FileSave` via derived.

The expected sequence for a formatting save is: format edits arrive → reducer applies them → the manager sees the resulting BufferChanged, re-requests formatting internally (or the user does nothing) → the next `Edits` is empty → save cleanup runs → save fires. If the server returns non-empty edits, they are applied as buffer edits and the save does not fire yet — the user sees the buffer change and the "Formatting..." alert stays until the second (empty) reply.

**`Action::SaveNoFormat`** (`Ctrl-d`) skips the format entirely and writes the buffer as-is.

Prettier is a special case in the manager: for JS/TS/JSON paths it walks up from the file looking for `node_modules/.bin/prettier` (`manager.rs:2101`) and, if found, uses prettier as the formatter instead of the language server's `textDocument/formatting`. The user sees this as an identical format-on-save round-trip.

### Inlay hints

`Ctrl-t` (`lsp_toggle_inlay_hints`) toggles `lsp.inlay_hints_enabled`. While enabled, derived watches the active buffer and issues `LspOut::InlayHints { path, start_row, end_row }` for the viewport plus a 10-row buffer, deduped by `(path, scroll_row / 5, version)` — so small scroll motions don't re-pull, but a page-shift does. Responses land as `LspIn::InlayHints` → `Mut::LspInlayHints` → `buf.set_inlay_hints(hints)`. Render draws each hint as a virtual annotation between text columns.

### Progress indicator

The status bar shows LSP busy state as a spinner plus optional detail string. The driver forwards `LspIn::Progress { server_name, busy, detail }` (rate-limited internally). `Mut::LspProgress` writes `lsp.server_name`, `lsp.busy`, and `lsp.progress = Some { title: detail, .. }`. Derived starts a repeated 80ms `TimersOut::Set { name: "spinner" }` timer when `lsp.busy` becomes true and cancels when false (`derived.rs:328-340`); each tick bumps `lsp.spinner_tick`, and the status renderer animates the glyph off that counter. In practice with fake-lsp the begin→end transition happens in the same message so the spinner barely ticks — real servers (rust-analyzer during `cargo check`) produce long-lived busy spans.

### Issue navigation (shared with git / PR)

`Alt-.` (`next_issue`) / `Alt-,` (`prev_issue`) navigate a ranked cycle of "issues" — LSP errors, then LSP warnings, then git hunks, then PR line comments, then PR diff lines (`nav_of.rs:135`, uses `IssueCategory::NAV_LEVELS`). The first non-empty level wins. Same-buffer jumps emit `Mut::BufferUpdate` + `Mut::Alert("Jumped to <category> X/N")`; cross-buffer jumps additionally emit `Mut::SetActiveTab`; targets in not-yet-open files emit `Mut::RequestOpen` + `Mut::SetTabPendingCursor`. The cycle wraps. LSP is the primary consumer of this machinery but it is not LSP-specific.

## User flow

The user opens `src/main.rs` with unresolved `let unused = 42;`. led materialises the buffer, detects the `rs` extension, and the LSP driver spawns `rust-analyzer`. A `didOpen` is sent; the server starts analysing. `LspIn::Progress { busy: true, detail: Some("rust-analyzer") }` arrives, the status bar spins. When analysis quiesces, `LspIn::Diagnostics` arrives tagged with the content hash from window-open; the buffer accepts it (fast path — the doc hasn't changed) and the gutter + inline squiggle show up. The user presses `Alt-.` — cursor jumps to the `unused` span and the status bar reads `"Jumped to LSP error 1/1"`.

The user types `lit ` (`l-i-t`). After the first keystroke `editing_of` notices the buffer has trigger characters registered and emits `LspRequestPending(Complete)`; derived fires `LspOut::Complete`. The manager returns fuzzy-matched items; a popup appears. As the user keeps typing `it`, the manager re-filters in place (no new server request) and the list narrows. The user presses `Enter` (or `Tab`) — the `text_edit` replaces the prefix with `let`, the cursor moves to after it, and the popup closes.

The user wants to rename `unused` to `discarded`. `Ctrl-r` opens the rename overlay, pre-seeded. Typing replaces the input; `Enter` submits. The manager returns a `WorkspaceEdit`; `lsp_of` applies the edits; the overlay closes.

The user saves with `Ctrl-s`. Because LSP is active, led emits an alert "Formatting...", then `LspOut::Format`. rust-analyzer returns formatting edits; they apply; the next format round returns empty; `LspFormatDone` fires and the save request goes through. The new content hash triggers a fresh diagnostic window: the manager freezes, pulls diagnostics for `main.rs` (and every other open Rust file), and once all pulls return, unfreezes. The diagnostic for `unused` is gone because it was renamed. The buffer's `status.diagnostics` is replaced with an empty list, the gutter clears.

The user presses `Alt-Enter` on a call site — goto-definition. The driver returns a `Location`; `lsp_of` records a jump, re-centers the cursor at the target row, and activates the target tab (opening it if needed). `Alt-,` (JumpBack) would restore the previous cursor.

## State touched

- `AppState.lsp: LspState` — popup + overlay data, server name, busy, pending request, pending save-after-format flag, inlay hints toggle, spinner tick.
  - `lsp.completion: Option<CompletionState>` — items, prefix start, selected, scroll offset. Written by `Mut::LspCompletion`, `Mut::LspCompletionAction`, read by render + InsertChar routing.
  - `lsp.code_actions: Option<CodeActionPickerState>` — titles + selection. Written by `Mut::LspCodeActions`.
  - `lsp.rename: Option<RenameState>` — input + cursor. Written by `Mut::SetLspRename`, `Mut::LspRenameAction`.
  - `lsp.pending_request: Versioned<Option<LspRequest>>` — version-bumped on every write so derived fires exactly once per request even if the same variant is re-set.
  - `lsp.inlay_hints_enabled: bool` — persisted across sessions? `[unclear — not obviously in the session DB schema; check persistence.md]`.
  - `lsp.server_name`, `lsp.busy`, `lsp.progress`, `lsp.spinner_tick` — status bar.
  - `lsp.pending_save_after_format: bool` — gate for "next empty format reply → trigger save".
- `BufferState.status.diagnostics: Vec<led_lsp::Diagnostic>` — read by render, written only via `buf.offer_diagnostics(diags, content_hash)` (fast / replay / reject).
- `BufferState.status.inlay_hints` — read by render, written by `Mut::LspInlayHints`.
- `BufferState.completion_triggers: Vec<String>` — per-buffer copy of trigger chars, set on `Mut::LspTriggerChars`, read by `editing_of` to decide whether to request completion on identifier chars.
- `AppState.focus: PanelSlot` — set to `Overlay` when a code-action picker or rename overlay opens; restored to `Main` on accept/abort.
- `AppState.save_request: Versioned<()>` — set by `Mut::LspFormatDone` to trigger the save.
- `AppState.jump_list` — written by `Mut::JumpRecord` from goto-definition.
- `AppState.tabs` — a goto-def or issue navigation to a closed file appends via `Mut::RequestOpen`.

## Extract index

- **Actions**: `LspGotoDefinition`, `LspRename`, `LspCodeAction`, `LspFormat`, `LspToggleInlayHints`, `NextIssue`, `PrevIssue`, `Save` (LSP path), `SaveNoFormat`. Popup/overlay actions are handled by polymorphic dispatch on `InsertChar`, `InsertNewline`, `InsertTab`, `DeleteBackward`, `MoveUp`/`MoveDown`, `Abort`. See `docs/extract/actions.md` § LSP and the polymorphic entries.
- **Keybindings**: main mode — `Alt-Enter` (lsp_goto_definition), `Ctrl-r` (lsp_rename), `Alt-i` (lsp_code_action), `Ctrl-t` (lsp_toggle_inlay_hints), `Alt-.` (next_issue), `Alt-,` (prev_issue), `Ctrl-s` (save, LSP-aware), `Ctrl-d` (save_no_format). Contexts: `lsp_rename`, `lsp_code_actions`, `lsp_completion` — see `docs/extract/keybindings.md` § Context: LSP ...
- **Driver events**: all `LspIn::*` — `Diagnostics`, `Completion`, `CodeActions`, `Edits`, `Navigate`, `InlayHints`, `TriggerChars`, `Progress`, `Error`. See `docs/extract/driver-events.md` § lsp.
- **Driver commands**: all `LspOut::*` — `Init`, `Shutdown`, `BufferOpened`, `BufferChanged`, `BufferClosed`, `RequestDiagnostics`, `GotoDefinition`, `Complete`, `CompleteAccept`, `Rename`, `CodeAction`, `CodeActionSelect`, `Format`, `InlayHints`. Issued by `derived.rs:609-819`.
- **Timers**: `"spinner"` (80ms repeated while LSP busy) — see `docs/extract/driver-events.md` § timers. The internal freeze timeout (5s) is a driver-internal `tokio::time::Instant`, not a model timer.
- **Config keys**: no user-facing LSP config — server commands are hardcoded in `crates/lsp/src/registry.rs`; only the hidden test flag `--test-lsp-server` exists. See `docs/extract/config-keys.md` § CLI.
- **Theme keys**: `inlay_hint` style, plus diagnostic severity styles in the theme (error/warn/info/hint). See `docs/extract/config-keys.md`.

## Edge cases

- **Server binary not installed**: `ServerError { not_found: true }`. No alert, only info log. The language is simply LSP-dormant for this session; other languages continue to work.
- **Server crashes mid-session**: the transport task exits, `LspIn::Error { message }` is emitted, and `Mut::Warn { key: "lsp", ... }` shows an alert. `[unclear — no golden for explicit restart-on-crash; behavior for "does led attempt to respawn?" is not obviously coded. The manager holds a single `Arc<LanguageServer>` per language in `self.servers` and there is no visible restart path. Treat as: crash = LSP dormant until led restarts.]`
- **Diagnostic at line 0**: works — the gutter and inline renderer handle `Row(0)`. Covered by `edge/lsp_diagnostic_line_zero`.
- **Diagnostic on empty buffer**: the diagnostic is still accepted (the content hash matches) and `shift_annotations` handles zero-length ranges. Covered by `edge/lsp_diagnostic_empty_buffer`.
- **Diagnostic arriving after buffer has moved (rebase)**: the core rewrite-blocking scenario. Diagnostic carries the snapshot hash; the buffer compares it to its current hash; mismatch triggers the replay path that walks forward through recorded edits, dropping diagnostics on content-edited rows and shifting diagnostics below structural edits. Covered by `edge/lsp_rebase_after_insert`.
- **Completion in a non-LSP-supported file**: `completion_triggers` on the buffer is empty → `editing_of` does not fire `LspRequestPending(Complete)` on identifier chars. Typing `.` in plaintext never opens a popup.
- **Trigger char with no results**: the manager returns an empty item list → `Mut::LspCompletion` with `items.is_empty()` → reducer clears `lsp.completion` to `None` (no popup).
- **Completion arrives after cursor has moved out of the identifier**: the manager's `refilter_completion` detects edit-row or edit-col outside `[prefix_start_col, id_end]` and calls `clear_completion`, pushing an empty Completion message that dismisses the popup.
- **Push server sends publishDiagnostics mid-session after pull mode was detected**: pull mode is downgraded permanently to push mode; the active window (if any) is discarded and re-opened in push mode (`DiagPushResult::RestartWindow`).
- **RequestDiagnostics while LSP is still initializing (pre-quiescence)**: deferred via `init_delayed_request` until quiescence. Only servers that implement `experimental/serverStatus` (rust-analyzer) expose quiescence; others are considered ready immediately.
- **Freeze timeout**: if not all pulls return within 5 seconds, `cancel_freeze` fires — `frozen = false`, `pending_pulls.clear()`, a `diag: pull freeze timeout` warning is logged. The missing paths simply don't get updated this cycle; the next content-hash change re-opens the window.
- **Goto-definition target is in a file outside the workspace**: opens the file anyway via `Mut::RequestOpen`; `[unclear — whether such a buffer gets LSP features depends on whether its language server was started for that file's path, which is driven by `BufferOpened.language`.]`.
- **Rename while another LSP request is pending**: `pending_request` is a single slot — submitting rename overwrites whatever was there. In practice this is guarded because the rename overlay is modal.

## Error paths

- **Server fails to start (binary missing)**: `ServerError { not_found: true }` — info log, no alert. LSP dormant for that language.
- **Server fails to start (spawn error other than not-found)**: `LspIn::Error { message }` → alert.
- **Server crashes mid-session**: transport stream closes, final `LspIn::Error` if one is emitted, then silence. `[unclear — no explicit restart; confirm by reading `crates/lsp/src/server.rs` and `transport.rs` supervisor behavior.]`
- **Request timeout**: `[unclear — no explicit per-request timeout is visible in `manager.rs` for GotoDefinition/Complete/etc. The only timeout is the 5s freeze deadline for pull diagnostics. A hung `completion` request would seemingly just leave `lsp.completion` as `None` indefinitely; the user's next keystroke would not re-request (there's no retry logic).]`
- **Malformed response**: `lsp-types` deserialization failure surfaces as an error in the request handler, which produces `RequestResult::Error { message }` → `LspIn::Error` → alert. `[unclear — need to confirm error propagation in `manager.rs` request futures.]`
- **LSP `applyEdit` request from server (workspace edits pushed by server)**: `[unclear — not obviously handled. Typical servers do this for rename-by-code-action or organize-imports-as-command. Search in manager.rs for "applyEdit" returns no hits.]`
- **`window/showMessage` at level error**: forwarded to the model as `LspIn::Error` (the `simulate_error` test hook relies on this); `[unclear — confirm the notification handler path specifically normalises showMessage to Error vs just logging it.]`
- **`pending_save_after_format` set but format never returns**: the save never fires. The user sees a stuck "Formatting..." alert. Recovery: `Esc` does not clear this; the user would have to `Ctrl-d` to save-without-format, or edit the buffer to trigger a BufferChanged which doesn't actually clear the flag. `[unclear — no visible timeout or fallback; possible papercut worth capturing in the rewrite.]`
