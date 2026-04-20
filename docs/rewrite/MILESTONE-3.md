# Milestone 3 — editing

The third vertical slice. After M2 the rewrite binary moves a visible
cursor around a read-only buffer; after M3 the user can actually type,
delete, and split lines, with dirty tracking on the tab bar. Saving
stays out — that's M4.

Prerequisite reading (diff vs MILESTONE-2.md):

1. `../../../drv/EXAMPLE-ARCH.md` § "Atoms: two kinds of ground truth"
   — reinforces why the edited buffer is a **separate** source from
   the disk buffer.
2. `MILESTONE-2.md` — the cursor / scroll baseline this milestone
   extends.
3. `README.md` § "Key decisions already made" — especially the
   allocation discipline bullet; the edit hot path needs to honour it.

---

## Goal

```
$ cargo run -p led -- Cargo.toml
# type characters → they appear
# Backspace / Delete → they disappear
# Enter → line splits
# Tab bar shows a `*` next to modified buffers
# Ctrl-C still exits cleanly
```

- Printable `Char(c)` keys insert at the cursor and advance it.
- `Enter` inserts a newline and drops the cursor to column 0 of the
  new line.
- `Backspace` deletes the char before the cursor; at column 0 it
  joins with the previous line.
- `Delete` deletes the char at the cursor; at end-of-line it joins
  with the next line.
- Cursor stays in a valid buffer coordinate after every edit.
- Dirty tabs show a `*` glyph in the tab bar.
- All M1 / M2 behaviour is preserved: tab cycling, arrow movement,
  viewport scroll, resize, Ctrl-C.

## Scope

### In
- New `BufferEdits` source holding the user's edited rope per path,
  with `version` + `dirty` metadata.
- Four edit primitives in dispatch: `insert_char`,
  `insert_newline`, `delete_back`, `delete_forward`.
- Lazy seeding of `BufferEdits` entries when a buffer first lands in
  `BufferStore` as `Ready` (via a new completion list returned from
  `FileReadDriver::process`).
- Cursor clamp + scroll adjust re-use the M2 helpers against the
  **edited** rope (from `BufferEdits`), falling back to the disk
  rope (from `BufferStore`) when the buffer hasn't been edited yet.
- `body_model` reads edited content; `tab_bar_model` reads the
  `dirty` bit and renders a `*` before modified labels.
- Trace additions: `edit | kind=<insert|delete|newline> path=<p>
  version=<n>` — optional; useful for goldens later but gated on
  `--golden-trace`.

### Out

Each item links to its scheduled milestone in `ROADMAP.md`:

- **Saving to disk** → M4. `Ctrl-S` is not wired yet.
- **Undo / redo** → M8. The explicit edit log lands there — until
  then M3 stores only the materialised rope + `version` + `dirty`.
- **Clipboard + kill ring + selection** → M7.
- **Tab key for indentation** → M23 (auto-indent). Tab stays bound
  to `tab.next` in the M5 keymap.
- **Unicode-width aware column math** → M25. Column stays a char
  index; wide CJK chars and combining marks won't render correctly
  until then.
- **Smart-indent / auto-indent** → M23.
- **Line-ending preservation (CRLF vs LF)** → not scheduled; write
  whatever the rope contains verbatim. Add to roadmap if it bites.
- **Rebase queries for async data** → M16 (first LSP consumer). The
  `version` counter is the anchor those queries will walk through
  the op log introduced in M8.

## Key design decisions

### D1 — Edits live in a new `BufferEdits` source, not in `BufferStore`

`BufferStore` is the external-fact source: "what's on disk." Its load
state is driven by the file-read driver and mirrored back out on
reload. Mixing user edits into it blurs the boundary that
EXAMPLE-ARCH § "Atoms: two kinds of ground truth" calls out — we'd
lose the ability to answer "what was on disk the last time we
looked?" (needed for dirty, reload, save, diff-against-HEAD-ish
questions).

`BufferEdits` is a user-decision source. No driver, no async side —
same crate shape as `state-tabs`. Lives in
`crates/state-buffer-edits/`.

### D2 — The source stores a materialised rope, not an op log

Two viable shapes:

- **Op log:** store the sequence of edits; materialise the rope on
  demand via a memo.
- **Materialised rope:** store the current rope + a monotonically
  increasing `version`.

The materialised form is cheaper on the read side: `body_model` is
called on every input-change tick, and re-playing an op log per call
is O(edits). Even with memoisation, the first-miss cost would grow
linearly with session length. Ropey's clone-and-edit cycle is already
cheap enough (structural sharing of chunks) that materialising inline
is the right default.

The `version: u64` field doubles as both (a) a cheap invalidation
signal for memos that don't care about rope contents (dirty badge,
tab bar, status line) and (b) the anchor future rebase queries will
translate coordinates against.

A dedicated op log will be added as a sibling field when the first
consumer appears (LSP diagnostics — M6+). The version-counter
scaffold today is specifically so that addition is mechanical.

### D3 — `BufferEdits` is seeded eagerly on first load

Alternative was lazy seeding (create the entry on first keystroke).
Eager seeding wins:

- `body_model` reads only from `BufferEdits`, with no fallback logic
  — a single source of truth for "what the user sees."
- Dispatch's edit primitives never have to call "hydrate from
  BufferStore" paths; they always find a live entry (or the buffer
  isn't loaded and the keystroke is rightly a no-op).
- Cursor movement already consults the rope; using `BufferEdits`
  uniformly avoids special-casing "edited vs not."

Seeding happens in the runtime ingest phase: `FileReadDriver::process`
now returns the list of newly-`Ready` paths, and the runtime inserts
a clean `EditedBuffer { rope, version: 0, dirty: false }` into
`BufferEdits` for each. On true idle ticks the returned list is
empty — no allocation.

### D4 — Dispatch signature grows, not Event

The existing `dispatch(ev, tabs, store, terminal)` grows to
`dispatch(ev, tabs, edits, store, terminal)`. Edit handling is
sync, mutates a single user-decision source, and needs no new event
variant. This is consistent with how M2 added cursor — no event
plumbing, just mutation.

### D5 — Cursor math works against the edited rope

After edits, the rope extent (line count, line lengths) changes; the
M2 cursor helpers already clamp against whatever rope they're given.
Dispatch wires them to the **edited** rope. There's no separate
"disk cursor" — the user operates on the current view.

An invariant worth writing down: `tab.cursor` always points at a
valid position in the edited rope. Edit primitives maintain this
invariant themselves (insert advances, delete retreats, newline
splits) — no post-hoc clamp is needed inside dispatch.

### D6 — Dirty is a bool, not a comparison

`dirty: bool` on `EditedBuffer` flips to `true` on the first edit
and stays there until save (M4) or reload (later). Cheaper than
comparing rope-to-disk on every query, and matches how real editors
surface the bit to the UI.

If the user does an edit and then undoes it back to the original
content, we still say "dirty" — that's what most editors do, and
it's correct until we have an explicit undo feature tracking the
disk anchor.

---

## Types

### `state-buffer-edits` crate

```rust
use imbl::HashMap;
use led_core::CanonPath;
use ropey::Rope;
use std::sync::Arc;

/// User-decision source: the edited view of each open buffer.
///
/// Seeded from [`BufferStore`] by the runtime when a file finishes
/// loading. Mutated only by dispatch, in response to character /
/// deletion / newline keypresses.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BufferEdits {
    pub buffers: HashMap<CanonPath, EditedBuffer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditedBuffer {
    /// Current rope = disk base + all user edits applied.
    pub rope: Arc<Rope>,
    /// Monotonically increasing; bumped on every edit. Cheap
    /// invalidation key for memos that don't need the rope itself
    /// (dirty badge, tab bar), and the anchor future rebase queries
    /// will translate coordinates against.
    pub version: u64,
    /// `false` while rope == disk base; flips to `true` on the first
    /// edit. Reset by save (M4) / reload (later).
    pub dirty: bool,
}
```

`BufferEdits` carries no driver — it's a user-decision source. Same
shape as `state-tabs`.

### `FileReadDriver` — process returns completions

```rust
// crates/driver-buffers/core/src/lib.rs
pub struct LoadCompletion {
    pub path: CanonPath,
    pub rope: Arc<Rope>,
}

impl FileReadDriver {
    pub fn process(&self, store: &mut BufferStore) -> Vec<LoadCompletion> {
        let mut out = Vec::new();   // empty on idle → no heap alloc
        while let Ok(done) = self.rx_done.try_recv() {
            self.trace.file_load_done(&done.path, &done.result);
            let entry = match &done.result {
                Ok(rope) => {
                    out.push(LoadCompletion { path: done.path.clone(), rope: rope.clone() });
                    LoadState::Ready(rope.clone())
                }
                Err(msg) => LoadState::Error(Arc::new(msg.clone())),
            };
            store.loaded.insert(done.path, entry);
        }
        out
    }
}
```

`Vec::new()` doesn't allocate until first `push`; on idle ticks with
no completions this stays zero-alloc.

### Query: `body_model` prefers edits

A new `#[drv::input]` projects `BufferEdits.buffers`:

```rust
// runtime/src/query.rs
#[drv::input]
#[derive(Copy, Clone)]
pub struct EditedBuffersInput<'a> {
    pub buffers: &'a imbl::HashMap<CanonPath, EditedBuffer>,
}

impl<'a> EditedBuffersInput<'a> {
    pub fn new(e: &'a BufferEdits) -> Self { Self { buffers: &e.buffers } }
}
```

`body_model` grows the parameter and reads edits first:

```rust
#[drv::memo(single)]
pub fn body_model<'e, 'a, 'b>(
    edits: EditedBuffersInput<'e>,
    store: StoreLoadedInput<'a>,
    tabs:  TabsActiveInput<'b>,
    dims:  Dims,
) -> BodyModel {
    let Some(id)  = *tabs.active else { return BodyModel::Empty };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else { return BodyModel::Empty };
    let path_display = ...;

    if let Some(eb) = edits.buffers.get(&tab.path) {
        return render_content(&eb.rope, tab.cursor, tab.scroll, dims);
    }
    // Fallback to store: covers the small window between FileReadDone
    // and runtime seeding. Also covers Pending / Error paths.
    match store.loaded.get(&tab.path) {
        None | Some(LoadState::Pending) => BodyModel::Pending { path_display },
        Some(LoadState::Error(msg))     => BodyModel::Error { path_display, message: Arc::<str>::from(msg.as_str()) },
        Some(LoadState::Ready(rope))    => render_content(rope, tab.cursor, tab.scroll, dims),
    }
}
```

In steady state (buffer loaded + seeded) the `edits` branch wins.
The `Ready` branch in the fallback stays for the sub-millisecond
gap between `FileReadDone` and the next ingest tick.

### Query: `tab_bar_model` shows dirty

```rust
#[drv::input]
#[derive(Copy, Clone)]
pub struct DirtyFlagsInput<'a> {
    pub buffers: &'a imbl::HashMap<CanonPath, EditedBuffer>,
}

#[drv::memo(single)]
pub fn tab_bar_model<'a, 'b>(
    tabs: TabsActiveInput<'a>,
    dirty: DirtyFlagsInput<'b>,
) -> TabBarModel {
    let labels: Vec<String> = tabs.open.iter().map(|t| {
        let base = t.path.file_name()
            .map(|os| os.to_string_lossy().into_owned())
            .unwrap_or_else(|| t.path.display().to_string());
        let d = dirty.buffers.get(&t.path).map(|b| b.dirty).unwrap_or(false);
        if d { format!("*{base}") } else { base }
    }).collect();
    ...
}
```

(A `format!`-per-tab on recompute isn't ideal under the allocation
discipline — but it only fires when an input actually changes, and
the `Arc<Vec<String>>` wrapper keeps cache-hit clones cheap. An
alternative would be making `TabBarModel::labels` hold `(String,
bool)` tuples and letting paint handle the prefix; revisit if tab
bar recomputes become a measurable cost.)

## Dispatch

### Signature

```rust
pub fn dispatch(
    ev:       Event,
    tabs:     &mut Tabs,
    edits:    &mut BufferEdits,
    store:    &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome;
```

### Key handling

```rust
match (k.modifiers, k.code) {
    // Existing M1/M2 branches...
    (m, KeyCode::Char(c))
        if m.is_empty() || m == KeyModifiers::SHIFT =>
    {
        insert_char(tabs, edits, c);
    }
    (m, KeyCode::Enter) if m.is_empty() => insert_newline(tabs, edits),
    (m, KeyCode::Backspace) if m.is_empty() => delete_back(tabs, edits),
    (m, KeyCode::Delete)    if m.is_empty() => delete_forward(tabs, edits),
    _ => DispatchOutcome::Continue,
}
```

Ctrl-`Char(c)` branches (Ctrl-C quit) stay above the plain `Char`
match-arm and take precedence.

### Edit primitives

All four take `&mut Tabs` + `&mut BufferEdits`, resolve the active
tab, locate its edit entry, clone-and-mutate the rope, bump version,
mark dirty, update the cursor. None of them need `&BufferStore` —
if the edit entry doesn't exist (file hadn't finished loading) they
no-op.

```rust
fn with_active<F>(tabs: &mut Tabs, edits: &mut BufferEdits, f: F)
where
    F: FnOnce(&mut Tab, &mut EditedBuffer),
{
    let Some(id)  = tabs.active else { return };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else { return };
    let tab = &mut tabs.open[idx];
    let Some(eb) = edits.buffers.get_mut(&tab.path) else { return };
    f(tab, eb);
}

fn bump(eb: &mut EditedBuffer, new_rope: Rope) {
    eb.rope = Arc::new(new_rope);
    eb.version = eb.version.saturating_add(1);
    eb.dirty = true;
}

fn insert_char(tabs: &mut Tabs, edits: &mut BufferEdits, ch: char) {
    with_active(tabs, edits, |tab, eb| {
        let mut rope = (*eb.rope).clone();    // O(chunks), structural sharing
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.insert_char(char_idx, ch);
        bump(eb, rope);
        tab.cursor.col += 1;
        tab.cursor.preferred_col = tab.cursor.col;
    });
}

fn insert_newline(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.insert_char(char_idx, '\n');
        bump(eb, rope);
        tab.cursor.line += 1;
        tab.cursor.col = 0;
        tab.cursor.preferred_col = 0;
    });
}

fn delete_back(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        if tab.cursor.line == 0 && tab.cursor.col == 0 { return; }
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.remove(char_idx - 1 .. char_idx);
        bump(eb, rope);
        if tab.cursor.col > 0 {
            tab.cursor.col -= 1;
        } else {
            tab.cursor.line -= 1;
            // Line above lost its newline; new col = old prev line len.
            tab.cursor.col = line_char_len(&eb.rope, tab.cursor.line);
        }
        tab.cursor.preferred_col = tab.cursor.col;
    });
}

fn delete_forward(tabs: &mut Tabs, edits: &mut BufferEdits) {
    with_active(tabs, edits, |tab, eb| {
        let line_count = eb.rope.len_lines();
        let at_end_of_last_line =
            tab.cursor.line + 1 >= line_count
            && tab.cursor.col >= line_char_len(&eb.rope, tab.cursor.line);
        if at_end_of_last_line { return; }
        let mut rope = (*eb.rope).clone();
        let char_idx = rope.line_to_char(tab.cursor.line) + tab.cursor.col;
        rope.remove(char_idx .. char_idx + 1);
        bump(eb, rope);
        // Cursor stays put; preferred_col unchanged because col didn't move.
    });
}
```

### Cursor movement after M3

Cursor movement (M2) needs the rope to clamp against. Current M2
code reads the rope from `BufferStore`. After M3 it should read from
`BufferEdits` when present. The change is local to
`move_cursor` in `runtime/src/dispatch.rs`:

```rust
fn move_cursor(tabs: &mut Tabs, edits: &BufferEdits, store: &BufferStore, terminal: &Terminal, m: Move) {
    let Some(active) = tabs.active else { return };
    let Some(idx)    = tabs.open.iter().position(|t| t.id == active) else { return };
    let path = &tabs.open[idx].path;
    let rope = match edits.buffers.get(path) {
        Some(eb) => eb.rope.clone(),
        None => match store.loaded.get(path) {
            Some(LoadState::Ready(r)) => r.clone(),
            _ => return,
        },
    };
    // ... rest unchanged ...
}
```

## Runtime wiring

```rust
// crates/runtime/src/lib.rs
pub fn run(
    tabs:     &mut Tabs,
    edits:    &mut BufferEdits,
    store:    &mut BufferStore,
    terminal: &mut Terminal,
    drivers:  &Drivers,
    stdout:   &mut impl Write,
    trace:    &SharedTrace,
) -> io::Result<()> {
    let mut last_frame: Option<Frame> = None;

    loop {
        // ── Ingest ──────────────────────────────────────────────
        let completions = drivers.file.process(store);
        for LoadCompletion { path, rope } in completions {
            edits.buffers.entry(path).or_insert_with(|| EditedBuffer {
                rope, version: 0, dirty: false,
            });
        }
        drivers.input.process(terminal);

        let mut quit = false;
        while let Some(term_ev) = terminal.pending.pop_front() {
            let ev = match term_ev {
                TermEvent::Key(k) => Event::Key(k),
                TermEvent::Resize(d) => Event::Resize(d),
            };
            match dispatch(ev, tabs, edits, store, terminal) {
                DispatchOutcome::Continue => {}
                DispatchOutcome::Quit => { quit = true; break; }
            }
        }
        if quit { break Ok(()); }

        // ── Query ───────────────────────────────────────────────
        let actions = file_load_action(...);
        let frame   = render_frame(
            TerminalDimsInput::new(terminal),
            EditedBuffersInput::new(edits),
            StoreLoadedInput::new(store),
            TabsActiveInput::new(tabs),
            DirtyFlagsInput::new(edits),
        );

        // ── Execute ─────────────────────────────────────────────
        drivers.file.execute(actions.iter(), store);

        // ── Render ──────────────────────────────────────────────
        if frame != last_frame {
            if let Some(f) = &frame {
                trace.render_tick();
                paint(f, stdout)?;
            }
            last_frame = frame;
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}
```

`DirtyFlagsInput` and `EditedBuffersInput` both project
`BufferEdits.buffers`; drv caches them independently. When only the
`dirty` bit changes but no rope does (hard to construct in M3 but
possible after undo lands), `body_model` hits cache while
`tab_bar_model` invalidates. The split is zero-cost.

## Crate layout

```
crates/
  core/
  state-tabs/
  state-buffer-edits/           ← new
  driver-buffers/
    core/                       process() signature grows
    native/                     unchanged
  driver-terminal/
    core/                       unchanged
    native/                     unchanged
  runtime/                      query.rs + dispatch.rs + lib.rs grow
led/                            main.rs constructs BufferEdits
```

Workspace `Cargo.toml` grows one member + one workspace dep.

## Trace

Proposed optional line format (added when `--golden-trace` is on;
harness isn't live yet, so this is forward-looking):

```
edit | kind=insert     path=src/main.rs version=1 ch=h
edit | kind=insert     path=src/main.rs version=2 ch=i
edit | kind=newline    path=src/main.rs version=3
edit | kind=delete_back path=src/main.rs version=4
```

M3 can punt on wiring this through the Trace trait; revisit when the
goldens harness is live.

## Testing

Unit tests at each layer, matching M2's pattern:

- `state-buffer-edits` — construction + default invariants.
- `runtime::dispatch` —
  - `insert_char_advances_cursor_and_bumps_version`
  - `insert_newline_splits_line_and_drops_cursor`
  - `backspace_at_column_zero_joins_with_previous_line`
  - `delete_forward_at_end_of_line_joins_with_next`
  - `delete_back_at_origin_is_a_noop`
  - `delete_forward_at_eof_is_a_noop`
  - `edits_survive_tab_switch` — set up two tabs, edit each, Tab
    cycle, assert both ropes retain edits + cursors.
  - `unloaded_buffer_swallows_edit_keys` — no `EditedBuffer` entry
    → insert_char / newline / delete is a no-op, cursor unchanged.
- `runtime::query::body_model` —
  - `body_model_reads_edited_rope_when_present` — seed
    `BufferEdits` with a rope differing from the disk rope; assert
    the edited version shows up.
  - `body_model_falls_back_to_store_when_edits_absent` — keeps the
    M1/M2 behaviour on the pre-seed window.
- `runtime::query::tab_bar_model` —
  - `dirty_flag_prefixes_label_with_asterisk`
  - `clean_buffer_has_no_prefix`

Expected delta from M2: ~15–20 new tests, total north of 65.

## Done criteria

- All M1 / M2 tests pass.
- New edit tests pass.
- `cargo clippy --all-targets` warning count unchanged from baseline.
- Interactive smoke test: `cargo run -p led -- some-file.txt` — type
  characters, use Enter / Backspace / Delete, observe edits render;
  switch tabs and return, observe edits + cursor preserved; Ctrl-C
  exits cleanly.
- Allocation discipline holds: on an idle tick after an edit, no
  memo recomputes (version hasn't changed since last tick) and no
  allocation occurs.

## Growth-path hooks

- **M4 — saving.** New `FileWriteDriver` consuming a save-action memo
  whose input is `(EditedBuffers, BufferStore)`. The action says
  "these edited buffers differ from disk — write them." `dirty`
  flips back to `false` on successful write + `BufferStore` update.
- **M6+ — LSP / git rebase.** Add `history: Vec<Edit>` alongside the
  rope in `EditedBuffer`. Rebase queries take a `(from_version,
  coord)` pair and walk the slice `history[from_version..]` to
  translate the coord into current space.
- **Undo / redo.** Also a use for the history field, with a cursor
  into it. Out of scope until someone asks.
- **Config keymap (M5).** The hardcoded arms in `dispatch_key` move
  into a keymap table. Edit primitives become named commands
  (`edit.insert-char`, `edit.backspace`, etc.) that the keymap binds
  to.
