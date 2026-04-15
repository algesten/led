# Query-driven architecture for led

This doc describes the **target architecture** for the led rewrite. It's the "what," not the "how" (see `REWRITE-PLAN.md` for execution).

It assumes familiarity with the `drv` crate (see `../drv/README.md`). Read that first if you haven't.

---

## TL;DR

- **AppState disappears as one big struct.** Replaced by several **domain atoms** (`BufferState`, `GitState`, `LspState`, etc.), each owned by its respective driver.
- **The FRP `model()` stream graph disappears.** Replaced by a **sync non-blocking event handler** that applies events to the appropriate domain reducer. Still fine-grained muts per domain; still trivial per-domain reducers.
- **The `derived()` layer disappears.** Replaced by **queries** (memoized pure functions via `drv`) pulled on demand by the renderer.
- **Drivers split into two kinds.** *Input drivers* (keyboard, fs-watch, LSP server-pushed notifications, timers) push events into the handler. *Resource drivers* (file read, git scan, syntax parse, LSP request) are fire-and-forget from the handler; results come back as events.
- **Async mismatch is handled by versioned data + rebase queries**, not by keeping things in sync imperatively.

---

## Why we're doing this

The current FRP architecture has served led well, but it has specific pain points the rewrite targets:

1. **Eager push everywhere.** The `derived()` graph runs on every state change whether anyone's looking or not. Wasted work; careful dedupe/MDF needed to avoid feedback loops.
2. **Resource drivers are awkwardly shaped.** Things like file reads and git scans want to be lazy ("only work if someone needs the result"), but FRP pushes them to be eager.
3. **Logic spread across the reducer.** To understand "what happens when the user presses Ctrl-S" you trace streams across many `_of` files. The combinator graph is powerful but non-local.
4. **Single `AppState` blob.** Every concern — buffers, LSP, git, PR, session, file search — mutates fields on one struct. Ownership boundaries are conventional, not structural.
5. **Position-sensitive async data is fragile.** Diagnostics and git hunks have offsets tied to a specific buffer version. Keeping them synced with edits currently lives in buffer-side imperative code.

The query model swaps the tradeoff: push becomes pull (less ambient work), a single reducer becomes per-domain reducers (tighter ownership), combinator graphs become query graphs (locality via memoization), and async/version mismatch becomes a rebase-query concern (declarative).

---

## The three layers

```
┌─────────────────────────────────────────────────────────────┐
│  INPUT LAYER                                                │
│  - input drivers (keyboard, fs-watch, LSP notifs, timers)   │
│  - resource-driver results (FileRead done, GitScan done…)   │
│  ────────────────┬─────────────────────────────────────────│
│                  ▼                                         │
│  HANDLER LAYER   event → mutations on domain atoms         │
│  ────────────────┬─────────────────────────────────────────│
│                  ▼                                         │
│  STATE LAYER     domain atoms (BufferState, GitState, …)   │
│  ────────────────┬─────────────────────────────────────────│
│                  ▼                                         │
│  QUERY LAYER     memoized pure functions (drv)             │
│                  - render queries → terminal frame         │
│                  - dispatch queries → "what's missing?"    │
└─────────────────────────────────────────────────────────────┘
                   ▲                       │
                   │                       ▼
          async completions         fire-and-forget
          flow back as events       dispatches to
          into input layer          resource drivers
```

The inner loop is one sync function:

```rust
fn tick(state: &mut DomainAtoms, event: Event, dispatch: &Dispatch) {
    apply_event(state, event);              // domain reducers
    request_collector.clear();
    let frame = render_query(&state);       // memoized pull
    for req in dedupe(request_collector.drain()) {
        if !state.is_pending(&req) {
            state.mark_pending(&req);
            dispatch.spawn(req);            // fire-and-forget
        }
    }
    terminal.draw(&frame);
}
```

`tick` is sync, never blocks on I/O, returns in microseconds. The runtime (tokio, mio, whatever) calls it per event.

---

## Domain atoms

Instead of one `AppState`, each driver concern owns its own atom:

```rust
#[drv::atom]
pub struct BufferState {
    pub contents: HashMap<CanonPath, Rope>,
    pub versions: HashMap<CanonPath, u64>,
    pub edits:    HashMap<CanonPath, im::Vector<Edit>>,  // edit log per path
    pub cursors:  HashMap<CanonPath, Cursor>,
    pub active:   Option<CanonPath>,
    pub tabs:     im::Vector<CanonPath>,
    // ...
}

#[drv::atom]
pub struct GitState {
    pub file_status: HashMap<CanonPath, Loaded<GitFileStatus>>,
    pub line_status: HashMap<CanonPath, Loaded<(u64, Vec<LineStatus>)>>,
    pub branch:      Loaded<String>,
    pub pr:          Loaded<Option<PrInfo>>,
}

#[drv::atom]
pub struct LspState {
    pub servers:    HashMap<ServerId, ServerInfo>,
    pub diagnostics: HashMap<CanonPath, Loaded<(u64, Vec<Diagnostic>)>>,
    pub inlay_hints: HashMap<CanonPath, Loaded<(u64, Vec<InlayHint>)>>,
    pub completions: Loaded<CompletionSet>,
    pub pending:    Option<LspRequest>,
}

#[drv::atom]
pub struct UiState {
    pub phase:          Phase,
    pub focus:          PanelSlot,
    pub show_side_panel: bool,
    pub dims:           Option<Dimensions>,
    pub alerts:         AlertState,
    pub browser:        FileBrowserState,
    // ...
}

#[drv::atom]
pub struct SessionState { /* ... */ }

// plus SearchState, JumpState, KillRingState, MacroState, ConfigState, etc.
```

There is **no composite `AppState`**. Queries take references to whichever atoms they need:

```rust
#[drv::memo]
fn render_buffer_line(
    buf: &BufferActiveLens,      // from BufferState
    diag: &DiagnosticsForPathLens, // from LspState
    git: &LineStatusForPathLens,   // from GitState
    ui: &RenderSettingsLens,       // from UiState
) -> RenderedLine {
    // compose from all sources
}
```

The "app" is the tuple of all domain atoms held by the runtime; drivers mutate their own atom.

### Sizing domain atoms

Each domain's atom is **scoped to what that domain owns as ground truth**. It should NOT hold derived values — those are queries. Rule of thumb:

- **In the atom**: what this driver writes to, what the system must remember across ticks.
- **Not in the atom**: anything computable from (this atom + other atoms).

E.g. "is this buffer dirty?" might or might not be in `BufferState` — if dirtiness is a cheap query (compare current contents to on-disk contents), it's a query. If on-disk contents aren't in any atom, it's stored as a `bool` in `BufferState` and maintained by the reducer.

---

## Per-domain muts and reducers

Each domain has its own `Mut` enum and its own reducer. The reducers remain **trivial** in the sense of the current `CLAUDE.md` Principle 1: one or two lines of field assignment per variant.

```rust
pub enum BufferMut {
    OpenBuffer { path: CanonPath, doc: Rope, cursor: Cursor },
    CloseBuffer(CanonPath),
    SetActive(Option<CanonPath>),
    InsertAt { path: CanonPath, at: usize, text: String },
    DeleteAt { path: CanonPath, at: usize, len: usize },
    MoveCursor { path: CanonPath, to: Cursor },
    // ... fine-grained, one field (or cohesive pair) per variant
}

impl BufferMut {
    pub fn apply(self, s: &mut BufferState) {
        match self {
            BufferMut::OpenBuffer { path, doc, cursor } => {
                s.contents.insert(path.clone(), doc);
                s.versions.insert(path.clone(), 0);
                s.edits.insert(path.clone(), im::Vector::new());
                s.cursors.insert(path, cursor);
            }
            BufferMut::SetActive(a)       => { s.active = a; }
            BufferMut::MoveCursor { path, to } => { s.cursors.insert(path, to); }
            BufferMut::InsertAt { path, at, text } => {
                apply_insert(s, &path, at, &text);  // bumps version, appends edit
            }
            // ...
        }
    }
}
```

The "dispatch layer" (below) translates external events into the right sequence of fine-grained muts across domains.

### Why the fine-grained mut principle still holds

The current codebase's principle ("trivial reducer, all logic upstream in combinators") was about keeping `apply` mechanical so the combinator graph could encode all decisions declaratively. In the new architecture the equivalent is: **all decisions in the dispatch layer, reducers are mechanical.** Same property, different upstream shape.

---

## The event handler (replaces `model()`)

A single event type, routed to the right domain(s):

```rust
pub enum Event {
    // Input drivers
    Key(KeyEvent),
    Resize { w: u16, h: u16 },
    FsChanged(PathBuf),
    ClockTick(Instant),
    LspNotif(LspNotification),

    // Resource driver completions
    FileRead(CanonPath, Result<String, io::Error>),
    FileSaved(CanonPath, Result<(), io::Error>),
    GitScanDone { statuses, branch },
    LspResponse(LspResponseId, Result<LspResponse, LspError>),
    SyntaxParsed { path, version, highlights },
    FindFileListed(CanonPath, Vec<DirEntry>),
    // ...
}
```

`apply_event` is the main dispatch — a `match` over `Event` that decides which muts to produce for which domains, then applies them:

```rust
fn apply_event(
    buffers: &mut BufferState,
    git:     &mut GitState,
    lsp:     &mut LspState,
    ui:      &mut UiState,
    /* ... */
    event: Event,
) {
    match event {
        Event::Key(k) => dispatch_key(k, buffers, ui, /*...*/),
        Event::Resize { w, h } => {
            UiMut::Resize(w, h).apply(ui);
        }
        Event::FsChanged(p) => {
            dispatch_fs_change(p, buffers, git);
        }
        Event::FileRead(p, Ok(contents)) => {
            BufferMut::OpenBuffer { path: p, doc: Rope::from(contents), cursor: Cursor::default() }
                .apply(buffers);
        }
        Event::LspResponse(id, Ok(resp)) => {
            dispatch_lsp_response(id, resp, lsp, buffers, ui);
        }
        // ...
    }
}
```

`dispatch_key`, `dispatch_fs_change`, `dispatch_lsp_response` are the small per-input-source fan-out functions. Each reads the state it needs, decides what it means in context, and applies the appropriate muts across the relevant domains. This is where the decision-making from the FRP combinator layer lives now.

### What dispatch functions look like

```rust
fn dispatch_key(key: KeyEvent, buffers: &mut BufferState, ui: &mut UiState) {
    // Consult keymap (in ui or config atom) given current context.
    let Some(action) = ui.keymap.resolve(&key, ui.focus) else { return };
    dispatch_action(action, buffers, ui);
}

fn dispatch_action(action: Action, buffers: &mut BufferState, ui: &mut UiState) {
    match action {
        Action::MoveDown => {
            let Some(path) = buffers.active.clone() else { return };
            let cur = buffers.cursors.get(&path).copied().unwrap_or_default();
            let next = compute_cursor_down(buffers, &path, cur);
            BufferMut::MoveCursor { path, to: next }.apply(buffers);
        }
        Action::Save => {
            let Some(path) = buffers.active.clone() else { return };
            let dirty = is_dirty(buffers, &path);
            if dirty {
                BufferMut::MarkSaving(path.clone()).apply(buffers);
                // Dispatch the async write (see below).
                UiMut::Alert(info("saving...")).apply(ui);
            }
        }
        // ...
    }
}
```

These dispatch functions are direct code, not streams. They aren't *composable* the way streams were (can't subscribe multiple times to the same filtered slice) — but they are *local* and readable top-to-bottom.

---

## The query layer (replaces `derived()`)

The renderer calls one (or a few) top-level queries. Those queries call sub-queries. Everything is memoized by `drv` based on the lenses declared on each memo.

```rust
#[drv::memo]
fn render_frame(
    buf: &BufferActiveLens,
    lsp: &LspAllForActiveLens,
    git: &GitAllForActiveLens,
    ui:  &UiChromeLens,
) -> Frame {
    let mut f = Frame::new(ui.dims);
    render_gutter(&mut f, buf, git);
    render_text(&mut f, buf);
    render_diagnostics_overlay(&mut f, buf, lsp);
    render_status_bar(&mut f, ui, buf);
    if ui.show_side_panel { render_sidebar(&mut f, ui); }
    f
}
```

Each `render_*` helper is itself a `#[drv::memo]` taking the slices it needs. Only the parts whose input lenses changed recompute.

### Rebase queries for position-sensitive data

This is the critical pattern for handling async mismatch. LSP diagnostics arrive stamped with the buffer version they were computed against; the buffer may have since advanced. A rebase query projects old-version data onto the current version:

```rust
#[drv::memo]
fn current_diagnostics(
    buf: &BufferEditsForPathLens,   // current version + edit log
    lsp: &DiagnosticsRawLens,       // (version_at, raw diagnostics)
) -> Vec<Diagnostic> {
    let Some(Loaded::Ready((v_at, raw))) = lsp.diagnostics.get(&buf.path) else {
        return vec![];
    };
    let v_now = buf.version(&buf.path);
    if *v_at == v_now {
        raw.clone()
    } else {
        let deltas = buf.edits_between(*v_at, v_now);
        rebase_diagnostics(raw, &deltas)
    }
}
```

`rebase_diagnostics` is a small pure function: given diagnostics at v=5 and edits between v=5 and v=8, shift offsets. Same pattern for `current_git_hunks`, `current_inlay_hints`, etc. Each position-sensitive data type gets its own rebase function (they may behave slightly differently — a diagnostic spanning a deletion might collapse or extend, domain-specific).

### What goes in the edit log

Compact enough to rebase all the position-sensitive data kinds you care about:

```rust
pub enum Edit {
    Insert { at: usize, len: usize },  // byte-based
    Delete { at: usize, len: usize },
    // extendable as needed
}
```

Stored as `im::Vector<Edit>` per path; each applied edit appends and bumps `versions[path]`. Can be truncated behind the oldest version referenced by any resource-driver result stored in other atoms. Because `im::Vector` is structurally shared, retention is cheap.

---

## Cache-miss dispatch

When a query reads a domain atom and finds data it needs marked `Idle` (or absent), it:

1. Pushes a request onto a thread-local collector.
2. Returns a placeholder (`Pending`, empty, default).

The loop drains the collector after the render query returns, dedupes, dispatches fire-and-forget to the appropriate resource driver, and marks the resource as `Pending` in the atom.

```rust
pub enum Loaded<T> {
    Idle,
    Pending,
    Ready(T),
    Error(Box<dyn std::error::Error + Send + Sync>),
}

#[drv::memo]
fn syntax_tree(buf: &BufferContentsLens) -> Loaded<SyntaxTree> {
    let Some(path) = &buf.active else { return Loaded::Idle };
    match buf.contents.get(path) {
        Some(rope) => {
            request_load(Request::ParseSyntax(path.clone(), buf.version(path)));
            // Returns Pending; the parsed tree will arrive as a SyntaxParsed event.
            Loaded::Pending
        }
        None => {
            request_load(Request::ReadFile(path.clone()));
            Loaded::Pending
        }
    }
}
```

`request_load` is a thread-local side effect that does not participate in the query's return value, so `drv`'s memoization is unaffected. Dedup happens at the dispatch layer (check state before spawning).

### Why side channel vs explicit return

Explicit `(Output, Vec<Request>)` returns compose poorly through many nested memo calls — every parent has to thread child requests. Side channel keeps memos' return types clean. The ergonomic cost is "don't spawn parallel renders that race on the collector," which the sync handler structure naturally prevents.

---

## Drivers: two kinds

Previously every driver was a single object with input and output streams. In the new arch they split by role:

### Input drivers

Run in the background; push `Event`s into the handler. Similar to before but with a tighter contract.

| Current driver | Becomes input driver that emits |
|----------------|----------------------------------|
| `terminal-in`  | `Event::Key`, `Event::Resize`, focus events |
| `fs` (watch side) | `Event::FsChanged` |
| `lsp` (server-pushed notifications) | `Event::LspNotif` |
| `timers`       | `Event::ClockTick(name)` |

### Resource drivers

Idle until `dispatch.spawn(request)`. They do async work on their own thread / runtime task. When done, post the result as an `Event::*Done(...)` on the same channel.

| Current driver | Becomes resource driver for |
|----------------|-----------------------------|
| `fs` (read/list side) | `Request::ReadFile`, `Request::ListDir`, `Request::FindFile` |
| `docstore`     | `Request::ReadFile`, `Request::Write` (save) |
| `lsp` (request side) | `Request::LspGotoDef`, `Request::LspFormat`, `Request::LspCompletion`, ... |
| `git`          | `Request::GitScan`, `Request::GitLineStatus` |
| `syntax`       | `Request::ParseSyntax(path, version)` |
| `file-search`  | `Request::Search`, `Request::Replace` |
| `gh-pr`        | `Request::LoadPr`, `Request::PollPr` |
| `clipboard`    | `Request::ClipboardRead`, `Request::ClipboardWrite` |
| `workspace`    | `Request::LoadSession`, `Request::SaveSession`, `Request::FlushUndo` |

Some drivers have both roles (fs is the most obvious — watches AND reads).

See `DRIVER-INVENTORY-PLAN.md` for the full translation table.

---

## Lifecycle

The runtime owns the domain atoms and a channel of `Event`s:

```rust
struct Runtime {
    buffers:  BufferState,
    git:      GitState,
    lsp:      LspState,
    ui:       UiState,
    /* ... */
    events:   Receiver<Event>,
    dispatch: Dispatch,
}

impl Runtime {
    fn new() -> Self { /* construct empty atoms, spawn input drivers */ }

    fn run_one(&mut self, event: Event) {
        apply_event(&mut self.buffers, &mut self.git, &mut self.lsp, &mut self.ui, event);
        REQUEST_COLLECTOR.with(|c| c.clear());
        let frame = render_frame(&self.buffers, &self.lsp, &self.git, &self.ui);
        let requests = REQUEST_COLLECTOR.with(|c| c.drain());
        for req in dedupe(requests) {
            if !self.is_pending(&req) {
                self.mark_pending(&req);
                self.dispatch.spawn(req);
            }
        }
        self.terminal.draw(&frame);
    }
}
```

The outer loop — wherever it lives (tokio `select!`, mio event loop, thread-with-channel) — calls `run_one` per event. Nothing else.

### Startup

Startup is the same shape as any other event flow:

1. Runtime constructs empty atoms (with default `UiState`, empty `BufferState`, etc.) and starts input drivers.
2. A synthetic `Event::Startup(args)` is pushed first.
3. `apply_event` handles it by dispatching `Request::LoadSession`, `Request::ReadConfig`, etc.
4. Those results arrive as events; the first render with meaningful content happens after enough state is loaded.

No special "startup mode" code paths — just events and dispatched requests.

### Shutdown

On `Event::Quit`:

1. `apply_event` transitions `UiState::phase` to `Exiting` and dispatches `Request::SaveSession`, `Request::FlushUndo`.
2. Runtime checks after each event: if phase is `Exiting` and there are no pending resource requests, shut down.
3. Pending-request tracking is already needed for dedup; reuse it here.

---

## What happens to the current principles

The current `CLAUDE.md` lists 10 principles for FRP code. Here's how each maps (or doesn't) to the new arch:

| # | FRP principle | Query-driven equivalent |
|---|---------------|-------------------------|
| 1 | Trivial reducer ("just assign") | **Carries.** Per-domain reducers must still be one-to-few lines of field assignment. |
| 2 | Split one signal into multiple muts | **Carries.** Dispatch layer produces multiple fine-grained muts (possibly across domains) per event. |
| 3 | Max ~3 lines per combinator closure | **Dissolves.** No combinators. Replacement principle: dispatch functions max ~20 lines, further extraction into named helpers. |
| 4 | Map-Dedupe-Filter ordering | **Dissolves.** No filter-then-dedupe issue since there are no streams. |
| 5 | Never derive from state then sample state | **Dissolves.** No stream derivation. |
| 6 | Group streams in `_of` functions | **Adapts.** Group dispatch helpers by domain (`dispatch_key`, `dispatch_lsp_response`, `dispatch_fs_change`). |
| 7 | Fine-grained `Mut` variants | **Carries** per domain. |
| 8 | Explicit invariant maintenance post-fold | **Adapts.** Any cross-domain invariant (e.g. "active_tab must be a buffer in BufferState") is enforced by the dispatch layer; or by a post-tick normalizer if necessary. |
| 9 | No mega-dispatcher | **Adapts.** The `Action` dispatcher in `dispatch_action` may approach this shape. Keep it split by domain and by action group; avoid a single 500-line match. |
| 10 | No `flat_map` to `Vec` | **Dissolves.** |

A new principle specific to the query architecture:

**Query purity.** Queries are pure functions of their lens inputs and value parameters. They may call `request_load` (side channel) but must not otherwise observe or cause mutation. Two queries with the same inputs must return the same output — this is what makes memoization correct.

---

## Multi-crate organization

Each atom and its memos must live in the same crate (`drv::assemble!()` requirement). One natural layout:

```
crates/
  state-buffer/   BufferState + buffer muts + rebase functions + buffer queries
  state-git/      GitState + git muts + git queries
  state-lsp/      LspState + lsp muts + lsp queries
  state-ui/       UiState + ui muts + render queries
  state-session/  SessionState + session muts + session queries
  state-search/   SearchState + search muts + search queries
  ...
  runtime/        Event enum, apply_event, dispatch functions, Runtime
  drivers/        driver implementations (each existing driver, rewritten or kept)
  led-bin/        main()
```

Cross-domain queries (like `render_frame` that touches buffer + lsp + git + ui) live in the crate that naturally owns the integration — probably `state-ui` since that's what the renderer cares about. Or a top-level `queries/` crate. TBD during execution.

---

## Testing implications

The query architecture makes testing easier in two ways:

1. **Queries are pure.** Test by constructing domain atoms with specific data, calling the query, asserting on output. No runtime, no harness required.
2. **The handler is pure.** `apply_event(&mut state, event)` has no dependencies on runtimes or I/O. Test by constructing state, calling `apply_event`, asserting on resulting state.

Golden tests layer on top of these to lock down end-to-end behavior. See `GOLDENS-PLAN.md`.

---

## What's deliberately left open

The following are deferred to execution:

- **Exact edit-log format and rebase function signatures.** Depends on what position-sensitive data kinds need rebasing. Start with diagnostics; generalize as others come online.
- **Dispatch function shape.** Whether dispatch is one big match in `apply_event` or per-domain functions (`dispatch_buffer_event`, `dispatch_lsp_event`). Probably per-domain, but cross-domain events (keypresses) cut across domains.
- **How `request_load` is parameterized.** Thread-local vs passed explicitly through memo args. Thread-local is ergonomic; explicit is traceable. Both work.
- **How `Dispatch` is implemented.** Tokio tasks, thread pool, single background thread per driver. Not architecturally material; pick whatever's operationally simplest.
- **Whether to keep `Action` as a distinct layer.** Currently keybindings → Action → state change. The new arch might keep this (Action is useful for testing, macros, command palette) or collapse it into direct muts. Likely keep.
- **Macro recording and playback.** Currently a slice of `AppState`. In the new arch it's a slice of `UiState` (or a `MacroState` atom). Playback re-dispatches actions; integrates cleanly with `Action`-level events.

---

## Summary of invariants

Anyone reading code in the new architecture should be able to rely on these:

- **No I/O in queries.** A query that needs data it doesn't have returns a `Pending` placeholder and requests a load. It does not read files, make network calls, or spawn anything.
- **No blocking in the handler.** `apply_event` and the `tick` function return in microseconds. All async work is dispatched, never awaited.
- **Domain atoms are only mutated by their domain's reducer.** No code outside `crate::state_git::` mutates a field on `GitState`.
- **Queries are pure.** Same inputs → same output. No hidden reads.
- **Position-sensitive data is version-stamped at its source.** Current-time projection is a query, not a mutation.
- **Muts are fine-grained.** One variant per meaningful state change; reducer is mechanical.
- **Events are coarse, dispatch is fine.** One external event produces many fine-grained muts across relevant domains via the dispatch layer.
