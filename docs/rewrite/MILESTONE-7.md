# Milestone 7 — mark, region, kill ring, clipboard

Seventh vertical slice. After M7 the user can set a mark, kill a
region, kill-to-end-of-line with coalescing, and yank from either
the system clipboard or an internal kill ring.

Prerequisite reading:

1. `docs/spec/editing.md` § "Mark and region", "Kill ring".
2. `docs/drivers/clipboard.md` — legacy driver shape.
3. `docs/extract/actions.md` entries for `SetMark`, `KillRegion`,
   `KillLine`, `Yank`.
4. `MILESTONE-6.md` — the keymap the new commands slot into.

---

## Goal

```
$ cargo run -p led -- README.md
# Ctrl-Space               → mark set at cursor
# Ctrl-N Ctrl-N Ctrl-W      → kill the two lines just moved over
# Ctrl-Y                   → paste killed text at cursor
# Cmd/Ctrl-C in another app → clipboard fills
# Ctrl-Y in led            → system clipboard is pasted
```

## Scope

### In
- `Tab.mark: Option<Cursor>` — the second anchor of a region.
- `SetMark` (`ctrl+space`): sets `mark = Some(cursor)`.
- `Abort` (`esc`, `ctrl+g`): clears the mark (alongside its
  pre-existing no-op placeholder from M6).
- Region = `min(mark, cursor) .. max(mark, cursor)` when mark is
  set. Clamped to current rope extent on read (mark survives edits
  but may end up out-of-range).
- `KillRegion` (`ctrl+w`): remove mark..cursor, store the killed
  text in the kill ring, clear the mark, move cursor to the region
  start. Also write to the system clipboard.
- `KillLine` (`ctrl+k`): kill cursor..end-of-line; if cursor is
  already at EOL, kill the `\n` (joins with next line). Consecutive
  `KillLine` presses **coalesce** their text into one kill-ring
  entry. Any other command breaks the coalescing flag.
- `Yank` (`ctrl+y`): paste at cursor. Strategy:
  1. Fire an async clipboard read.
  2. When the read returns `Some(text)`, insert that text.
  3. If the read returns `None` (empty clipboard) or fails, fall
     back to the kill ring's latest entry.
  4. If the kill ring is also empty, no-op.
- New `state-kill-ring/` crate holding the ring (currently
  single-slot, future milestones grow it).
- New `driver-clipboard/{core,native}/` crate pair. Native uses
  `arboard` for system clipboard access.
- Clipboard writes happen on every kill (both `KillRegion` and
  `KillLine`) so pasting to another app after kill-ing text works.

### Out

Per ROADMAP.md:

- **Yank-pop / ring cycling** (`alt+y`) — not scheduled by itself;
  lands with general yank-ring work when a user asks. The ring
  shape today is `Option<Arc<str>>`; the field name and behaviour
  are growth-compatible with turning it into a real ring later.
- **Region highlight in the rendered frame** — M9 (UI chrome).
  Until then, having a mark set has no visible effect.
- **Rectangular / column selection** — not in legacy; not scheduled.
- **Primary selection on X11** — `arboard` handles this by
  convention (copy goes to both); we inherit whatever it does.

## Key design decisions

### D1 — Mark lives on `Tab`, not on a separate source

Mark is per-view state. Two tabs on the same file can have
independent marks — matches legacy and matches how cursor / scroll
are modelled today. Adding `mark: Option<Cursor>` to `Tab`
mirrors M2's approach for cursor and scroll.

Mark **does not** clear on ordinary edits. The mark position stays
put in buffer coordinates; if an edit shifts where "that position"
really is, mark ends up stale. We clamp at read-time rather than
maintain a rebase query today. If the user notices, Abort clears it.
A later rebase story (M8 / M16 op log) can translate mark through
edits.

### D2 — Kill ring is its own user-decision source

Kill ring is session-global (not per-buffer). New crate:
`state-kill-ring/`:

```rust
pub struct KillRing {
    /// Latest killed text. M7 is single-slot; future work (yank-pop
    /// cycling) promotes this to an ordered ring.
    pub latest: Option<Arc<str>>,
    /// True iff the last command was `KillLine`. Consecutive
    /// `KillLine`s append; any other command resets the flag.
    pub last_was_kill_line: bool,
}
```

`Arc<str>` keeps the paste-from-yank path O(1) clone: dispatch
can hold an `Arc<str>` and insert it into the rope without re-copying.

### D3 — Coalescing is a dispatch-time flag

Legacy led uses a `kill_ring_break_s` stream that fires on every
action except `KillLine` and sets a break flag. The rewrite does
the same in `dispatch_key`: after `run_command` returns, if the
command was not `KillLine`, set `kill_ring.last_was_kill_line =
false`. `KillLine` itself sets it `true`.

Net effect: `Ctrl-K Ctrl-K Ctrl-K` → one kill-ring entry with all
three kills joined. `Ctrl-K Ctrl-A Ctrl-K` → two entries (latest
overwritten — single-slot behaviour).

### D4 — Yank's clipboard-first policy lives in a query + two ticks

This is the first genuinely async-reaching user command. Flow:

1. **Dispatch (`Ctrl-Y`):** set `kill_ring.pending_yank = Some(tab_id)`.
   The tab id captures the target — if the user switches tabs
   before the clipboard responds, paste lands on the original tab
   (matches "the tab I pressed Ctrl-Y in").
2. **Query (`clipboard_action`):** if `pending_yank` is Some and
   no read is already in flight, emit `ClipboardAction::Read`.
3. **Execute:** sync-write "read in flight" into the clipboard
   source; send `ClipboardCmd::Read` to the worker.
4. **Ingest (clipboard completion):**
   - `Ok(Some(text))`: insert `text` at the saved tab's cursor.
     Clear `pending_yank`, clear "read in flight".
   - `Ok(None)` (empty clipboard) or `Err(_)`: insert
     `kill_ring.latest` if set, else no-op. Clear flags.
5. **Cleanup:** if the target tab was closed while the read was in
   flight, drop the paste silently.

The two-tick latency (press → next tick spawns read → read tick
resolves) is invisible at 10 ms main-loop cadence — the user's
keypress produces a paste within a frame.

### D5 — Kills write the clipboard synchronously-intent + async

Just like saves: `KillRegion` / `KillLine` push a `ClipboardAction::Write(text)`
which the runtime hands to the driver. The kill ring is updated
sync so the in-process yank-from-kill-ring fallback works even
before the write lands on the system clipboard.

### D6 — Clipboard driver: small core, `arboard` native

```
crates/driver-clipboard/
  core/    ClipboardDriver, ClipboardAction, ClipboardCmd,
           ClipboardDone, Trace trait.
  native/  arboard-backed worker thread. spawn() convenience.
```

Arboard's `get_text` / `set_text` are blocking (tens of ms on a
cold clipboard, < 1 ms warm). Running them on a worker thread
keeps the main loop non-blocking.

Stateless driver — no source of its own. The single "in-flight"
bit lives on `KillRing` as `read_in_flight: bool`. Completions
come back as `Vec<ClipboardDone>` (same pattern as
`FileWriteDriver` in M4).

## Types

### `state-tabs` change

```rust
pub struct Tab {
    pub id:     TabId,
    pub path:   CanonPath,
    pub cursor: Cursor,
    pub scroll: Scroll,
    pub mark:   Option<Cursor>,   // NEW — M7
}
```

`Default` derives; callers that construct Tab explicitly add
`mark: None` or use `..Default::default()`.

### `state-kill-ring` crate

```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KillRing {
    pub latest: Option<Arc<str>>,
    pub last_was_kill_line: bool,
    pub pending_yank: Option<TabId>,
    pub read_in_flight: bool,
}
```

(TabId lives in state-tabs; state-kill-ring depends on it. Both
user-decision atoms, both in `state-*/`. Fine.)

### `driver-clipboard/core`

```rust
pub enum ClipboardAction {
    Read,
    Write(Arc<str>),
}

pub enum ClipboardCmd {
    Read,
    Write(Arc<str>),
}

pub struct ClipboardDone {
    pub result: Result<ClipboardResult, String>,
}

pub enum ClipboardResult {
    /// Read completed.
    Text(Option<Arc<str>>),  // None = clipboard was empty
    /// Write completed.
    Written,
}

pub struct ClipboardDriver {
    tx_cmd:  mpsc::Sender<ClipboardCmd>,
    rx_done: mpsc::Receiver<ClipboardDone>,
    trace:   Arc<dyn Trace>,
}

pub trait Trace: Send + Sync {
    fn clipboard_read_start(&self);
    fn clipboard_read_done(&self, ok: bool, empty: bool);
    fn clipboard_write_start(&self, bytes: usize);
    fn clipboard_write_done(&self, ok: bool);
}
```

### Dispatch signature grows

Dispatch already takes seven parameters. M7 adds a kill-ring state
reference. We pass `&mut KillRing` alongside `&mut BufferEdits`:

```rust
pub fn dispatch_key(
    k:         KeyEvent,
    tabs:      &mut Tabs,
    edits:     &mut BufferEdits,
    kill_ring: &mut KillRing,
    store:     &BufferStore,
    terminal:  &Terminal,
    keymap:    &Keymap,
    chord:     &mut ChordState,
) -> DispatchOutcome;
```

`#[allow(clippy::too_many_arguments)]` is already on `run()`;
`dispatch_key` joins.

## Runtime integration

Main loop gains a clipboard ingest phase and a clipboard execute
phase. The `kill_ring` atom threads through with the others.

```rust
// Ingest
let clipboard_completions = drivers.clipboard.process();
for done in clipboard_completions {
    match done.result {
        Ok(ClipboardResult::Text(Some(text))) => {
            apply_yank(tabs, edits, kill_ring, text);
        }
        Ok(ClipboardResult::Text(None)) | Err(_) => {
            // Fallback to kill ring.
            if let Some(fallback) = kill_ring.latest.clone() {
                apply_yank(tabs, edits, kill_ring, fallback);
            } else {
                kill_ring.pending_yank = None;
            }
        }
        Ok(ClipboardResult::Written) => { /* nothing */ }
    }
    kill_ring.read_in_flight = false;
}

// Query + Execute additions
let clip_actions = clipboard_action(
    ClipboardIntentInput::new(kill_ring),
);
for act in &clip_actions {
    if matches!(act, ClipboardAction::Read) {
        kill_ring.read_in_flight = true;
        kill_ring.pending_yank = None; // single pending
    }
}
drivers.clipboard.execute(&clip_actions);
```

`apply_yank` is a dispatch-like helper that locates the tab by
pending_yank id, inserts text at its cursor (using the rope-clone
+ bump pattern from M3), and clears the pending flag.

## Crate layout

```
crates/
  state-tabs/                   + mark field
  state-kill-ring/              NEW — user-decision source
  state-buffer-edits/           unchanged
  driver-clipboard/
    core/                       NEW — ClipboardDriver, types, Trace
    native/                     NEW — arboard worker
  driver-buffers/               unchanged
  driver-terminal/              unchanged
  runtime/                      dispatch.rs + query.rs + lib.rs grow
                                trace.rs gets clipboard events
led/                            constructs KillRing + clipboard driver
```

`arboard` already in the workspace dep list (carried over from
legacy). Workspace adds two new members (`state-kill-ring`,
`driver-clipboard/{core,native}`).

## Testing

- `state-kill-ring` — default is empty/clean; serialization-trivial.
- `dispatch` —
  - `set_mark_captures_current_cursor`
  - `abort_clears_mark`
  - `kill_region_removes_marked_range_and_fills_ring`
  - `kill_region_with_cursor_before_mark` (direction-agnostic)
  - `kill_line_kills_to_eol`
  - `kill_line_at_eol_joins_with_next_line`
  - `consecutive_kill_lines_coalesce`
  - `non_kill_command_breaks_coalescing`
  - `yank_from_kill_ring_when_clipboard_empty` (direct, no driver)
  - `yank_noop_when_ring_and_clipboard_empty`
- `driver-clipboard/core` — mpsc-boundary tests (read then return
  text; write then return Written).
- `driver-clipboard/native` — **no** integration test that actually
  touches the system clipboard; side-effects would interfere with
  the developer's running paste buffer. Instead a trivial "spawn +
  drop" lifecycle test. Realistic tests come via goldens later,
  which run in a sandboxed tmpdir session.
- Integration: a dispatch-level test that seeds a scripted
  `ClipboardDriver` and verifies the two-tick read → paste flow.

Expected: +18 unit tests.

## Done criteria

- All existing tests pass; new M7 tests pass.
- Clippy warning count unchanged from post-M6 (13).
- Interactive sanity: set mark with `Ctrl-Space`, move cursor, hit
  `Ctrl-W` — the region disappears and `Ctrl-Y` re-inserts it.
  Kill a line with `Ctrl-K`, switch to another app, `Cmd-V` pastes
  what led killed. Copy something in another app, `Ctrl-Y` in led
  pastes it.
- Goldens baseline unchanged in number (0 / 257) — frame diffs
  still dominate. Trace diffs improve for kill/yank scenarios.

## Growth-path hooks

- **Yank-pop (`alt+y`)**: promote `latest: Option<Arc<str>>` to
  `ring: VecDeque<Arc<str>>` + `cursor: usize`. `Yank` uses
  `ring[cursor]`; `YankPop` post-yank advances the cursor.
- **Mark-through-edits**: once the edit log lands (M8), add a
  rebase query on `Tab.mark` so marks survive edits without
  drifting. For M7 the mark is a raw cursor position, clamped on
  read.
- **X11 primary selection / OS-specific clipboards**: `arboard`
  handles the common cases; anything more specific is deferred.
- **Region highlight**: M9 (UI chrome). `body_model` grows an
  optional `region: Range<(u16, u16)>` the painter inverts.
