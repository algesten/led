# Driver: docstore

## Purpose

The docstore is the resource driver for the full open/save lifecycle of user
files. It is the **only** component in led that reads or writes user documents.
Given a `DocStoreOut::Open { path, create_if_missing }`, it reads the file
asynchronously, constructs a `TextDoc` (a Rope‑backed document) from the bytes,
and emits `DocStoreIn::Opened { path, doc }`. Given a `DocStoreOut::Save` or
`SaveAs`, it serializes the in‑memory `Doc` to a tmpfile in the parent
directory, fsync‑renames into place, and replies with `Saved` / `SavedAs`. In
addition to the request/response surface, it registers a non‑recursive watch on
the parent directory of every path it has successfully opened so that external
editors (another editor, `git checkout`, a linter) trigger `ExternalChange` or
`ExternalRemove` events that the model can reconcile against the in‑memory
buffer.

See `crates/docstore/src/lib.rs:13-31` for the command enum,
`crates/docstore/src/lib.rs:33-61` for the response enum, and
`crates/docstore/src/lib.rs:100-200` for the driver loop.

## Lifecycle

- **Start**: spawned in `led/src/lib.rs` during driver wiring. The driver
  receives a `Stream<DocStoreOut>` (the out‑stream) and an
  `Arc<FileWatcher>` (shared with the workspace driver). It immediately spawns
  two `tokio::task::spawn_local` tasks: one consumes the mpsc command channel
  in a `tokio::select!` loop alongside the shared watcher channel, the other
  bridges the result channel back onto the `rx::Stream` (`crates/docstore/src/lib.rs:117-197`).
- **Stop**: when the out‑stream mpsc is dropped, the `cmd_rx.recv()` returns
  `None` and the select loop breaks, terminating both spawned tasks. There is
  no explicit shutdown handshake; in practice led exits by dropping the
  runtime.
- **Ordering requirements**: the watcher registration must happen **before**
  the file is read so that a rapid external write that lands between our
  read and the next poll is not missed. The current code registers first,
  then emits `Opening`, then reads (`crates/docstore/src/lib.rs:130-139`).
- **`--no-workspace`**: docstore runs identically in standalone mode. It has
  no workspace dependency and uses the shared `FileWatcher` in its normal
  mode (which falls back to an inert no‑op if the platform watcher is
  unavailable). Parent directories outside any workspace root are still
  watched.

## Inputs (external → led)

1. **User document contents on disk** — read via `tokio::fs::read` on `Open`,
   read again on every `WatchEventKind::Create|Modify` for a watched path
   (`crates/docstore/src/lib.rs:90-93` and `:366-378`).
2. **Shared `FileWatcher`** (notify‑crate backed, wrapped in
   `led_core::FileWatcher`) — delivers `WatchEvent { kind, paths }` whenever
   any file in a registered parent directory is created, modified, or removed.
   Docstore owns per‑parent `Registration` handles in a `HashMap<CanonPath,
   Registration>` (`crates/docstore/src/lib.rs:120`) and a
   `HashSet<CanonPath>` of individual watched files used to filter watcher
   events (`crates/docstore/src/lib.rs:123`).

## Outputs from led (model → driver)

Values of `DocStoreOut` (`crates/docstore/src/lib.rs:13-31`):

| Variant                                                  | What it causes                                                                                                          | Async? | Returns via                                            |
|----------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------|--------|--------------------------------------------------------|
| `Open { path, create_if_missing }`                       | Register parent‑dir watch, emit `Opening`, read disk, emit `Opened` or (if read fails and `create_if_missing=false`) `OpenFailed` | yes    | `DocStoreIn::Opening`, then `Opened`/`OpenFailed`      |
| `Save { path, doc }`                                     | Serialize doc to tmpfile `.led-save-<pid>` in parent dir, `fs::rename` atomic replace                                   | yes    | `DocStoreIn::Saved` on success, `Err(Alert::Warn)` on I/O failure |
| `SaveAs { path, doc, new_path }`                         | Same as Save but writes to `new_path`; also swaps `watched_paths` entry (removes old, inserts new) and registers `new_path` parent | yes    | `DocStoreIn::SavedAs` on success, `Err(Alert::Warn)` on failure |

Dispatchers in `led/src/derived.rs`:

- `materialize` stream (`led/src/derived.rs:208-231`): walks
  `s.tabs` and emits `Open` for every tab whose buffer is `Unmaterialized`.
  Uses `b.mark_requested()` interior mutability to prevent re‑dispatch before
  the reducer lands (see Materialization feedback in MEMORY).
- `save_out` / `save_all_out` streams
  (`led/src/derived.rs:234-261`): triggered by `s.save_request` /
  `s.save_all_request` versioned fields and emit `Save` for every dirty
  materialized buffer with `save_in_flight() == true`.
- `save_as_out` (`led/src/derived.rs:264-278`): triggered by
  `s.pending_save_as`, emits `SaveAs` for the active tab.

## Inputs to led (driver → model)

Every variant is wrapped in `Result<DocStoreIn, Alert>`. `Alert` is used for
save‑path I/O failures. All matches below are `Ok(...)` unless marked
otherwise.

| Variant                              | Cause                                                                 | Frequency                       | Consumed in                                                                                                                                         |
|--------------------------------------|-----------------------------------------------------------------------|---------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------|
| `Opening { path }`                   | Immediate ack at top of async open handler, before the disk read      | per `Open` request              | `led/src/model/buffers_of.rs:164` — explicitly dropped. Currently inert: no consumer uses it to flip state. Deliberately kept so the model *could* distinguish "open accepted" from "open lost" if that becomes useful. |
| `Opened { path, doc }`               | Read completed for `Open`                                              | per file open                   | `buffers_of.rs:18-91` → `Mut::BufferOpen` — materializes the buffer, applies restored cursor/scroll from `state.session.positions`, attaches persisted undo history if `doc.content_hash()` matches the saved hash, decides `activate` based on startup args / session order. |
| `Saved { path, doc }`                | `fs::rename` succeeded on `Save`                                       | per save (C‑x C‑s)              | `buffers_of.rs:92-106` → `Mut::BufferSaved` — calls `buf.save_completed(doc)`, and if the buffer had `save_in_flight()` set, emits `undo_clear_path` so the workspace driver wipes persisted undo. |
| `SavedAs { path, doc }`              | `fs::rename` succeeded on `SaveAs` (path is the **new** path)         | per save‑as (C‑x C‑w)           | `buffers_of.rs:107-126` → `Mut::BufferSavedAs` — renames the buffer, swaps tab key, optionally triggers `undo_clear_path` for the old path.        |
| `ExternalChange { path, doc }`       | Parent‑dir watcher fired Create/Modify for a watched file; driver re‑reads and constructs fresh `doc` | per external edit of an open file | `buffers_of.rs:127-163` — three branches: (a) content hash unchanged + buffer was dirty without local edits → `mark_externally_saved`; (b) buffer has local edits → silently dropped; (c) hash changed → `reload_from_disk(doc)`. |
| `ExternalRemove { path }`            | Parent‑dir watcher fired Remove for a watched file                    | per external delete             | `buffers_of.rs:165` — **dropped with `None`**. Event fires but model currently ignores it. See "Edge cases" below. |
| `OpenFailed { path }`                | Read failed and `create_if_missing=false`                              | per failed open                 | `buffers_of.rs:166` → `Mut::SessionOpenFailed` — session‑restore path for a file that no longer exists: removes tab + buffer, marks resume entry `Failed`. |
| `Err(Alert::Warn(msg))`              | Save path failed at any step (parent‑dir create, serialize, tmpfile write, rename)                    | rare                            | `buffers_of.rs:167` → `Mut::alert(a)` — surface in the alert row.                                                                                  |

## State owned by this driver

Internal to the async task (not in `AppState`):

- `registrations: HashMap<CanonPath, Registration>` — one `Registration`
  handle per watched parent directory. Cloning the mpsc sender into
  `FileWatcher::register` increments the per‑path refcount; dropping the
  `Registration` decrements. Docstore never removes entries, so once a parent
  is watched it stays watched for the life of the process (acceptable: on
  save‑as the parent of the **new** path is added; the old parent's
  registration lingers but causes no harm because `watched_paths` is used as
  the per‑file filter).
- `watched_paths: HashSet<CanonPath>` — set of canonical paths docstore has
  successfully opened. The watcher fires on the whole parent directory, so
  this set filters events down to files we actually care about
  (`crates/docstore/src/lib.rs:352-357`).
- `cmd_rx / result_rx / watcher_rx` — three mpsc channels that form the
  select loop's three arms.

## External side effects

- **Filesystem reads**: `tokio::fs::read(path)` on every `Open`; re‑read on
  every Create/Modify watcher event for a watched path.
- **Filesystem writes**: `tokio::fs::write(tmpfile, bytes)` + `tokio::fs::rename`
  on every `Save` and `SaveAs`. On error the tmpfile is removed with
  `tokio::fs::remove_file` to avoid leaking `.led-save-<pid>` files.
- **Directory creation**: `tokio::fs::create_dir_all(parent)` on save when
  the parent does not exist (new files in new dirs).
- **Watcher registration**: `FileWatcher::register(parent, NonRecursive, tx)`
  on first open per parent directory.

## Known async characteristics

- **Latency**:
  - `Open`: dominated by `tokio::fs::read` — typically sub‑ms for small files,
    bounded by disk on large ones. Rope construction is in‑memory and fast.
  - `Save`: tmpfile write + rename — two syscalls; typically sub‑ms.
  - `ExternalChange`: watcher coalescing varies by platform (FSEvents on
    darwin is <3 ms per MEMORY; inotify is similar).
- **Ordering**: the driver processes commands strictly serially in a single
  `tokio::select!` loop. Two rapid `Save` commands for the same file are
  processed in order. However the **watcher arm** of the select can
  interleave with the command arm: a `Save` that triggers a watcher event
  will be followed by that event being delivered before any subsequent
  command. The model sees `Saved` then `ExternalChange` in temporal order.
- **Cancellation**: none. An in‑flight `Save` cannot be cancelled; a second
  `Save` queues.
- **Backpressure**: both mpsc channels are sized 64
  (`crates/docstore/src/lib.rs:105-106`). The command bridge uses
  `try_send().ok()` so if the channel is full the command is silently
  dropped. In practice 64 is well above typical edit rates. The watcher
  channel is 256.

## Translation to query arch

| Current behavior                             | New classification                                                                 |
|----------------------------------------------|------------------------------------------------------------------------------------|
| `DocStoreOut::Open`                          | `Request::OpenBuffer { path, create_if_missing }`, result `Event::BufferOpened` or `Event::BufferOpenFailed` |
| `DocStoreOut::Save`                          | `Request::SaveBuffer { path, doc }`, result `Event::BufferSaved` or `Event::SaveFailed` |
| `DocStoreOut::SaveAs`                        | `Request::SaveBufferAs { path, doc, new_path }`, result `Event::BufferSavedAs` or `Event::SaveFailed` |
| Parent‑dir watcher → `ExternalChange`        | Input driver → `Event::FileExternallyChanged { path, doc }` (can be consolidated into the `fs` input driver) |
| Parent‑dir watcher → `ExternalRemove`        | Input driver → `Event::FileExternallyRemoved { path }`                             |
| `DocStoreIn::Opening` ack                    | Drop entirely — query arch uses `Request::is_pending` for that signal              |

The external‑change detection is already filesystem‑driven and duplicates
infrastructure the `fs` driver already needs. In the rewrite, this should be
consolidated into a single fs‑watching subsystem.

## State domain in new arch

- `Request::OpenBuffer` result lands as `Loaded<Doc>` in `BufferState` keyed
  by path. The cursor / scroll / undo attachment currently performed by
  `buffers_of.rs` is a **derived transformation** that runs when the
  `Loaded<Doc>` appears and a matching `SessionPosition` is present in
  `SessionState`.
- `Request::SaveBuffer` result updates `BufferState.save_point` and clears
  `dirty`.
- `Event::FileExternallyChanged` is transient and applied to `BufferState`
  via a reducer that re‑runs the three‑branch logic currently in
  `buffers_of.rs:127-163`.

## Versioned / position‑sensitive data

Docstore outputs `Arc<dyn Doc>` — whole documents. They are **not**
version‑sensitive against buffer edits because they represent the disk
state, which is a world parallel to the in‑memory buffer. The reconciliation
(`content_hash` comparison in `buffers_of.rs:137-149`) is the version check.

`doc.content_hash()` is a `PersistedContentHash(u64)` computed from Rope
content and stored in `BufferState` after every edit. It is used as the
key linking persisted undo history to buffer content (see
`crates/workspace/src/db.rs:66-73`, `buffer_undo_state.content_hash`). When
`ExternalChange` arrives with a matching hash but the buffer is dirty, that
means another instance saved a version identical to our in‑memory state —
we mark the buffer externally‑saved.

## Edge cases and gotchas

- **Docstore does not deduplicate.** Pushing the same `Open` three times
  produces three `Opened` events (see the test
  `duplicate_opens_each_produce_opened` at `crates/docstore/src/lib.rs:427`).
  Deduplication is the model's responsibility: `buffers_of.rs:26-32` checks
  `state.buffers.get(&path).is_some_and(|b| b.is_materialized())` and drops
  the second `Opened`. The query rewrite must preserve this invariant — the
  Request dispatcher should check `is_pending(Request::OpenBuffer{path})` /
  `BufferState::is_materialized`.
- **Stale preview opens.** `buffers_of.rs:21-23`: if `Opened` arrives for a
  path that is no longer in any tab (user closed the preview before the disk
  read completed), the result is dropped with no state change.
- **`ExternalRemove` is inert.** The model ignores it
  (`buffers_of.rs:165`). The tab is not closed, the buffer is not evicted.
  The user discovers the deletion only when they try to save. This is
  arguably a bug; the rewrite should decide whether to preserve this
  behaviour or remove the tab.
- **Watcher event for a file we just wrote.** After `Save`, the watcher
  will fire Create/Modify for our own write. Docstore doesn't suppress this
  (no debouncing or fingerprinting). The model reconciles via
  `content_hash`: the re‑read doc has the same hash as the freshly‑saved
  buffer, so `buffers_of.rs:137-149` takes the "externally‑saved" branch
  only if the buffer is dirty and has no local edits — otherwise dropped.
- **Save‑as parent mismatch.** `SaveAs` returns `DocStoreIn::SavedAs { path
  }` where `path` is the **new** path. `buffers_of.rs:110-123` reconstructs
  the old path from the active buffer — this means `SaveAs` handling
  assumes the active tab has not changed between dispatch and response.
  In practice the response latency is sub‑ms; in the query arch we should
  carry both paths in the event.
- **`save_in_flight()` gate.** Only buffers where the `BufferState` has
  `save_in_flight() == true` are dispatched by `save_all_out`
  (`led/src/derived.rs:253-258`). The single `save_out` path doesn't check
  this and saves whatever is active. The `save_in_flight` flag is also the
  trigger for the post‑save `undo_clear` dispatch.
- **FileWatcher "inert mode".** `led_core::FileWatcher` falls back to a
  no‑op watcher on platforms where `notify` is unavailable. In that mode
  `register()` returns a `Registration` but no events are ever delivered —
  `ExternalChange`/`ExternalRemove` simply never fire.
- **Save path is atomic but not fsync'd.** The tmpfile write is followed by
  rename but no `File::sync_all` / fsync. Acceptable for an editor; power
  loss between write and metadata flush can lose a save. Noted for
  faithful translation.

## Goldens checklist

Under `goldens/scenarios/driver_events/docstore/` we have (per the existing
directory listing):

- `opened/` — natural via any CLI arg scenario (smoke `open_empty_file`).
- `saved/` — natural via type + C‑s (smoke `type_and_save`).
- `saved_as/` — needs find‑file/save‑as dialog. Status: [unclear — scenario
  exists directory; verify it actually exercises save‑as].
- `external_change/` — needs mid‑test fs write mechanism
  (`fs-write <path> <content>` script command). See extract doc
  `/Users/martin/dev/led/docs/extract/driver-events.md:131`.

Missing / to add:

- `opening/` — currently inert, arguably not needed unless the rewrite
  reintroduces the ack.
- `external_remove/` — event fires but model ignores. Golden should
  demonstrate the no‑op.
- `open_failed/` — needs a session entry that references a path deleted
  before spawn; or a pre‑seeded session DB mechanism.
- `save_error/` (`Err(Alert::Warn)`) — needs a read‑only target directory
  mechanism (`chmod -w` on parent dir before save).
- `duplicate_open_deduplicated/` — dispatcher issues two Opens; asserts
  only one buffer materializes (the model‑layer dedupe).
- `save_then_watcher_noop/` — after our own save, the watcher fires but
  `content_hash` matches, so no `Mut::BufferUpdate`.
- `external_change_while_dirty/` — buffer has local edits, watcher event
  is dropped (branch at `buffers_of.rs:150-153`).

[unclear — whether current runner supports mid‑test fs mutation script
commands or if they need to be built as part of this rewrite.]
