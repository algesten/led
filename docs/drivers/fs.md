# Driver: fs

## Purpose

The `fs` driver currently serves **only the resource‑driver role**: given a
`FsOut::ListDir { path }` or `FsOut::FindFileList { dir, prefix, show_hidden }`
it synchronously reads the directory, filters and sorts entries, and returns
`FsIn::DirListed` or `FsIn::FindFileListed`. The file‑**watching** role (the
"input driver" side of fs) currently lives inside two other drivers:
`docstore` registers per‑parent watches for external‑change detection, and
`workspace` registers recursive root + non‑recursive notify‑dir watches for
workspace tree and cross‑instance sync. They all share a single
`Arc<FileWatcher>` (`led_core::FileWatcher`, backed by `notify`).

This doc covers `crates/fs/` as it exists today, and flags the watcher work
currently done by other drivers so the rewrite can cleanly split input
(watch events) from resource (list/find) into a single `fs` subsystem.

See `crates/fs/src/lib.rs` for the full implementation — it is one file, 168
lines.

## Lifecycle

- **Start**: `driver(out: Stream<FsOut>) -> Stream<FsIn>` spawns one
  `tokio::spawn` task that reads commands from an mpsc channel, handles them
  synchronously via `std::fs::read_dir`, and pushes results into a second
  mpsc bridged onto an `rx::Stream` via `tokio::task::spawn_local`
  (`crates/fs/src/lib.rs:47-91`).
- **Stop**: when the command stream is dropped, `cmd_rx.recv()` returns
  `None` and the worker task exits naturally.
- **`--no-workspace`**: fs runs identically. It never touches workspace
  state; it is a pure function of path arguments.

## Inputs (external → led)

Resource‑driver role only:

1. **Directory entries** via `std::fs::read_dir` on `ListDir`.
2. **Directory entries with prefix filter** via `std::fs::read_dir` on
   `FindFileList`, filtered with case‑insensitive prefix match.

**Not implemented today**: a watch‑mode input. The `FileWatcher` abstraction
used by docstore and workspace is shared (owned by `led_core`, constructed
at startup in `led/src/lib.rs`) and is not part of the `fs` crate.

## Outputs from led (model → driver)

Values of `FsOut` (`crates/fs/src/lib.rs:7-17`):

| Variant                                        | What it causes                                                                                                                       | Async? | Returns via                                                            |
|------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|--------|------------------------------------------------------------------------|
| `ListDir { path }`                             | `std::fs::read_dir(path)`, drop dot‑files, sort (dirs first, then alpha), return `Vec<DirEntry { name, is_dir }>`                    | yes (executed in the driver task, but uses blocking `std::fs`) | `FsIn::DirListed`                                                      |
| `FindFileList { dir, prefix, show_hidden }`    | `std::fs::read_dir(dir)`, filter by case‑insensitive prefix, optionally drop dot‑files, compute display name (dirs get trailing `/`), sort (dirs first, case‑insensitive alpha). Each entry includes `full: CanonPath` via `UserPath::new(...).canonicalize()`. | yes                                                            | `FsIn::FindFileListed`                                                 |

Dispatchers in `led/src/derived.rs`:

- `browser_list` stream (`led/src/derived.rs:365-369`): triggered by
  `s.pending_lists` versioned field, emits one `ListDir` per path in the
  vec. Seeded at startup by the workspace driver (`pending_lists` populated
  from `session.browser_expanded_dirs` plus the workspace root), and
  refreshed on `WorkspaceChanged` events.
- `ff_list` stream (`led/src/derived.rs:371-384`): triggered by
  `s.pending_find_file_list` versioned field, emits one `FindFileList` per
  prefix change in the find‑file dialog.

## Inputs to led (driver → model)

| Variant                                      | Cause                                       | Frequency                                                                                 | Consumed in                                                                                                                                 |
|----------------------------------------------|---------------------------------------------|-------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------|
| `DirListed { path, entries }`                | Response to `FsOut::ListDir`                | Bursts at startup (workspace root + every session‑restored expanded dir) and on `WorkspaceChanged`; then per user‑initiated browser expand | `led/src/model/mod.rs` → `Mut::DirListed` — merges entries into `state.browser.dir_contents` keyed by `path`.                              |
| `FindFileListed { dir, entries }`            | Response to `FsOut::FindFileList`           | Per find‑file prefix change (debounced by the dialog's own typing model before reaching the driver) | `led/src/model/mod.rs` → `Mut::FindFileListed` — updates the completion popup list.                                                         |

Types:

- `DirEntry { name: String, is_dir: bool }` (`crates/fs/src/lib.rs:33-37`).
- `FindFileEntry { name: String, full: CanonPath, is_dir: bool }`
  (`crates/fs/src/lib.rs:40-45`) — `name` includes the trailing `/` for dirs
  so the UI can render it verbatim.

## State owned by this driver

**None.** fs is stateless. Each request is handled independently by
synchronous `std::fs::read_dir` calls. There is no caching, no rate
limiting, no coalescing.

Compare with `file-search` (which coalesces to the latest request) and
`docstore` (which owns `registrations` + `watched_paths`). The lack of
state here is intentional — directory listings are cheap and idempotent.

## External side effects

- **Filesystem reads**: `std::fs::read_dir(path)`, `entry.file_type()`,
  `entry.file_name()` per entry. Dot‑files are always excluded from
  `ListDir`; `FindFileList` respects `show_hidden`.
- **Path canonicalization**: `UserPath::new(dir.as_path().join(name))
  .canonicalize()` for every `FindFileEntry` — this resolves symlinks,
  which is a syscall each. For large directories this can add up; no
  profile has flagged it.

**No filesystem writes.** fs is read‑only.

## Known async characteristics

- **Latency**: dominated by `std::fs::read_dir` and per‑entry syscalls.
  Typical cold cache: ≲1 ms for small dirs (under 100 entries), scales
  linearly. Canonicalization in `FindFileList` adds one syscall per
  surviving entry.
- **Ordering**: the worker task is a single `while let Some(cmd) =
  cmd_rx.recv().await` loop, so commands are processed strictly FIFO. Two
  rapid `ListDir` commands for the same path produce two `DirListed`
  events in order.
- **Cancellation**: none. An in‑flight listing cannot be interrupted.
- **Backpressure**: mpsc channels sized 64 (`crates/fs/src/lib.rs:49-50`).
  The command bridge uses `try_send().ok()` — overflow silently drops the
  command. With 64 slots this is not a practical concern.
- **No coalescing**: unlike `file-search`, fs does not drain the queue to
  the latest request. Every `ListDir` in the mpsc is honoured. The model
  side handles redundancy by gating dispatch on `pending_lists` /
  `pending_find_file_list` version bumps.
- **Blocking in an async task**: `std::fs::read_dir` is synchronous. The
  worker uses `tokio::spawn` (not `spawn_blocking`), so large directory
  listings block the tokio worker thread. Acceptable for an editor where
  directories are small, but worth flagging for the rewrite.

## Translation to query arch

| Current behavior                                          | New classification                                                                                              |
|-----------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------|
| `FsOut::ListDir { path }`                                 | Resource driver for `Request::ListDir(path)`, result `Event::DirListed { path, entries }`                       |
| `FsOut::FindFileList { dir, prefix, show_hidden }`        | Resource driver for `Request::FindFile { dir, prefix, show_hidden }`, result `Event::FindFileListed`            |
| *(new)* Parent‑dir watcher currently in docstore          | Input driver → `Event::FsChanged { kind, paths }` — absorb docstore's external‑change detection into this subsystem |
| *(new)* Root watcher currently in workspace               | Input driver → `Event::FsChanged` with path filtering done by the workspace domain reducer                      |
| *(new)* Notify‑dir watcher currently in workspace         | Input driver → `Event::NotifyTouch { path_hash }` — separate event because it is a signal, not a content change |

The rewrite should absorb all three watch responsibilities (docstore's
parent‑dir watch, workspace's root watch, workspace's notify‑dir watch)
into the fs input driver so there is **one place** that owns the
`notify::Watcher` and dispatches to whichever domain cares about each path.

## State domain in new arch

- `Request::ListDir` result lands as `Loaded<Vec<DirEntry>>` in
  `BrowserState.dir_contents` keyed by path.
- `Request::FindFile` result lands as `Loaded<Vec<FindFileEntry>>` in
  `UiState.find_file.completions` (or a dedicated `FindFileState`).
- `Event::FsChanged` is transient; applied to whichever domain atom cares:
  `BufferState` for open‑file external change, `BrowserState.dir_contents`
  for tree refresh, `GitState` for `.git/`‑internal sentinel hits.

## Versioned / position‑sensitive data

None. Directory listings are not position‑sensitive — they are a snapshot
of the directory at read time. The `pending_lists` / `pending_find_file_list`
versioned fields on `AppState` exist only to gate dispatch, not to stamp
results. Results are keyed by `path` (for `DirListed`) or `dir` (for
`FindFileListed`) so the reducer can handle interleaved responses: if two
`ListDir`s are in flight for the same path, whichever arrives last wins.

Watch events produced in the query‑arch translation **also** are not
version‑sensitive against buffer edits — they're disk snapshots like
`ExternalChange` is today. Reconciliation with in‑memory buffers uses
`content_hash` comparison, not a version stamp.

## Edge cases and gotchas

- **Dot‑files hard‑coded off in `ListDir`.** `crates/fs/src/lib.rs:156`:
  `if name.starts_with('.') { return None; }` — there is no
  `show_hidden` parameter on `ListDir`. Only `FindFileList` honours it
  (`:112`). The browser panel therefore cannot show hidden files. The
  rewrite should decide whether to keep this asymmetry.
- **Errors are swallowed.** A failed `read_dir` (permission denied, not a
  directory, etc.) logs a warning via `log::warn!` and returns an empty
  `Vec` (`:96-103` and `:145-150`). The model cannot distinguish "empty
  directory" from "read failed". In the query arch this should be an
  explicit `Result`.
- **`FindFileList` canonicalizes every entry.** Large directories (e.g.
  `node_modules/` with thousands of entries) will hit the disk once per
  surviving entry for symlink resolution. No profile has flagged this as
  hot, but it's worth noting.
- **Prefix match is case‑insensitive via `to_lowercase()`.** This
  allocates a new `String` per comparison. Acceptable for typical prefix
  lengths. Does not honour Unicode case folding
  (`.to_lowercase()` does ASCII‑style fold for most code points).
- **No coalescing for rapid prefix changes.** If the user types "abc"
  very quickly, all three prefixes are enqueued. The model side debounces
  in the find‑file dialog before the driver sees the request.
- **Watcher inert on unsupported platforms.** When the fs driver gains the
  input role, it must preserve the current fallback: if the platform has
  no notify support, registration silently succeeds but events never
  fire. Code paths that depend on watcher events (external‑change reload,
  workspace tree refresh, cross‑instance sync) must therefore be tolerant
  of "no events ever" as a normal run mode.

## Goldens checklist

Under `goldens/scenarios/driver_events/fs/`:

- `dir_listed/` — natural via any workspace scenario; at least the root
  listing on workspace init produces one.
- `find_file_listed/` — natural via `press Ctrl-x Ctrl-f`, type prefix.

Missing / to add (post‑rewrite when fs absorbs the watcher role):

- `fs_changed_create/` — mid‑test file creation; currently driven via
  `WorkspaceChanged` (see extract doc `driver-events.md:83`).
- `fs_changed_remove/` — mid‑test file deletion.
- `fs_changed_git_sentinel/` — mid‑test `.git/index` touch; currently
  surfaces as `WorkspaceIn::GitChanged`. In the new arch this is the same
  `Event::FsChanged` with a domain reducer classifying the path.
- `list_dir_error/` — `ListDir` on a non‑existent directory; asserts the
  reducer surfaces a user‑visible error rather than silently dropping.
- `find_file_show_hidden/` — toggles `show_hidden` and verifies dot‑files
  appear/disappear.

[unclear — whether the new `fs` driver should also own `.git/` sentinel
detection or whether that stays in the git driver. Current workspace
driver has `is_git_sentinel()` built in (`crates/workspace/src/lib.rs:519-538`).
Cleanest split is probably: fs emits raw `FsChanged`, git/workspace/browser
reducers each classify.]
