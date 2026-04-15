# Driver: workspace

## Purpose

The workspace driver is the largest driver in the tree. It bundles five
distinct responsibilities behind a single `Stream<WorkspaceOut>` /
`Stream<WorkspaceIn>` interface:

1. **Workspace detection** — find the nearest ancestor with `.git/`, treat
   that directory as the workspace root; fall back to the start dir if no
   ancestor has one.
2. **Primary‑instance lock** — `flock(LOCK_EX | LOCK_NB)` on a per‑workspace
   lockfile under `$config/primary/<hash>`. One led instance per workspace
   wins and becomes "primary"; others run as "secondary" (read‑only config,
   no session save).
3. **Session persistence** — SQLite database at `$config/db.sqlite` stores
   per‑workspace open tabs, cursor/scroll positions, browser state, and a
   free‑form KV map.
4. **Undo persistence + cross‑instance sync** — per‑buffer undo history is
   written to the DB on an idle timer; other led instances see it via a
   notify‑dir touch, read the new entries back, and apply them to their
   in‑memory buffers if chain/content hashes agree.
5. **Workspace file watchers** — a recursive watcher on the workspace root
   (filters out `.git/` contents except `index`, `HEAD`, `refs/*` which
   surface as `GitChanged`), and a non‑recursive watcher on `$config/notify/`
   (filters to Create/Modify, debounces 100 ms).

Init is one‑shot: after `WorkspaceOut::Init { startup }` lands, the driver
fires a fixed sequence `Workspace` → `SessionRestored` → `WatchersReady`. The
rest of the driver's emissions are responses to commands (`SaveSession`,
`FlushUndo`, `ClearUndo`, `CheckSync`) or watcher‑driven
(`WorkspaceChanged`, `GitChanged`, `NotifyEvent`).

See `crates/workspace/src/lib.rs` (546 lines) for the driver and
`crates/workspace/src/db.rs` (832 lines) for the SQLite schema and
operations.

## Lifecycle

- **Start**: `driver(out: Stream<WorkspaceOut>, file_watcher: Arc<FileWatcher>) -> Stream<WorkspaceIn>`
  spawns one `tokio::spawn` task that owns an `Option<Connection>`, an
  `Option<File>` (the lock file), a HashMap of pending notify events, and
  three select arms (`cmd_rx`, `root_watch_rx`, `notify_watch_rx`). Until
  `Init` lands, `current` is `None` and all watcher arms are dormant
  (`crates/workspace/src/lib.rs:160-188`).
- **Init dispatch**: `led/src/derived.rs:59-63` emits one `WorkspaceOut::Init
  { startup }` once at startup (deduped on `s.startup` which is effectively
  a constant).
- **Init sequence** (`crates/workspace/src/lib.rs:189-295`):
  1. If `startup.no_workspace`: emit only `SessionRestored { session: None
     }` then `WatchersReady`. Skip all of the workspace machinery.
  2. Otherwise: `find_git_root(start_dir)`, compute `user_root` via
     `CanonPath::to_user_path` to preserve symlinks.
  3. `try_become_primary(config, root)` (skipped in headless/tests which
     always set `primary=true`). Acquires or fails `flock` on
     `$config/primary/<16‑hex hash of root>`.
  4. Send `WorkspaceIn::Workspace { workspace }`.
  5. Open `$config/db.sqlite` via `db::open_db` (runs migrations).
  6. If primary, `db::load_session(root)`; for each buffer load undo via
     `db::load_undo_all`. Non‑primary gets `session=None` (it should not
     restore tabs that the primary owns).
  7. Send `WorkspaceIn::SessionRestored { session }`.
  8. `fs::create_dir_all($config/notify/)`.
  9. Register root recursive watch and notify non‑recursive watch on the
     shared `FileWatcher`.
  10. Send `WorkspaceIn::WatchersReady`.
- **Stop**: mpsc drop exits the loop. The SQLite `Connection` and the lock
  `File` are dropped, releasing resources and the flock.
- **`--no-workspace`**: as above, only `SessionRestored(None)` +
  `WatchersReady` are emitted so the model's phase machine can advance
  Init → Running without hanging. No DB, no watchers, no session save, no
  undo persistence, no cross‑instance sync.

## Inputs (external → led)

1. **Shared `FileWatcher`** — delivers `WatchEvent { kind, paths }` for the
   recursive root watch and the non‑recursive notify watch.
2. **Filesystem for session DB** — `$config/db.sqlite` (WAL mode), read
   during session restore, written during session save, flush‑undo,
   clear‑undo, and check‑sync.
3. **Filesystem for primary lock** — `$config/primary/<hash>` opened with
   `flock(LOCK_EX | LOCK_NB)`.
4. **Filesystem for notify dir** — `$config/notify/<path_hash>` files
   touched by another led instance after it flushes or clears undo for a
   file we have open.
5. **Filesystem for git sentinel detection** — `.git/index`, `.git/HEAD`,
   `.git/refs/*` modifications are reported by the root watcher and
   classified by `is_git_sentinel()` (`lib.rs:519-538`).

## Outputs from led (model → driver)

Values of `WorkspaceOut` (`crates/workspace/src/lib.rs:30-53`):

| Variant                                                                                                                   | What it causes                                                                                                                                                      | Async?                     | Returns via                                                                |
|---------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------------------|----------------------------------------------------------------------------|
| `Init { startup }`                                                                                                        | Run the full init sequence above (git‑root, flock, DB open, session load, watcher register)                                                                         | yes                        | `Workspace`, `SessionRestored`, `WatchersReady` (in that order)            |
| `SaveSession { data }`                                                                                                    | `db::save_session(root, data)` replaces the `workspaces`, `buffers`, `session_kv` rows and cascades undo deletion for dropped files                                 | yes                        | `SessionSaved` (always, even on DB error — error is just logged)           |
| `FlushUndo { file_path, chain_id, content_hash, undo_cursor, distance_from_save, entries }`                               | `db::flush_undo` upserts `buffer_undo_state` and appends `entries` to `undo_entries`; returns the new max `seq`. Then `touch_notify_file` writes to `$config/notify/<path_hash>` to wake other instances | yes                        | `UndoFlushed { file_path, chain_id, persisted_undo_len, last_seen_seq }`   |
| `ClearUndo { file_path }`                                                                                                 | `db::clear_undo` deletes `buffer_undo_state` + `undo_entries` rows for that file; also touches notify file                                                          | yes, fire‑and‑forget       | *(none — silent)*                                                          |
| `CheckSync { file_path, last_seen_seq, current_chain_id }`                                                                | `db::load_undo_after(seq)`; classify result as `SyncEntries` (new remote entries), `NoChange` (empty and same chain), or `ExternalSave` (state row missing — other instance cleared after save) | yes                        | `SyncResult { result: SyncResultKind }`                                    |

Dispatchers in `led/src/derived.rs`:

- `workspace_init` (`:59-63`)
- `session_save` (`:67-117`) — triggered on `Phase::Exiting` transition
  when primary; constructs `SessionData` from all non‑preview tabs plus
  `build_session_kv(state)` for browser/jump/expanded‑dirs.
- `undo_flush` (`:120-134`) — triggered by `s.pending_undo_flush` version.
- `undo_clear` (`:137-143`) — triggered by `s.pending_undo_clear` version
  (bumped after save completes).
- `sync_check` (`:146-162`) — triggered by `s.pending_sync_check` version
  (bumped on `NotifyEvent`), reads `last_seen_seq` and `chain_id` from
  the matching buffer.

## Inputs to led (driver → model)

Values of `WorkspaceIn` (`crates/workspace/src/lib.rs:55-80`):

| Variant                                                                                                         | Cause                                                                                       | Frequency                                                                                 | Consumed in                                                                                                                                                  |
|-----------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `Workspace { workspace }`                                                                                       | Git root resolved, flock attempted                                                          | Once at startup (skipped in no‑workspace mode)                                            | `led/src/model/mod.rs:64-79` → `Mut::Workspace` — sets `state.workspace`, seeds browser root, seeds `pending_lists` with root and any expanded dirs, bumps `git.pending_file_scan` |
| `SessionRestored { session }`                                                                                   | After `Workspace`, or alone in no‑workspace mode (always `None` there)                      | Once at startup                                                                           | `led/src/model/session_of.rs` — fans out to 14 child streams (see below)                                                                                     |
| `SessionSaved`                                                                                                  | `SaveSession` completed                                                                     | Once on quit (primary only)                                                               | `led/src/model/mod.rs:56-62` → `Mut::SessionSaved` sets `session.saved = true`; `lib.rs:339-350` gates actual process exit on this flag                      |
| `UndoFlushed { file_path, chain_id, persisted_undo_len, last_seen_seq }`                                        | `FlushUndo` completed                                                                       | Per `undo_flush` timer fire per buffer                                                   | `model/mod.rs:117-143` → `Mut::UndoFlushed` — calls `buf.undo_flush_confirmed` and stores `last_seen_seq`                                                    |
| `SyncResult { result: SyncResultKind::SyncEntries { file_path, chain_id, content_hash, entries, new_last_seen_seq } }` | Another instance wrote entries; our `last_seen_seq` is behind                               | Per cross‑instance write per open buffer                                                 | `model/sync_of.rs:35` → `BufferState::try_apply_sync(entries)` (validates chain+hash, applies ops)                                                            |
| `SyncResult { result: SyncResultKind::ExternalSave { file_path } }`                                             | DB has no undo rows (other instance saved + cleared) but mtime moved                        | Rare, per external save via another led instance                                          | `model/sync_of.rs:28-34` — invalidates buffer / triggers reload                                                                                              |
| `SyncResult { result: SyncResultKind::NoChange { file_path } }`                                                 | CheckSync race: entries empty and chain_id matches (our own write or spurious notify)       | Per spurious notify event                                                                 | `model/sync_of.rs:26` — dropped with `None`                                                                                                                  |
| `NotifyEvent { file_path_hash }`                                                                                | 100 ms‑debounced Create/Modify on `$config/notify/<hash>`                                   | Per cross‑instance flush/clear for a file we have open                                    | `model/mod.rs:147-158` → `Mut::NotifyEvent` → `state.pending_sync_check.set(path)` → derived layer dispatches `CheckSync`                                    |
| `WorkspaceChanged { paths }`                                                                                    | Root watcher fired Create/Remove for non‑`.git` paths                                       | Per external file create/delete in the workspace                                          | `model/mod.rs:81-107` → `Mut::WorkspaceChanged` — refreshes dir listings for visible parent dirs                                                              |
| `GitChanged`                                                                                                    | Root watcher fired on `.git/index`, `.git/HEAD`, or `.git/refs/*`                           | Per external git command (commit, checkout, add ‑u)                                       | `model/mod.rs:109-113` → `Mut::GitChanged`; also forked at `lib.rs:294-298` into the `git_activity` stream which triggers the `pr_settle` timer              |
| `WatchersReady`                                                                                                 | Both watchers registered                                                                    | Once at startup                                                                           | `model/mod.rs:56-62` → `Mut::WatchersReady` sets `session.watchers_ready = true` (gates cross‑instance sync machinery so tests can wait for it)              |

### `SessionRestored` fan‑out (session_of.rs)

`led/src/model/session_of.rs:104-253` parses `RestoredSession` into a
`SessionData` struct and branches into 14 child streams, each producing a
single‑field `Mut` per Principle 2 of the FRP architecture:

- `SetActiveTabOrder(Option<usize>)`
- `SetShowSidePanel(bool)` (skipped in standalone mode to preserve
  `AppState::new` default)
- `SetSessionPositions(HashMap<CanonPath, SessionBuffer>)`
- `SetBrowserState { selected, scroll_offset, expanded_dirs }`
- `SetJumpState { entries, index }`
- `SetPendingLists(Vec<CanonPath>)` for expanded dirs
- `SetPendingLists(vec![start_dir])` for standalone‑mode kickoff
- `EnsureTab(buf)` per pending open (resume path)
- `SetResumeEntries(pending_opens)`
- `SetPhase(Phase::Resuming)` when there are pending opens
- `SetPhase(Phase::Running)` when there are none
- `EnsureTab(buf).with_create_if_missing(true)` for each startup CLI arg
  when there are no pending opens
- `SetFocus(resolve_focus_slot(s))`
- `BrowserReveal(startup.arg_dir)`

## State owned by this driver

Internal to the spawned task (not in `AppState`):

- `current: Option<Workspace>` — the resolved workspace; `None` before
  Init, `Some` after (except in `--no-workspace` mode where it remains
  `None`).
- `root_str: String` — `root.to_string_lossy()` cached because every DB
  op takes a `&str` key.
- `_db: Option<rusqlite::Connection>` — SQLite connection with WAL +
  foreign keys enabled. Held for the lifetime of the driver.
- `_lock_file: Option<File>` — holds the flock. Dropped on driver shutdown
  to release the lock.
- `_root_reg: Option<Registration>`, `_notify_reg: Option<Registration>` —
  watcher registration handles. Dropping them unregisters; they are held
  for the lifetime of the driver.
- `pending_notify: HashMap<String, Instant>` — notify‑dir events within a
  100 ms quiet window are coalesced per path hash; emission happens on the
  50 ms polling arm of the select (`lib.rs:429-443`).

## External side effects

- **Filesystem reads**: session DB, primary lockfile.
- **Filesystem writes**: session DB (WAL journal), primary lockfile,
  `$config/notify/<hash>` touch files on flush/clear, `$config/notify/`
  directory creation.
- **System calls**: `libc::flock(LOCK_EX | LOCK_NB)` on the lockfile.
- **File watch registrations**: recursive root, non‑recursive notify dir.

## Known async characteristics

- **Latency**:
  - `Init`: git‑root walk is filesystem‑bound but trivial (a few `stat` calls
    per ancestor). Flock is one syscall. DB open + migrations are a few
    hundred microseconds on warm cache. Session load is a few queries.
    Total typical: 5–20 ms.
  - `SaveSession`: proportional to tab count; a dozen tabs is sub‑ms.
  - `FlushUndo`: proportional to entry count; writes `Vec<UndoEntry>` as
    rmp‑serde blobs. Typical per flush: <1 ms.
  - `CheckSync`: one `SELECT` plus a bounded `SELECT ... WHERE seq > ?`.
    Sub‑ms.
- **Ordering**: single `tokio::select!` loop; commands processed FIFO.
  Watcher arms may interleave with command arms between awaits.
- **Cancellation**: none. In‑flight DB writes run to completion.
- **Backpressure**: mpsc 64. Overflow silently drops commands.
- **Debouncing**: notify events are debounced 100 ms
  (`lib.rs:429-443`) — the polling tick every 50 ms flushes any entry in
  `pending_notify` whose timestamp is older than 100 ms. This absorbs
  burst‑writes from another instance flushing multiple files.
- **Flock is advisory**. Two processes not using `flock` can write to
  the DB simultaneously. In practice only led uses this lock.

## SQLite schema (`crates/workspace/src/db.rs:22-88`)

`user_version = 3`. On version mismatch, all tables are dropped and
recreated (destructive migration — acceptable because DB is cache‑like,
not source‑of‑truth).

```
workspaces (
  root_path       TEXT PRIMARY KEY,
  active_tab      INTEGER NOT NULL DEFAULT 0,
  show_side_panel INTEGER NOT NULL DEFAULT 1
)

buffers (
  root_path       TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
  file_path       TEXT NOT NULL,
  tab_order       INTEGER NOT NULL,
  cursor_row      INTEGER NOT NULL DEFAULT 0,
  cursor_col      INTEGER NOT NULL DEFAULT 0,
  scroll_row      INTEGER NOT NULL DEFAULT 0,
  scroll_sub_line INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (root_path, file_path)
)

session_kv (
  root_path  TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  PRIMARY KEY (root_path, key)
)

buffer_undo_state (
  root_path          TEXT NOT NULL,
  file_path          TEXT NOT NULL,
  chain_id           TEXT NOT NULL,
  content_hash       INTEGER NOT NULL,
  undo_cursor        INTEGER,
  distance_from_save INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (root_path, file_path)
)

undo_entries (
  seq         INTEGER PRIMARY KEY AUTOINCREMENT,
  root_path   TEXT NOT NULL,
  file_path   TEXT NOT NULL,
  entry_data  BLOB NOT NULL
)

INDEX idx_undo_entries_file ON undo_entries(root_path, file_path, seq)
```

Notes:

- `workspaces.root_path` is the primary key for all per‑workspace data;
  multiple led workspaces on one machine share the DB.
- `session_kv` is a free‑form KV store used for browser state
  (`browser.selected`, `browser.scroll_offset`, `browser.expanded_dirs`)
  and jump list (`jump_list.entries`, `jump_list.index`) — see
  `model/session_of.rs:54-90`. The serialization for `entries` is
  JSON (`VecDeque<JumpPosition>`).
- `buffer_undo_state` is per‑(root, file); `undo_entries` appends per flush.
  `seq` provides the monotonic ordering used by `CheckSync`.
- `entry_data` is an rmp‑serde‑encoded `UndoEntry` blob.
- `save_session` deletes undo rows for files no longer in the session
  (`db.rs:143-148`) — "cleanup on save". Note the cascade on
  `workspaces.root_path` handles only `buffers` and `session_kv`; undo
  tables are cleaned via the explicit `DELETE ... NOT IN (SELECT file_path
  FROM buffers)` statements.

## Translation to query arch

| Current behavior                                    | New classification                                                                                                                |
|-----------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------|
| `WorkspaceOut::Init`                                | Resource driver for `Request::InitWorkspace { startup }`. Results: `Event::WorkspaceResolved`, `Event::SessionLoaded`, `Event::WatchersReady` (three events, not one) |
| `WorkspaceOut::SaveSession`                         | Resource driver for `Request::SaveSession { data }`, result `Event::SessionSaved`                                                  |
| `WorkspaceOut::FlushUndo`                           | Resource driver for `Request::FlushUndo { file_path, entries, ... }`, result `Event::UndoFlushed`                                  |
| `WorkspaceOut::ClearUndo`                           | Resource driver for `Request::ClearUndo { file_path }`, fire‑and‑forget (no event)                                                 |
| `WorkspaceOut::CheckSync`                           | Resource driver for `Request::CheckSync { file_path, last_seen_seq, chain_id }`, result `Event::SyncResult(...)`                   |
| Root watcher → `WorkspaceChanged`                   | Input driver (consolidate into `fs` watcher) → `Event::FsChanged { paths, kind }` + reducer classification                         |
| Root watcher → `GitChanged`                         | Input driver → `Event::FsChanged` with path filter classifying `.git/index|HEAD|refs/*` hits                                       |
| Notify watcher → `NotifyEvent { file_path_hash }`   | Input driver → `Event::NotifyTouch { path_hash }`                                                                                  |
| Debounce 100 ms on notify                           | Keep — either in the driver or as a timer‑gated reducer                                                                            |
| Primary flock                                       | Keep as part of `InitWorkspace` result; exposed on `WorkspaceState`                                                                |
| WAL SQLite                                          | Keep                                                                                                                               |
| Schema‑version‑bumped destructive migration         | Keep — cache‑like data                                                                                                             |

The `Init → Workspace, SessionRestored, WatchersReady` sequence should
become three distinct events in the new arch rather than a single
multi‑response request. This matches the way downstream consumers
already treat them: phase machine advances on `SessionRestored`, sync
machinery gates on `WatchersReady`, browser seeds on `Workspace`.

## State domain in new arch

- `WorkspaceState { root, user_root, config, primary }` lives in its own
  domain atom. Loaded via `Event::WorkspaceResolved`.
- `SessionState { buffers: Vec<SessionBuffer>, active_tab_order,
  show_side_panel, kv }` lives in its own atom. Populated by
  `Event::SessionLoaded`. `SessionState.saved: bool` set on
  `Event::SessionSaved`. `SessionState.watchers_ready: bool` set on
  `Event::WatchersReady`.
- Per‑buffer undo state lives in `BufferState` (already does today). The
  DB‑side persistence is driver‑internal.

## Versioned / position‑sensitive data

- **Undo entries are version‑sensitive by chain**. Each entry has a
  `chain_id` (new per‑process GUID, `new_chain_id()` at `lib.rs:499-510`)
  and a `content_hash`. When sync delivers `SyncEntries`, the receiving
  buffer's `try_apply_sync` validates both before applying. A chain
  mismatch means the remote edits derived from a different base and must
  be rebased (currently: a full buffer reload via the `ExternalSave`
  path).
- **`last_seen_seq` is the high‑water mark** per (instance, root, file).
  `CheckSync` uses it to fetch only new entries. The model stores it on
  `BufferState.last_seen_seq` and bumps via `Mut::UndoFlushed`
  (`model/mod.rs:117-143`).
- **Session positions** (cursor row/col, scroll row/subline) are
  version‑independent — they are a snapshot at save time and only applied
  once, during `BufferOpen`. If the file changed on disk between save and
  restore, `buffers_of.rs:38-44` clamps `cursor_row` to the new line
  count. Col/scroll aren't clamped — [unclear — whether this can produce
  bad cursor positions on files that shrunk between sessions].
- **Workspace root**: a `CanonPath`. Symlink resolution happens at Init;
  `user_root` remembers the unresolved path for display. Not position‑
  sensitive but worth noting: if the symlink target moves, the next
  launch sees a different `root` and the session under the old root
  becomes orphaned in the DB.

## Edge cases and gotchas

- **Standalone mode (`--no-workspace`)** is a deliberate skip‑all‑
  side‑effects path. No `Workspace` event, no DB, no watchers, no session
  save, no undo persistence. The model must handle the absence of these
  gracefully — it does so via `WorkspaceState::Standalone` and
  consumers guarded on `workspace.loaded().is_some()`.
- **Non‑primary session restore returns `None`.** `lib.rs:240-244`: if
  `!primary`, `load_session` is not called even if data exists. Rationale:
  a second led instance on the same workspace should not re‑open the
  primary's tabs. The secondary gets a fresh empty session.
- **Headless = always primary.** `lib.rs:216-219`: when
  `startup.headless` (set by the golden runner), flock is skipped and
  `primary=true`. Avoids stale `.lock` files blocking tests between runs.
- **GitChanged is dispatched even without a workspace loaded... almost.**
  `lib.rs:394-398`: if `current.is_none()` (pre‑Init), watcher events
  are ignored — but watchers aren't even registered until Init completes,
  so this guard is belt‑and‑suspenders.
- **Root‑recursive watch sees `.git/`.** The watcher library watches
  recursively including `.git/`; the driver filters at event delivery
  time (`lib.rs:390-399`). `is_git_internal` checks any path component
  named `.git`; `is_git_sentinel` then narrows to
  `index|HEAD|refs/*`. `.git/objects/**` modifications are suppressed.
- **`WorkspaceChanged` only fires on Create/Remove.** Modify events
  inside the workspace (e.g. another editor writes a file) are suppressed
  (`lib.rs:402-411`). Per‑file external‑edit detection is docstore's
  parent‑dir watch, not this one. Create/Remove are reported because they
  change the browser tree.
- **Notify debouncing is polling‑based.** `tokio::select!` with
  `tokio::time::sleep(50ms)` as the idle arm means the driver burns a
  wakeup every 50 ms even when idle. Acceptable for a desktop editor; in
  the rewrite, consider a proper timer.
- **Undo entries are serialized as rmp‑serde.** Schema version bump on
  `UndoEntry` requires a DB migration. Today the migration strategy is
  "drop all tables on schema version mismatch" — so any `UndoEntry`
  struct change triggers full session loss.
- **`path_hash` collisions.** `lib.rs:512-517`: `DefaultHasher` over the
  path bytes, 16‑hex output. Birthday bound on `u64` is ~4 billion paths.
  In practice collisions are impossible; noted for completeness.
- **Primary lockfile never deleted.** `$config/primary/<hash>` persists
  across runs. When led exits, the flock is released (file closed) but
  the file itself stays. Cleanup is "only on manual `rm`". Not a leak
  (size ≈ 0 bytes each, one per workspace).
- **`SessionSaved` emits even on DB error.** `lib.rs:182-188`: the save
  path logs a warning and still emits the event. The quit gate
  (`lib.rs:339-350`) therefore proceeds to exit even if the session
  didn't actually persist. [unclear — intentional to avoid stuck‑quit
  bugs, or a hole that should propagate the error to the user.]
- **Cross‑instance sync is opt‑in per NotifyEvent.** The primary's flush
  touches the notify file; the secondary's notify watcher fires;
  secondary dispatches `CheckSync`. If the secondary is not running (no
  watcher registered), the primary's edits are only picked up on next
  session restore. Correct by construction.
- **`SessionRestored` is always emitted**, even in no‑workspace mode (as
  `None`). This is load‑bearing: the phase machine in
  `session_of.rs:199-203` advances to `Running` on `SessionRestored` with
  no pending opens. Removing this event would leave the app stuck in
  `Phase::Init` forever.
- **`resume.iter().all(|e| e.state != Pending)`** is the "resume complete"
  predicate. Not enforced by the workspace driver; relevant here because
  resume entries come from `SetResumeEntries` in session_of.rs, which is
  fed from `pending_opens` computed in `parse_session`. The docstore
  driver drives each entry to `Ok|Failed` via `Opened`/`OpenFailed`.

## Goldens checklist

Under `goldens/scenarios/driver_events/workspace/`:

- `workspace/` — natural at startup with `git_init = true`.
- `session_restored_none/` — natural for fresh workspace or
  `no_workspace=true`.
- `session_saved/` — natural via `C‑x C‑c` in a git workspace.
- `undo_flushed/` — natural via type + wait >500 ms (undo_flush timer).
- `watchers_ready/` — natural via any non‑standalone scenario.
- `workspace_changed/` — needs mid‑test fs mutation
  (`fs-create <path>` / `fs-remove <path>`).

Missing / to add:

- `session_restored_some/` — needs pre‑seeded session DB. Options per
  extract doc: (a) setup.toml `[[session_buffer]]` entries inserted via
  `db::save_session()` before spawn; (b) two‑phase test (quit + respawn).
- `git_changed/` — needs mid‑test `git add`/`git commit`. Script command
  `git-cmd <args...>`.
- `notify_event/` — needs mid‑test touch of
  `$config/notify/<path_hash>` by a sibling process. Script command
  `notify <path>` computing the hash.
- `sync_entries/` — needs a second led process writing to the same DB.
  Options: (a) setup.toml `[[sqlite_entry]]` that the runner inserts via
  `db::flush_undo` before spawn; (b) script command `sync-flush <path>
  <entries>`.
- `sync_external_save/` — same setup as sync_entries but with empty
  entry set (simulates other instance cleared after save).
- `sync_no_change/` — same as sync_entries but `chain_id` matches our
  own; verifies silent drop.
- `open_failed/` — deleted session file; relates to docstore but is
  driven by a seeded session entry, so lives here.
- `primary_lock/` — two led instances on the same workspace; first
  becomes primary, second sees `primary=false`. Needs two‑process test
  harness.

[unclear — whether golden runner currently supports pre‑seeding the
session DB (referenced in extract doc as a planned `[[session_buffer]]`
section). Most session‑restore variants depend on this mechanism.]

[unclear — whether the migration destructive‑drop behaviour should be
preserved across the rewrite, or whether the rewrite is a good time to
switch to additive migrations.]
