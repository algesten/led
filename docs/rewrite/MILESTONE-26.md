# Milestone 26 — File-watch + cross-instance sync

> **Status (2026-04-27): SHIPPED, including the LSP follow-up.**
> Five of the six M26-gated goldens are green; the sixth
> (`edge/external_change_while_dirty`) fails on a pre-existing
> harness `wait_ready` race independent of M26 (verified by
> stashing). The LSP `workspace/didChangeWatchedFiles` fan-out
> was originally deferred at ship time and landed in the same
> commit window — see [`M26-FOLLOWUP-LSP.md`](M26-FOLLOWUP-LSP.md)
> for the as-shipped breakdown of that piece. The design below
> remains the authoritative blueprint.

After M26, led notices when files on disk change underneath
it: external edits to an open buffer reload silently when the
buffer is clean, the workspace tree refreshes when files
appear or disappear, and a second led instance editing the
same workspace stays coherent through the SQLite undo log.
The plumbing is also intended to unblock LSP's
`workspace/didChangeWatchedFiles` so rust-analyzer re-indexes
when `Cargo.toml` changes outside the editor — that piece is
the deferred follow-up.

This is the milestone the `smoke/external_change`,
`edge/external_change_while_dirty`,
`edge/external_delete_open_file`,
`driver_events/docstore/external_change`,
`driver_events/workspace/workspace_changed`, and
`features/editing/type_delete_reflow` goldens have been gated
on since they were authored on `main`. M21 explicitly deferred
the `WorkspaceCheckSync` trace + cross-instance machinery to
this slot ([`MILESTONE-21.md`](MILESTONE-21.md) §"Out").

Prerequisite reading:

1. [`docs/spec/persistence.md`](../spec/persistence.md) §
   "Cross-instance sync" + § "Hashing" — authoritative for
   the notify-touch protocol and chain-id semantics.
2. [`docs/spec/buffers.md`](../spec/buffers.md) § "External
   filesystem change" — three-branch reconcile (same hash +
   dirty / different hash + dirty / different hash + clean).
3. [`docs/drivers/fs.md`](../drivers/fs.md) § "Translation to
   query arch" — the agreed plan to consolidate all watch
   responsibilities (docstore parent-dir watch, workspace
   root watch, notify-dir watch) into one fs subsystem.
4. [`docs/drivers/workspace.md`](../drivers/workspace.md) §
   "Inputs" — the legacy notify-dir watcher behaviour
   (non-recursive, 100ms debounce, Create+Modify only).
5. [`docs/rewrite/lsp-patterns.md`](lsp-patterns.md) §7.2 —
   the documented gap that M26 closes for rust-analyzer.
6. [`MILESTONE-21.md`](MILESTONE-21.md) §"Growth-path hooks"
   — already names the `<config>/notify/<hash(root)>/` watch
   as M26's hook-in point.
7. `crates/runtime/src/lib.rs` lines 1715–1791 — the existing
   per-tick `FlushUndo` debounce + `UndoPersistTracker`. M26
   adds a sibling memo (`sync_check_targets`) and a new
   `SyncResult` ingest arm.
8. `crates/driver-session/native/src/lib.rs` — the SQLite
   worker M26 extends with a `CheckSync` arm reading from
   `undo_entries` via `seq > last_seen_seq`.
9. `goldens/scenarios/{smoke,edge,driver_events,features}/...`
   — six target scenarios (listed above). Their
   `dispatched.snap` files are the contract.

---

## Goal

```
$ cd ~/project && led notes.txt
# Edit the file in another editor; on FSEvents/inotify fire,
# led re-reads the disk content and replaces the rope. The
# status bar still reads `notes.txt` (no `*`) — the buffer
# matches disk again.

$ cd ~/project && led shared.txt
# In led, type some unsaved edits. Now `git checkout` the file
# in a sibling shell. led's watcher fires; because the buffer
# has local edits, the on-disk write is silently dropped — the
# user's edits stay. (Future polish: an `Alert::Warn` UX; out
# of scope.)

$ cd ~/project && led            # terminal A — primary
$ cd ~/project && led            # terminal B — secondary
# Edit `foo.rs` in terminal B. After 200 ms terminal B's undo
# flushes to SQLite + touches `$config/notify/<hash(foo.rs)>`.
# Terminal A's notify watcher fires; A dispatches CheckSync;
# A's session driver returns the new entries; A's buffer
# applies them via try_apply_sync. The two views converge.

$ cargo add anyhow                # outside led
# rust-analyzer (running inside led) receives
# workspace/didChangeWatchedFiles for `Cargo.toml`, re-indexes,
# and the next pull diagnostics request reflects the new dep.
```

## Scope

### In

The In list is structured per `EXAMPLE-ARCH.md` Phase ordering:
**(1) sources** (driver-owned external-fact sources + the
shadow-source updates), **(2) ABI types** (Cmd/Event in driver
cores), **(3) inputs + memos** (declared in the runtime crate
that consumes them), **(4) main-loop wiring** (the four phases),
**(5) trace + harness**.

#### (1) Sources

- **`FileWatchState`** (driver-file-watch external-fact
  source). This is the source the driver owns. Per the
  "Stateless drivers still need an in-flight source"
  guideline (G3), the source carries:

  ```rust
  // crates/driver-file-watch/core/src/state.rs
  use imbl::HashMap;

  #[derive(Debug, Clone, Default, PartialEq)]
  pub struct FileWatchState {
      /// Currently-active registrations the driver believes
      /// it has installed on its `notify::Watcher`. Source-
      /// of-truth for the "actual" side of the desired/actual
      /// diff that produces `FileWatchCmd::Watch` /
      /// `Unwatch`. Imbl-backed so a memo input projection
      /// is a pointer copy on idle ticks (G14).
      pub registry: HashMap<WatcherId, Registration>,
      /// Most-recent `FileWatchEvent::Changed` per id, queued
      /// here by `process()`. Memos in the runtime read this
      /// during the Query phase. Cleared (replaced with an
      /// empty map) at the top of every Execute phase so
      /// each event drives at most one tick of dispatches —
      /// mirrors the `Mut`-as-replace pattern legacy used.
      pub recent_events: HashMap<WatcherId, Vec<FileWatchEvent>>,
      /// Backend health. `Healthy` is the steady state;
      /// `Inert` (platform unsupported) makes every
      /// `Watch` a silent no-op; `Failed` carries the last
      /// `notify` error so the runtime can decide whether
      /// to surface an alert.
      pub backend: BackendStatus,
  }

  #[derive(Debug, Clone, PartialEq)]
  pub struct Registration {
      pub path: CanonPath,
      pub recursive: bool,
      pub debounce_ms: u32,
  }

  #[derive(Debug, Clone, Default, PartialEq)]
  pub enum BackendStatus {
      #[default]
      Healthy,
      Inert,
      Failed { message: String },
  }
  ```

  Per G1, this source carries **only** the driver-owned
  external-fact view ("what the OS watcher knows about");
  it carries no user decisions. The set of paths the user
  *wants* watched is derived from open buffers + workspace
  config in a memo (see §3).

- **`LspWatchedGlobs`** (state-lsp external-fact source —
  *new*). The LSP server tells us via
  `client/registerCapability` which globs to watch. Per G1
  this is an *external fact* (chosen by the server, not the
  user) and lives co-located with other LSP server state
  in `state-lsp`, **not** as a fresh field on `runtime::
  Atoms`:

  ```rust
  // crates/state-lsp/src/lib.rs — additions
  use imbl::HashMap;

  #[derive(Debug, Clone, Default, PartialEq)]
  pub struct LspWatchedGlobs {
      /// Per-server registrations. Outer key is the LSP
      /// server name (e.g. "rust-analyzer"); inner Vec is
      /// the list of compiled glob matchers + watch kinds
      /// the server registered. Replaced wholesale on each
      /// `client/registerCapability` notification — globs
      /// are immutable strings, so a single Arc'd Vec per
      /// server is the natural cache-hit shape.
      pub by_server: HashMap<String, Arc<Vec<RegistrationGlob>>>,
  }

  #[derive(Debug, Clone)]
  pub struct RegistrationGlob {
      /// Compiled `globset::GlobMatcher`. Matching is
      /// allocation-free.
      pub matcher: globset::GlobMatcher,
      /// Bitset of `Created | Changed | Deleted`.
      pub kinds: u8,
  }

  // PartialEq is conservative: we compare by raw glob
  // pattern strings, not the compiled matcher. Wraps a
  // `String` alongside the matcher in the real shape.
  impl PartialEq for RegistrationGlob { … }
  ```

  The `state-lsp` crate already exists; this just adds a
  field. No new state-* crate.

- **Shadow-source seeding clarification on `EditedBuffer`**
  (state-buffer-edits, *no shape change*). Per the
  EXAMPLE-ARCH § "Shadow sources": `EditedBuffer.rope` is
  the user-decision shadow of disk content (an external
  fact). M26 formalises the seeding protocol that was
  previously implicit:

  - **Seed on first completion** — already done at
    `BufferOpen` (M1).
  - **Subsequent external updates don't auto-apply** —
    *new*. The watcher signals "disk changed"; the runtime
    queues a reload candidate (`PendingReread`); only the
    clean-buffer reconcile arm propagates it back into
    `eb.rope`.
  - **Writes are explicit** — already done via
    `Action::Save`.

  No new struct on `EditedBuffer`. The pending-reread queue
  lives on the file-watch driver source's `recent_events`
  (queryable during the next Query phase), not on the user
  source.

#### (2) ABI types — `driver-file-watch/{core,native}`

  ```rust
  // crates/driver-file-watch/core/src/lib.rs
  use led_core::CanonPath;

  /// Stable id assigned by the runtime to each registered
  /// watch. Lets one driver service multiple watch intents
  /// (open-buffer parent dirs, workspace root, notify dir)
  /// without coupling the registrants. Per G13 (ABI types
  /// in driver core): defined here, never on the consumer
  /// side.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub struct WatcherId(pub u64);

  #[derive(Debug, Clone)]
  pub enum FileWatchCmd {
      /// Watch `path` (file or directory). `recursive=true`
      /// includes descendants. The driver coalesces overlapping
      /// registrations onto a single `notify::Watcher` handle
      /// and fans events out by registration.
      Watch {
          id: WatcherId,
          path: CanonPath,
          recursive: bool,
          /// 0 = no debounce; otherwise wait this many ms of
          /// quiet before emitting. Used by the notify-dir
          /// watch (legacy: 100 ms).
          debounce_ms: u32,
      },
      /// Drop a previously-registered watch. Idempotent.
      Unwatch { id: WatcherId },
  }

  #[derive(Debug, Clone)]
  pub enum FileWatchEvent {
      /// One filesystem change, post-debounce + path-filter.
      /// `kinds` is bitset-shaped because notify can fire
      /// "Created+Modified" within one debounce window.
      Changed {
          id: WatcherId,
          path: CanonPath,
          kinds: ChangeKinds,
      },
      /// Backend reported a fatal error (rare — usually a
      /// platform watcher running out of fds). Per-id so the
      /// runtime can decide whether to fall back.
      Failed { id: WatcherId, message: String },
  }

  bitflags::bitflags! {
      #[derive(Debug, Clone, Copy, PartialEq, Eq)]
      pub struct ChangeKinds: u8 {
          const CREATED  = 0b001;
          const MODIFIED = 0b010;
          const REMOVED  = 0b100;
      }
  }

  pub trait Trace: Send + Sync {
      /// Emitted only in debug builds — input-side traces are
      /// noise in dispatched.snap. Kept here so the
      /// goldens-trace harness can opt in via a verbose flag
      /// when investigating watch-driven flakes.
      fn file_watch_event(
          &self, id: WatcherId, path: &CanonPath, kinds: ChangeKinds,
      );
  }

  pub struct FileWatchDriver { /* tx + rx + trace */ }
  impl FileWatchDriver {
      /// Execute pattern (G3): writes intent into
      /// `FileWatchState.registry` *synchronously* for each
      /// `Watch` (so the next tick's `desired_watch_set`
      /// memo sees it as actual) and forwards the command
      /// to the native worker for the async `notify::
      /// Watcher::watch` call. Symmetric for `Unwatch`.
      pub fn execute<'a>(
          &self,
          cmds: impl IntoIterator<Item = &'a FileWatchCmd>,
          state: &mut FileWatchState,
      );
      /// Drain any worker-emitted events into
      /// `FileWatchState.recent_events`. The runtime's
      /// query phase reads from there.
      pub fn process(&self, state: &mut FileWatchState);
  }
  ```

  Native: `notify = "8"` (already an indirect dep via the
  legacy code path; promote to a direct workspace dep).
  One `std::thread` named `led-file-watch`. Per
  `feedback_no_tokio_for_drivers` the worker uses
  `mpsc::Receiver<...>` exclusively. Internal state:

  - `watcher: notify::RecommendedWatcher` (single instance —
    notify supports multiple `watch()` registrations on one
    handle).
  - `registrations: HashMap<WatcherId, RegInfo>` where
    `RegInfo { path, recursive, debounce_ms,
    pending: HashMap<CanonPath, (ChangeKinds, Instant)> }`.
  - On the watcher callback, the driver classifies the event,
    finds matching registrations (a path is "matching" when
    it equals the registered path or has it as an ancestor +
    `recursive`), accumulates into the per-id `pending` map,
    and arms a 50 ms select-timer to drain entries whose
    age exceeds `debounce_ms`.

  Native cross-platform notes:

  - macOS: FSEvents via `notify::FsEventWatcher`. ~3 ms
    per event from MEMORY (`feedback_fsevents_not_slow.md`).
  - Linux: inotify. Recursion is implemented by walking the
    tree at registration; M26 inherits notify's behaviour
    (currently watches only the registered dirs; subdir
    additions need a re-`watch`). The workspace-tree
    Create/Remove path picks this up because we re-register
    on each Create event for new dirs. **Out** below details
    the deferral.
  - Windows: ReadDirectoryChangesW. Untested by us; should
    "just work" via notify.
  - Inert fallback: if `notify::recommended_watcher` errors
    out (containerised CI without inotify, etc.) the driver
    silently treats every `Watch` as a no-op. Spec
    `docs/drivers/fs.md` § "Watcher inert on unsupported
    platforms" already documents this contract.

  The driver does NOT re-read disk content on a `Modified`
  event. It only emits the signal. Re-reading is the
  runtime's job — it dispatches a `FileReadCmd::Read` (M1's
  existing buffer driver) the next tick, which preserves
  the single-source-of-truth contract for buffer materialisation.

- **`SessionCmd::CheckSync` + `SessionEvent::SyncResult`** —
  extend the existing session driver core enums.

  ```rust
  // crates/driver-session/core/src/lib.rs — additions
  #[derive(Debug, Clone)]
  pub enum SessionCmd {
      // existing variants …
      /// Read-back of new undo entries from peers. The
      /// runtime emits this after a notify-dir touch fires
      /// for a buffer we have open. `last_seen_seq` is the
      /// `seq` value the last successful `FlushUndo` returned;
      /// the driver fetches `WHERE seq > last_seen_seq`.
      /// `current_chain_id` lets the driver classify
      /// "your own write echoing back" (chain matches, no
      /// new entries) vs "peer wrote and cleared post-save"
      /// (state-row missing).
      CheckSync {
          path: CanonPath,
          last_seen_seq: i64,
          current_chain_id: String,
      },
  }

  #[derive(Debug, Clone)]
  pub enum SessionEvent {
      // existing variants …
      /// Result of a CheckSync. One of three shapes — see
      /// `SyncResultKind`.
      SyncResult { kind: SyncResultKind },
  }

  #[derive(Debug, Clone)]
  pub enum SyncResultKind {
      /// Peer wrote new entries since `last_seen_seq`.
      /// `chain_id` and `content_hash` are taken from the
      /// peer's `buffer_undo_state` row; runtime validates
      /// against the live buffer before applying.
      SyncEntries {
          path: CanonPath,
          chain_id: String,
          content_hash: PersistedContentHash,
          entries: Vec<EditGroup>,
          new_last_seen_seq: i64,
      },
      /// Peer saved + cleared undo (the state row vanished).
      /// Buffer should reload from disk.
      ExternalSave { path: CanonPath },
      /// Either own self-echo (chain matches, entries empty)
      /// or a notify-touch we already drained. Drop silently.
      NoChange { path: CanonPath },
  }
  ```

  Native worker arm: one `SELECT chain_id, content_hash FROM
  buffer_undo_state WHERE root_path = ? AND file_path = ?`,
  followed by `SELECT seq, entry_data FROM undo_entries WHERE
  root_path = ? AND file_path = ? AND seq > ?`. Classify per
  the legacy logic (`docs/spec/persistence.md` § "Cross-
  instance sync"). Sub-ms typical. Returns one of three
  variants; runtime handles the dispatch.

- **`SessionCmd::FlushUndo` + `ClearUndo`** — the existing
  worker arms gain a `touch_notify_file(config_dir,
  hash(file_path))` side-effect, mirroring legacy
  `lib.rs:540-545`. One `std::fs::write(config_dir.join(
  "notify").join(hash), [])` — empty file is enough; the
  watcher fires on Modify.

  Hash function: 16-char lowercase hex of `DefaultHasher`
  over the canonical path's bytes. Defined once in
  `led_core::path_hash` so the runtime side (notify-hash
  → buffer index) and the driver side (touch path) agree.

- **`led_core::path_hash`** — new helper.

  ```rust
  // crates/core/src/lib.rs — additions
  pub fn path_hash(path: &CanonPath) -> String {
      use std::collections::hash_map::DefaultHasher;
      use std::hash::{Hash, Hasher};
      let mut h = DefaultHasher::new();
      path.as_path().hash(&mut h);
      format!("{:016x}", h.finish())
  }
  ```

#### (3) Inputs and memos (declared in `crates/runtime/`)

Per G2, M26's dispatch is **derived state**, not transition
handlers. Every "what should happen because the watcher
fired" question is one memo. Per G11, the inputs and memos
all live in `runtime/` (the consumer crate); none of the
sibling driver-`*-core` crates depend on `drv`.

**Inputs** (projections from sibling crates' source structs):

```rust
// crates/runtime/src/query.rs — additions

#[derive(drv::Input)]
struct FileWatchEventsInput<'a> {
    pub recent_events: &'a imbl::HashMap<WatcherId, Vec<FileWatchEvent>>,
    pub registry: &'a imbl::HashMap<WatcherId, Registration>,
}
impl<'a> FileWatchEventsInput<'a> {
    pub fn new(s: &'a FileWatchState) -> Self {
        Self { recent_events: &s.recent_events, registry: &s.registry }
    }
}

#[derive(drv::Input)]
struct OpenBuffersInput<'a> {
    /// path → (saved_version, version, disk_content_hash) projection.
    /// Matches what `external_reread_targets` and `notify_hash_index`
    /// actually read; an EditedBuffer's rope changing every keystroke
    /// must NOT recompute these memos.
    pub by_path: &'a imbl::HashMap<CanonPath, BufferIdentity>,
}

#[derive(drv::Input)]
struct WorkspaceRootInput<'a> {
    pub root: &'a Option<CanonPath>,
    pub config_dir: &'a Option<CanonPath>,
}

#[derive(drv::Input)]
struct UndoPersistInput<'a> {
    /// path → (chain_id, last_seq) for the CheckSync builder.
    pub by_path: &'a imbl::HashMap<CanonPath, ChainCursor>,
}

#[derive(drv::Input)]
struct LspGlobsInput<'a> {
    pub by_server: &'a imbl::HashMap<String, Arc<Vec<RegistrationGlob>>>,
}
impl<'a> LspGlobsInput<'a> {
    pub fn new(g: &'a LspWatchedGlobs) -> Self {
        Self { by_server: &g.by_server }
    }
}
```

**Desired-state memos** (G2). Each one answers a single
"what should be true" question. Outputs are `Arc`-wrapped or
`imbl`-backed so cache-hit clones are pointer copies (G14).

```rust
// crates/runtime/src/query.rs — additions

/// Reverse index hash → path. Pure derivation from the open-
/// buffer set; cache-hits when the buffer set is stable.
#[drv::memo(single)]
fn notify_hash_index<'a>(buffers: OpenBuffersInput<'a>)
    -> Arc<HashMap<String, CanonPath>>;

/// "What watch registrations should be installed right now?"
/// Three intents, derived end-state. Diff against
/// `FileWatchState.registry` produces the watch_actions list.
#[drv::memo(single)]
fn desired_watch_set<'a, 'b>(
    buffers: OpenBuffersInput<'a>,
    ws: WorkspaceRootInput<'b>,
) -> Arc<imbl::HashMap<WatcherId, Registration>>;

/// Diff: desired vs actual → list of `Watch` / `Unwatch` cmds.
/// Returned as `Arc<Vec<...>>` so an idle tick (desired ==
/// actual) produces a cached `Arc::clone` of an empty Vec —
/// no allocation.
#[drv::memo(single)]
fn watch_actions<'a, 'b, 'c>(
    desired: DesiredWatchInput<'a>,
    actual: FileWatchEventsInput<'b>,
    ids: WatchIdSeqInput<'c>,
) -> Arc<Vec<FileWatchCmd>>;

/// "Which open buffers need a reread because their disk
/// content just changed?" Reads recent_events for any
/// per-buffer parent watch with MODIFIED, intersected with
/// the open-buffer set.
#[drv::memo(single)]
fn external_reread_targets<'a, 'b>(
    events: FileWatchEventsInput<'a>,
    buffers: OpenBuffersInput<'b>,
) -> Arc<imbl::HashSet<CanonPath>>;

/// "Which buffers need a CheckSync dispatched?" Reads the
/// NOTIFY_DIR watch's recent_events, looks each touched
/// hash up in `notify_hash_index`, intersects with open
/// buffers.
#[drv::memo(single)]
fn sync_check_targets<'a, 'b, 'c>(
    events: FileWatchEventsInput<'a>,
    hashes: NotifyHashIndexInput<'b>,
    persist: UndoPersistInput<'c>,
) -> Arc<Vec<SessionCmd /* CheckSync only */>>;

/// "What `workspace/didChangeWatchedFiles` notifications
/// should fire this tick?" Per-server fan-out: matches the
/// runtime's view of recent file events against each LSP
/// server's registered globs.
#[drv::memo(single)]
fn lsp_watched_file_notifications<'a, 'b>(
    events: FileWatchEventsInput<'a>,
    globs: LspGlobsInput<'b>,
) -> Arc<Vec<LspCmd /* DidChangeWatchedFiles only */>>;

/// "Should the workspace tree refresh fire this tick?"
/// Folds workspace-root watch's CREATED/REMOVED events.
/// Returns the set of parent dirs needing relisting plus a
/// `git_scan` boolean.
#[drv::memo(single)]
fn workspace_tree_refresh<'a>(
    events: FileWatchEventsInput<'a>,
) -> Arc<WorkspaceRefresh>;
```

The runtime's main-loop **execute phase** is then a flat
list of memo-driven dispatches:

```rust
// Phase 3 — Execute. All inputs already gathered above.
let watch_cmds = watch_actions(desired, actual, ids);
drivers.file_watch.execute(watch_cmds.iter(), &mut atoms.file_watch);

let sync_cmds = sync_check_targets(events, hashes, persist);
drivers.session.execute(sync_cmds.iter());
for c in sync_cmds.iter() {
    if let SessionCmd::CheckSync { path, .. } = c {
        trace.workspace_check_sync(path);
    }
}

let reread_paths = external_reread_targets(events, buffers);
let reread_cmds: Vec<FileReadCmd> = reread_paths.iter()
    .map(|p| FileReadCmd::Reread { path: p.clone() }).collect();
drivers.file.execute(reread_cmds.iter());

let lsp_notifs = lsp_watched_file_notifications(events, globs);
drivers.lsp.execute(lsp_notifs.iter());

let refresh = workspace_tree_refresh(events);
if refresh.git_scan { atoms.git_scan_pending = true; }
for parent in refresh.dirs.iter() {
    atoms.dir_listing_pending.insert(parent.clone());
}
```

No `pending_external_reread`/`pending_sync_check` fields on
the runtime. The events themselves live on `FileWatchState.
recent_events` (driver-owned source) and the dispatch is the
**diff** the memos compute. After the execute phase, the
runtime calls `file_watch_state.recent_events.clear()` so
each event drives at most one tick of dispatches — same
pattern legacy used for `Mut`-as-replace.

#### (4) Main-loop wiring

The four phases (G4) M26 touches:

- **Ingest** — `drivers.file_watch.process(&mut atoms.
  file_watch)` drains `FileWatchEvent`s into
  `FileWatchState.recent_events`. `drivers.session.process(
  ...)` ingests `SyncResult` events (existing pattern; one
  new arm). `drivers.file.process(...)` ingests
  `RereadDone` events. **Invariant enforcement** (per
  EXAMPLE-ARCH § "Invariant enforcement"): on each
  `RereadDone`, the runtime executes the three-branch
  reconcile (clean / dirty / hash-match) inline, mutating
  the user-decision shadow source `EditedBuffer.rope`. This
  is application logic in the ingest phase, exactly as the
  arch doc describes for cleaning up user state in
  response to external facts.
- **Query** — the seven memos above.
- **Execute** — the flat list shown above.
- **Render** — unchanged. `body_model` reads `eb.rope` as
  always; the reload's mutation is invisible to the painter
  beyond a re-render of the new content.

#### (5) Driver wiring

`Drivers.file_watch: FileWatchDriver` +
`_file_watch_native: FileWatchNative`. Spawned alongside
existing drivers; uses the shared `Notifier` to wake the
main loop on event arrival. Drop order (per G12): the sync
`FileWatchDriver` field declared *before* `_file_watch_native`,
so the `Sender` drops first and the worker self-exits on
channel hangup. Native handle's `Drop` does **not** call
`join()`.

`Atoms` gains exactly two new fields:

```rust
pub file_watch: FileWatchState,
pub watch_id_seq: u64,  // monotonically-allocated WatcherId source
```

Plus the existing-crate touches: `state-lsp` gains
`LspWatchedGlobs`. No other source struct changes.

Startup wiring runs as soon as `fs.root` lands and
`session.init_done` flips true: the `desired_watch_set`
memo's first non-empty result yields three Watch commands
(ROOT recursive, NOTIFY_DIR non-recursive 100ms debounce,
plus per-buffer parent dirs for any tabs already open from
session restore). The first execute phase dispatches them
all; subsequent ticks are cache-hits unless the buffer set
moves.

- **`FileReadCmd::Reread`** — the existing read driver gains
  a sibling variant. Same disk-read path; the difference is
  in how the runtime ingests the result.

  ```rust
  // crates/driver-buffers/core/src/lib.rs — addition
  pub enum FileReadCmd {
      // existing Read { path } …
      /// Re-read on watcher-driven external change. Result
      /// arrives as `FileReadEvent::RereadDone { path,
      /// result }` so the runtime can branch its
      /// reconcile logic separately from initial open.
      Reread { path: CanonPath },
  }

  pub enum FileReadEvent {
      // existing ReadDone { path, result } …
      RereadDone { path: CanonPath, result: ReadResult },
  }
  ```

  No new worker thread; the existing read worker handles
  both variants. Trace: new
  `Trace::file_reread_start(&CanonPath)`, mirrors existing
  `file_load_start` ("`FileReread\tpath=<p>`"). Quiet by
  default — none of the existing M26-gated goldens contain
  a `FileReread` line because the reload is re-traced as
  the post-reload `WorkspaceFlushUndo` (the one new undo
  entry the reload produces).

- **External-change reconcile** — application logic in the
  **ingest** phase (per EXAMPLE-ARCH § "Invariant
  enforcement"). Runs on each `FileReadEvent::RereadDone`
  arrival, mutating the user-decision shadow source
  `EditedBuffer.rope` in response to the external fact (new
  disk content). Three branches mirroring
  `docs/spec/buffers.md` § "External filesystem change":

  ```rust
  match (eb.dirty(), new_hash == eb.disk_content_hash) {
      (false, false) => {
          // Clean reload. Replace rope, refresh
          // disk_content_hash, generate one undo group so
          // Ctrl-/ takes the user back to the prior content.
          eb.history.push_group(EditGroup::full_replace(
              prev_rope.clone(), new_rope.clone()));
          eb.rope = Arc::new(new_rope);
          eb.disk_content_hash = new_hash;
          eb.version = eb.version.saturating_add(1);
          eb.saved_version = eb.version; // stays clean
      }
      (true, false) => {
          // Dirty + content diverges from saved baseline.
          // Legacy parity: silently drop. Future: surface
          // a non-blocking alert "shared.txt changed on
          // disk; your edits are kept."
      }
      (_, true) => {
          // Hash matches our `disk_content_hash` — this is
          // our own save echoing back, or a peer wrote
          // identical bytes. No-op.
          if eb.dirty() {
              // The "externally saved" branch from legacy:
              // the user typed, then someone else saved a
              // version identical to our in-memory rope.
              // Treat as save-for-us — the buffer is
              // already clean by the hash test, so just
              // ensure saved_version catches up.
              eb.saved_version = eb.version;
          }
      }
  }
  ```

  The `EditGroup::full_replace` shape is already supported by
  the rope-edit primitives (delete-everything + insert) — it
  just re-uses the existing op log. The version bump cascades
  through the FlushUndo debounce, which produces the
  `WorkspaceFlushUndo` line that the
  `smoke/external_change` golden expects. The notify-touch
  side-effect of the flush then triggers the
  `WorkspaceCheckSync` on the same buffer (self-echo,
  classified as `NoChange` at the driver, dropped at the
  runtime).

- **`SessionEvent::SyncResult` ingest** — adds an arm to the
  existing session-event drain (`runtime/src/lib.rs:1129
  +/-`). Like the reconcile above, this is **application
  logic in the ingest phase** (G7 — clean up user state in
  response to an external fact: peer-applied undo entries).
  The `pending_external_reread` mismatch branch is expressed
  by **synthesising a `FileWatchEvent::Changed { kinds:
  MODIFIED }` into `FileWatchState.recent_events`** so the
  next Query phase's `external_reread_targets` memo picks it
  up — keeping a single end-state source of truth for "what
  paths need a reread", rather than a parallel pending-set:

  ```rust
  SessionEvent::SyncResult { kind } => match kind {
      SyncResultKind::SyncEntries {
          path, chain_id, content_hash, entries, new_last_seen_seq,
      } => {
          let Some(tracker) = undo_persistence.get_mut(&path) else { continue };
          let Some(eb) = edits.buffers.get_mut(&path) else { continue };
          if tracker.chain_id != chain_id
              || eb.disk_content_hash != content_hash
          {
              // Mismatch: queue a synthetic reread event into
              // FileWatchState.recent_events; next tick's
              // external_reread_targets memo emits the Reread.
              file_watch.synthesize_reread(&path);
              continue;
          }
          apply_remote_entries(eb, &entries);
          tracker.last_seq = new_last_seen_seq;
          tracker.persisted_len += entries.len();
      }
      SyncResultKind::ExternalSave { path } => {
          file_watch.synthesize_reread(&path);
      }
      SyncResultKind::NoChange { .. } => { /* drop */ }
  }
  ```

  `apply_remote_entries` lives next to the existing
  `apply_undo_chain` helper in `runtime/src/lib.rs`; same
  shape, walking each group's `EditOp` against the rope.

- **LSP `workspace/didChangeWatchedFiles`** — **SHIPPED
  alongside the M26 core, 2026-04-27.** Originally deferred
  at ship time because none of the six M26-gated goldens
  exercise the path; landed in the same commit window once
  the file-watch substrate stabilised. The
  [`M26-FOLLOWUP-LSP.md`](M26-FOLLOWUP-LSP.md) breakdown
  reflects the as-shipped wiring; the design below remains
  the authoritative spec.

  Registration payload lands on `state-lsp`'s
  `LspWatchedGlobs` source; the runtime's
  `compute_lsp_watched_file_notifications` helper (above)
  computes the per-tick fan-out. New scenario
  `goldens/scenarios/features/lsp/did_change_watched_files`
  exercises the end-to-end path.

  - **`driver-lsp/native/classify.rs`** — extend the
    existing `client/registerCapability` arm. Today it
    forwards as a notification with no payload extraction
    (`crates/driver-lsp/native/src/classify.rs:159`). M26
    parses the `registrations[].registerOptions.watchers`
    array when the method is
    `workspace/didChangeWatchedFiles`, compiles each glob
    via `globset`, and emits a typed
    `LspEvent::WatchedFilesRegistered { server, globs:
    Arc<Vec<RegistrationGlob>> }` to the runtime. Symmetric
    `Unregister` path emits `WatchedFilesUnregistered {
    server, registration_id }`.
  - **Runtime ingest** of `LspEvent::WatchedFilesRegistered`
    writes `LspWatchedGlobs.by_server` directly. Per G1
    this is the external-fact landing site (the server told
    us); the runtime owns no ingest logic beyond the field
    assignment.
  - **`LspCmd::DidChangeWatchedFiles { server: String,
    changes: Vec<FileEvent> }`** — new variant in
    `driver-lsp/core` (per G13: ABI types in driver core).
    The native side formats it as the LSP notification and
    sends.
  - `RegistrationGlob` itself lives in `state-lsp` (it is
    parsed from the LSP message but the parsed form is
    state, not an ABI type — same shape as `LspStatus`'s
    decomposition of `initialize` results). The driver-lsp/
    native parsers depend on `state-lsp` for the type, the
    same way the LSP driver already does for other parsed
    server responses.
  - Trace: new
    `Trace::lsp_did_change_watched_files(server, n)` — one
    line per dispatched notification.
  - `globset = "0.4"` added to workspace deps if not already
    present (it is — `driver-file-search` uses it).

- **Trace additions** (`crates/runtime/src/trace.rs`):
  ```rust
  pub trait Trace: Send + Sync {
      // existing methods …
      /// Per-buffer cross-instance sync probe. Emitted once
      /// per dispatched `SessionCmd::CheckSync` cmd produced
      /// by the `sync_check_targets` memo. Legacy line:
      /// `WorkspaceCheckSync\tpath=<p>`.
      fn workspace_check_sync(&self, path: &CanonPath);
      /// Per-buffer reread on external change. Quiet by
      /// default; included for parity with `file_load_start`.
      fn file_reread_start(&self, path: &CanonPath);
      /// Per LSP did-change-watched-files dispatch.
      fn lsp_did_change_watched_files(
          &self, server: &str, n_changes: usize,
      );
  }
  ```

  `FileTrace` formats them as:
  ```
  WorkspaceCheckSync\tpath=<p>
  FileReread\tpath=<p>
  LspDidChangeWatchedFiles\tserver=<name> changes=<n>
  ```

  The `WorkspaceCheckSync` shape is fixed by the existing
  goldens (e.g. `goldens/scenarios/smoke/external_change/
  dispatched.snap:7`).

- **Goldens harness** — already supports `fs_write` /
  `fs_delete` script commands
  (`goldens/src/scenario.rs:130, 141`); no harness change
  needed. The 1.5 s baseline wait inside `fs_write` for
  FSEvents propagation
  (`goldens/src/lib.rs:339-350`) already accommodates
  the watcher round-trip latency.

  One new harness primitive: the goldens runner needs the
  ability to **spawn a sibling led process** for the
  cross-instance scenarios. Out of M26 scope (the existing
  `features/editing/type_delete_reflow` scenario doesn't
  need a sibling — its `WorkspaceCheckSync` is the
  self-echo from its own `WorkspaceFlushUndo`). Truly
  cross-instance scenarios (the
  `driver_events/workspace/sync_entries` / `sync_external_save`
  variants flagged in `docs/drivers/workspace.md` § "Goldens
  checklist") are deferred until a follow-up adds a
  `sync-flush` script command.

### Out

Per the roadmap (`ROADMAP.md` M26 entry) and the explicit
deferrals in the spec docs:

- **External-delete UX surfaces (alert / tab close).** Legacy
  silently keeps the buffer open with stale content
  (`docs/spec/buffers.md` § "External delete is inert. The
  model ignores it"). M26 matches. A follow-up can add an
  `Alert::Warn("file deleted on disk")` and / or auto-close
  when the buffer is clean. Filed as the
  M26-followup `external_remove_ux` orphan.

- **Surfaced "external change while dirty" alert.** Legacy
  silently drops the on-disk content. M26 matches. Future
  polish: a non-blocking warn alert "`<file>` changed on
  disk; your unsaved edits are kept" plus an explicit
  `Action::ReloadFromDisk` to opt into discarding local
  edits. Filed as the M26-followup
  `external_change_dirty_alert` orphan.

- **Recursive watch on Linux for newly-created subdirs.**
  notify's inotify backend doesn't auto-watch new
  subdirectories. The workspace recursive-watch fires for
  the parent dir's Create event; M26 re-registers there.
  This catches the common case ("create a subfolder, drop a
  file in"). Edge case: a deeply-nested mkdir batch can
  outrun the re-registration. Filed as
  `linux_recursive_subdir_race`; not flaky in practice for
  developer-paced workspace mutations.

- **Cross-instance two-process goldens.** The
  `goldens/scenarios/driver_events/workspace/sync_entries/`
  / `sync_external_save/` / `sync_no_change/` slots
  flagged in `docs/drivers/workspace.md` need a
  `sync-flush <path>` script command that writes directly to
  the SQLite undo tables to simulate a peer. The
  self-echo path (our own `FlushUndo` → notify-touch →
  `CheckSync`) covers the same dispatch surface for the M26
  goldens that exist today; the two-process variants are a
  separate harness / fake-led story.

- **`driver-fs-list` consolidation.** `docs/drivers/fs.md` §
  "Translation to query arch" suggests collapsing
  `driver-fs-list` and `driver-file-watch` into one fs
  subsystem. M26 keeps them separate — `fs-list`'s
  command-shaped read interface and `file-watch`'s
  event-shaped input role have different lifetimes and
  different testing surfaces. Refactor at our convenience
  later; not required for the goldens.

- **Configurable debounce.** The 100 ms notify-dir debounce
  + the implicit FSEvents coalescing both stay hardcoded.
  When (if) a user reports "my changes take too long to
  show up", a `[file_watch]` config section can expose them.

- **Watch-event tracing in `dispatched.snap`.** Watch events
  are input-side; tracing them would add per-keystroke
  noise on platforms where the editor's own writes echo back
  through the watcher. Kept silent in goldens. The
  `Trace::file_watch_event` debug hook exists for verbose
  investigation builds.

- **LSP file-watch payload (`changes` array shape) optimisation.**
  M26 sends one notification per change. LSP allows
  batching `Vec<FileEvent>` into a single
  `workspace/didChangeWatchedFiles` call. Performance fine for
  typical edit rates; revisit if an LSP server complains.

- **`session_kv` (browser + jump list) cross-instance sync.**
  Out — those are write-on-quit-only; no per-edit sync
  story.

- **Tab-stop config / theme reload from `theme.toml`.** M26
  doesn't subscribe `theme.toml` to the watcher; reloads
  remain on next-launch. A separate theme-watch milestone
  (or `Action::ReloadTheme`) could land later.

## Architecture conformance

M26 is audited against every guideline in `EXAMPLE-ARCH.md`:

- **G1 — External facts vs user decisions in separate
  sources.** The new external-fact source is
  `FileWatchState` (driver-file-watch-owned). The new
  external-fact source on the LSP side is `LspWatchedGlobs`
  on `state-lsp` (not on the runtime crate's `Atoms`
  bag). The user-decision shadow source affected by M26 is
  `EditedBuffer.rope`; its seeding protocol from disk
  (clean reload) is documented above per the EXAMPLE-ARCH
  § "Shadow sources" guidance.

- **G2 — Queries describe desired state, not transitions.**
  Every dispatch site goes through a memo:
  `desired_watch_set`, `watch_actions`, `notify_hash_index`,
  `external_reread_targets`, `sync_check_targets`,
  `lsp_watched_file_notifications`, `workspace_tree_refresh`.
  No "on watcher event do X" handlers. Reconnection,
  buffer churn, and LSP server restart all fall out of the
  memos returning the diff against actual state.

- **G3 — Execute writes intent synchronously.**
  `FileWatchDriver::execute(cmds, &mut state)` writes
  intent into `FileWatchState.registry` for each `Watch` /
  `Unwatch` *before* sending the async `notify::Watcher::
  watch` call. So the next tick's `desired_watch_set` ==
  `actual` and `watch_actions` returns
  `Arc::clone(empty_vec)` — cache-hit, no double-register.

- **G4 — Main loop is ingest → query → execute → render.**
  Phase boundaries kept clean: ingest writes to
  `FileWatchState.recent_events` and to user shadow sources
  on `RereadDone`; query computes the seven memos; execute
  dispatches the four driver command lists; render
  unchanged. After execute, `recent_events.clear()` so
  events drive at most one tick.

- **G5 — `imbl` collections.** `FileWatchState.registry`,
  `.recent_events`, `LspWatchedGlobs.by_server` all use
  `imbl::HashMap`. Memo outputs use `Arc<Vec<...>>` /
  `Arc<imbl::HashSet<...>>` so cache-hit clones are
  pointer copies.

- **G6 — One memo per view.** No new view models in M26 —
  the body/status/sidebar models already exist and consume
  refreshed atom content automatically.

- **G7 — Clean up stale user decisions in ingest.** The
  three-branch external-change reconcile runs in the
  ingest phase (per the worked example's "Invariant
  enforcement" pattern) — it cleans up
  `EditedBuffer.rope` (user-decision shadow source) in
  response to disk content (external fact) changing.
  Similarly, the `SessionEvent::SyncResult` ingest arm
  applies remote entries to the user-decision history.

- **G8 — Drivers ignorant of each other.**
  `driver-file-watch/core` imports `led_core::CanonPath`
  only — no state-* crate, no other driver-* crate. The
  LSP fan-out is **runtime-mediated**: the file-watch
  driver does not import `driver-lsp-core`, and
  `driver-lsp` does not import `driver-file-watch-core`.
  Both are reachable only from `runtime/`.

- **G9 — Crate boundaries enforce ignorance.** The
  `Cargo.toml`s for `driver-file-watch/{core,native}`
  list only `led-core`, `notify`, `bitflags`,
  `imbl`. The compiler rejects accidental coupling.

- **G10 — Portable core + platform native.**
  `driver-file-watch/core` is the portable sync API +
  source + ABI types. `driver-file-watch/native` owns the
  `notify::Watcher` thread.

- **G10a — One native crate per platform, not cfg.** M26
  ships `driver-file-watch/native` (single native, like
  the existing `driver-git/native`). When iOS/Android
  targets land, those become `native-ios` / `native-android`
  siblings. The desktop native uses `notify`'s
  `RecommendedWatcher` which already handles macOS /
  Linux / Windows internally — that's the "small leaf
  differences inside an otherwise-shared native" exception
  EXAMPLE-ARCH explicitly allows.

- **G11 — Consumer declares inputs.** All
  `#[derive(drv::Input)]` projections (`FileWatchEventsInput`,
  `OpenBuffersInput`, `WorkspaceRootInput`,
  `UndoPersistInput`, `LspGlobsInput`,
  `NotifyHashIndexInput`, `WatchIdSeqInput`,
  `DesiredWatchInput`) are declared in `runtime/src/query.rs`
  alongside the memos that consume them. The
  `driver-file-watch/core` and `state-lsp` crates do not
  depend on `drv`.

- **G12 — Don't `join()` natives in `Drop`.**
  `FileWatchNative` carries `_marker: ()` only, like the
  existing `SessionNative`. The worker self-exits on
  channel hangup when the sync `FileWatchDriver`'s sender
  drops. `Drivers` field declaration order ensures the
  sender drops first.

- **G13 — `platform-*` tier criterion.** File-watch is
  **not** promoted. The criterion is "multiple domain
  drivers issue requests to it". Only `runtime/` issues
  `FileWatchCmd`s; the LSP driver never imports
  `driver-file-watch-core`. Stays as `driver-*`. ABI
  types (`WatcherId`, `FileWatchCmd`, `FileWatchEvent`,
  `ChangeKinds`, `BackendStatus`, `Registration`) live in
  `driver-file-watch/core` per the second G13 entry. The
  `LspCmd::DidChangeWatchedFiles` variant + `FileEvent`
  type live in `driver-lsp/core`. `RegistrationGlob` lives
  in `state-lsp` (parsed-state, not ABI-crossing).

- **G14 — Zero alloc on idle.** Walk-through:
  - `FileWatchDriver::process(state)` returns `()`; on
    idle ticks the worker channel is empty, no
    `recent_events` mutation.
  - All seven memos cache-hit on idle (every input is
    pointer-equal: `imbl::HashMap` clones are refcount
    bumps, `Arc<Vec>` outputs are likewise). The diff
    memos return `Arc::clone` of cached empty/identical
    vectors.
  - `FileWatchState.registry` is `imbl::HashMap` — the
    next memo reading it gets a pointer-equal clone.
  - `LspWatchedGlobs.by_server` is `imbl::HashMap<String,
    Arc<Vec<RegistrationGlob>>>` — same shape.
  - The reconcile path runs only on `RereadDone` arrival;
    idle ticks have no `FileReadEvent::RereadDone` events
    in the channel.

  No idle allocation in any new code path.

## Key design decisions

### D1 — One driver, multiple watch intents (`WatcherId`)

Three distinct watch consumers (workspace root, notify dir,
per-buffer parent dir, plus LSP-server-supplied globs)
could each have their own driver. They don't because:

- All four sit on top of the same `notify::Watcher` handle —
  splitting drivers would require either four watchers
  (fd cost; on macOS two FSEvents streams compete for the
  same kernel queue) or a shared watcher hidden behind a
  Mutex (worse than just owning it).
- The 100 ms debounce + per-id pending map is uniform; the
  only per-intent variation is `recursive` / `debounce_ms` /
  glob-filter, which fit naturally as `Watch` parameters.
- A `WatcherId` lets the runtime add and drop registrations
  cheaply when a buffer opens or closes. No driver-side
  refcounting on paths; just an id-keyed `HashMap<WatcherId,
  RegInfo>`.

### D2 — Reload re-uses `FileReadDriver`, not a new path

Two reasons:

1. The existing read worker is the canonical disk-IO path.
   Adding a separate "external-reread" path means two places
   that turn raw bytes into `Rope` + `PersistedContentHash`,
   two places to keep the encoding handling consistent.
2. The trace shape stays familiar:
   `FileReread\tpath=<p>` matches `FileLoad\tpath=<p>`.

The cost: a new `FileReadCmd::Reread` variant and a
`FileReadEvent::RereadDone` reply variant, distinguished from
initial open. Worth the duplication — the reconcile branch
on `RereadDone` is fundamentally different (initial open
materialises a buffer; reread reconciles against an existing
rope).

### D3 — Three-branch reconcile is `match (dirty, hash)`

The legacy code spreads the three branches across a
multi-line `if/else` that re-reads `eb` fields each time.
M26 keeps the same logic but expresses it as a
`match (eb.dirty(), new_hash == eb.disk_content_hash)`.
Cleaner; the three legitimate combinations (clean+changed,
dirty+changed, hash-equal-anything) map to three arms.

The four-way truth table is degenerate: `(false, true)` =
no-op (we already saw this hash) and `(true, true)` = the
"externally saved by an instance writing identical bytes"
branch from legacy. Both fold into the `(_, true)` arm
because the action is the same: clear `dirty` if it was set,
otherwise no-op.

### D4 — External-change reload generates an undo group

The user expectation: hitting `Ctrl-/` after an external
reload should restore the prior content. Without an undo
group, the rope just teleports forward, and the prior
state is lost. So the reload synthesises one
`EditGroup::full_replace { from: prev_rope, to: new_rope }`
and pushes it through `eb.history`. The next FlushUndo
debounce serialises it to SQLite, so reopening the file
preserves the undo stack — exactly the behaviour M21
documented for cross-session undo.

The `EditGroup::full_replace` shape is a sugar over
`vec![Delete { at: 0, text: prev_rope.to_string() },
Insert { at: 0, text: new_rope.to_string() }]`. The op log
already supports it. New helper, two lines.

### D5 — External-change while dirty is silent (legacy parity)

`docs/spec/buffers.md` is explicit: the legacy buffer
silently drops the external write. M26 matches the contract.
The user's edits are protected; the cost is no UI signal
that the disk-side changed underneath.

A future polish (the `external_change_dirty_alert` orphan)
adds a non-blocking `Alert::Warn` and an `Action::Reload`
that explicitly discards local edits. Out of scope here so
the goldens stay tight and the surface to verify is
minimal.

### D6 — `notify_hash_index` is a derived memo, not an atom

We need two indices on the same set of open buffers:

- **Path → hash** for writing the touch file
  (`<config>/notify/<hash>`), used by the session driver
  in its `FlushUndo` / `ClearUndo` arms — computed at
  the touch site via `led_core::path_hash(path)`.
- **Hash → path** for reverse-mapping an inbound
  `FileWatchEvent::Changed { id: NOTIFY_DIR, path:
  <config>/notify/<hash> }` back to the buffer it
  belongs to.

Both derive from `edits.buffers` keys + the
deterministic `path_hash` function. Per G2 the reverse
index is a **memo** (`notify_hash_index`) returning
`Arc<HashMap<String, CanonPath>>`, not an atom field
written at buffer-open time. The memo cache-hits when
the buffer set is stable (G14); the `Arc` wrapping makes
input projection a refcount bump.

### D7 — `CheckSync` debounce is in the driver, not the runtime

Three reasons:

1. The 100 ms debounce window applies to the **filesystem
   watcher** (notify can fire Modify+Create within the same
   tick); coalescing inside the driver keeps the runtime's
   ingest loop from seeing multiple events per touch.
2. The `sync_check_targets` memo deduplicates per path
   inside the same Query phase — multiple events on the
   same hash collapse to one `CheckSync` cmd.
3. The `WorkspaceCheckSync` trace fires per dispatch, so
   the goldens see one line per unique path-touch even if
   FSEvents fired five Modify events.

### D8 — Server's globs live on `state-lsp`, matching runs as a runtime memo

Per G1, the LSP server's registered globs are an **external
fact** (chosen by the server, not the user). They belong on
the LSP driver's external-fact source — `state-lsp`'s new
`LspWatchedGlobs` — alongside other LSP server state.
*Not* on the runtime's `Atoms` bag.

The driver-lsp/native parser writes globs in via the
existing `LspEvent` channel; the runtime's
`lsp_watched_file_notifications` memo reads
`LspGlobsInput` + `FileWatchEventsInput` and produces the
fan-out `Vec<LspCmd::DidChangeWatchedFiles>`. Cross-driver
coupling stays one-way: file-watch driver → runtime → LSP
driver. No file-watch ↔ LSP direct wire.

### D9 — No new SQLite schema; reuse `buffer_undo_state` + `undo_entries`

`CheckSync` reads the same tables `FlushUndo` writes. No
new columns, no schema bump, no migration. The state-row
absence (`SELECT chain_id FROM buffer_undo_state ... = 0
rows`) is the `ExternalSave` signal; the entries-after-seq
query is the `SyncEntries` payload. Mirrors legacy exactly.

### D10 — `FileWatchCmd::Watch` is idempotent

Re-issuing `Watch` for an already-watched (id, path) pair
is a no-op. Lets the runtime over-register without
bookkeeping: `BufferOpen` always fires `Watch` for the
parent; if the parent is already covered by the recursive
root watch, the per-buffer watch overlaps but the driver
de-dupes internally. Zero correctness cost, simpler caller
side.

### D11 — Shutdown ordering: drop watch-driver first

The Drop order on `Drivers` already runs the sync driver
core first (sender) then native (joiner). M26 puts
`driver-file-watch` ahead of `driver-session` because
notify-dir touches happen on `FlushUndo` / `ClearUndo`
which fire on `Shutdown`-equivalent paths; if the watcher
were still alive when the session driver issued its
final touch, we'd self-trigger a CheckSync against a
shutting-down session driver. Sequence:

```
Drivers fields drop in declared order:
  file_watch                   (sender → native joins)
  _file_watch_native           (worker exits)
  …
  session                      (sender; final FlushUndo + ClearUndo land)
  _session_native              (worker exits)
```

## Types

### `led-core` additions

```rust
// crates/core/src/lib.rs
pub fn path_hash(path: &CanonPath) -> String;
```

### `state-tabs` / `state-buffer-edits`

No state-shape changes. `EditedBuffer.disk_content_hash`,
`.dirty()`, `.history` are M26's external-change anchors and
already exist post-M21.

### `state-session`

No struct shape change. `SessionState.last_seq` /
`chain_id` already live on `UndoPersistTracker` in
`runtime/src/lib.rs:336-349`.

### `driver-file-watch/core` (new crate)

```rust
pub struct WatcherId(pub u64);
pub enum FileWatchCmd { Watch { … }, Unwatch { id } }
pub enum FileWatchEvent { Changed { … }, Failed { … } }
pub struct ChangeKinds(u8);  // bitflags
pub trait Trace: Send + Sync { … }
pub struct FileWatchDriver { … }
```

### `driver-file-watch/native` (new crate)

```rust
pub struct FileWatchNative { _marker: () }
pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier)
    -> (FileWatchDriver, FileWatchNative);
```

### `driver-session/core` additions

```rust
pub enum SessionCmd {
    // existing …
    CheckSync { path, last_seen_seq, current_chain_id },
}
pub enum SessionEvent {
    // existing …
    SyncResult { kind: SyncResultKind },
}
pub enum SyncResultKind {
    SyncEntries { path, chain_id, content_hash, entries, new_last_seen_seq },
    ExternalSave { path },
    NoChange { path },
}
```

### `driver-buffers/core` additions

```rust
pub enum FileReadCmd {
    Read { path },                        // existing
    Reread { path },                      // NEW
}
pub enum FileReadEvent {
    ReadDone { path, result },            // existing
    RereadDone { path, result },          // NEW
}
```

### `driver-lsp/core` additions

```rust
pub enum LspCmd {
    // existing …
    DidChangeWatchedFiles {
        server: String,                  // server name; runtime fans out per-server
        changes: Vec<FileEvent>,
    },
}
pub struct FileEvent {
    pub uri: String,                     // file:// URI
    pub kind: FileEventKind,             // Created | Changed | Deleted
}
```

### `runtime/src/lib.rs::Atoms`

```rust
pub struct Atoms {
    // existing …
    /// Driver-owned external-fact source for the file-watch
    /// driver. Owns the actual-side of the desired/actual
    /// diff that produces FileWatchCmd commands.
    pub file_watch: FileWatchState,
    /// Monotonic id allocator for new WatcherId values.
    /// Mutated only when `desired_watch_set` introduces a
    /// path that hasn't been seen this session.
    pub watch_id_seq: u64,
}
```

No new pending-flag fields. The dispatch derivations live
on memos (G2); the in-flight state lives on
`FileWatchState.registry` per the EXAMPLE-ARCH "stateless
drivers still need an in-flight source" rule.

## Crate changes

```
crates/
  core/
    src/lib.rs                      + pub fn path_hash(...)
  driver-file-watch/                NEW crate pair
    core/
      Cargo.toml                    bitflags, imbl, led-core
      src/lib.rs                    Cmd / Event / Driver handle
      src/state.rs                  FileWatchState source struct
    native/
      Cargo.toml                    notify, led-driver-file-watch-core, led-core
      src/lib.rs                    spawn + worker loop + debounce
  driver-buffers/core/src/lib.rs    + Reread variant on Cmd / Event
  driver-buffers/native/src/lib.rs  + Reread arm in worker
  driver-session/core/src/lib.rs    + CheckSync, SyncResult, SyncResultKind
  driver-session/native/src/lib.rs  + CheckSync arm + touch_notify_file in
                                    FlushUndo / ClearUndo arms
  driver-lsp/core/src/lib.rs        + LspCmd::DidChangeWatchedFiles, FileEvent
  driver-lsp/native/src/classify.rs Extend client/registerCapability arm to
                                    parse workspace/didChangeWatchedFiles
                                    payload and emit
                                    LspEvent::WatchedFilesRegistered
  state-lsp/src/lib.rs              + LspWatchedGlobs, RegistrationGlob
  runtime/
    Cargo.toml                      + globset (if not already present)
    src/lib.rs                      + Drivers.file_watch + _file_watch_native
                                    + Atoms.file_watch + Atoms.watch_id_seq
                                    + ingest arms (FileWatchEvent drain,
                                       SessionEvent::SyncResult,
                                       FileReadEvent::RereadDone with
                                       three-branch reconcile)
                                    + execute phase: emit watch_actions,
                                       sync_check_targets,
                                       external_reread_targets,
                                       lsp_watched_file_notifications,
                                       workspace_tree_refresh
                                    + apply_remote_entries helper
                                    + recent_events.clear() at end of
                                       execute phase
    src/query.rs                    + FileWatchEventsInput,
                                       OpenBuffersInput,
                                       WorkspaceRootInput,
                                       UndoPersistInput,
                                       LspGlobsInput,
                                       NotifyHashIndexInput,
                                       WatchIdSeqInput,
                                       DesiredWatchInput
                                    + memos: notify_hash_index,
                                       desired_watch_set, watch_actions,
                                       external_reread_targets,
                                       sync_check_targets,
                                       lsp_watched_file_notifications,
                                       workspace_tree_refresh
    src/trace.rs                    + workspace_check_sync,
                                       file_reread_start,
                                       lsp_did_change_watched_files
goldens/scenarios/
  smoke/external_change/             (existing — moves green)
  edge/external_change_while_dirty/  (existing — moves green)
  edge/external_delete_open_file/    (existing — moves green)
  driver_events/docstore/external_change/ (existing — moves green)
  driver_events/workspace/workspace_changed/ (existing — moves green)
  features/editing/type_delete_reflow/ (existing — moves green)
```

New workspace members:

```toml
"crates/driver-file-watch/core",
"crates/driver-file-watch/native",
```

With workspace-dep aliases `led-driver-file-watch-core`,
`led-driver-file-watch-native`.

## Testing

### `core::path_hash` (unit)

- Same path → same hash; round-trip with the legacy
  `<config>/primary/<hash>` shape (16-char lowercase hex).
- Different paths → different hashes (brute-force a few
  fixtures; collision odds astronomical).

### `driver-file-watch/core` (unit)

- `execute` forwards a batch into the channel.
- `process` returns events in arrival order.
- `ChangeKinds` bitset combine works
  (`CREATED | MODIFIED == 0b011`).

### `driver-file-watch/native` (integration-style)

Spawn against a tempdir.

- `Watch { recursive: false }` on a dir, write a sibling
  file → one `Changed { kinds: CREATED|MODIFIED }`.
- `Watch { recursive: true }` on a dir, mkdir + write inside
  → one `Changed` for the parent + one for the file.
- `Unwatch` then write — no further events.
- `Watch` with `debounce_ms: 100`, write the same file 5
  times within 50 ms — one `Changed` after the quiet
  window, with `kinds` = union of all observed.
- `Watch` for the same `(id, path)` twice — second is a
  no-op (idempotency).
- Inert fallback: in CI without inotify (containerised),
  the driver constructs a no-op watcher. Verify by feature-
  gating a `with_inert_fallback()` constructor.

### `driver-session/native` (integration-style)

- After `FlushUndo`, the touch file
  `<config>/notify/<path_hash>` exists with
  `metadata().len() == 0`.
- After `ClearUndo`, the touch file is updated again
  (stat mtime moves).
- `CheckSync` with `last_seen_seq` behind the latest entry
  → `SyncResultKind::SyncEntries { entries: vec![…],
  new_last_seen_seq: latest_seq }`.
- `CheckSync` with `last_seen_seq == latest_seq` and
  matching `current_chain_id` → `NoChange`.
- `CheckSync` after the buffer's row was deleted (post-save
  clear) → `ExternalSave`.
- `CheckSync` against a workspace whose DB was opened with
  `primary == false` (secondary) — same SELECT-only path,
  works regardless of primary state.

### `runtime::query` memos (unit, pure-function tests)

- `notify_hash_index` — round-trip with `path_hash`: every
  open buffer's `path_hash(path)` exists as a key in the
  output and resolves to the same path.
- `desired_watch_set` — empty open-buffers + `Some(root)` →
  exactly two registrations (ROOT, NOTIFY_DIR). With one
  open buffer → three registrations including its parent.
  Adding a buffer in a parent that's already covered by
  ROOT is still recorded as a per-buffer registration
  (driver-side dedupe).
- `watch_actions` — desired == actual → empty
  `Arc<Vec>` (cache-hit on the empty case is the idle
  path, G14). One desired-only id → `Watch` cmd. One
  actual-only id → `Unwatch` cmd.
- `external_reread_targets` — recent_events with a
  per-buffer parent watch + MODIFIED matching an open
  buffer → set contains that path. REMOVED → no entry
  (legacy parity). Sibling-file event (path not in open
  buffers) → no entry.
- `sync_check_targets` — NOTIFY_DIR event whose basename
  hashes back to an open buffer → one CheckSync cmd with
  the right (chain_id, last_seen_seq). Hash with no open
  buffer → no cmd.
- `lsp_watched_file_notifications` — recent event for
  `Cargo.toml` + server with `**/*.toml` registered → one
  `DidChangeWatchedFiles` cmd targeting that server.
  Event for `foo.rs` + same server → no cmd. Two servers
  with non-overlapping globs → fan-out targets each
  separately.
- `workspace_tree_refresh` — ROOT watch event with
  CREATED → output has `git_scan: true` + the parent dir
  in `dirs`. MODIFIED-only ROOT event → no refresh
  (Modify suppression).

### `runtime::reconcile_external_change` (unit, ingest-phase)

- Clean + new content → rope replaced, version bumps,
  `disk_content_hash` updated, one new `EditGroup` in
  history (G7 invariant enforcement).
- Dirty + new content → no rope change, no version bump,
  no history entry (legacy parity silent drop).
- Hash matches existing → `dirty` cleared if it was set;
  no rope change.

### `runtime::SessionEvent::SyncResult` ingest (unit)

- `SyncEntries` with matching chain + hash → entries
  applied to rope, `tracker.last_seq` updated,
  `tracker.persisted_len` advanced.
- `SyncEntries` with chain mismatch → synthetic
  `FileWatchEvent::Changed { kinds: MODIFIED }` queued
  into `FileWatchState.recent_events`; entries dropped.
- `SyncEntries` with hash mismatch → same synthetic-event
  fallback.
- `ExternalSave` → synthetic-event fallback.
- `NoChange` → no atom change.

### LSP `WatchedFilesRegistered` ingest (unit)

- `client/registerCapability` with method
  `workspace/didChangeWatchedFiles` and one watcher glob
  `**/*.toml` → `LspWatchedGlobs.by_server[server].len()
  == 1`, glob compiled.
- `Unregister` for the same registration id →
  `by_server[server]` is removed.
- Two registrations from the same server with different
  ids → `by_server[server].len() == 2`.

### Integration

Unit tests cover the wire shape; the goldens validate the
end-to-end. The six M26-gated goldens are the contract:

- `smoke/external_change` — open a file, externally
  rewrite it; trace ends with `WorkspaceFlushUndo` +
  `WorkspaceCheckSync` (the reload's new undo group + the
  self-echo CheckSync). Frame shows the new content,
  cursor at L1:C1, no dirty marker.
- `edge/external_change_while_dirty` — type local edits
  (triggers `WorkspaceFlushUndo` + self-echo
  `WorkspaceCheckSync`), then externally rewrite. Frame
  preserves the local edits + dirty marker; trace gains
  no further lines past the workspace-changed
  `FsListDir` + `GitScan` (because the dirty branch is
  silent).
- `edge/external_delete_open_file` — open a file,
  externally delete it. Trace shows two
  `FsListDir` + one `GitScan` after delete (the
  workspace-tree-changed refresh path); buffer stays
  open (legacy parity, no `*` marker).
- `driver_events/docstore/external_change` — same script
  as `smoke/external_change`, captured under a different
  cluster. Trace identical.
- `driver_events/workspace/workspace_changed` — open `a.txt`,
  externally write `fresh.txt`. Sidebar gains `fresh.txt`;
  trace adds `FsListDir` + `GitScan`.
- `features/editing/type_delete_reflow` — pure self-echo
  scenario. User types + Ctrl-q reflows; the reflow's
  edits trigger `WorkspaceFlushUndo`; the FlushUndo's
  notify-touch triggers self-echo `WorkspaceCheckSync`.
  No fs_write in the script.

Expected delta: ~30 unit tests, six existing goldens move
to green.

## Done criteria

- All existing tests pass (unit + integration).
- All new tests green.
- `cargo clippy --all-targets`: net delta ≤ +3 from
  post-M25.
- Goldens (single-threaded baseline):
  - `smoke/external_change` — green (was failing).
  - `edge/external_change_while_dirty` — green (was failing).
  - `edge/external_delete_open_file` — green (was failing).
  - `driver_events/docstore/external_change` — green (was
    failing).
  - `driver_events/workspace/workspace_changed` — green
    (was failing).
  - `features/editing/type_delete_reflow` — green (was
    failing).
  - No regressions in any other suite. Goldens total moves
    from 255/9 to 261/3 (the three remaining failures are
    the pre-existing `wait_ready` harness flakes).
- Interactive smoke:
  - `cd /tmp/test && echo hi > foo.txt && cargo run -p led
    -- foo.txt`. In a sibling shell:
    `echo external > foo.txt`. Within ~3 s the rewrite's
    body shows `external`, no dirty marker, cursor at
    L1:C1.
  - Same setup, type local edits in led, then
    `echo from-disk > foo.txt`. The local edits stay; no
    visible UI signal. (Future polish: alert.)
  - `rm foo.txt` while led has it open. Buffer stays open;
    sidebar (if showing the parent) refreshes and `foo.txt`
    disappears from the listing.
  - In a `cargo` workspace with rust-analyzer attached:
    `cargo add anyhow` from a sibling shell. Within ~5 s
    rust-analyzer's diagnostics for the buffer reflect the
    new dependency (the `didChangeWatchedFiles` arrived).
  - Two-terminal: `led` in A and B on the same workspace.
    Edit `foo.rs` in B. Within ~1 s A's view of `foo.rs`
    shows the same edits.
- `GOLDEN-TODO.md` updated: total moves from 255/9 to
  261/3, M26 entry added under "What's solid",
  Cluster B (M26-gated) cleared.
- `ROADMAP.md` updated: M26 marked SHIPPED.

## Growth-path hooks

- **External-delete UX** — `external_remove_ux` orphan.
  When the user-visible signal lands, the dispatch
  handler is the same `pending_external_remove` set; the
  reducer adds an `Alert::Warn` and (optionally) auto-
  closes clean tabs.
- **External-change-while-dirty alert + reload action** —
  `external_change_dirty_alert` orphan. Reducer adds
  `Alert::Warn("X changed on disk; your edits are kept")`;
  `Action::Reload` discards local edits and applies the
  reread.
- **Two-process cross-instance goldens** — adds a
  `sync-flush <path> <entries>` script command to the
  goldens harness that writes directly to the SQLite
  `undo_entries` table to simulate a peer. Unblocks
  `driver_events/workspace/sync_entries`,
  `sync_external_save`, `sync_no_change`.
- **Configurable debounce + watch-tuning** — a
  `[file_watch]` config section exposing
  `notify_debounce_ms`, `external_change_debounce_ms`.
- **Recursive new-subdir watch on Linux** — the inotify
  re-registration on `Created`-of-a-dir fires today; a
  follow-up could add a brief 50 ms post-create scan to
  catch deeply-nested batches.
- **`fs-list` + `file-watch` consolidation** — collapse
  the two crates into one `driver-fs/` per `docs/drivers/
  fs.md`. Refactor; no behaviour change.
- **LSP file-watch payload batching** — coalesce multiple
  `FileEvent`s into one `workspace/didChangeWatchedFiles`
  per server per dispatch tick. Reduces wire chatter for
  bulk fs operations.
- **Theme + keymap hot-reload** — subscribe `theme.toml`
  and `config.toml` to the watcher. Reload `Atoms.theme`
  / `Atoms.keymap` on Modified.
- **Mouse / drag-and-drop file open** — when mouse-input
  lands, drop-events for files in the workspace can use
  the same notify infra to detect "freshly created file"
  and offer a quick-open.
