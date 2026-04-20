# Milestone 4 — saving

The fourth vertical slice. After M3 the rewrite binary accepts edits
and shows a `*` on modified buffers; after M4 `Ctrl-S` flushes the
active buffer to disk and clears the dirty indicator.

Prerequisite reading (diff vs MILESTONE-3.md):

1. `MILESTONE-3.md` — the editing baseline and `BufferEdits` shape
   this milestone extends.
2. `../../../drv/EXAMPLE-ARCH.md` § "The execute pattern" — the
   sync-write-intent-then-spawn discipline that's applied a second
   time here.

---

## Goal

```
$ cargo run -p led -- Cargo.toml
# edit a buffer — tab bar shows *Cargo.toml
# Ctrl-S → file written, tab bar loses the asterisk
# further edits → * comes back; Ctrl-S clears it again
# save failure (e.g. read-only file) → trace logs err; * stays
```

## Scope

### In
- `Ctrl-S` on the active tab requests a save of that buffer.
- New `FileWriteDriver` (sync core in `driver-buffers/core/`, async
  worker in `driver-buffers/native/`) handling atomic writes via
  tmp-file + rename.
- `BufferEdits` grows `pending_saves: HashSet<CanonPath>` (the set
  of paths the user has requested save for) and each `EditedBuffer`
  replaces `dirty: bool` with `saved_version: u64`; `dirty()`
  derives from `version > saved_version`.
- Query `file_save_action` diffs pending-saves against dirty
  buffers, emitting `SaveAction { path, rope, version }`.
- Runtime clears `pending_saves` entries **synchronously** as part
  of the execute phase so the next tick's query returns an empty
  action list (mirrors M1's file-load sync-write-intent pattern).
- Ingest: `FileWriteDriver::process()` returns a list of
  completions. On success: update `BufferStore.loaded` to the saved
  rope and bump `EditedBuffer.saved_version`. On failure: trace an
  error line and leave the buffer dirty.
- Trace additions: `file_save_start | path=... version=N`,
  `file_save_done | path=... version=N ok=<bool> [err=...]`.

### Out

Each item links to its scheduled milestone in `ROADMAP.md`:

- **Save-All** (`ctrl+x ctrl+a`) → M6. Adds the `SaveAll` command;
  dispatch inserts every dirty path into `pending_saves`.
- **Undo / save history** → M8 (edit log).
- **User-facing error surface** → M9 (alert system). M4 errors land
  in the trace; M9 surfaces them in the status bar.
- **Reload from disk / external change** → M26.
- **File locking / conflict detection** → M26 as well; the watch
  driver's mtime check is the natural hook.
- **Line-ending normalisation (CRLF vs LF)** → not scheduled. Write
  verbatim. Add to roadmap if it bites.
- **BOM handling** → not scheduled. Same answer.
- **Save-As** (`ctrl+x ctrl+w`) → M12 (find-file overlay: SaveAs
  reuses that overlay).
- **Format-on-save** → M18 (LSP extras). M4 just writes the rope;
  M18 rewires save to run LSP format first when attached.

## Key design decisions

### D1 — Replace `dirty: bool` with `saved_version: u64`

M3's `dirty: bool` flips true on first edit and stays that way. M4
needs to know *what version* was last saved — so that a save
completing after a further edit leaves the buffer dirty, and a save
that races ahead of a newer edit doesn't accidentally clear the
flag.

```rust
pub struct EditedBuffer {
    pub rope: Arc<Rope>,
    pub version: u64,
    pub saved_version: u64,   // 0 == "matches disk"
}

impl EditedBuffer {
    pub fn dirty(&self) -> bool { self.version > self.saved_version }
    pub fn fresh(rope: Arc<Rope>) -> Self {
        Self { rope, version: 0, saved_version: 0 }  // matches disk
    }
}
```

On save completion for version V: `saved_version = max(saved_version, V)`. `max` is belt-and-suspenders against
out-of-order completions (sequential worker makes this impossible
in practice, but the guard is free).

`dirty` becomes a method. M3's tab-bar prefix (`*` glyph) and test
helpers switch from field access to `.dirty()`.

### D2 — `pending_saves` is a user-decision source, lives on `BufferEdits`

Saves are user-driven; a set of "paths the user has asked to save"
is user-decision state, same domain as the edits themselves. Add
it to `BufferEdits` rather than spinning up a new `state-*` crate:

```rust
pub struct BufferEdits {
    pub buffers: imbl::HashMap<CanonPath, EditedBuffer>,
    pub pending_saves: imbl::HashSet<CanonPath>,
}
```

Dispatch on `Ctrl-S` inserts the active tab's path (if dirty).
Runtime clears entries during the execute phase. Query reads both
fields through two separate projections so cache invalidation is
fine-grained (a cursor move inside `buffers` doesn't touch
`pending_saves`).

### D3 — `FileWriteDriver` lives alongside `FileReadDriver`

Both drivers touch `BufferStore`: reads populate it, writes
round-trip it (the saved rope becomes the new disk baseline).
Sharing a crate is cleaner than cross-driver imports. The existing
`driver-buffers/{core,native}` grows a second driver + its ABI
types + a merged `spawn` helper.

The strict-isolation rule remains honoured — nothing else depends
on the read or write driver; `state-buffer-edits` stays ignorant of
them.

### D4 — Atomic write via tmp + rename

The worker writes to `<dir>/.led.<unique>.tmp`, then
`std::fs::rename`s onto the target path. Power loss / crash in the
middle leaves either the old file or the new one intact — never a
partial write. Zed and most real editors do this; allocation
discipline doesn't preclude correctness.

`<unique>` is the rope's `version` — unique per buffer because the
worker is single-threaded and a new version supersedes any older
tmp for the same path. No global counter needed.

### D5 — Execute sync-clears `pending_saves` before spawning

Classic execute-pattern: without the sync write, next tick's query
would see the same pending set and emit duplicate writes. The
runtime does the sync clear (not the driver) so the driver stays
ignorant of sibling source layouts:

```rust
let save_actions = file_save_action(
    PendingSavesInput::new(&edits),
    EditedBuffersInput::new(&edits),
);
for action in &save_actions {
    edits.pending_saves.remove(&action.path);
}
drivers.file_write.execute(&save_actions);
```

### D6 — Completion round-trip updates both `BufferStore` and `EditedBuffer`

On success, the saved rope *becomes* the disk baseline, so
`BufferStore.loaded[path] = LoadState::Ready(rope)` is updated
alongside `saved_version`. This keeps the "external-fact"
invariant honest: `BufferStore` is what's on disk — and we just
wrote what's now there.

On failure, neither field is touched. The buffer stays dirty; the
user sees the `*` and can retry. The error lands in trace.

---

## Types

### `EditedBuffer` (updated in `state-buffer-edits`)

```rust
pub struct EditedBuffer {
    pub rope: Arc<Rope>,
    pub version: u64,
    pub saved_version: u64,
}

impl EditedBuffer {
    pub fn dirty(&self) -> bool { self.version > self.saved_version }
    pub fn fresh(rope: Arc<Rope>) -> Self {
        Self { rope, version: 0, saved_version: 0 }
    }
}
```

### `BufferEdits` (updated)

```rust
pub struct BufferEdits {
    pub buffers: imbl::HashMap<CanonPath, EditedBuffer>,
    pub pending_saves: imbl::HashSet<CanonPath>,
}
```

### `FileWriteDriver` (new in `driver-buffers/core`)

```rust
pub enum SaveAction {
    Save { path: CanonPath, rope: Arc<Rope>, version: u64 },
}

pub enum WriteCmd {
    Write { path: CanonPath, rope: Arc<Rope>, version: u64 },
}

pub struct WriteDone {
    pub path: CanonPath,
    pub version: u64,
    pub result: Result<Arc<Rope>, String>,
}

pub struct FileWriteDriver {
    tx_cmd: Sender<WriteCmd>,
    rx_done: Receiver<WriteDone>,
    trace: Arc<dyn Trace>,
}

impl FileWriteDriver {
    pub fn execute(&self, actions: &[SaveAction]);
    pub fn process(&self) -> Vec<WriteDone>;
}
```

The driver's `Trace` grows:

```rust
pub trait Trace: Send + Sync {
    // existing read events
    fn file_load_start(&self, path: &CanonPath);
    fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>);
    // new write events
    fn file_save_start(&self, path: &CanonPath, version: u64);
    fn file_save_done(&self, path: &CanonPath, version: u64, result: &Result<(), String>);
}
```

The unified runtime `Trace` + adapter pick up the new methods.

### Query projections + memo

```rust
#[drv::input]
#[derive(Copy, Clone)]
pub struct PendingSavesInput<'a> {
    pub paths: &'a imbl::HashSet<CanonPath>,
}

#[drv::memo(single)]
pub fn file_save_action<'p, 'b>(
    pending: PendingSavesInput<'p>,
    buffers: EditedBuffersInput<'b>,
) -> Vec<SaveAction> {
    pending.paths.iter()
        .filter_map(|path| {
            let eb = buffers.buffers.get(path)?;
            if !eb.dirty() { return None; }
            Some(SaveAction::Save {
                path: path.clone(),
                rope: eb.rope.clone(),
                version: eb.version,
            })
        })
        .collect()
}
```

`PendingSavesInput` projects only the save-request set — a cursor
move doesn't invalidate this memo's cache.

On idle ticks `pending.paths` is empty, the memo returns `Vec::new()`
(no heap alloc), and execute's for-loop is a no-op — allocation
discipline holds.

### Dispatch — `Ctrl-S`

```rust
(m, KeyCode::Char('s')) if m.contains(KeyModifiers::CONTROL) => {
    request_save_active(tabs, edits);
    DispatchOutcome::Continue
}

fn request_save_active(tabs: &Tabs, edits: &mut BufferEdits) {
    let Some(id)  = tabs.active else { return };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else { return };
    let Some(eb)  = edits.buffers.get(&tab.path) else { return };
    if eb.dirty() {
        edits.pending_saves.insert(tab.path.clone());
    }
}
```

Ctrl-S on a clean buffer is a no-op. Ctrl-S before the buffer
loaded is a no-op (the `get` misses). Both correct.

### Runtime — execute + ingest

```rust
// --- Ingest ---
for completion in drivers.file_write.process() {
    match completion.result {
        Ok(rope) => {
            // Round-trip: the saved rope is now the disk baseline.
            store.loaded.insert(
                completion.path.clone(),
                LoadState::Ready(rope),
            );
            if let Some(eb) = edits.buffers.get_mut(&completion.path) {
                eb.saved_version = eb.saved_version.max(completion.version);
            }
        }
        Err(_msg) => {
            // Already traced inside process(); buffer stays dirty.
        }
    }
}

// --- Query + Execute (new chunk) ---
let save_actions = file_save_action(
    PendingSavesInput::new(edits),
    EditedBuffersInput::new(edits),
);
for SaveAction::Save { path, .. } in &save_actions {
    edits.pending_saves.remove(path);
}
drivers.file_write.execute(&save_actions);
```

## Crate layout

Unchanged skeleton — writes merge into `driver-buffers`:

```
crates/
  state-buffer-edits/           EditedBuffer.saved_version, dirty()
                                + BufferEdits.pending_saves
  driver-buffers/
    core/                       + FileWriteDriver, SaveAction,
                                  WriteCmd, WriteDone, Trace grows
    native/                     + write worker thread, tmp+rename
                                  atomicity, extended spawn()
  runtime/                      + file_save_action memo,
                                  PendingSavesInput, Ctrl-S branch,
                                  run() save path
```

Workspace `Cargo.toml` has no new members.

## Testing

- `state-buffer-edits` —
  - `fresh_is_clean_and_version_zero`
  - `dirty_flips_when_version_exceeds_saved_version`
  - `dirty_clears_when_saved_version_matches_current`
- `dispatch` —
  - `ctrl_s_inserts_active_path_into_pending_saves`
  - `ctrl_s_on_clean_buffer_is_noop`
  - `ctrl_s_on_unloaded_buffer_is_noop`
  - `ctrl_s_targets_only_active_tab`
- `runtime/query` —
  - `file_save_action_emits_save_for_pending_dirty_buffer`
  - `file_save_action_skips_clean_buffers`
  - `file_save_action_empty_when_nothing_pending`
- `driver-buffers/core::FileWriteDriver` — mpsc-boundary tests using
  direct channels:
  - `execute_sends_write_cmd`
  - `process_surfaces_completion`
- `driver-buffers/native` — integration test writing to a `tempfile`
  directory: `spawn → execute → await completion → verify file
  contents → verify atomic rename left no `.tmp` detritus`.

Expected: +10–12 new tests, total ≥ 72.

## Done criteria

- `cargo test` — all green.
- `cargo clippy --all-targets` warning count at baseline.
- Interactive: edit → Ctrl-S → asterisk clears → file on disk
  matches. Further edits restore the asterisk; save clears again.
- Allocation discipline: idle tick after a save completes allocates
  zero bytes (pending_saves empty, file_save_action cache-hits
  `[]`, no `execute` work).

## Growth-path hooks

- **Save-all.** A keymap entry (or command-palette command in M5+)
  that inserts every `edits.buffers` path into `pending_saves`.
- **Reload / external change detection.** Future `FileWatchDriver`
  fires `ExternalChange { path, rope }` → runtime applies to
  `BufferStore`. Merge conflict detection: compare external rope
  vs `EditedBuffer.rope`; flag user.
- **Error surfacing.** Add `edits.last_save_error:
  HashMap<CanonPath, Arc<str>>`. `tab_bar_model` could render a
  `!` prefix for errored paths.
- **LSP `textDocument/didSave`.** When LSP lands (M6+), save
  completion becomes an input for the LSP driver's action memo.
