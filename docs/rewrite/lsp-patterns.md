# Legacy LSP — usage patterns catalogue

Living document. Each section captures a distinct pattern with exact
line citations into `/Users/martin/dev/led/crates/lsp/` and the
`led/` consumers. Every rewrite-side decision should cite a section
here rather than re-derive.

Status:
- `[READ]` — I've read the code end-to-end.
- `[PARTIAL]` — key sections read; other details still open.
- `[TODO]` — not yet read.

---

## Crate map (legacy)

| File | Lines | Concern |
|---|---|---|
| `lsp/src/lib.rs` | 231 | ABI: `LspOut` (out-of-model) + `LspIn` (into-model) + driver entry. |
| `lsp/src/registry.rs` | 92 | Language → binary + args; `server_override` for test harness. |
| `lsp/src/transport.rs` | 242 | JSON-RPC framing, id correlation, auto-reply, stderr forwarding. |
| `lsp/src/server.rs` | 324 | Per-server subprocess, initialize handshake, shutdown. |
| `lsp/src/manager.rs` | 2414 | Event loop; owns every server; diagnostic windows; progress throttling. |
| `lsp/src/convert.rs` | 485 | lsp-types ↔ domain-type conversion. |
| `state/src/annotations.rs` | 164 | Diagnostic atom. |
| `led/src/derived.rs` | 952 | Emits `LspOut` based on state deltas. |
| `led/src/model/lsp_of.rs` | 213 | Folds `LspIn` into `AppState`. |
| `led/src/model/nav_of.rs` | 486 | Diagnostic navigation + status-bar. |
| `led/src/model/action/lsp.rs` | 219 | User actions (goto, rename, code action …). |
| `ui/src/display.rs` | — | Spinner, gutter, underlines, status-bar priority. |

---

## 1. ABI boundary `[READ]` — `lsp/src/lib.rs`

**`LspOut` (derived → driver), 14 variants:**
`Init{root}`, `Shutdown`, `BufferOpened{path, language, doc}`,
`BufferChanged{path, doc, edit_ops, do_save}`, `BufferClosed{path}`,
`RequestDiagnostics`, `GotoDefinition`, `Complete`, `CompleteAccept`,
`Rename`, `CodeAction`, `CodeActionSelect`, `Format`, `InlayHints`.

**`LspIn` (driver → model), 9 variants:**
`Navigate`, `Edits`, `Completion`, `CodeActions`, `Diagnostics{path,
diagnostics, content_hash}`, `InlayHints`, `TriggerChars{extensions,
triggers}`, `Progress{server_name, busy, detail}`, `Error{message}`.

**Key domain types:**
- `Diagnostic { start_row, start_col, end_row, end_col, severity, message, source, code }` — `code` is used for "match the same diag between push and pull" (lib.rs:57).
- `PersistedContentHash` — stamps every diag delivery; consumer version-gates on it.

**Driver entry** (lib.rs:204) takes `Stream<LspOut>` + `Option<String>` override, returns `Stream<LspIn>`. Bridges via tokio mpsc to the async manager task. `tokio::spawn_local` on the inbound half to respect rx::Stream's thread-locality.

---

## 2. Server spawn + initialize `[READ]` — `lsp/src/server.rs`

### 2.1 Spawn

- `Command::new(config.command).args(config.args).current_dir(root.as_path())` — **CWD is the workspace root**. rust-analyzer needs this to find Cargo.toml (server.rs:35).
- `.kill_on_drop(true)` (server.rs:39) — tokio kills the child when `Child` drops. No leaked subprocesses on panic.
- **`io::ErrorKind::NotFound` → info-log only** (server.rs:42-44). `LspError { not_found: true }` flows up; manager converts to silent `ServerError` event (manager.rs:736-741). **All other errors → log::error + surface as `LspIn::Error` to model.**

### 2.2 stderr forwarding

- Separate task reads stderr line-by-line (server.rs:60-73).
- Every line logged at `warn` level.
- Lines starting with `"error"` become a synthetic `$/stderr` notification (server.rs:66-71). Manager forwards the param string as `LspIn::Error`. So users see rust-analyzer's stderr errors in the status bar even though LSP wire-protocol doesn't carry them.

### 2.3 Server-name normalisation

- Stderr and trace output use the **binary basename** (`fake-lsp`), not the full path (server.rs:79-84). Makes golden traces stable across machines.
- Internal `name` field keeps the full command for log messages.

### 2.4 Initialize params — full capability declaration

Listed here because they're not decorative — several servers (esp. rust-analyzer) gate behaviour on these.

| Capability | Line | Why |
|---|---|---|
| `textDocument.synchronization.did_save: true` | 117 | Enables server to receive `didSave`. |
| `textDocument.publish_diagnostics` | 147 | Pairs with diagnostic ABI. |
| `textDocument.diagnostic` | 158 | Pull-capable servers enter pull mode. |
| `textDocument.completion` with `insert_replace_support` | 162 | Completion ranges. |
| `textDocument.inlay_hint` | 154 | Enables inlay requests. |
| `workspace.configuration: true` | 196 | Server's `workspace/configuration` requests expect a reply. |
| `workspace.did_change_watched_files.dynamic_registration: true` | 186 | rust-analyzer registers file-watch globs at runtime. |
| `workspace.workspace_edit.document_changes: true` | 192 | Enables renames across files. |
| `window.work_done_progress: true` | 181 | **Required** for `$/progress` to flow from the server. |
| `experimental.serverStatusNotification: true` | 199 | rust-analyzer's quiescence extension. |
| `workspace_folders` | 205 | Named folder list in addition to `rootUri`. |
| `client_info { name: "led", version: "0.1.0" }` | 214 | Diagnostic niceness. |

### 2.5 Post-initialize sequence

Strictly ordered (server.rs:222-236):

1. `request("initialize", params)` — await response, store capabilities in `Mutex<Option<ServerCapabilities>>`.
2. `notify("initialized", {})`.
3. **`notify("workspace/didChangeConfiguration", { settings: {} })`** — this is the one that's non-obvious. Comment at line 229: _"rust-analyzer waits for this before indexing"_. The server blocks its cold-index phase until configuration arrives; we send an empty config to release it. Without this, rust-analyzer stays frozen "waiting for client configuration" indefinitely.

### 2.6 Request correlation

- `LanguageServer.next_id: AtomicI32` — monotonically assigns IDs per server (server.rs:245).
- `response_handlers: Arc<Mutex<HashMap<RequestId, oneshot::Sender<…>>>>`. Caller `.await`s the oneshot.
- Each `request<P, R>` call inserts the oneshot, sends the JSON, awaits. Deserialize on the caller's side; wrong-shape responses surface as `LspError`.
- `notify` is fire-and-forget — no id, no handler.

### 2.7 Shutdown — request + sleep + exit + wait + kill

`server.shutdown()` (server.rs:296-323):

1. Send `shutdown` request (fire, don't await the response).
2. `tokio::time::sleep(100ms)` — give the server a moment to flush anything in-flight.
3. Send `exit` notification.
4. Spawn a task: `timeout(5s, child.wait())` then unconditional `child.kill()`.

---

## 3. Transport & auto-reply `[READ]` — `lsp/src/transport.rs`

### 3.1 Frame wire shape

`Content-Length: N\r\n\r\n{…body…}` — standard LSP base protocol. Writer (transport.rs:42) prepends the header; reader (transport.rs:112) parses line by line.

### 3.2 Golden trace emission

Both reader and writer emit `LspSend` / `LspRecv` trace lines when `led_core::golden_trace::is_active()` (transport.rs:50, 165). Format is `server=<name> kind=<req/resp/notif> method=<m> id=<n> path=<uri> version=<v>`. `trace_fields` (transport.rs:73) is the compact single-line formatter.

### 3.3 Message classification (reader — transport.rs:174-239)

- `id: null` ⇒ treat as absent. Some servers include `id: null` on notifications; tolerated.
- `(has_id AND has_method)` — **server-initiated request**. Auto-reply:
  - `workspace/configuration` ⇒ reply with `Array(n * Object(empty))` where `n` is `params.items.len()` (default 1). Servers stall without a reply.
  - Anything else ⇒ reply `result: null`.
  - **Also forward as notification** if method is `client/registerCapability` — manager needs to react. Other server-requests stay handled in-transport.
- `(has_id AND !has_method)` — **response**. Look up id in `response_handlers`, send `Ok(result)` or `Err(LspError)` through the oneshot.
- `(!has_id AND has_method)` — **notification**. Forward to `notification_tx` channel → manager.
- Otherwise → log at warn, drop.

### 3.4 Error response shape

When the server returns `{"error": {"code": N, "message": "..."}}` the transport wraps it as `LspError { message, not_found: false }` and sends through the oneshot. Caller's `serde_json::from_value::<R>` doesn't even run.

---

## 4. Registry & language resolution `[READ]` — `lsp/src/registry.rs`

- 8 languages hardcoded in legacy: Rust, TypeScript, Python, C, Swift, Toml, Json, Bash. TypeScript's `extensions` covers both JS and TS.
- `server_override`: when set, **every** entry's `command` is replaced (with `args` cleared) by `Box::leak`ing the supplied string into `'static`. Used by goldens to point every language at `fake-lsp`.
- `config_for_language(LanguageId)` and `extensions_for_language(LanguageId)` are the only query methods. `ServerConfig` fields: `language`, `command`, `args`, `extensions`.

---

## 5. Manager thread — event loop and coordination `[PARTIAL]`

### 5.1 Struct state (manager.rs:459-495)

36 fields. The important ones for diagnostics:

- `servers: HashMap<LanguageId, Arc<LanguageServer>>` — one server per language.
- `pending_starts: HashSet<LanguageId>` — de-dupes concurrent spawn attempts.
- `opened_docs: HashSet<CanonPath>` — paths we've sent `didOpen` for. `send_did_close` removes.
- `pending_opens: HashSet<CanonPath>` — paths awaiting their server to finish initialize. Flushed in `ServerStarted` handler.
- `docs: HashMap<CanonPath, Arc<dyn Doc>>` — latest doc snapshot per path, updated on each `BufferChanged`.
- `languages: HashMap<CanonPath, Option<LanguageId>>` — language detection done once at `BufferOpened`, cached here.
- `doc_versions: HashMap<CanonPath, i32>` — LSP `textDocument.version` counter, separate from our `DocVersion`. Starts at 0, `next_version()` bumps.
- `progress_tokens: HashMap<String, ProgressState>` — `$/progress` tokens by id.
- `quiescent: HashMap<LanguageId, bool>` — per-language quiescent flag. Default "busy" (true → idle; absent → `is_busy()` treats as busy).
- `diag_source: DiagnosticSource` — the state machine.
- `last_progress_sent: Instant` — throttle timestamp.
- `trigger_characters: Vec<String>` — from completion capabilities.
- `completion_seq: u64` — monotonic to reject stale completion responses.

### 5.2 Main loop (manager.rs:532-557)

```
loop {
    if let Some(deadline) = diag_source.deadline() {
        // FROZEN (pull mode): only read internal events, with timeout.
        tokio::select! {
            event = event_rx.recv() => handle_event(event),
            _ = sleep_until(deadline) => diag_source.cancel_freeze(),
        }
    } else {
        // Normal: read BOTH cmd channel + event channel.
        tokio::select! {
            cmd = cmd_rx.recv() => handle_command(cmd),
            event = event_rx.recv() => handle_event(event),
        }
    }
}
```

**Key insight I missed initially:** the `deadline` branch *does not* read `cmd_rx`. That's how legacy freezes user-driven commands while a pull is in flight. Server notifications still arrive through `event_rx` and get handled. Commands back up in their channel until the freeze lifts (via all pulls responded OR deadline expired).

### 5.3 `handle_command` routing (manager.rs:563-676)

- `Init { root }` — just stores the root.
- `Shutdown` — `shutdown_all().await` (drain + shutdown every server).
- `BufferOpened` — `docs.insert`, `languages.insert`, `ensure_server_for_path`, `send_did_open`.
- `BufferChanged` —
  1. `if diag_source.should_close_window(&path, &doc) { diag_source.close_window(); diag_source.invalidate_cache(&path); }` — the buffer moved past the snapshot, so the open diagnostic window is stale. Cache entry for this path is now wrong content; drop it.
  2. `docs.insert` (returns old doc; kept for incremental sync).
  3. `send_did_change(&path, &edit_ops, old_doc.as_deref())`.
  4. `if do_save { send_did_save(&path); }`.
  5. Completion trigger-char detection → either fresh `spawn_completion` or `refilter_completion`.
- `RequestDiagnostics` — `if diag_source.should_defer_request() { diag_source.init_delayed_request = true; } else { open_diag_window(...).await; }`. **Single entry point for the full cycle.**
- `BufferClosed` — `send_did_close(&path)`, remove from `docs` and `doc_versions`.
- Feature requests (`Complete`, `GotoDefinition`, `Rename`, `CodeAction`, `Format`, `InlayHints`) — each spawns a tokio task that awaits its request and posts a `ManagerEvent::RequestResult` back.

### 5.4 `handle_event` routing (manager.rs:680-753)

- `ServerStarted { language, server }` — inspects capabilities, decides diagnostic mode:
  - `has_pull = capabilities.diagnostic_provider.is_some()` → `DiagMode::Pull` else `Push`.
  - Extracts `completion_provider.trigger_characters` → stored + sent as `LspIn::TriggerChars`.
  - Flushes `pending_opens` — all paths that tried to open before this server was ready now get `send_did_open`.
  - **Quiescence detection deferred to runtime** (manager.rs:706-708): NOT inferred from initialize response. Triggered by first `experimental/serverStatus` notification arriving. Critical — rewrite's parser can leave `has_quiescence` false at initialize time and it's still correct.
- `ServerError { not_found }` — if `not_found`, `log::info` only. Otherwise log error + send `LspIn::Error`.
- `Notification(language, notif)` — delegates to `handle_notification`.
- `RequestResult(...)` — delegates to `handle_request_result`.
- `FileChanged(path, kind)` — forwards as `workspace/didChangeWatchedFiles`.

### 5.5 Notification handling (manager.rs:1333-1474) — KEY PATTERNS

#### 5.5.1 `textDocument/publishDiagnostics`

1. Deserialize `PublishDiagnosticsParams`. Convert URI to `CanonPath` (drop if fails).
2. Build a `line_at(row)` closure that first consults `docs[path]`, then falls back to reading the file. The conversion needs line text for col ↔ byte conversion.
3. `convert_diagnostics(&params.diagnostics, &line_at)` → domain `Vec<Diagnostic>`.
4. `diag_source.on_push(path, diagnostics)` → `DiagPushResult`. Route each:
   - **`Forward(p, diags, h)`** — directly send `LspIn::Diagnostics { path, diagnostics, content_hash: h }`. `h` is the snapshot hash for that path in the current window.
   - **`RestartWindow`** — mode switched pull → push. `open_window` **then drain cache** into `LspIn::Diagnostics` per path. Same flow as if the window opened freshly in push mode.
   - **`ForwardClearing(p)`** — clearing push (empty diags) arrived outside a window. Fetch `content_hash` from `docs[p]` current state. Forward empty list stamped with that hash.
   - **`Ignore`** — drop.

#### 5.5.2 `$/progress`

1. Deserialize `ProgressParams`. Token is `NumberOrString` → stringify.
2. `classify_progress(&params.value)` → `ProgressUpdate { Begin | Report | End }`. Note: `Report { percentage: 100 }` is promoted to `End` (manager.rs:2042-2052).
3. `apply_progress_update(&token, update)` mutates `progress_tokens`:
   - Begin inserts.
   - Report partially updates message/percentage if new values present.
   - End removes.
4. `send_progress_throttled(result_tx)`.

#### 5.5.3 `experimental/serverStatus`

1. Parse `quiescent: Option<bool>` from raw `params` (not deserialised via `lsp_types`).
2. **First serverStatus arrival** (manager.rs:1443-1448): `if !diag_source.has_quiescence { has_quiescence = true; lsp_ready = false; }`. That is, the first notification — regardless of its `quiescent` value — *proves* the server supports quiescence and flips `lsp_ready` to false. Until that moment, the `DiagnosticSource` thinks the server is ready by default.
3. `let was_busy = !*self.quiescent.get(&language).unwrap_or(&true)`. **Default-absent means default-idle**; `unwrap_or(&true)` ⇒ was_busy = false. But the `on_push` side sets default-idle in `is_busy` too, so this is internally consistent.

   Wait — re-reading: `unwrap_or(&true)` gives `q = true` (idle), so `was_busy = !true = false`. So the first ever serverStatus (no entry yet) reads was_busy=false. That makes `was_busy && q && ...` false, so no `open_diag_window` fires on the first notification. It's only the busy → idle transition that opens the window.

4. `self.quiescent.insert(language, q)` — stash the new value.
5. `if was_busy && q && self.diag_source.on_quiescence()` → `open_diag_window(result_tx).await`. **Only when** all three hold:
   - was busy (previous value was `false` = not quiescent = busy),
   - now quiescent,
   - `on_quiescence()` returned true (we had an `init_delayed_request` waiting).
6. `send_progress_throttled` unconditionally (every serverStatus is a UI tick).

#### 5.5.4 `client/registerCapability`

Handed off to `handle_register_capability` (manager.rs:1845). This is where rust-analyzer's dynamic file-watch globs arrive, and trigger-char updates.

#### 5.5.5 `$/stderr`

Synthetic notification from `server.rs` stderr reader. Forwarded as `LspIn::Error` unchanged.

#### 5.5.6 Unknown notifications

Logged at `debug` level, dropped.

### 5.6 Progress throttle (manager.rs:1672-1709)

- `is_busy() = any quiescent=false OR any progress_tokens`. Two independent sources.
- `send_progress_throttled`: min interval 200ms, **EXCEPT** busy → idle transitions are sent unconditionally. Ensures the "done" state is never lost.
- `progress_lsp_in`:
  - `server_name` = first server's name (`self.servers.values().next()`).
  - `busy = is_busy()`.
  - `detail` = first progress token's `"{title} {message}"` or just `"{title}"`. **No serverStatus message is propagated as detail** — only `$/progress` feeds the detail slot. When the only source of busy is `quiescent=false` with no progress tokens, `detail = None`. This is why rust-analyzer shows just the server name (no detail) during parts of its cold-index when only serverStatus is active.

### 5.7 Pull diagnostics path (manager.rs:1955-2023)

`open_diag_window` — called from `RequestDiagnostics` command and from the `was_busy && quiescent && on_quiescence` trigger:

1. `pull_paths = diag_source.open_window(&self.docs, &self.opened_docs)` — returns which paths need pulling.
2. **Push mode short-circuit** — if `mode == Push`, drain the push cache into `LspIn::Diagnostics` events. Then still proceed to issue pulls for any path without cache (if the server also advertised pull capability).
3. For each `pull_path`: `spawn_pull_diagnostics(path, server)` — spawns a task that sends `textDocument/diagnostic` and posts `ManagerEvent::RequestResult(RequestResult::Diagnostics { path, raw })` on return. Pulls flow in parallel.
4. Response handler (manager.rs:1558-1578): convert raw via `convert_diagnostics` with a `line_at` closure; `diag_source.on_pull_response(path, diags)` → optional forward; emit `LspIn::Diagnostics`. Unchanged reports (non-`Full`) yield empty list.

### 5.8 `DiagnosticSource` state machine (manager.rs:42-367)

(Already ported to rewrite. Reread to validate against the rewrite impl. Patterns captured separately in `crates/driver-lsp/core/src/diag_source.rs` comments.)

Callouts I didn't internalise the first time:
- **`DiagMode::Push` is the default.** Pull is opt-in via `diagnosticProvider` capability.
- **Pull → Push is one-way.** A `publishDiagnostics` arriving while in pull mode flips to push permanently.
- **Push → Pull is never triggered.** A pull-capable server that only pushes stays in push mode forever after its first push.
- **`push_cache` survives `close_window`.** Only `invalidate_cache(&path)` or an explicit clear drops it.
- **Pull mode: window open = freeze.** `cmd_rx` is not read. Hard 5-second deadline.
- **Push mode: window open = no freeze, but tracks snapshot hashes.** Used to decide `should_close_window` on `BufferChanged`.
- **`should_defer_request`** checks `!lsp_ready`. `lsp_ready` flips to false only when first `serverStatus` arrives; flips back to true on the first `quiescent=true`.
- **`on_quiescence` returns true ONCE per init-delay**, not every quiescence event. The `init_delayed_request` flag is consumed.

---

## 6. Consumer side — model + UI patterns

### 6.1 Convert layer `[READ]` — `lsp/src/convert.rs`

#### 6.1.1 URI ↔ path

- `uri_from_path(&CanonPath) -> Option<Uri>`: `format!("file://{}", path_str).parse()`. Nothing fancy, no percent-escaping — LSP clients tolerate raw ASCII paths fine, and we're macOS/Linux-only.
- `path_from_uri(&Uri) -> Option<CanonPath>`: strip `file://`, re-canonicalize. Re-canonicalizing is the critical step — servers echo back slightly different path strings (symlink resolution, double slashes) and the consumer must key on the same canonical form we sent.

#### 6.1.2 UTF-16 ↔ char (convert.rs:24-63)

Four functions total, all tiny:

```
utf16_col_to_char_col(line, utf16_col) — walk chars, accumulate len_utf16()
char_col_to_utf16_col(line, char_col) — walk chars, accumulate len_utf16()
lsp_pos(row, col, Option<&str>) → Position  (encode)
from_lsp_pos(&Position, Option<&str>) → (Row, Col)  (decode)
```

**The `Option<&str>` line is load-bearing.** When `None`, col is passed through unchanged (i.e. treat the col as if it were ASCII). That's fine for ASCII lines but corrupts anything with surrogate pairs (emoji, CJK, etc). Every call site provides the line text — so the conversion runs the walk — except during error paths.

Callers always produce the line via either `doc_line(&dyn Doc, Row)` (live buffer) or `disk_line(&CanonPath, Row)` (not-yet-opened file). This is why `convert_diagnostics` threads a `line_at: impl Fn(usize) -> Option<String>` closure: push diagnostics may arrive for files that aren't in `docs`; `manager.rs` builds the closure to consult `docs[path]` first then fall through to `std::fs::read_to_string` + `.lines().nth(row)`.

#### 6.1.3 TextEdit conversion (convert.rs:93-114)

`lsp_text_edit_to_domain` clones the `new_text` and converts both endpoints. Tuned so that when end_line == start_line, the line is fetched once and cloned — no double disk read. Our `TextEdit` struct is `{start_row, start_col, end_row, end_col, new_text}` — no `version` field; the caller version-gates externally.

#### 6.1.4 WorkspaceEdit expansion (convert.rs:118-185)

Handles both shapes the LSP protocol lets servers return:
- `edit.changes: HashMap<Uri, Vec<TextEdit>>` — the simple legacy shape.
- `edit.document_changes` — newer shape that may include create/rename/delete ops. We only extract `Edit` operations (rename/create/delete ignored — **no rewrite concern yet since M16 doesn't rename across files**).

Result is keyed by `CanonPath` so the caller can dispatch per-file. The `collect_edits` helper reads each file from disk once to build `line_at`, rather than opening each target file. Sensible for small refactors where the changed files may not be open tabs.

#### 6.1.5 On-disk edit application (convert.rs:189-242)

`apply_edits_to_disk(path, edits)`: read file → `lines.split('\n')` → sort edits descending (so earlier edits don't shift later ones) → splice each → preserve trailing newline iff original had one → `fs::write`.

Intended for the "apply workspace edit to files that aren't open" path (cross-file rename). Skip for M16.

#### 6.1.6 GotoDefinition response (convert.rs:247-270)

Three shapes — `Scalar(Location)`, `Array(Vec<Location>)`, `Link(Vec<LocationLink>)`. All collapse to `Vec<(CanonPath, Row, Col)>`. For each location, `disk_line(path, row)` is consulted for UTF-16 conversion (definitions usually target files not yet open).

#### 6.1.7 Completion (convert.rs:351-464)

Notable details:
- **`prefix_start_col` = "where to start replacing"**. Prefer the `TextEdit` range from the first item that has one; else scan backwards from cursor over alphanumeric + `_`. This is passed as `LspIn::Completion { items, prefix_start_col }` so the model can replace from that column on accept.
- `insert_text` falls back to `label` when neither `text_edit` nor `insert_text` is set.
- `additional_text_edits` — mapped via `lsp_text_edit_to_domain`; carries auto-imports (`use foo::bar;` appended above).
- Sort order: `sort_text` first, falling back to `label`. Matches VSCode behaviour.

#### 6.1.8 Code action titles (convert.rs:468-476)

Just pulls `.title` out of either `CodeActionOrCommand::CodeAction` or `CodeActionOrCommand::Command`. The picker displays these; the selected index is fed back to the server via `CodeActionSelect`.

### 6.2 State atom — `state/src/annotations.rs` `[READ]`

**This is the `feedback_materialization.md` boundary point:** `annotations.rs` is the *single source of truth* for both the file-browser's per-file coloring AND the per-line gutter marking. The three consumers (browser, gutter, cursor popover) go through this module — not direct buffer inspection — to prevent drift.

#### 6.2.1 `file_categories(state, path) -> HashSet<IssueCategory>` (annotations.rs:23-53)

Union of three sources:
- `state.git.file_statuses[path]` — git clean/modified/staged/untracked.
- `state.git.pr.comments[path]` → `PrComment`; `pr.diff_files[path]` → `PrDiff`.
- **`state.buffers.values().find(|b| b.path() == Some(path))`** — first open buffer for this path. For each diagnostic: Error → `LspError`, Warning → `LspWarning`, else skip. Info/Hint do NOT contribute to file-level browser marks.

The iteration over `buffers.values()` to find a path is O(n_buffers) per call. Legacy's buffer count is low so this is fine; in rewrite, consider a reverse index if we ever support many buffers.

#### 6.2.2 `file_categories_map(state)` (annotations.rs:58-98)

Bulk variant. Same three sources but one pass through each. Used by the browser for directory-level aggregation (child categories bubble up).

#### 6.2.3 `buffer_line_annotations(state, buf)` (annotations.rs:111-139)

**Per-line** (not per-file) — what the gutter needs:
- Start from `buf.status().git_line_statuses()` — the per-line git status vec.
- **Add PR diff ranges ONLY when the file hasn't diverged from the PR's committed version** (`pr_file_diverged` checks `content_hash != pr_hash || is_dirty()`). Once drifted, the line numbers are meaningless. **Comments still show** — the comment text is useful even when lines have moved.
- Convert each `PrComment` into a 1-line `LineStatus` range.

**Diagnostics are NOT merged into line_annotations.** The gutter renders diagnostics as a **separate layer** via `buf.status().diagnostics()` — see §6.7.

#### 6.2.4 `comments_at_line(state, path, row)` (annotations.rs:143-154)

Cursor popover source. Returns `&PrComment`s on the cursor's exact row. Not wired to diagnostics — diagnostics get their own popover (§6.7.4).

### 6.3 Derived emitters `[READ]` — `led/src/derived.rs`

Already captured in §5-equivalent earlier. Confirmed by re-reading:

- `lsp_init` — fires when workspace root becomes Some. Standalone phase never emits.
- `lsp_buf_opened` — tracks opened-buf set via `loaded_buf_paths` dedupe; emits on additions. Removes are intentionally NOT emitted as `BufferClosed` because the LSP has its own file watcher handling closures (derived.rs:650-651).
- `lsp_buf_changed` — dedupes on Σversion; emits per buffer whose version advanced. `do_save` is `true` when the change-reason is `LocalSave` or `ExternalFileChange`.
- `lsp_request_diag_hash` — dedupes on `(phase, Σ(content_hash & 0xFFFFFFF_FFFFFF) + saved_version)`. Filters to `Phase::Running`. Fires every time the sum moves.
- `lsp_request_diag_running` — dedupes on `phase`; filters to `Running`. Fires **once** on transition.
- `lsp_inlay_hints` — dedupes on (active path, scroll_row / 5, version). Viewport-driven.
- `lsp_requests` — dedupes on `pending_request.version()`. Covers goto/complete/rename/etc.

Critically: **TWO separate streams** for RequestDiagnostics. Both pipe into `lsp_out`. The dedupe-on-phase stream is the one that fires on initial load. The dedupe-on-hash stream fires on every edit/save thereafter. Collapsing them (as I did in the rewrite) breaks the initial load.

### 6.4 State update — `led/src/model/lsp_of.rs` + `mod.rs` reducer `[READ]`

#### 6.4.1 `lsp_of.rs` — `LspIn → Mut`

Every `LspIn` variant is split into combinator children that each emit one `Mut`. No driver types survive the call; everything is domain.

**`Navigate { path, row, col }`** (lsp_of.rs:12-72) is the most interesting — split into 5 children off a common `sample_combine(state)` parent:
1. `JumpRecord` — save current cursor/scroll to jump list.
2. `BufferUpdate` — if buffer exists, clone it, clamp row, set cursor + scroll, emit.
3. `RequestOpen(path)` — if buffer doesn't exist, request it.
4. `SetTabPendingCursor` — set pending cursor+scroll on the tab so that when the buffer materializes, it jumps to the right place.
5. `ActivateBuffer(path)` — unconditional.

**`Edits { edits }`** (lsp_of.rs:75-107) splits into three:
1. Apply all edits unconditionally (`Mut::LspEdits`).
2. **If all edits are empty AND `pending_save_after_format`** — apply save cleanup + `record_diag_save_point()` on the active buffer.
3. Same filter → `Mut::LspFormatDone` (clears the flag + triggers the actual save).

The format-done branch is the reason the format request is run-then-save: format completes, empty-edits signals "nothing changed / done", save fires.

**`Diagnostics { path, diagnostics, content_hash }`** (lsp_of.rs:131-145) — pass-through to `Mut::LspDiagnostics`. No filtering in derived; acceptance happens in the buffer.

**`Progress`** (lsp_of.rs:155-169) — pass-through to `Mut::LspProgress`.

**`Error`** (lsp_of.rs:171-180) — pass-through to `Mut::Warn { key: "lsp", message: format!("LSP: {}", …) }`. Fixed alert key so repeated errors replace rather than stack.

**`TriggerChars { extensions, triggers }`** (lsp_of.rs:182-194) — applied to all buffers of those extensions.

#### 6.4.2 `mod.rs` reducer — `Mut::LspDiagnostics` branch (mod.rs:1130-1145)

```rust
Mut::LspDiagnostics { path, diagnostics, content_hash } => {
    // CRITICAL: Create unmaterialized buffer if needed.
    if !s.buffers.contains_key(&path) {
        s.buffers_mut().insert(path.clone(),
            Rc::new(BufferState::new_from_canon(path.clone())));
    }
    if let Some(buf) = s.buf_mut(&path) {
        buf.offer_diagnostics(diagnostics, content_hash);
    }
}
```

**Diagnostics for unopened files create placeholder buffers.** rust-analyzer pushes diagnostics for every file in the workspace, not just open ones. Legacy creates an unmaterialized `BufferState` just to park the diagnostic list on. `file_categories` then picks it up for browser coloring.

Same pattern for `Mut::LspInlayHints` (mod.rs:1146-1156).

**Rewrite currently doesn't do this.** Our `DiagnosticsStates` is a separate atom keyed by path, decoupled from `buffers`. Probably the right call — a diagnostic arrival shouldn't hallucinate a buffer. But if we want the browser to colour unopened files by LSP diagnostic severity, we still need to read from `DiagnosticsStates` (not `BuffersState`) when computing file categories.

#### 6.4.3 `buf.offer_diagnostics(diags, content_hash)` (state/src/lib.rs:1205-1254)

Three paths:

1. **Unmaterialized** (l.1210): always accept. No content to stale against.
2. **Fast path** (l.1221): `content_hash == doc.content_hash()`. Common case — also catches "user undid back to save-point". Accept as-is.
3. **Replay path** (l.1231): `undo.find_save_point(content_hash)` returns the undo-log index where this hash was recorded. Walk the entries from that point forward, transforming diagnostic positions:
   - Clear diagnostics on edited rows (row becomes "no longer what the server analysed").
   - Shift rows after structural changes (inserted/deleted newlines).
   - Final output may be shorter than input.
4. **Reject** (l.1246): neither hash matches nor save-point found. Drop. Logged at `debug`.

The save-point machinery is driven by `record_diag_save_point()` calls from:
- `save_of.rs:55, 75` — every save commits a save-point.
- `buffers_of.rs:145, 161` — buffer materialization also commits one.
- `lsp_of.rs:96` — format-done (post-format save cleanup).

**Rewrite intentionally omits this.** `feedback_lsp_no_smear.md` rejected replay as too fiddly. The rewrite uses `BufferVersion` — strict equality gate — and hides diagnostics the moment the version moves. Tradeoff is the fast-path benefit (undo-back-to-save shows correct diags again) is lost. Re-confirm this tradeoff in §8 open questions.

### 6.5 Navigation — `led/src/model/nav_of.rs` `[READ]`

Handles **Alt-./Alt-,** (next-issue / prev-issue). The cycle is NOT "all diagnostics"; it's a **tiered walk** over `IssueCategory::NAV_LEVELS`.

#### 6.5.1 `compute_navigation(state, forward) -> Option<NavOutcome>` (nav_of.rs:134-142)

```rust
for &level in IssueCategory::NAV_LEVELS {
    let cats = IssueCategory::at_level(level);
    if let Some(outcome) = scan_level(state, forward, cats) {
        return Some(outcome);
    }
}
None
```

First level with **any** items wins. The full cycle stays inside that level. So if there's a single LspError in the workspace, Alt-. cycles over errors only — warnings don't appear in the cycle until the error is fixed. That's how users escape error-rich files without ever seeing warnings.

#### 6.5.2 `scan_level(state, forward, cats)` (nav_of.rs:152-181)

1. `collect_positions(state, cats)` — gather all `Pos { path, row, col, category }` from the 3 sources:
   - **Diagnostics** (nav_of.rs:233): every open buffer. Error → `LspError`, Warning → `LspWarning`, else skip. `clamp_row_to_buffer` clamps to `doc.line_count() - 1` so we don't land past EOF (stale diagnostics from version N+1 can refer to lines that shrank).
   - **Git line statuses** (nav_of.rs:255): file-level `file_statuses[path]` filtered by cats; if the buffer is open, drill into `git_line_statuses()`; else file-level fallback (row=0).
   - **PR comments/diff** (nav_of.rs:310).
2. Sort `(path, row, col)`.
3. **Dedupe by `(path, row, col)`** — multiple categories at the same line collapse. Without this, the cursor lands on one but `pick_target_index` treats the others as separate items, breaking the cycle.
4. `pick_target_index(&positions, cursor, forward)` — see below.
5. Emit `NavOutcome { target_path, target_row, target_col, category, position (1-based), total }`.

#### 6.5.3 `pick_target_index` (nav_of.rs:196-217)

Forward: first position `> cursor`, wrap to 0 on miss.
Backward: last position `< cursor`, wrap to `total-1` on miss.

No cursor at all → index 0.

#### 6.5.4 Output Muts (nav_of.rs:37-128)

5 children off the `nav_parent` stream:
1. `Alert { info: Some("Jumped to {category.label} {position}/{total}") }` — always.
2. **Same-buffer** (`active_tab == target_path`): clone, `close_group_on_move`, set cursor, `adjust_scroll` (viewport scroll helper that keeps cursor visible with lead-in); emit `BufferUpdate`.
3. **Other-buffer already-open**: same as (2) but uses simple half-height scroll — no `adjust_scroll`. Note: different scroll strategy than same-buffer. Probably intentional (when landing on a buffer you weren't looking at, center it) but worth flagging.
4. **SetActiveTab** when target is in a different tab.
5. **File-not-yet-open** emits three Muts: `RequestOpen`, `SetActiveTab`, `SetTabPendingCursor`.

**Alert text is the status-bar feedback.** No special status-bar message format for nav — it's just the generic info alert.

### 6.6 Action dispatch — `led/src/model/action/lsp.rs` `[READ]`

Out of M16 scope but documented for orientation.

#### 6.6.1 `handle_completion_action` (action/lsp.rs:7-126)

Dispatches when `state.lsp.completion.is_some()`. Reads each action:
- MoveUp/MoveDown — cursor within list, with scroll window `max_visible = 10`.
- InsertNewline/InsertTab — accept: build a `TextEdit { cursor_row, prefix_start_col → cursor_col, new_text }`, `apply_text_edits(buf, &[te])`, place cursor at end of inserted text (accounting for newlines). Then apply `additional_edits` (auto-imports) separately. Emit `CompleteAccept { index }` to request resolve from the server.
- Abort — clear completion state.
- `InsertChar(_) | DeleteBackward` — **return false** (pass through to normal editing, which will trigger re-filter via the combinator chain).
- Anything else — clear + return false (dismiss, normal action runs).

`shift_annotations(state, path, edit_row, old_lines, old_ver)` is called after each edit; that's how diagnostic positions are kept in sync with typed-through edits (runs the "shift" part of replay without needing a save-point).

#### 6.6.2 `handle_code_action_picker` (action/lsp.rs:128-167)

Overlay picker. MoveUp/MoveDown, InsertNewline accepts and emits `CodeActionSelect { index }`, Abort dismisses.

#### 6.6.3 `handle_rename_action` (action/lsp.rs:169-219)

Single-line text input overlay. Accept emits `Rename { new_name }`. Non-printable keys absorbed while overlay is active.

### 6.7 UI paint — `crates/ui/src/display.rs` `[READ]`

#### 6.7.1 Diagnostic data extraction (display.rs:183-188)

```rust
let diagnostics: Vec<(Row, Col, Row, Col, DiagnosticSeverity)> =
    buf.status().diagnostics().iter().map(|d| (d.start_row, d.start_col, d.end_row, d.end_col, d.severity)).collect();
```

Flat tuple vec handed to the renderer. No filtering by severity at this layer — `Info` and `Hint` reach the painter and the style table decides whether they're visible (often the same style as Warning → effectively all four severities render).

#### 6.7.2 Theme keys (display.rs:197-198)

```
theme.diagnostics.error
theme.diagnostics.warning
```

Two styles, not four. Info/Hint fall back to whatever the theme bleeds through — typically Warning. Rewrite currently has four style fields; simplify to match legacy if we care about theme compatibility.

#### 6.7.3 Inlay hints (display.rs:190-195, 205)

Parallel to diagnostics — `buf.status().inlay_hints()` → `(Row, Col, String)` tuples. Gated by `s.lsp.inlay_hints_enabled` (user toggle).

Theme key: `theme.editor.inlay_hint` with a `DarkGray` fallback.

#### 6.7.4 Diagnostic popover on cursor row (display.rs:995-1018)

```rust
if let Some(buf) = s.active_tab.as_ref().and_then(|path| s.buffers.get(path)) {
    let crow = buf.cursor_row();
    let messages: Vec<_> = buf.status().diagnostics().iter()
        .filter(|d| crow >= d.start_row && crow <= d.end_row)
        .filter(|d| matches!(d.severity, Error | Warning))
        .map(|d| (d.severity, format_diagnostic_message(d)))
        .collect();
    if !messages.is_empty() {
        return OverlayContent::Diagnostic { messages, anchor_x: cursor_x, anchor_y: cursor_y };
    }
}
```

**The popover is the cursor-line-has-diagnostic feedback.** Only Error/Warning get a popover; Info/Hint are silent. Anchored at cursor position. This is the "what does this squiggle mean" UX.

Rewrite currently has no popover. To match legacy:
- Add an overlay painter that reads `DiagnosticsStates[active_path]` and filters by cursor row.
- Query the painter composes over the body (above the body_model output, below any modal overlays).

#### 6.7.5 `format_lsp_status` (display.rs around 803-831)

Already ported to rewrite's `query::format_lsp_status` — confirmed verbatim down to spinner frames and busy/idle ordering.

#### 6.7.6 Sidebar file-level marker

Browser/file-finder gutter. Driven by `file_categories_map(state)` (§6.2.2). Colours the filename; directory aggregation bubbles child categories up. Rewrite's file-finder doesn't currently consume LSP diagnostics — would need to extend its category source from just `git.file_statuses` to also read `DiagnosticsStates`.

---

## 7. Rewrite compliance audit

### 7.1 Driver-side deviations (in M16 scope)

1. **`workspace/didChangeConfiguration` empty-settings is not sent.** Rewrite's manager.rs stops at `initialized`; never sends `didChangeConfiguration`. rust-analyzer may stall indefinitely in "waiting for client configuration". [§2.5]
2. **Quiescence detection is tied to `initialize.capabilities.experimental.serverStatusNotification`.** Legacy discovers it at runtime from the first `serverStatus` notification. Rewrite fix: both paths — advertise via initialize AND flip `has_quiescence` on first-notification arrival. [§5.5.3]
3. **`RequestDiagnostics` fires only on state-sum delta.** Legacy has TWO signals: content-hash-sum delta AND phase → Running transition. Rewrite has the `fresh_open` guard but it isn't an exact port. Add the second signal. [§6.3]
4. **Progress + serverStatus unified via `is_busy()` + throttle.** Legacy funnels both notification types through a single `send_progress_throttled(result_tx)` call that computes `busy = is_busy()` (any quiescent=false OR any progress_tokens) and picks detail from the first `$/progress` token only. Rewrite currently emits two separate LspEvent variants. [§5.5.3 + §5.6]
5. **Progress throttle.** Legacy throttles to **200ms min**, **except** busy→idle transitions always fire. Rewrite has no throttle — emits every event verbatim. Adds UI jitter during heavy progress bursts. [§5.6]
6. **`detail` comes only from `$/progress` tokens, not serverStatus.** Rewrite currently uses serverStatus `message` as detail. Legacy discards serverStatus message intentionally — the busy slot is driven by serverStatus, the detail slot by progress tokens. [§5.6]
7. **Triple-gate `was_busy && quiescent && on_quiescence` for opening the diagnostic window.** Rewrite fires on quiescent=true alone. Legacy only opens the window on the busy→idle transition, which matters because the user may fire multiple diagnostics requests while idle and we shouldn't re-pull each time. [§5.5.3]
8. **Incremental `didChange` when only 1 edit op.** Legacy uses LSP `Range`-based incremental sync for single edits; full-text only when ops > 1 or no old_doc (manager.rs:863-899). Rewrite does full-text unconditionally. Performance, not correctness, but rust-analyzer becomes sluggish on large Rust files with full-text churn. [§5.3]

### 7.2 Driver-side deviations (out of M16 scope)

9. **`workspace/didChangeWatchedFiles`**: legacy forwards `FileChanged` events from an `fsevents` watcher to each server. Rewrite has no watcher wiring. rust-analyzer won't notice Cargo.toml edits or new files added outside the editor.
10. **Completion, hover, code-action, goto, rename, format, inlay hints** — catalogued in §5.3, §6.1.7, §6.6. Each of these brings its own pattern set (completion trigger-char detection via `completion_seq`, format-done save cleanup, rename across files, etc).

### 7.3 Consumer-side deviations (in M16 scope)

11. **Unmaterialized buffer creation on LspDiagnostics.** Legacy creates a placeholder `BufferState` when diagnostics arrive for a not-yet-opened file (mod.rs:1136-1141), so `file_categories` picks up LSP error markers in the browser for unopened files. Rewrite's `DiagnosticsStates` is a standalone atom — which is cleaner — but the browser's `file_categories` equivalent needs to consult both atoms. [§6.4.2]
12. **Cursor-line diagnostic popover.** Legacy renders an overlay when the cursor's on a line with Error/Warning — `OverlayContent::Diagnostic { messages, anchor at cursor }` (display.rs:995-1018). Rewrite has no popover at all. Under the "no-smear" rule this still makes sense: when diagnostics are visible, show the popover; when hidden (stale version), no popover. [§6.7.4]
13. **Two-tone diagnostics theme (`.error` / `.warning` only).** Legacy has exactly two style keys; Info/Hint inherit Warning. Rewrite has four. Worth trimming to two if theme-file compatibility with legacy matters. [§6.7.2]
14. **Alt-./Alt-, issue navigation.** Not implemented in rewrite yet. Legacy walks `IssueCategory::NAV_LEVELS` top-down; first level with any items becomes the cycle. Includes diagnostics, git line statuses, PR comments, PR diff. [§6.5]

### 7.4 Consumer-side deviations (out of M16 scope)

15. **Browser LSP badges.** `file_categories_map` scans every open buffer's diagnostics and stamps the file with `LspError` / `LspWarning`. Rewrite's file-finder doesn't consume LSP diagnostics. Needed for feature parity with legacy browser. [§6.2.2]

### 7.5 Intentional deviations (preserved from rewrite plan)

- **No replay-through-undo-log.** Legacy's 3-way offer_diagnostics (fast/replay/reject) is replaced by strict `BufferVersion` equality. Diagnostics hide the instant the buffer moves, reappear when a fresh pull lands. Tradeoff: the "fast-path on undo-back-to-save" user experience is lost. [§6.4.3]
- **Monotonic `BufferVersion` replaces `PersistedContentHash`.** Same acceptance guarantee (reject stale) with a smaller attack surface — no hash collisions, no save-point undo-log coupling. [§6.4.3]
- **`DiagnosticsStates` is its own atom, decoupled from `buffers`.** Legacy parks LSP state on `BufferState`; rewrite keeps them separate. Rationale: driver isolation discipline (state-diagnostics is the only crate the LSP driver touches for delivery), and the browser needs LSP data for files that have no buffer at all.

---

## 8. Open questions for you

1. **Intentional deviations (§7.5) — still the right calls?** Three of them:
   - No replay-through-undo-log.
   - BufferVersion instead of content_hash.
   - DiagnosticsStates as a separate atom.

2. **§7.3 #11 — Browser LSP badges.** If we want file-finder / browser to colour files that have diagnostics-but-no-buffer, we need to either (a) read from both `BuffersState` and `DiagnosticsStates` in `file_categories_map` equivalent, or (b) adopt legacy's placeholder-buffer approach (creates phantom entries in `BuffersState`). Which direction?

3. **§7.3 #12 — Cursor-line popover.** Ship with M16 or defer? The feature is orthogonal to the diagnostics pipeline (just a read from `DiagnosticsStates[active_path]` + filter by cursor row) but expands the overlay painter surface.

4. **§7.3 #14 — Alt-./Alt-, nav.** The full issue-navigation cycle (not just LSP) pulls in git line statuses + PR comments + PR diff. Shipping nav without all four sources is worse than not shipping it, because users learn "Alt-. cycles diagnostics" then get surprised when git hunks appear next week. Defer to after git+PR are landed?

5. **§7.2 #9 — `didChangeWatchedFiles` + file watcher.** Ship with M16 or defer? rust-analyzer loses sensitivity to Cargo.toml / new-file events without this. I'd defer — implementing a watcher driver is a milestone on its own.

6. **§7.1 priority order.** The 8 driver-side deviations aren't all equal. My ranking (highest → lowest impact):
   1. #1 (didChangeConfiguration) — blocks rust-analyzer indexing.
   2. #3 (second RequestDiagnostics signal) — blocks initial-load diagnostics.
   3. #2 (quiescence detection path) — subtle; wrong capability check skips quiescence-aware path for rust-analyzer.
   4. #7 (triple-gate) — subtle re-pull bug, not user-visible often.
   5. #4/#5/#6 (progress/throttle/detail) — UX polish for status bar.
   6. #8 (incremental didChange) — performance only, rust-analyzer on large files.
   
   Fix in that order? Or different priority?
