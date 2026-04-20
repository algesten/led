# Milestone 2 — cursor, movement, viewport scrolling

The second vertical slice. After M1 the rewrite binary can open files
and switch tabs; after M2 a visible cursor moves around inside the
active tab and the body scrolls to keep it in view.

Prerequisite reading (diff vs MILESTONE-1.md):

1. `../../../drv/EXAMPLE-ARCH.md` — stays the canonical architecture
   reference. No new patterns enter in M2.
2. `MILESTONE-2-SCOPE.md` — the short scope note this doc answers.
3. `MILESTONE-1.md` — the baseline this milestone extends.

---

## Goal

```
$ cargo run -p led -- Cargo.toml
```

- Arrow keys move a visible cursor inside the active tab's body.
- Body scrolls when the cursor would leave the visible rows; scroll
  state persists per tab (switching tabs preserves both cursor and
  scroll).
- `Tab` / `Shift-Tab` still cycle tabs; `Ctrl-C` still quits; resize
  still re-renders. Nothing M1 shipped regresses.

## Scope

### In
- `Cursor { line, col }` + `Scroll { top }` as per-tab state, stored
  on `Tab` in `state-tabs`.
- `Up` / `Down` / `Left` / `Right` dispatch on the active tab.
- `Home` / `End` / `PageUp` / `PageDown` as cheap extensions.
- Scroll-follows-cursor invariant maintained in dispatch, using the
  current `Terminal.dims` for viewport size.
- `body_model` reads active tab's cursor + scroll, emits a scrolled
  slice and a body-relative cursor position.
- `Frame.cursor: Option<(u16, u16)>` — absolute screen coords for the
  painter. `paint` emits `cursor::Show` + `cursor::MoveTo` when `Some`,
  keeps `cursor::Hide` when `None`.

### Out
- Editing (M3).
- Word / paragraph movement.
- Selection / region / mark / jump list.
- Multi-cursor.
- Mouse input.
- Configurable keybindings (M5 — config).
- Scroll margin padding (nice-to-have; deferred).

## Design questions, answered

### Q1 — Where does cursor state live?

**On `Tab`.** `Tab` already models per-view state; cursor is
per-view (two tabs on the same path must be able to hold independent
positions). MILESTONE-1.md's `Tab` left a comment reserving the field:

```rust
pub struct Tab {
    pub id: TabId,
    pub path: CanonPath,
    // M2: pub is_preview: bool,
    // M3: pub cursor: Cursor,  ← becomes M2 now
}
```

`Cursor` + `Scroll` types live in `state-tabs` (not `core/`). They
are atom-field shapes specific to the tab model; promoting them to
`core/` is premature until another atom needs them.

### Q2 — Where does scroll offset live?

**Also on `Tab`.** Same argument as Q1. Resulting shape:

```rust
// in state-tabs
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor { pub line: usize, pub col: usize }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll { pub top: usize }   // first visible body row

pub struct Tab {
    pub id:     TabId,
    pub path:   CanonPath,
    pub cursor: Cursor,
    pub scroll: Scroll,
}
```

`usize` matches ropey's line-index API. Cursor/scroll positions are
in buffer coordinates, not screen coordinates — the painter translates.

### Q3 — Does `Tabs` get mutated per keystroke now?

Yes. Every arrow key mutates the active `Tab`'s `cursor` (and
sometimes `scroll`). That invalidates any memo whose input projects
`&imbl::Vector<Tab>`, including `file_load_action` and
`tab_bar_model`.

**That's acceptable.** Both memos recompute cheaply (O(n_tabs) over a
label list / path filter) and their outputs equal the previous value
when only cursor moved, so drv's value-equality check at the **output
side** protects every downstream memo (render_frame hits, paint
skipped). The only cost is the inner recompute, which is negligible
at n_tabs ≲ 100.

We revisit if profiling shows otherwise — plausible fix would be
promoting cursor+scroll to a sibling `HashMap<TabId, TabView>` inside
`Tabs`, at the cost of splitting data that belongs together. Not
doing it speculatively.

The `body_model` memo is the one memo that *should* invalidate on
cursor moves: its output depends on cursor + scroll. So no narrower
projection for it either.

### Q4 — Does `driver-terminal/native` need new keys?

No. `KeyCode` already carries `Left` / `Right` / `Up` / `Down` /
`Home` / `End` / `PageUp` / `PageDown` and `translate_key` already
maps the crossterm variants. M2 is purely a dispatch extension.

### Q5 — How is the cursor drawn?

**Via a `Frame.cursor: Option<(u16, u16)>` field**, set by
`render_frame`, honoured by `paint`. The alternative — painter
recomputes cursor position from the body — couples layout logic to
the painter. Using Frame as the single source of truth for what the
screen should look like (cells + cursor) keeps paint a dumb emitter.

Two-step translation:

1. `body_model` returns `BodyModel::Content { lines, cursor }` where
   `cursor: Option<(u16, u16)>` is **body-relative** screen coords
   (`(body_row, col)`). `None` when the cursor is outside the scroll
   window (shouldn't happen if dispatch maintains the invariant, but
   the model is defensive).
2. `render_frame` translates the body-relative cursor by the tab-bar
   offset (1 row) and stores the result in `Frame.cursor`.

Non-`Content` body variants (`Empty`, `Pending`, `Error`) produce no
cursor.

### Q6 — Testing

Unit tests at the dispatch + memo level are the primary layer, as M1.

- `dispatch_key` with each arrow key, over known-shape fixtures, asserting
  the active tab's cursor + scroll after the call.
- `body_model` over a small rope with various `(cursor, scroll, dims)`
  tuples, asserting the visible-slice start and the body-relative cursor
  coordinates.
- `render_frame` composition test asserting `Frame.cursor` equals the
  expected absolute coordinates.

Golden coverage under `goldens/scenarios/actions/move_*` is out of
scope for M2 code — the goldens harness + `--test-clock` are not
wired yet. Listed in MILESTONE-2-SCOPE.md's "stretch / deferred"
bucket; revisit once the harness lands.

---

## Crates that change

| Crate | Change |
|-------|--------|
| `state-tabs/` | Add `Cursor`, `Scroll` types. Extend `Tab` with both fields. |
| `driver-terminal/core/` | `BodyModel::Content` gains `cursor: Option<(u16, u16)>`. `Frame` gains `cursor: Option<(u16, u16)>`. No new dependencies. |
| `driver-terminal/native/` | `paint` emits `cursor::Show` + `cursor::MoveTo` when `Frame.cursor` is `Some`, keeps `cursor::Hide` otherwise. |
| `runtime/src/query.rs` | `body_model` reads cursor + scroll from the active tab and emits the visible slice + body-relative cursor. `render_frame` fills `Frame.cursor` from `BodyModel`. |
| `runtime/src/dispatch.rs` | `dispatch` + `dispatch_key` grow a `&BufferStore` and `&Terminal` parameter. New arrow-key branches mutate the active tab's cursor (clamped to rope extent) and recompute scroll to keep the cursor visible. |
| `runtime/src/lib.rs` | Call-site updates for the wider `dispatch` signature. |
| `runtime/src/trace.rs` | No new variants — existing `key_in` line already traces each arrow keypress. |

**No new driver crates. No new atoms.** M2 is an extension inside the
shape M1 already established.

---

## Detailed design

### `state-tabs` additions

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Zero-based line index in the buffer rope.
    pub line: usize,
    /// Zero-based grapheme column on that line. (M2: char index; revisit
    /// for unicode widths when syntax work comes online.)
    pub col: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll {
    /// Zero-based buffer row shown at the top of the body viewport.
    pub top: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tab {
    pub id: TabId,
    pub path: CanonPath,
    pub cursor: Cursor,
    pub scroll: Scroll,
}
```

### `body_model` — new signature

```rust
#[drv::memo(single)]
pub fn body_model<'a, 'b>(
    store: StoreLoadedInput<'a>,
    tabs:  TabsActiveInput<'b>,
    dims:  Dims,
) -> BodyModel {
    // Active tab lookup (unchanged).
    let Some(id)  = *tabs.active else { return BodyModel::Empty };
    let Some(tab) = tabs.open.iter().find(|t| t.id == id) else { return BodyModel::Empty };

    let path_display = tab.path.display().to_string();
    match store.loaded.get(&tab.path) {
        None | Some(LoadState::Pending) => BodyModel::Pending { path_display },
        Some(LoadState::Error(msg))     => BodyModel::Error { path_display, message: (**msg).clone() },
        Some(LoadState::Ready(rope))    => render_content(rope, tab.cursor, tab.scroll, dims),
    }
}
```

`render_content` is a private helper:

```rust
fn render_content(rope: &Rope, cursor: Cursor, scroll: Scroll, dims: Dims) -> BodyModel {
    let body_rows = dims.rows.saturating_sub(1) as usize;
    let line_count = rope.len_lines();

    // The visible buffer rows [scroll.top, scroll.top + body_rows).
    let lines: Vec<String> = (scroll.top..scroll.top + body_rows)
        .filter(|&ln| ln < line_count)
        .map(|ln| {
            let s = rope.line(ln).to_string();
            let s = s.strip_suffix('\n').unwrap_or(&s);
            let s = s.strip_suffix('\r').unwrap_or(s);
            truncate_to_cols(s, dims.cols as usize)
        })
        .collect();

    // Body-relative cursor position, if the cursor is in view.
    let cursor = visible_cursor(cursor, scroll, dims);
    BodyModel::Content { lines, cursor }
}

fn visible_cursor(c: Cursor, s: Scroll, dims: Dims) -> Option<(u16, u16)> {
    let body_rows = dims.rows.saturating_sub(1) as usize;
    if c.line < s.top || c.line >= s.top + body_rows { return None; }
    let row = (c.line - s.top) as u16;
    let col = c.col.min(dims.cols as usize) as u16;
    Some((row, col))
}
```

### `render_frame` — fills `Frame.cursor`

```rust
#[drv::memo(single)]
pub fn render_frame<'t, 'b, 'a>(
    term:  TerminalDimsInput<'t>,
    store: StoreLoadedInput<'b>,
    tabs:  TabsActiveInput<'a>,
) -> Option<Frame> {
    let dims = (*term.dims)?;
    let tab_bar = tab_bar_model(tabs);
    let body    = body_model(store, tabs, dims);
    let cursor  = match &body {
        BodyModel::Content { cursor: Some((r, c)), .. } => Some((*c, r + 1)),  // (col, row), +1 for tab bar
        _ => None,
    };
    Some(Frame { tab_bar, body, cursor, dims })
}
```

Two notes on the conversion:

- crossterm's `cursor::MoveTo(col, row)` takes column-major; store
  `Frame.cursor` as `(col, row)` to match the eventual call.
- `+1` accounts for the tab-bar row — `body_top` in `paint_body`.

### `Frame` + `BodyModel` in `driver-terminal/core/`

```rust
pub enum BodyModel {
    Empty,
    Pending { path_display: String },
    Error   { path_display: String, message: String },
    Content {
        lines:  Vec<String>,
        cursor: Option<(u16, u16)>,   // body-relative (row, col)
    },
}

pub struct Frame {
    pub tab_bar: TabBarModel,
    pub body:    BodyModel,
    /// Absolute (col, row) for the terminal cursor. `None` = hide.
    pub cursor:  Option<(u16, u16)>,
    pub dims:    Dims,
}
```

### `paint` — cursor emission

```rust
pub fn paint(frame: &Frame, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, terminal};
    queue!(out, cursor::Hide, cursor::MoveTo(0, 0))?;
    paint_tab_bar(&frame.tab_bar, frame.dims, out)?;
    paint_body(&frame.body, frame.dims, out)?;
    queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;

    if let Some((col, row)) = frame.cursor {
        queue!(out, cursor::MoveTo(col, row), cursor::Show)?;
    }
    out.flush()
}
```

The per-frame `cursor::Hide` in the prelude prevents flicker while
drawing; the trailing `Show` + `MoveTo` places the cursor on top of
the finished frame.

### `dispatch` — arrow keys + scroll

The signature grows to include the data dispatch needs for clamping:

```rust
pub fn dispatch(
    ev:       Event,
    tabs:     &mut Tabs,
    store:    &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome;

pub fn dispatch_key(
    k:        KeyEvent,
    tabs:     &mut Tabs,
    store:    &BufferStore,
    terminal: &Terminal,
) -> DispatchOutcome;
```

Arrow handling:

```rust
match (k.modifiers, k.code) {
    (m, KeyCode::Up)       if m.is_empty() => move_cursor(tabs, store, terminal, Move::Up),
    (m, KeyCode::Down)     if m.is_empty() => move_cursor(tabs, store, terminal, Move::Down),
    (m, KeyCode::Left)     if m.is_empty() => move_cursor(tabs, store, terminal, Move::Left),
    (m, KeyCode::Right)    if m.is_empty() => move_cursor(tabs, store, terminal, Move::Right),
    (m, KeyCode::Home)     if m.is_empty() => move_cursor(tabs, store, terminal, Move::LineStart),
    (m, KeyCode::End)      if m.is_empty() => move_cursor(tabs, store, terminal, Move::LineEnd),
    (m, KeyCode::PageUp)   if m.is_empty() => move_cursor(tabs, store, terminal, Move::PageUp),
    (m, KeyCode::PageDown) if m.is_empty() => move_cursor(tabs, store, terminal, Move::PageDown),
    // ... Tab / Shift-Tab / Ctrl-C unchanged ...
}
```

`move_cursor` locates the active tab, reads the rope (if loaded),
computes the new cursor, and recomputes scroll:

```rust
fn move_cursor(tabs: &mut Tabs, store: &BufferStore, terminal: &Terminal, m: Move) {
    let Some(active) = tabs.active else { return };
    let Some(idx)    = tabs.open.iter().position(|t| t.id == active) else { return };
    let rope = match store.loaded.get(&tabs.open[idx].path) {
        Some(LoadState::Ready(r)) => r.clone(),
        _ => return,                      // no data → cursor stays put
    };

    let tab = &mut tabs.open[idx];
    let body_rows = terminal.dims.map(|d| d.rows.saturating_sub(1) as usize).unwrap_or(0);

    tab.cursor = apply_move(tab.cursor, &rope, m, body_rows);
    tab.scroll = adjust_scroll(tab.scroll, tab.cursor, body_rows);
}
```

`apply_move` is pure and unit-testable:

```rust
fn apply_move(c: Cursor, rope: &Rope, m: Move, body_rows: usize) -> Cursor {
    let line_count = rope.len_lines().max(1);
    let clamp_col = |line: usize, col: usize| -> usize {
        let len = line_char_len(rope, line);
        col.min(len)
    };
    match m {
        Move::Up        => Cursor { line: c.line.saturating_sub(1),                col: clamp_col(c.line.saturating_sub(1), c.col) },
        Move::Down      => { let nl = (c.line + 1).min(line_count - 1); Cursor { line: nl, col: clamp_col(nl, c.col) } }
        Move::Left      => Cursor { line: c.line, col: c.col.saturating_sub(1) },
        Move::Right     => Cursor { line: c.line, col: clamp_col(c.line, c.col + 1) },
        Move::LineStart => Cursor { line: c.line, col: 0 },
        Move::LineEnd   => Cursor { line: c.line, col: line_char_len(rope, c.line) },
        Move::PageUp    => { let nl = c.line.saturating_sub(body_rows.max(1));     Cursor { line: nl, col: clamp_col(nl, c.col) } }
        Move::PageDown  => { let nl = (c.line + body_rows.max(1)).min(line_count - 1); Cursor { line: nl, col: clamp_col(nl, c.col) } }
    }
}

fn adjust_scroll(s: Scroll, c: Cursor, body_rows: usize) -> Scroll {
    if body_rows == 0 { return s; }
    if c.line < s.top {
        Scroll { top: c.line }
    } else if c.line >= s.top + body_rows {
        Scroll { top: c.line + 1 - body_rows }
    } else {
        s
    }
}
```

`line_char_len(rope, line)` is a small helper: the line's character
count with any trailing `\n` / `\r\n` stripped. Bounds checked against
`rope.len_lines()`.

Clarifications on intent:

- **Column preservation on Up/Down.** For M2 we clamp immediately to
  the destination line's length rather than persisting a "preferred
  column" across movements. That's the minimum for correctness and
  matches the scope doc's note that column-preservation is optional
  polish.
- **No wrap at line boundaries on Left/Right.** Left at col 0 stays
  at col 0; Right at EOL stays at EOL. Wrapping is a later refinement.
- **PageUp / PageDown step by one viewport.** Standard; the body-rows
  value used is a single screenful, not a fraction.

### Why scroll adjustment runs in dispatch (not as a memo)

Keeping cursor-in-view is a **desired state** rule, but its output
(scroll) is also source state the user owns (tabs remember their
scroll across tab switches). Computing it in a query requires a
writeback step — essentially the `execute` pattern, awkward when the
target is a user-decision source with no driver.

Dispatch is the simpler home: the function that just mutated
`cursor` also mutates `scroll`. No extra query cache, no writeback
plumbing. If M4+ introduces standalone scroll inputs (e.g. mouse
wheel, `C-v` / `M-v`), the rule still lives in dispatch and stays
symmetrical.

### Resize handling

Resize does **not** re-clamp scroll or cursor for M2. If the terminal
shrinks enough to push the cursor off-screen, the cursor remains in
buffer coordinates; next keystroke re-clamps. This keeps M2 cheap
and stays consistent with the "dispatch owns the invariant" rule:
`Resize` is already applied by `TerminalInputDriver.process` and
isn't routed through `dispatch_key`. A small follow-up could route
`Event::Resize` through dispatch to re-clamp — defer until it
actually annoys someone.

---

## Trace format — unchanged

Arrow keys are already surfaced by the M1 `key_in` line
(`key_in | key=Left`, etc.). No new trace variants.

A future render-trace extension (`render_tick | cursor=(col,row)`) is
obvious but premature — M2 has no golden scenarios that need it.

## Done criteria

- `cargo run -p led -- <multi-line-file>` shows a blinking cursor at
  the start of the file; arrow keys move it; the body scrolls when
  the cursor would leave the viewport; switching tabs preserves each
  tab's cursor/scroll; Ctrl-C still exits cleanly.
- All M1 tests green; new `state-tabs` / `body_model` / dispatch
  unit tests green.
- No new clippy warnings introduced.

## Growth path hooks

- **M3 — editing.** `apply_move` primitives carry over. Editing adds
  a `BufferEdits` source and a rebase query; cursor clamping may
  grow to consult a version counter so async diagnostics can be
  translated into post-edit coordinates.
- **M5 — config / keymap.** `dispatch_key` grows a config lookup
  between keypress and the `move_cursor` call; the move functions
  remain pure and keymap-agnostic.
- **Render trace for goldens.** Adding `cursor=(col,row)` to the
  `render_tick` line is mechanical once the harness needs it.
