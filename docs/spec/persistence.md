# persistence

## Summary

`led` persists two things across invocations: a **session** (which files were
open, where the cursor was, panel layout, browser state, jump list) and
**undo history** (per-buffer edit log with content hashes and chain IDs).
Both live in one SQLite database at `<config_dir>/db.sqlite`, alongside a
`primary/` lock directory and a `notify/` directory used for cross-instance
coordination. A workspace is identified by the **git root** found by walking
up from the start directory; each workspace is a row keyed by the
canonical root path. The first led instance to start on a given workspace
acquires a file lock (`flock(LOCK_EX|LOCK_NB)`) and becomes the "primary"
— only the primary writes session rows. Non-primary instances still write
undo entries so that changes made in either copy are visible to the other
via a lightweight notify-and-poll mechanism.

## Behavior

### Paths

- **`<config_dir>/db.sqlite`** — the SQLite file. `<config_dir>` defaults to
  `~/.config/led`, override via `--config-dir`. Opened with WAL journal mode
  and `foreign_keys = ON` (`crates/workspace/src/db.rs:14-15`).
- **`<config_dir>/primary/<hash(root)>`** — the lock file per workspace.
  `hash(root)` is a 16-char hex Rust `DefaultHasher` of the canonical root
  path bytes (`lib.rs:477-496`). Opened with `O_CREAT|O_WRONLY`; the fd
  holds `flock(LOCK_EX|LOCK_NB)` for the life of the process. OS releases on
  process exit regardless of clean shutdown.
- **`<config_dir>/notify/<hash(file_path)>`** — touch files used for
  cross-instance notification. Same hashing function as primary locks but
  keyed on the canonical *file* path (`lib.rs:512-517`).
- **`<config_dir>/keys.toml`**, **`<config_dir>/theme.toml`** — config files
  (not part of this area; see `config.md`).

### Workspace detection (git-root walk)

`find_git_root(start_dir)` (`crates/workspace/src/lib.rs:461-475`):

1. Walk up from `start_dir`, popping one segment at a time.
2. At each level, check whether `<dir>/.git` exists **and is a directory**
   (`.git` as a file — gitdir links for submodules — is not treated as a
   root). Track the deepest (highest in the walk) match in the ancestry.
3. If any match found → return it canonicalized.
4. Otherwise → return `start_dir`.

The choice of "deepest match" (topmost ancestor with `.git`) means that in
a repo with submodules the submodule's parent repo becomes the workspace.
This is explicit: the loop keeps scanning after finding a match.

### Primary-instance lock

`try_become_primary(config, root)` (`lib.rs:477-496`):

1. Ensure `<config>/primary/` exists.
2. `hash = format!("{:016x}", DefaultHasher(root.as_bytes()))`.
3. Open `<config>/primary/<hash>` with `create + write`.
4. `flock(fd, LOCK_EX | LOCK_NB)`. On success, keep the `File` in
   `_lock_file` so the fd stays open for the process lifetime. On failure,
   drop the `File` and report non-primary.

Non-primary implications:
- `db::load_session` is skipped (returns `None` without querying).
- The session save path in `derived.rs:67-117` filters on
  `workspace.loaded().primary == true` — secondary instances never write a
  session row.
- Undo entries are still flushed (primary and secondary both write to
  `undo_entries`), so cross-instance sync works regardless of which role.
- Headless tests bypass the flock entirely (`startup.headless → primary = true`).

### SQLite schema

`SCHEMA_VERSION = 3` (`crates/workspace/src/db.rs:20`). When
`user_version != 3`, the driver drops every known table
(`undo_entries`, `buffer_undo_state`, `session_kv`, `buffers`,
`workspaces`, and two legacy names `session_buffers`/`session_meta`) and
recreates from scratch. There is no forward/backward migration — a version
bump wipes all prior session + undo state. This is the mechanism used by
`--reset-config`, which goes further and also `rm`s the whole file.

Schema (verbatim from `db.rs:39-85`):

```sql
CREATE TABLE IF NOT EXISTS workspaces (
    root_path       TEXT PRIMARY KEY,
    active_tab      INTEGER NOT NULL DEFAULT 0,
    show_side_panel INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS buffers (
    root_path       TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
    file_path       TEXT NOT NULL,
    tab_order       INTEGER NOT NULL,
    cursor_row      INTEGER NOT NULL DEFAULT 0,
    cursor_col      INTEGER NOT NULL DEFAULT 0,
    scroll_row      INTEGER NOT NULL DEFAULT 0,
    scroll_sub_line INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (root_path, file_path)
);

CREATE TABLE IF NOT EXISTS session_kv (
    root_path   TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    PRIMARY KEY (root_path, key)
);

CREATE TABLE IF NOT EXISTS buffer_undo_state (
    root_path          TEXT NOT NULL,
    file_path          TEXT NOT NULL,
    chain_id           TEXT NOT NULL,
    content_hash       INTEGER NOT NULL,
    undo_cursor        INTEGER,
    distance_from_save INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (root_path, file_path)
);

CREATE TABLE IF NOT EXISTS undo_entries (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    root_path   TEXT NOT NULL,
    file_path   TEXT NOT NULL,
    entry_data  BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_undo_entries_file
    ON undo_entries(root_path, file_path, seq);
```

Notes:
- `buffers` cascades from `workspaces` on delete (via the FK), so deleting
  a workspace row wipes its tabs.
- `buffer_undo_state` and `undo_entries` **do not** have foreign keys to
  `workspaces`; they live as independent (root, file) namespaces. Cleanup
  of orphaned undo is done explicitly by `save_session` (see below).
- `undo_entries.entry_data` is a msgpack-serialized `UndoEntry` (via
  `rmp_serde::to_vec`; `db.rs:245-248`).
- `content_hash` is stored as `i64` but the Rust side is `u64` wrapped in
  `PersistedContentHash`; lossy at the sign bit, but collision odds are
  astronomical.
- `undo_cursor` is nullable. `None` means "undo cursor at head"; some
  integer means "user has undone some entries but not saved yet".
- `distance_from_save` is a signed counter representing how many undo
  groups separate the current buffer state from the last-save state. Used
  to compute the "modified" dot on reopen.

### Session save/load

`save_session(conn, root_path, data)` (`db.rs:92-151`) is a single
transaction:

1. UPSERT into `workspaces(root_path, active_tab, show_side_panel)`.
2. `DELETE FROM buffers WHERE root_path = ?` — replaces previous tab set.
3. INSERT each `SessionBuffer { file_path, tab_order, cursor_row, cursor_col,
   scroll_row, scroll_sub_line }`.
4. `DELETE FROM session_kv WHERE root_path = ?`, then INSERT each kv pair.
5. **Orphan cleanup**: `DELETE FROM undo_entries WHERE root_path = ? AND
   file_path NOT IN (SELECT file_path FROM buffers WHERE root_path = ?)`
   and the same for `buffer_undo_state`. Closing a tab and then saving
   session purges that file's undo.
6. Commit.

Triggered once per quit. The derived layer (`derived.rs:67-117`) builds
`SessionData` from the current `AppState.tabs` (non-preview tabs only,
materialized buffers only) + `show_side_panel` + a KV blob (see below).

`load_session(conn, root_path)` (`db.rs:153-211`) returns `Option<RestoredSession>`:

- Reads `(active_tab, show_side_panel)` from `workspaces`; returns `None`
  if the row is missing.
- Reads `buffers` ordered by `tab_order`; returns `None` if the list is
  empty (treating empty-tab-list as no session).
- Reads `session_kv` into a HashMap.
- For each `SessionBuffer` the workspace driver then calls `load_undo_all`
  (see below) and stuffs the restored entries into `SessionBuffer.undo`
  before handing off to the model.

### Session KV blob

`session_kv` stores things that would otherwise require more columns. Read
back in `session_of.rs:54-89`:

- `browser.selected` — integer index of selected entry in the sidebar.
- `browser.scroll_offset` — integer scroll offset.
- `browser.expanded_dirs` — newline-separated list of canonical paths;
  restored to a `HashSet<CanonPath>` and also drives which dirs get
  `FsOut::ListDir` fired at startup.
- `jump_list.entries` — JSON-serialized `VecDeque<JumpPosition>`.
- `jump_list.index` — current index into the deque.

Write side: `build_session_kv` in the derived layer assembles these from
current `AppState`. `[unclear — exact location of `build_session_kv`; it is
invoked in `derived.rs:113` but the implementation may live in a helper
module]`.

### Undo persistence

**When entries are flushed**: the `undo_flush` timer (defined in
`derived.rs:303-326`). A 200ms `KeepExisting` one-shot fires whenever any
materialized, non-preview buffer has `undo_history_len() > persisted_undo_len()
|| is_dirty()`. `KeepExisting` means a continuous burst of edits does not
keep resetting the countdown — the first edit starts the 200ms, subsequent
edits within the window do not extend it. On fire, the model samples state
(`mod.rs:386-421`) and emits one `Mut::UndoFlushReady { path, flush }` per
buffer with unpersisted entries. Derived then dispatches
`WorkspaceOut::FlushUndo` per buffer (`derived.rs:120-134`).

(Note: `docs/extract/driver-events.md` says "default 500ms" for `undo_flush`;
current code is 200ms in `derived.rs:323`. `[unclear — which is authoritative;
treat the code as ground truth.]`)

`flush_undo(conn, root, file, chain_id, content_hash, undo_cursor,
distance_from_save, entries)` (`db.rs:215-258`) in one transaction:

1. INSERT OR REPLACE into `buffer_undo_state` — overwrites the per-buffer
   row with the latest chain/hash/cursor/distance.
2. INSERT each new entry (msgpack-serialized) into `undo_entries`. `seq`
   autoincrements monotonically across all (root, file) namespaces.
3. SELECT the max `seq` for this (root, file) as `last_seq`; returned to
   the caller and threaded through `WorkspaceIn::UndoFlushed { ..,
   last_seen_seq }` so the buffer remembers where it last synced.

After a successful flush the driver calls `touch_notify_file(config,
hash(file_path))` (`lib.rs:540-545`) — a write of empty bytes to
`<config>/notify/<hash>`. Other instances' notify watchers see the
Modify/Create event and enqueue a `NotifyEvent` (debounced 100ms per-hash
by the driver, see `lib.rs:414-443`).

**When entries are cleared**: `WorkspaceOut::ClearUndo { file_path }`
triggers `clear_undo(conn, root, file)` — `DELETE FROM undo_entries ...`
and `DELETE FROM buffer_undo_state ...`, then `touch_notify_file` to tell
peers. Dispatched by the save flow: after a buffer is saved, its undo log
becomes irrelevant for the "since last save" semantic and is purged. The
trigger chain is `derived::undo_clear` (`derived.rs:137-143`) keyed on
`pending_undo_clear.version()`; `save_of.rs` bumps this on `BufferSaved`.

### Cross-instance sync

Two instances on the same workspace share `db.sqlite`; writes are
serialized by SQLite's WAL. The notify mechanism exists so that the *other*
instance finds out about the writes without polling:

1. Instance A completes `flush_undo` → touches `notify/<hash(file_path)>`.
2. Instance B's non-recursive watcher on `notify/` fires Create/Modify.
3. Driver debounces 100ms by hash (`lib.rs:429-443`) and emits
   `WorkspaceIn::NotifyEvent { file_path_hash }`.
4. Model looks up the hash in `AppState.notify_hash_to_buffer`
   (`mod.rs:147-158`) and emits `Mut::NotifyEvent { path }`, which sets
   `pending_sync_check.set(path)`.
5. Derived emits `WorkspaceOut::CheckSync { file_path, last_seen_seq,
   current_chain_id }` (`derived.rs:146-162`).
6. Workspace driver calls `load_undo_after(conn, root, file, last_seen_seq)`
   — returns any entries with `seq > last_seen_seq` plus the current
   chain_id and content_hash.
7. Result is one of three `SyncResultKind`:
   - `SyncEntries { entries, chain_id, content_hash, new_last_seen_seq }`
     — apply to the buffer (validated by `BufferState::try_apply_sync`).
   - `ExternalSave { file_path }` — no undo-after rows but the DB entry
     disappeared (another instance saved and cleared); buffer marks itself
     externally-saved.
   - `NoChange { file_path }` — chain matches and no new entries; dropped.

### Hashing

`path_hash` (`lib.rs:512-517`) and the primary hash both use Rust's
`DefaultHasher` (`SipHash-1-3`, unstable across Rust versions — but
`led` stores nothing durable under these hashes, so collisions across
releases are only a cache-miss, not a correctness issue). The hash is
formatted as 16-char lowercase hex (`{:016x}`).

### Retention / growth

- `session_kv` / `buffers`: fully rewritten on every save. Cap is
  "however many tabs you had open".
- `buffer_undo_state`: one row per (workspace, open-buffer). Replaced
  on each flush.
- `undo_entries`: append-only **during a session**; cleared in two ways:
  (a) `clear_undo` after save; (b) orphan cleanup in `save_session` for any
  file not in the new tab set. So the long-term growth envelope is
  "entries of currently-open, currently-dirty buffers across all
  workspaces, plus the tail after the most recent flush of files whose
  owning workspace has not been saved since the file closed".
- There is no explicit retention cap, vacuum, or age-based GC.

### Standalone mode (`--no-workspace`)

The workspace driver short-circuits (`lib.rs:199-205`): no git-root walk,
no flock, no DB open, no watcher registration. It emits
`SessionRestored { session: None }` and `WatchersReady` and stops. The
whole persistence story is bypassed. `AppState.workspace` stays
`WorkspaceState::Standalone` forever.

## User flow

- **Normal save cycle**: user types; 200ms after last keystroke undo flushes
  to DB; seconds/minutes later user saves; undo for that buffer is cleared
  from DB.
- **Two terminals, same project**: first `led` in terminal A acquires the
  flock. Second `led` in terminal B starts, sees flock fail → no session
  restore, empty tab list. User edits file `foo.rs` in terminal B; after
  200ms the undo entries are flushed. Terminal A's notify watcher fires;
  `CheckSync` returns the entries; terminal A's buffer updates live.
- **Quit and reopen**: primary saves session on quit. Reopen restores tabs
  at cursor positions; for each buffer, its undo history is restored too,
  so `Ctrl-/` still works across restarts.
- **Crash / kill**: session row is stale (last save) but undo is current
  (within 200ms of last edit). On reopen, tabs at old cursors + undo log
  of unsaved edits means the user can undo the un-saved work.

## State touched

- `AppState.workspace: WorkspaceState` — `Loading`, `Loaded(Workspace)`,
  `Standalone`.
- `AppState.session: SessionState` — `{ resume: Vec<ResumeEntry>, saved:
  bool, watchers_ready: bool }`. `saved` gates quit.
- `AppState.notify_hash_to_buffer: HashMap<String, CanonPath>` — populated
  as buffers open (`buffers_of.rs:62`).
- `AppState.pending_undo_flush: Mut<Option<UndoFlush>>` — observed by
  derived's `undo_flush` chain.
- `AppState.pending_undo_clear: Mut<CanonPath>` — observed by derived.
- `AppState.pending_sync_check: Mut<CanonPath>` — observed by derived.
- `BufferState.chain_id`, `BufferState.persisted_undo_len`,
  `BufferState.last_seen_seq`, `BufferState.undo_history`, `.is_dirty()`,
  `.content_hash()` — per-buffer persistence cursors.

## Extract index

- Driver outputs: `WorkspaceOut::{Init, SaveSession, FlushUndo, ClearUndo,
  CheckSync}` → `docs/extract/driver-events.md` § workspace + driver
  inventory.
- Driver events: `WorkspaceIn::{Workspace, SessionRestored, SessionSaved,
  UndoFlushed, SyncResult(SyncResultKind::{SyncEntries, ExternalSave,
  NoChange}), NotifyEvent, WorkspaceChanged, GitChanged, WatchersReady}`.
- Timer: `undo_flush` (200ms, KeepExisting) → `docs/extract/driver-events.md`
  § timers.
- CLI flags: `--config-dir`, `--reset-config`, `--no-workspace` →
  `docs/extract/cli.md` (`main.rs:17-61`).
- No user-facing keybindings own persistence actions directly; it is all
  driven by `Action::Quit`, `Action::Save`, and the timer.

## Edge cases

- **Schema version mismatch**: `user_version != 3` → all tables dropped,
  recreated empty, `user_version` updated. Prior session + undo gone.
- **Save_all race** (`POST-REWRITE-REVIEW.md:40-44`): `Ctrl-x Ctrl-a`
  iterates dirty buffers in HashMap order, saves dispatch non-deterministic.
  Subsequent undo-clear ordering is therefore non-deterministic too;
  status-bar "Saved X" ends on whichever save completed last. Rewrite
  should iterate in tab order.
- **Non-primary crash**: no session write; next non-primary start sees
  same non-primary view; any undo entries it wrote are still in DB and
  seen by the primary on next notify.
- **Primary crash mid-flush**: transaction is atomic (SQLite); either all
  new entries landed or none. `buffer_undo_state.undo_cursor` is updated
  in the same tx as the INSERTs so there's no tearing.
- **Undo_cursor nullable**: `load_undo_all` returns `Option<i64>` →
  translated to `Option<usize>`; consumers preserve the None meaning
  ("cursor at head").
- **Session row with zero tabs**: `load_session` treats this as "no
  session" (returns `None`). The zero-row workspace can only exist if
  someone closed all tabs and saved with none open — then the row persists
  but `load_session` ignores it. `[unclear — is this intentional? It means
  closing all tabs effectively resets active_tab/show_side_panel on next
  load.]`
- **Notify debounce**: 100ms quiet window (`lib.rs:429-443`). Rapid writes
  to the same `notify/<hash>` file collapse to one `NotifyEvent`.
- **Notify hash collision**: unlikely (64-bit SipHash) but would cause a
  `CheckSync` for the wrong file. The driver would return `NoChange` or
  empty entries, no corruption.
- **Symlink-chain file names**: session stores `UserPath` (user-typed
  spelling). On load the path is canonicalized; subsequent same-symlink
  re-invocations can reconstruct the chain (`session_of.rs:170-187`).
- **Workspace root changes (e.g. submodule promoted)**: `find_git_root`
  yields a different path; it's a brand-new row in `workspaces`. Prior
  row remains unreferenced (no cascade, since the file-path keys are not
  unique across workspaces — they share the `root_path` namespace).

## Error paths

- **`db::open_db` fails**: warning logged (`lib.rs:264-267`); no DB held.
  Session load returns `None`. Undo flushes silently no-op (the `if let
  Some(ref conn)` guard in `FlushUndo` handler).
- **`flush_undo` INSERT fails**: transaction rolls back. Warning logged.
  No `UndoFlushed` event emitted, so the buffer's `persisted_undo_len`
  does not advance and the next timer fire retries.
- **`save_session` fails**: warning logged; `SessionSaved` is still
  emitted (so quit proceeds — `lib.rs:182-188`). **This is a bug by the
  rewrite's lights**: a silent session-save failure on quit should alert
  the user.
- **Notify touch fails**: silent (`std::fs::write(...).ok()`). Peers don't
  see the update; they would only learn on their own `CheckSync` or on
  next mtime change. `[unclear — is there a fallback poll? Based on
  reading the code, no.]`
- **Notify watcher registration fails**: silent; inert watcher replaces
  the real one. No `NotifyEvent` ever fires for this instance.
- **SyncResult dispatch but buffer was closed in the meantime**: the
  model's sync chain (`sync_of.rs:14-40`) looks up the buffer; if missing,
  the `filter_map` drops the event. No state corruption.
- **Disk full during `flush_undo`**: WAL may fail writeahead. Transaction
  aborts, warning logged, retries on next timer.

## Gaps

- `[unclear — build_session_kv location]`: referenced in `derived.rs:113`;
  `Grep`ing found a call site but not the impl (likely a small helper).
  Not material for the rewrite but worth pinning down.
- `[unclear — `DocStoreIn::ExternalRemove` handling]`: the docstore
  produces this event but `buffers_of.rs:165` drops it. A deleted file
  with an open buffer doesn't get any session-side cleanup. Related to
  persistence insofar as the session still holds the now-missing path on
  next launch (handled by `OpenFailed` flow).
- `[unclear — schema migration policy]`: currently every bump is destructive.
  The rewrite may want at least a data-preserving migration story for
  undo (which users might miss).
- `[unclear — undo DB growth in long-lived workspaces]`: no vacuum, no
  age-based retention. An installation that never quits could accumulate
  unreachable entries for files long since closed but whose workspace row
  never saw a save.
- `[unclear — `content_hash` i64/u64 cast]`: values with the high bit set
  round-trip through `as i64` / `as u64`, but it's worth spelling out for
  the rewrite.
