# Milestone 1: tabs + buffers from disk

> **Status (2026-04-19) — partly shipped.** The skeleton runs end-to-end:
> `cargo run -p led -- FILE...` opens each file as a tab, async-loads
> contents from disk, renders via crossterm, and responds to `Tab` /
> `Shift-Tab` / `Ctrl-C`. 35 unit tests passing.
>
> **What diverged from this plan:**
>
> - **Crate layout is different.** This doc proposed
>   `state-tabs / state-buffers / state-terminal + driver-file-read /
>   driver-terminal`. Actual shipped layout uses the strict-isolation
>   driver pattern from [`../../../drv/EXAMPLE-ARCH.md`](../../../drv/EXAMPLE-ARCH.md)
>   § "Organizing the code": each driver splits into
>   `driver-<name>/core/` + `driver-<name>/native/`, and **all
>   cross-atom lenses + memos live in `crates/runtime/`** (not in the
>   `state-*` crates). `state-tabs` is the only non-driver atom crate
>   (user-decision, no async side). See `README.md` § "Current crate
>   layout" for the shipped shape and `M1-arch.svg` for a picture.
> - **`drv::Atom<T>` wrapper is gone.** drv was refactored to value-
>   compared memo caches; atoms are plain structs. Drivers take
>   `&mut BufferStore`, not `&mut Atom<BufferStore>`.
> - **All memos use `#[drv::memo(single)]`.** The cache strategy is
>   mandatory now.
> - **`active_buffer_view` is not wired into the render path.** The
>   actual render tree is `render_frame → tab_bar_model + body_model`;
>   `body_model` inlines the active-tab resolution logic rather than
>   calling `active_buffer_view`.
> - **No goldens yet.** The five smoke scenarios listed below remain
>   aspirational; the PTY harness has not been built.
>
> The design narrative below is preserved for historical context —
> most of its reasoning (atoms split by external-fact vs user-decision;
> sync intent-write before async spawn; render-frame as memoized pure
> function) carried through unchanged. Concrete code snippets use
> pre-0.2.0 drv API — treat them as pseudocode.

The smallest end-to-end slice of the query-driven rewrite that exercises
every architectural layer: atoms, drivers, ingest/query/execute/render,
desired-state queries, and intent-written-sync.

Prerequisite reading, in order:

1. `../../../drv/README.md` — the memoization primitive
2. `../../../drv/EXAMPLE-ARCH.md` — the target application shape
3. `QUERY-ARCH.md` — the led-specific translation (this milestone reconciles it against EXAMPLE-ARCH)
4. `REWRITE-PLAN.md § Phase 3` — where this milestone sits in the phased plan

---

## Goal

A binary that takes one or more file paths on the command line, opens
each as a tab, loads their contents from disk asynchronously, and renders
the active tab's content in a terminal UI. `Tab` / `Shift-Tab` switches
the active tab. `Ctrl-C` quits. Resize re-renders.

This proves:

- atoms separate external-facts (disk contents, terminal dims) from user-decisions (tab order, active tab)
- a query (`desired_loaded_paths`) drives an action (`file_load_action`), not a transition handler
- `execute` writes intent (`LoadState::Pending`) synchronously before spawning async I/O, closing the re-trigger loop
- the render path is pure memoized queries, not a push graph
- `--golden-trace` emits one line per ingest / execute event, matching the Phase 0 binary contract

---

## Scope

### In
- CLI: `led FILE...` — each arg becomes a tab, first tab active
- Two drivers: `FileReadDriver` (application-specific, async disk reads) and `TerminalInputDriver` (crossterm events on a background thread)
- One synchronous render call per tick via ratatui
- `Tab` / `Shift-Tab` to cycle active tab; `Ctrl-C` to quit
- Resize → re-render at new dims
- `--golden-trace <path>` emits trace lines for: `key_in`, `resize`, `file_load_start`, `file_load_done`, `render_tick`
- Error surface: if `FileReadDriver` returns `Err`, the tab body shows `<error: message>` in place of content; no recovery UI

### Out
- Cursor, cursor movement, scrolling within a file (show first viewport-height lines; truncate rest)
- Any editing, saving, or mutation of buffer contents
- File browser, find-file, search, any way to open additional files from inside the app
- Config files, keybinding customization
- Session persistence
- Syntax highlighting, LSP, git
- Side panel, preview pane, multi-pane splits
- `--test-clock`, `--test-lsp-server`, `--test-gh-binary` (not needed for this slice — wire in later milestones when the relevant drivers come online)

The out list is non-negotiable for milestone 1. Adding any of it before the skeleton is validated defeats the "smallest possible test" purpose.

---

## Atoms

Three atoms, split on the external-fact vs user-decision axis per EXAMPLE-ARCH § "Atoms: two kinds of ground truth".

### `Tabs` — user-decision

Which tabs are open, which is active. Mutated only by dispatch in response to user input (and, for milestone 1, by initial CLI parsing).

Each [`Tab`] carries its own stable `TabId`. The same file can be open in multiple tabs (future splits) and per-view state (preview flag M2, cursor M3) attaches to the tab, not the path. `Tab` is stored inline in `open: imbl::Vector<Tab>` — no separate metadata map, no ghost entries.

`path` is a `CanonPath`, never a raw `PathBuf`. The distinction between `UserPath` (what the user typed) and `CanonPath` (canonicalized internal key) comes from the legacy led and carries over by design: the type system prevents mixing user-spelled paths with canonical internal identities.

```rust
// in crate `core`:
id_newtype!(TabId);    // expands to a Copy + Clone + Eq + Hash + Debug + Display newtype
pub struct UserPath(PathBuf);      // what the user supplied
pub struct CanonPath(PathBuf);     // canonicalized; internal key
impl UserPath { pub fn canonicalize(&self) -> CanonPath { ... } }

// in crate `state-tabs`:
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Tab {
    pub id:   TabId,
    pub path: CanonPath,
    // M2: pub is_preview: bool,
    // M3: pub cursor: Cursor,
}

#[drv::atom]
pub struct Tabs {
    #[drv::lens(TabListLens)]
    pub open: imbl::Vector<Tab>,

    #[drv::lens(ActiveTabLens)]
    pub active: Option<TabId>,
}
```

Invariants (maintained by dispatch, checked in debug assertions):
- `active.is_some()` iff `!open.is_empty()`
- when `Some`, `active` is the id of exactly one `Tab` in `open`

`TabId` allocation: a `u64` counter in the runtime bumped on each new tab. Never reused. Five lines.

### `BufferStore` — external-fact

What each file looks like on disk, plus its load state. Mutated by `FileReadDriver` (process + execute). Keyed by `CanonPath`.

```rust
#[drv::atom]
pub struct BufferStore {
    #[drv::lens(LoadStateLens, BufferContentLens)]
    pub loaded: imbl::HashMap<CanonPath, LoadState>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoadState {
    Pending,                    // request in flight
    Ready(Arc<Rope>),           // content available
    Error(Arc<String>),         // message from io::Error
}
```

Notes:
- Absent from the map ≡ "never requested." Drivers never observe `Idle` as a stored value; that state is represented by absence, which `file_load_action` reads as "need to load."
- `Arc<Rope>` keeps cache-hit comparison O(1) (pointer equality) even as the rope grows. Same for `Arc<String>` on error.
- `imbl::HashMap` for O(1) clone on cache miss.

### `Terminal` — external-fact

Terminal viewport dimensions plus the pending-input queue drained each tick. Mutated by `TerminalInputDriver`.

```rust
#[drv::atom]
pub struct Terminal {
    #[drv::lens(DimsLens, RenderLens)]
    pub dims: Option<Dims>,     // None until first resize event observed

    pub pending: std::collections::VecDeque<TermEvent>,  // drained in ingest, not lensed
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Dims { pub cols: u16, pub rows: u16 }

#[derive(Clone, Debug, PartialEq)]
pub enum TermEvent {
    Key(KeyEvent),        // crossterm::event::KeyEvent re-exported or mirrored
    Resize(Dims),
}
```

`pending` is deliberately **not** exposed via any lens — it's ingest-only scratch space that dispatch drains and turns into mutations on other atoms. Nothing queries it.

---

## Events

A single `Event` enum routed through dispatch. Milestone 1 only has four variants:

```rust
pub enum Event {
    Key(KeyEvent),
    Resize(Dims),
    FileReadDone(PathBuf, Result<Arc<Rope>, String>),
    Quit,
}
```

`Event::Quit` is set by dispatch when it sees `Ctrl-C`; the main loop checks a `quit` flag after dispatch and breaks.

Events enter the system via:
- `TerminalInputDriver` → `Event::Key` / `Event::Resize`
- `FileReadDriver` → `Event::FileReadDone`

Dispatch consumes events and produces atom mutations.

---

## Queries

Four memos. Each is a pure function; all memoized by `drv`.

### `desired_loaded_paths` — "what files should be loaded right now?"

```rust
#[drv::memo]
fn desired_loaded_paths(tabs: &TabListLens) -> imbl::HashSet<CanonPath> {
    tabs.open.iter().map(|t| t.path.clone()).collect()
}
```

Trivial for milestone 1 — it's the path of every open tab. Later milestones will prune (e.g., only the active tab + neighbors, capping memory). The shape is in place today so adding that rule is a one-line change.

### `file_load_action` — diff between desired and actual

```rust
pub enum LoadAction {
    Load(PathBuf),
    Noop,
}

#[drv::memo]
fn file_load_action(
    store: &LoadStateLens,
    tabs: &TabListLens,
) -> imbl::Vector<LoadAction> {
    let desired = desired_loaded_paths(tabs);
    desired
        .into_iter()
        .filter(|p| !matches!(
            store.loaded.get(p),
            Some(LoadState::Pending) | Some(LoadState::Ready(_)) | Some(LoadState::Error(_))
        ))
        .map(LoadAction::Load)
        .collect()
}
```

Absent from the map → emit `Load`. Anything else (`Pending` / `Ready` / `Error`) → skip. This is the EXAMPLE-ARCH "actual == desired treats `Pending` the same as `Ready`" pattern: once a load is in flight, we don't re-trigger it.

### `active_buffer_view` — sub-query, view-model for the active tab's content

```rust
pub enum ActiveBufferView {
    Empty,                              // no tabs
    Pending(CanonPath),
    Error(CanonPath, Arc<String>),
    Ready { path: CanonPath, content: Arc<Rope> },
}

#[drv::memo]
fn active_buffer_view(
    store: &BufferContentLens,
    tabs: &ActiveTabLens,
) -> ActiveBufferView {
    let Some(id) = tabs.active else { return ActiveBufferView::Empty };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else { return ActiveBufferView::Empty };
    let path = tab.path.clone();
    match store.loaded.get(&path) {
        None | Some(LoadState::Pending) => ActiveBufferView::Pending(path),
        Some(LoadState::Ready(rope))    => ActiveBufferView::Ready { path, content: rope.clone() },
        Some(LoadState::Error(msg))     => ActiveBufferView::Error(path, msg.clone()),
    }
}
```

Note: `ActiveTabLens` needs to project both `open` and `active` now that lookup iterates the Vector. Update the lens declaration on `Tabs` accordingly:
```rust
#[drv::lens(TabListLens, ActiveTabLens)]
pub open: imbl::Vector<Tab>,
#[drv::lens(ActiveTabLens)]
pub active: Option<TabId>,
```

### `render_frame` — top-level view-model

Terminal is the first lens parameter so `drv::assemble!()` places the cache on the `Terminal` atom and the memo physically lives in `state-terminal/` alongside its sibling view-model memos.

```rust
pub struct Frame {
    pub tab_bar: TabBarModel,
    pub body:    BodyModel,
    pub dims:    Dims,
}

#[drv::memo]
fn render_frame(
    term:  &RenderLens,
    store: &BufferContentLens,
    tabs:  &ActiveTabLens,
) -> Option<Frame> {
    let dims = term.dims?;
    Some(Frame {
        tab_bar: tab_bar_model(tabs),
        body:    body_model(store, tabs, dims),
        dims,
    })
}
```

`tab_bar_model` and `body_model` are their own memos; `body_model` calls `active_buffer_view`. Each layer is independently cached. All three memos (and any view-model helpers) live in `state-terminal/` for grep-locality.

---

## Drivers

Two drivers. Both follow EXAMPLE-ARCH § "Driver structure": background async work, results come back via mpsc, main-thread `process()` drains into the atom, `execute()` writes intent sync before spawning.

### `FileReadDriver`

Manages `BufferStore`. Owns a thread pool (single background thread is fine at this scale) and an mpsc channel for completions.

```rust
pub struct FileReadDriver {
    tx_cmd: mpsc::Sender<ReadCmd>,          // main → worker
    rx_done: mpsc::Receiver<ReadDone>,      // worker → main
    trace: Trace,                           // for --golden-trace
}

enum ReadCmd { Read(PathBuf) }
struct ReadDone { path: PathBuf, result: Result<Arc<Rope>, String> }

impl FileReadDriver {
    /// Drain completions into the atom. Called in ingest.
    pub fn process(&self, store: &mut BufferStore) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(done) = self.rx_done.try_recv() {
            self.trace.emit(TraceLine::FileLoadDone {
                path: &done.path,
                ok: done.result.is_ok(),
                bytes: done.result.as_ref().ok().map(|r| r.len_bytes()),
            });
            store.loaded.insert(done.path.clone(), match &done.result {
                Ok(rope)  => LoadState::Ready(rope.clone()),
                Err(msg)  => LoadState::Error(Arc::new(msg.clone())),
            });
            events.push(Event::FileReadDone(done.path, done.result));
        }
        events
    }

    /// Act on the query result. Writes intent, then spawns.
    pub fn execute(&self, actions: &[LoadAction], store: &mut BufferStore) {
        for action in actions {
            let LoadAction::Load(path) = action else { continue };
            store.loaded.insert(path.clone(), LoadState::Pending);  // (1) sync intent
            self.trace.emit(TraceLine::FileLoadStart { path });
            let _ = self.tx_cmd.send(ReadCmd::Read(path.clone()));  // (2) spawn
        }
    }
}
```

The sync write of `Pending` is the entire point. Without it, the next tick's `file_load_action` query would still see the path as absent and emit another `Load` for the same file.

### `TerminalInputDriver`

Wraps `crossterm::event::read` on a background thread. Main-thread `process()` drains events into `Terminal.pending`. Requires raw mode to be on; the `led` bin's `main()` enables raw mode via a `RawModeGuard` before spawning this driver.

```rust
pub struct TerminalInputDriver {
    rx: mpsc::Receiver<TermEvent>,
    trace: Trace,
}

impl TerminalInputDriver {
    pub fn process(&self, term: &mut Terminal) {
        while let Ok(ev) = self.rx.try_recv() {
            match &ev {
                TermEvent::Key(k)    => self.trace.emit(TraceLine::KeyIn(k)),
                TermEvent::Resize(d) => {
                    self.trace.emit(TraceLine::Resize(*d));
                    term.dims = Some(*d);
                }
            }
            term.pending.push_back(ev);
        }
    }
}
```

Note that `Resize` is applied directly (it's pure state, no dispatch needed) but also pushed to `pending` so dispatch sees it if it ever needs to. For milestone 1 only `Key` needs dispatch.

### Output (not a driver)

Rendering is a free function, not a driver. No ratatui — crossterm only. When the memoized `render_frame` produces a new `Frame`, the main loop calls `paint(frame, &mut stdout)` which emits crossterm escape sequences to draw the whole frame.

```rust
pub fn paint(frame: &Frame, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, style, terminal, queue};
    queue!(out, cursor::MoveTo(0, 0))?;
    for (row, line) in frame.tab_bar.rows().chain(frame.body.rows()).enumerate() {
        queue!(out, cursor::MoveTo(0, row as u16))?;
        for cell in line {
            queue!(out, style::SetAttribute(cell.attr), style::Print(cell.ch))?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }
    out.flush()
}
```

No frame diffing: the whole frame is redrawn whenever `frame != last_frame`. At 120×40 that's ~4800 cells per redraw — negligible at any reasonable change rate. Introduce a diff pass later only if measurements say it's needed.

### Raw-mode guard

Lives in `driver-terminal/` alongside input and `paint()` — everything crossterm-specific in one crate. A RAII type acquired once in `main()` before spawning the input driver; its `Drop` restores cooked mode on normal exit and on panic unwind.

```rust
pub struct RawModeGuard;

impl RawModeGuard {
    pub fn acquire() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
```

No SIGINT handler. In raw mode, `Ctrl-C` arrives as byte `0x03` → `KeyEvent { modifiers: CONTROL, code: Char('c') }` → dispatch sets quit. If the process is killed via SIGKILL or aborts so hard that `Drop` can't run, the user resets their terminal manually — acceptable.

---

## The main loop

```rust
fn main_loop(
    tabs: &mut Tabs,
    store: &mut BufferStore,
    terminal: &mut Terminal,
    drivers: &Drivers,
    stdout: &mut impl Write,
    trace: &Trace,
) -> io::Result<()> {
    let mut quit = false;
    let mut last_frame: Option<Frame> = None;

    while !quit {
        // ── Ingest ──────────────────────────────────────────────
        let file_events = drivers.file.process(store);
        drivers.input.process(terminal);

        // Dispatch: drain Terminal.pending + file_events into atoms.
        let events = std::mem::take(&mut terminal.pending)
            .into_iter()
            .map(Event::from_term)
            .chain(file_events);
        for ev in events {
            quit |= dispatch(ev, tabs, store, terminal);
        }
        if quit { break; }

        // ── Query ───────────────────────────────────────────────
        let actions = file_load_action(store, tabs);
        let frame   = render_frame(terminal, store, tabs);

        // ── Execute ─────────────────────────────────────────────
        drivers.file.execute(&actions, store);

        // ── Render ──────────────────────────────────────────────
        if frame != last_frame {
            if let Some(f) = &frame {
                trace.emit(TraceLine::RenderTick);
                paint(f, stdout)?;
            }
            last_frame = frame;
        }

        // ── Park ────────────────────────────────────────────────
        // Wait for any channel to have data. No polling; no ticks.
        select_any(&drivers.file.rx_done, &drivers.input.rx);
    }
    Ok(())
}
```

Notes on the loop:
- `select_any` uses crossbeam's `select!` or a tiny `Condvar`-based fan-in. No tokio, no timers. Milestone 1 is event-driven only.
- `last_frame != frame` avoids redundant `paint` calls. `Frame: PartialEq` + `render_frame` memoization means this is cheap when nothing has changed.
- Dispatch returns `bool` signaling "quit requested" rather than mutating a flag atom — quit is not a query-observable state for milestone 1.
- `RawModeGuard` is held in `main()` for the lifetime of this call. Its `Drop` restores cooked mode whether we exit normally or via panic unwind.

---

## Dispatch

One `dispatch` function per event variant, kept small. Matches EXAMPLE-ARCH's "invariant enforcement in ingest" for the quit case.

```rust
fn dispatch(ev: Event, tabs: &mut Tabs, store: &mut BufferStore, _term: &mut Terminal) -> bool {
    match ev {
        Event::Key(k) => dispatch_key(k, tabs),
        Event::Resize(_) => false,     // already applied in TerminalInputDriver.process
        Event::FileReadDone(_, _) => false,   // already applied in FileReadDriver.process
        Event::Quit => true,
    }
}

fn dispatch_key(k: KeyEvent, tabs: &mut Tabs) -> bool {
    match (k.modifiers, k.code) {
        (M::CONTROL, Code::Char('c')) => true,           // quit
        (M::NONE, Code::Tab)          => { cycle_active(tabs, 1);  false }
        (M::SHIFT, Code::BackTab)     => { cycle_active(tabs, -1); false }
        _ => false,
    }
}

fn cycle_active(tabs: &mut Tabs, delta: isize) {
    if tabs.open.is_empty() { return; }
    let n = tabs.open.len() as isize;
    let cur_idx = tabs.active
        .and_then(|id| tabs.open.iter().position(|t| t.id == id))
        .unwrap_or(0) as isize;
    let next_idx = (cur_idx + delta).rem_euclid(n) as usize;
    tabs.active = Some(tabs.open[next_idx].id);
}
```

The `dispatch_*` functions are direct code, not memos. They mutate atoms; they don't read/produce queries. Per QUERY-ARCH § "The event handler", this is where decision-making that used to live in FRP combinator chains goes.

---

## CLI + trace

### CLI surface

```
led [OPTIONS] [FILES...]

Arguments:
  FILES...   File paths to open as tabs.

Options:
  --golden-trace <PATH>   Append trace lines for each external event/execute.
  --config-dir <PATH>     Ignored for milestone 1; reserved for later.
```

Flags not wired in milestone 1 (per § Scope): `--test-clock`, `--test-lsp-server`, `--test-gh-binary`. The goldens runner passes these unconditionally (per HANDOFF recent-decisions); parse-and-ignore them in clap with a `#[arg(hide = true)]` so unknown-flag errors don't break existing scenarios.

### Trace format

One line per event, pipe-separated, canonical ordering. Paths are repo-relative; absolute paths are stripped to the golden scenario root at emission time (the runner sets `LED_TRACE_ROOT=<scenario-tmpdir>` and led emits paths relative to it).

```
key_in          | key=Tab
key_in          | key=Ctrl-c
resize          | cols=120 rows=40
file_load_start | path=src/main.rs
file_load_done  | path=src/main.rs ok=true bytes=4823
file_load_done  | path=missing.rs  ok=false err="No such file or directory"
render_tick
```

The existing goldens runner already normalizes this shape (see `goldens/` crate). Milestone 1 emits a subset; later milestones extend the set (lsp_request, git_scan, etc.).

---

## Crate layout

`drv::assemble!()` requires atom + its memos in the same crate. Cross-atom memos live in the crate of their first lens parameter (which owns the cache). First-lens ordering is chosen to place each memo in its semantic home, not just its cache home. Result:

```
crates/
  core/                 id_newtype! macro + UserPath/CanonPath newtypes.
                        Foundational atom-free types. Depends on nothing internal.
  state-tabs/           Tabs atom, Tab struct, TabId, desired_loaded_paths memo
                        (depends on core)
  state-buffers/        BufferStore atom, file_load_action memo, active_buffer_view memo
                        (depends on core, state-tabs for TabListLens / ActiveTabLens)
  state-terminal/       Terminal atom, render_frame memo (Terminal lens first),
                        tab_bar_model, body_model
                        (depends on state-tabs, state-buffers)
  driver-file-read/     FileReadDriver (depends on core, state-buffers)
  driver-terminal/      TerminalInputDriver (bg thread + mpsc + process()),
                        paint() free fn, RawModeGuard
                        (depends on state-terminal; depends on crossterm)
  runtime/              Event enum, dispatch fns, main_loop, Trace
                        (depends on everything above)
led/                    main.rs only — parses CLI, acquires RawModeGuard,
                        constructs drivers + atoms, calls runtime::main_loop
```

Primary-atom choices (where each cross-atom memo lives, determined by first lens param):
- `file_load_action(store, tabs)` → `state-buffers`
- `active_buffer_view(store, tabs)` → `state-buffers`
- `render_frame(term, store, tabs)` → `state-terminal` (Terminal first by design; see Q1 in Decisions)

### Why this shape

- Each `state-*` crate is a single atom + its memos. Grep-local, assembleable, zero shared mutable state across crate boundaries.
- Drivers are separate so they can be swapped in tests (milestone 1 uses real fs; golden scenarios could swap in a fake reader later without changing `state-buffers`).
- `core` is the foundational crate — `id_newtype!` macro, `UserPath`/`CanonPath`, and any future shared atom-free types. Everyone depends on it; it depends on nothing internal.
- Input and output for the terminal live in one `driver-terminal/` crate (input driver struct + `paint()` free function + `RawModeGuard`) because they're all crossterm-specific and would be two tiny crates otherwise. The atom + view-model memos stay in `state-terminal/` (no crossterm dep — pure state + pure queries).
- `runtime` is the only place wiring lives. `led/` bin is a thin `main()`.
- Adds 8 crates to the workspace (including `core/`). At milestone 2+ we'll add `state-lsp`, `state-git`, `driver-lsp`, `driver-git`, etc. — growth is linear and mechanical.

### Workspace `Cargo.toml` additions

After milestone 1 lands:

```toml
members = [
  "crates/core",
  "crates/state-tabs",
  "crates/state-buffers",
  "crates/state-terminal",
  "crates/driver-file-read",
  "crates/driver-terminal",
  "crates/runtime",
  "led",
]

[workspace.dependencies]
led-core              = { path = "crates/core" }
led-state-tabs        = { path = "crates/state-tabs" }
led-state-buffers     = { path = "crates/state-buffers" }
led-state-terminal    = { path = "crates/state-terminal" }
led-driver-file-read  = { path = "crates/driver-file-read" }
led-driver-terminal   = { path = "crates/driver-terminal" }
led-runtime           = { path = "crates/runtime" }
drv                   = { path = "../drv/drv", features = ["imbl"] }
imbl                  = "7"
ropey                 = "1.6"
crossterm             = "0.29"
# ratatui is NOT a dependency — crossterm only.
# (plus existing external deps already in workspace)
```

---

## Testing

Three layers, ordered cheapest → most expensive:

### 1. Unit tests on queries (per-crate, pure)

Queries are pure functions. Construct atoms, call the memo, assert output. EXAMPLE-ARCH § "Unit testing a query" is the template.

```rust
#[test]
fn file_load_action_skips_already_pending() {
    let mut store = BufferStore::default();
    let mut tabs = Tabs::default();
    tabs.open.push_back("a.txt".into());
    tabs.open.push_back("b.txt".into());
    tabs.active = Some(0);
    store.loaded.insert("a.txt".into(), LoadState::Pending);

    let acts = file_load_action(&store, &tabs);
    assert_eq!(acts.len(), 1);
    assert!(matches!(&acts[0], LoadAction::Load(p) if p == Path::new("b.txt")));
}
```

No async runtime, no tempdir, no PTY. Thousands of these can run in a second.

### 2. Unit tests on dispatch (runtime crate, sync)

Dispatch functions are direct code over `&mut` atoms. Construct atoms, call `dispatch_key`, assert mutations.

```rust
#[test]
fn tab_cycles_active_forward() {
    let mut tabs = tabs_with(&["a", "b", "c"], 0);
    dispatch_key(key(M::NONE, Code::Tab), &mut tabs);
    assert_eq!(tabs.active, Some(1));
    dispatch_key(key(M::NONE, Code::Tab), &mut tabs);
    assert_eq!(tabs.active, Some(2));
    dispatch_key(key(M::NONE, Code::Tab), &mut tabs);
    assert_eq!(tabs.active, Some(0));
}
```

### 3. Golden scenarios (black-box PTY, full binary)

The goldens runner spawns `led` with file args and scripts keystrokes. Milestone 1 gets a handful of scenarios under `goldens/scenarios/smoke/`:

- `open_one_file` — `led a.txt` → frame shows tab bar with one tab, body shows file content
- `open_two_files_tab_switches` — `led a.txt b.txt`, press Tab, assert active tab flipped
- `missing_file_shows_error` — `led nosuch.txt` → tab body says `<error: No such file or directory>`
- `resize_redraws` — `led a.txt`, resize PTY, assert new frame at new dims
- `ctrl_c_exits_cleanly` — `led a.txt`, Ctrl-C, assert exit code 0 and no trailing trace lines

The `dispatched.snap` for each scenario is the `--golden-trace` output; the `frame.snap` is the final `vt100` grid.

---

## Growth path

How each piece extends in subsequent milestones — keep this in mind so milestone 1 doesn't bake in dead ends.

| Milestone | Adds | Touches |
|-----------|------|---------|
| M2 — cursor + movement | `Cursor` field on `Tabs`; arrow keys; viewport scrolling | `Tabs` atom grows a `cursors: HashMap<PathBuf, Cursor>` field; `body_model` gains scroll params; dispatch adds arrow-key handling |
| M3 — editing | insert/delete chars; dirty tracking | New `BufferEdits` user-decision atom (distinct from `BufferStore` external-fact) holding the edit log per path; `ropey` edits produce `Mut::InsertAt` / `Mut::DeleteAt`; version counter added for async-data rebase groundwork |
| M4 — saving | Ctrl-S writes active buffer | New `FileWriteDriver`; `BufferStore` gets a `saved_version` field; dirty-query compares `saved_version` vs current |
| M5 — config | `.led/config.toml` for keybindings | New `ConfigState` external-fact atom; `FileReadDriver` repurposes for config load; dispatch consults config for keymap resolution |
| M6+ — LSP, git, syntax, session | per QUERY-ARCH | Each adds one driver + one `state-*` crate; renderer consumes new lenses; trace format extended |

### Things milestone 1 deliberately defers so they can be designed properly later

- **Edit log / versioning** — needed for LSP/git rebase queries. Milestone 3 when editing lands.
- **`Loaded<T>` enum** with `Idle | Pending | Ready | Error` (per QUERY-ARCH § "Cache-miss dispatch") — milestone 1 uses absent-from-map = Idle since only one resource type exists. Introduce the enum when the second driver (LSP?) lands and the pattern is shared.
- **`request_collector` thread-local** — EXAMPLE-ARCH's explicit execute pattern is cleaner when actions are enumerable. Only if a future milestone needs dispatch from deep inside a render memo (not an obvious case yet) do we reach for the collector.
- **Domain atoms inside `state-terminal`** — render_frame currently lives there; as UI state grows (phase, focus, side panel), a `UiState` atom will split out. For milestone 1, `Terminal` is the only UI-side atom.

---

## Decisions

Recorded so future readers see what was settled and why.

1. **`render_frame` lives in `state-terminal/`** with `Terminal` as its first lens parameter. Any lens change invalidates the cache regardless of parameter order, so first-lens choice is free for cache behavior — picked to put the memo in its semantic home (the "view" crate) instead of with the primary data it reads. If `drv::assemble!()` rejects this cross-crate arrangement at implementation time, fall back to making `state-buffers` the host and revisit.
2. **Crossterm only, no ratatui.** A `paint(frame, &mut stdout)` free function emits escape sequences directly. The full frame is redrawn whenever `frame != last_frame`; no diff engine. At 120×40 that's ~4800 cells per redraw — negligible.
3. **One `driver-terminal/` crate for input + output + raw-mode guard.** The `Terminal` atom + view-model memos stay in `state-terminal/` with no crossterm dependency. The driver crate owns everything crossterm-specific: input thread + mpsc + `process()`, the `paint()` free function, and `RawModeGuard`.
4. **`TabId` newtype from day one via `id_newtype!` macro** in a shared `ids/` crate. Current led already has preview tabs (which require identity separate from path), and splits (M5+) will need it too. Indirection is `tabs.meta[id].path`.
5. **No SIGINT handler.** Raw mode makes `Ctrl-C` arrive as a key event byte, not a signal. The `RawModeGuard`'s `Drop` restores cooked mode on normal exit and panic unwind. Verify against the goldens PTY harness with an early smoke scenario that sends `Ctrl-C` and asserts clean exit.

---

## Summary

```
Atoms:    Tabs (user, keyed by TabId), BufferStore (external), Terminal (external)
Drivers:  FileReadDriver, TerminalInputDriver    [paint() is a free fn, not a driver]
Queries:  desired_loaded_paths, file_load_action, active_buffer_view, render_frame
Loop:     ingest (drain drivers → dispatch events) → query → execute → render
Crates:   core, state-tabs, state-buffers, state-terminal,
          driver-file-read, driver-terminal, runtime, led
Output:   crossterm only, full redraw on frame change (no ratatui, no diff)
Trace:    key_in, resize, file_load_start, file_load_done, render_tick
Goldens:  5 smoke scenarios under goldens/scenarios/smoke/
```

One driver does the interesting work (file reads). One driver brings the
terminal in. The main loop is 40 lines. The queries are 15. Everything
else is types and crate scaffolding.

That's the whole milestone.
