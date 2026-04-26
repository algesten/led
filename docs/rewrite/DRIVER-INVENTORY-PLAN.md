# Driver inventory plan

How to document each driver so the new arch can faithfully reproduce (or consciously diverge from) its behavior.

Target output: `docs/drivers/<name>.md`, one file per driver in current led.

> **2026-04-19 note.** The **new-arch driver shape** — strict
> isolation, `driver-<name>/core/` + `driver-<name>/native/` split,
> ABI types between sync and async, cross-atom memos living in the
> runtime crate — is documented in [`../../../drv/EXAMPLE-ARCH.md`](../../../drv/EXAMPLE-ARCH.md)
> § "Organizing the code: crate layout". Two drivers (`driver-buffers`,
> `driver-terminal`) are built out in M1; use them as a template when
> porting the current-led drivers listed below.
>
> When writing each `docs/drivers/<name>.md`, the "Translation to query
> arch" section should now describe:
>
> 1. Which atom(s) the driver will own (goes in `*-core`).
> 2. The `Cmd` / `Event` types that cross the ABI boundary.
> 3. The sync API (`process` drains incoming events, `execute` issues
>    commands) the main loop will call.
> 4. Which platform(s) need a `*-native` — desktop is always needed;
>    iOS/Android may or may not, depending on whether the driver
>    concept exists there.
> 5. Which cross-atom memos in `runtime/src/query.rs` will consume this
>    atom (via a standalone `#[drv::lens]` + `From<&ThisAtom>` impl).

---

## Why this is its own artifact

Drivers are where behavior meets the outside world. In the FRP arch they're push/pull-style but uniform. In the query arch they split into **input drivers** (push events into the handler) and **resource drivers** (fire-and-forget dispatch, result as event). Understanding what each current driver does is what enables the correct split.

This is extracted from the extracts (Phase A.4) but expanded with details needed for the rewrite to route work correctly.

---

## Template

Each `docs/drivers/<name>.md` has this structure:

```markdown
# Driver: <name>

## Purpose
One paragraph. What does this driver exist to do?

## Lifecycle
When does it start? When does it stop? Any shutdown ordering requirements?

## Inputs (external → led)
What external sources feed into this driver? (OS events, server messages, filesystem, user input, timers, etc.)

## Outputs from led (model → driver)
List every `*Out` variant. What does each cause the driver to do?

| Variant                     | What it causes                          | Async? | Returns via               |
|-----------------------------|-----------------------------------------|--------|---------------------------|
| WorkspaceOut::Init          | resolve workspace root, load session    | yes    | WorkspaceIn::Workspace    |
| WorkspaceOut::SaveSession   | persist session DB                      | yes    | (none — fire-and-forget)  |
| WorkspaceOut::FlushUndo     | persist undo entries                    | yes    | WorkspaceIn::UndoFlushed  |

## Inputs to led (driver → model)
List every `*In` variant. What caused it? When does it fire?

| Variant                    | Cause                                 | Frequency         |
|----------------------------|---------------------------------------|-------------------|
| LspIn::Diagnostics{path,…} | LSP server sent diagnostic publish    | per edit settle   |
| LspIn::Completion{items}   | response to completion request        | per request       |

## State owned by this driver
What mutable state does the driver hold internally (not in AppState)? E.g.:
- LSP driver: server processes, pending request IDs, protocol state machines.
- Workspace driver: SQLite connection pool, session DB handle.

## External side effects
Files written, network calls, processes spawned.

## Known async characteristics
- Latency: typical/worst-case for each request.
- Ordering: does the driver preserve request ordering? Can responses overtake?
- Cancellation: can in-flight work be cancelled?
- Backpressure: does the driver drop events under load?

## Translation to query arch
How this driver splits in the new arch:

| Current behavior              | New classification                          |
|-------------------------------|---------------------------------------------|
| Watches filesystem            | Input driver → Event::FsChanged             |
| Serves ReadFile requests      | Resource driver for Request::ReadFile       |
| Serves ListDir requests       | Resource driver for Request::ListDir        |

## State domain in new arch
Where does this driver's data land?
- Input events: transient, applied to which domain atom(s)?
- Resource results: stored in which domain atom(s) as `Loaded<…>`?

## Versioned / position-sensitive data
Does this driver produce data that must be rebased against buffer edits? (E.g. LSP diagnostics do; git status does not; git line status does.) If so: how is version tracked? What's the rebase function?

## Edge cases and gotchas
Anything subtle the rewrite must preserve.

## Goldens checklist
What scenarios must exist in `tests/golden/drivers/<name>/`? (One per distinct event this driver produces or consumes.)
```

---

## Starter list

Populate one `docs/drivers/<name>.md` per driver below. Each bullet points at what to focus on.

### `terminal-in`
- Input driver (sync-push). Produces key events, resize, focus in/out.
- Raw key → canonical key mapping. Modifier handling (Shift, Alt, Ctrl, Meta, cmd).
- Terminal dimensions.
- No resource-driver role.
- **Translation**: input driver emitting `Event::Key`, `Event::Resize`, `Event::FocusChange`.

### `fs`
- Both input driver (watcher) and resource driver (reads, listings).
- FS watcher setup (notify crate). Which paths are watched; debouncing.
- `FsOut::ListDir`, `FsOut::FindFileList` — request/response.
- Rate limits / coalescing.
- **Translation**: input driver for `Event::FsChanged`; resource driver for `Request::ListDir`, `Request::FindFile`.

### `docstore`
- Resource driver for file open/save lifecycle.
- Distinct from `fs` in that it handles the full buffer lifecycle: open (read + parse as Rope + assign cursor), save (format + write), save-as (write + rename), detect external changes.
- Save-in-flight tracking.
- External-change reconciliation.
- **Translation**: resource driver for `Request::OpenBuffer`, `Request::SaveBuffer`, `Request::SaveBufferAs`. The external-change detection is already fs-driven; could be consolidated into fs.

### `clipboard`
- Resource driver for read/write clipboard.
- Platform differences (macOS, Linux/X11, Linux/Wayland).
- **Translation**: resource driver for `Request::ClipboardRead`, `Request::ClipboardWrite`.

### `workspace`
- Resource driver + stateful persistence.
- Workspace detection (find git root, handle multiple roots).
- Session DB (SQLite) schema and operations.
- Cross-instance sync (file-hash-based notifications).
- Undo persistence.
- **Translation**: resource driver for `Request::LoadSession`, `Request::SaveSession`, `Request::FlushUndo`, `Request::CheckSync`. Possibly emits input events for external-instance changes (`Event::RemoteChanged`).

### `syntax`
- Resource driver for tree-sitter parsing.
- Language detection heuristics (filename, content sniffing).
- Parse result: highlights, brackets, indent.
- Incremental parsing?
- **Translation**: resource driver for `Request::ParseSyntax(path, version)`. Critical: results must be version-stamped because buffers can edit ahead of the parse.

### `lsp`
- The most complex driver. Both input (server-pushed notifications) and resource (request/response).
- Server lifecycle per language.
- Protocol (JSON-RPC over stdio).
- Request correlation.
- Diagnostics publish (server-initiated).
- Completion, goto-def, rename, code-actions, formatting (client-initiated).
- Progress notifications.
- Inlay hints.
- **Translation**: input driver for `Event::LspNotif` (diagnostics, progress, server-state changes); resource driver for all `Request::Lsp*`. All per-file results must be version-stamped.

### `git`
- Resource driver (with timer-driven polling).
- File status scan (fast, whole-repo).
- Line status (per-file, more expensive).
- Branch detection.
- **Translation**: resource driver for `Request::GitScan` and `Request::GitLineStatus(path)`. Line status is per-file and potentially version-sensitive (depends on working-tree contents at scan time).

### `gh-pr`
- Resource driver with polling.
- Uses `gh` CLI under the hood.
- 15s polling interval while on a PR branch.
- **Translation**: resource driver for `Request::LoadPr`, `Request::PollPr`. Poll timer driven by the timers driver.

### `file-search`
- Resource driver for workspace-wide search and replace.
- Ripgrep-style underneath.
- Streaming results? Or all-at-once?
- **Translation**: resource driver for `Request::SearchFiles`, `Request::ReplaceAll`.

### `timers`
- Input driver (timer-expiry events).
- Named timers with set/cancel semantics.
- **Translation**: input driver emitting `Event::TimerFired(name)`. Set/cancel are dispatches (`Request::SetTimer`, `Request::CancelTimer`). Could also be a resource driver — indistinguishable in practice.

### `ui`
- Output-only (render) driver.
- Consumes `AppState`, produces terminal output.
- Also emits `UiIn::EvictOneBuffer` — a rare case of the ui driver pushing back into the model.
- **Translation**: renderer is absorbed into the runtime loop (`terminal.draw(&frame)`). The memory-pressure eviction becomes either an explicit query that decides "evict this," or an event the renderer emits on backpressure.

### `config-file`
- Resource driver + input driver (file watch).
- Loads `keys.toml`, `theme.toml` from disk.
- Hot-reloads on change.
- **Translation**: resource driver for `Request::LoadConfig(kind, path)`; input driver for `Event::ConfigChanged(kind)` via fs-watch. Possibly fold into `fs` + a config-domain reducer.

---

## Fields that currently live in AppState but belong in a domain atom

This is the transition table for domain-atom design. Each field below is currently on `AppState`; the table proposes which new domain atom owns it.

| AppState field                | Proposed new home           | Notes                                  |
|-------------------------------|-----------------------------|----------------------------------------|
| `startup`                     | `ConfigState` / const       | Startup is read-only after init.       |
| `workspace`                   | `WorkspaceState`            |                                        |
| `config_keys`, `config_theme` | `ConfigState`               |                                        |
| `keymap`                      | `ConfigState` (derived?)    | Possibly a query over raw config.      |
| `phase`                       | `UiState`                   |                                        |
| `focus`                       | `UiState`                   |                                        |
| `show_side_panel`             | `UiState`                   |                                        |
| `dims`                        | `UiState`                   |                                        |
| `force_redraw`                | (drop)                      | Rendering is a query; no manual force. |
| `alerts`                      | `UiState`                   |                                        |
| `buffers`                     | `BufferState`               | Including Rope contents, cursor, mark. |
| `tabs`                        | `BufferState`               |                                        |
| `active_tab`                  | `BufferState`               |                                        |
| `save_request`/`save_done`    | (drop)                      | Use `Request::SaveBuffer` + `Event::SaveDone`. |
| `browser`                     | `UiState`                   | Or its own `BrowserState`.             |
| `pending_open`                | (drop)                      | Use `Request::OpenBuffer`.             |
| `pending_lists`               | (drop)                      | Use `Request::ListDir` per lookup.     |
| `session`                     | `SessionState`              |                                        |
| `pending_undo_flush`          | (drop)                      | Use `Request::FlushUndo`.              |
| `pending_undo_clear`          | (drop)                      | Implicit in save flow.                 |
| `pending_sync_check`          | (drop)                      | Use `Request::CheckSync`.              |
| `notify_hash_to_buffer`       | `BufferState` (derived?)    | Maybe a query from `contents`.         |
| `confirm_kill`                | `UiState`                   |                                        |
| `kill_ring`                   | `EditState` (new)           | Kill ring is global editing state.     |
| `kbd_macro`                   | `MacroState` (new)          |                                        |
| `jump`                        | `BufferState` or `NavState` |                                        |
| `find_file`                   | `UiState` (overlay)         |                                        |
| `pending_find_file_list`      | (drop)                      | Use `Request::FindFile`.               |
| `pending_save_as`             | (drop)                      | Use `Request::SaveBufferAs`.           |
| `file_search`                 | `SearchState`               |                                        |
| `pending_file_search/replace` | (drop)                      | Use `Request::SearchFiles` / `Request::ReplaceAll`. |
| `pending_replace_opens`       | (drop)                      | Event-driven flow.                     |
| `pending_replace_all`         | `SearchState`               | Or `UiState` if purely UI-visible.     |
| `git`                         | `GitState`                  |                                        |
| `pending_open_url`            | (drop)                      | Use `Request::OpenUrl`.                |
| `lsp`                         | `LspState`                  |                                        |

Pattern recognition: every `pending_*` `Versioned<T>` in current led becomes a fire-and-forget `Request::*` in the new arch. The whole `Versioned` dance exists specifically because FRP needs a way to signal "dispatch this work" idempotently; the query arch replaces it with explicit `Request` + `is_pending` tracking.

---

## Translation invariants

When writing the per-driver doc, verify:

1. **Every `*Out` variant maps to a `Request::*` or a direct state mutation** in the new arch. If it can't, the new arch is missing a dispatch mechanism.
2. **Every `*In` variant maps to an `Event::*` variant** in the new arch. If it can't, the new arch is missing an event type.
3. **Every `Versioned<T>` field has a corresponding `Request::*`** and, optionally, a `Loaded<T>` slot in a domain atom for the in-flight / result state.
4. **Position-sensitive outputs are version-stamped at source.** LSP diagnostics, LSP inlay hints, syntax highlights, git line status — all need version stamps.

If any of these fail, surface it as an open question in `docs/rewrite/README.md` § "Key open questions."

---

## Suggested ordering

When producing `docs/drivers/*.md` files, start with the hardest and the ones that touch the most domains:

1. `lsp` — most complex; sets the pattern for versioned async data.
2. `docstore` — interacts with buffers, saves, external changes.
3. `workspace` — persistence surface.
4. `fs` — foundational resource driver.
5. `git` — similar pattern to lsp; validates the versioned-data pattern.
6. `syntax` — also versioned-async.
7. `ui` — confirms rendering split.
8. Others (`clipboard`, `timers`, `file-search`, `gh-pr`, `terminal-in`, `config-file`) — mechanical.

Doing the complex ones first means patterns get established early; the simple drivers slot in.
