# Milestone 11 — File browser sidebar

Eleventh vertical slice, and the biggest one to date. After M11 the
left 25 columns of the screen hold a persistent tree-view of the
workspace: directories expand/collapse, arrow keys scan files with
live preview, `Enter` promotes a preview to a real tab. This is
the milestone that finally starts moving the goldens counter
off 0/257 — every existing golden expects the side panel to be
visible.

Prerequisite reading:

1. `docs/spec/file-browser.md` — whole file. The authoritative
   reference for layout, state, keybindings, and edge cases.
2. `docs/spec/ui-chrome.md` § "Side panel / file browser" —
   layout integration with the tab bar + status bar.
3. `MILESTONE-9.md` § "D11 — Dims helpers don't grow yet" — M11
   adds the Layout struct that M9 deferred.
4. Legacy `led/src/model/action/browser.rs` and
   `crates/state/src/lib.rs:1532-1594` for the exact mutation
   semantics we're porting.
5. One representative golden: `goldens/scenarios/keybindings/
   browser/right/frame.snap` — expansion indicators, ordering,
   live-preview tab row.

---

## Goal

```
$ cargo run -p led -- alpha.txt beta.txt sub/nested.txt
# Frame (120 × 40):
#   cols 0..24:   side panel (25-wide)
#   col 25:       │ (border)
#   cols 26..27:  2-col gutter
#   cols 28..:    buffer text
#   row 38:       tab bar
#   row 39:       status bar
# Initial panel contents (sorted by dirs-first, alpha):
#     ▷ sub
#       alpha.txt
#       beta.txt
#
# Alt-Tab               → focus moves to side panel; `Down`/`Up`
#                         now scan the tree, previewing files.
# Down                  → selection on alpha.txt; preview tab opens,
#                         content shows in main pane.
# Down                  → selection on beta.txt; preview replaces.
# Up Up                 → selection on sub.
# Right                 → sub expands (▷ → ▽), nested.txt appears.
# Down                  → selection on nested.txt.
# Enter                 → preview promotes to real tab, focus → Main.
# Alt-Tab               → focus back to side panel.
# Ctrl-q                → every expanded dir collapses; selection &
#                         scroll reset to 0.
# Ctrl-b                → side panel toggles off; body expands.
#                         Alt-Tab auto-swaps focus back to Main.
```

## Scope

### In

- **`state-browser` crate** — new workspace member. Owns
  `BrowserState`, `DirEntry`, `EntryKind`, `TreeEntry`.

  ```rust
  pub struct BrowserState {
      pub root:          Option<CanonPath>,
      pub dir_contents:  imbl::HashMap<CanonPath, imbl::Vector<DirEntry>>,
      pub expanded_dirs: imbl::HashSet<CanonPath>,
      pub entries:       Arc<Vec<TreeEntry>>,  // flattened, for paint
      pub selected:      usize,
      pub scroll_offset: usize,
      pub visible:       bool,                 // Ctrl-b toggle
  }

  pub struct DirEntry {
      pub name: String,
      pub path: CanonPath,
      pub kind: DirEntryKind, // File or Directory
  }

  pub enum EntryKind {
      File,
      Directory { expanded: bool },
  }

  pub struct TreeEntry {
      pub path:  CanonPath,
      pub name:  String,
      pub depth: usize,
      pub kind:  EntryKind,
  }
  ```

  `rebuild_entries(&mut self)` walks from the root, consulting
  `dir_contents` + `expanded_dirs`, and builds `entries`. Ordering
  matches legacy: directories first, then files, both sorted by
  locale-insensitive `name`. Hidden entries (leading `.`) are
  filtered out at this layer, matching legacy.

- **`driver-fs-list` crate (core + native)** — new workspace
  members.

  ```rust
  // core
  pub struct FsListDriver {
      cmd_tx: mpsc::Sender<ListCmd>,
      done_rx: mpsc::Receiver<ListDone>,
      trace: Arc<dyn Trace>,
  }
  pub struct ListCmd { pub path: CanonPath }
  pub struct ListDone { pub path: CanonPath, pub result: Result<Vec<DirEntry>, String> }

  // driver trace hook
  pub trait Trace: Send + Sync {
      fn list_start(&self, path: &CanonPath);
      fn list_done(&self, path: &CanonPath, result: &Result<Vec<DirEntry>, String>);
  }
  ```

  The core crate is driver-isolated (knows only its own ABI
  types); `native` spawns a worker thread that calls
  `std::fs::read_dir`, filters hidden entries, sorts legacy-style,
  and emits `ListDone`.

  Pending requests are tracked on `BufferStore`-style: the memo
  that diffs `expanded_dirs ∪ root ∪ pending_reveal_ancestors`
  against `dir_contents.keys()` emits `ListCmd` entries the
  runtime feeds to `FsListDriver::execute`.

- **Workspace root detection** — simplest possible: use the CWD.
  Most invocations are `cd <project> && led <file>`. A dedicated
  workspace driver (git root detection) is M19 territory.

- **`Focus` state** — new single field `session.focus: Focus`
  where `Focus` is `{ Main, Side }`. Lives on a new small crate
  `state-session` OR as a field on an existing state struct. For
  M11 it's simplest on `BrowserState` itself (`visible` already
  lives there); putting `focus` next to it keeps the chrome-state
  co-located.

  **Decision** (see D3): `focus: Focus` lives on `BrowserState`
  alongside `visible`. One crate; one state surface.

- **Context keymap** — `Keymap` grows a `browser_direct:
  HashMap<KeyEvent, Command>`. Lookup order when dispatching a
  key: if `focus == Side`, consult `browser_direct` first; fall
  back to the normal `direct` table; chord tables unchanged. The
  browser context can't have chords (matches legacy) — a natural
  simplification.

- **New commands**:
  - `ExpandDir`, `CollapseDir`, `CollapseAll`
  - `OpenSelected` (promotes preview or opens fresh)
  - `OpenSelectedBg` (declared; behaves identically to
    `OpenSelected` for now — M11 port of legacy's bound-but-dead
    action)
  - `ToggleSidePanel` (`Ctrl-b`): toggles `browser.visible`
  - `ToggleFocus` (`Alt-Tab`): toggles `browser.focus` between
    Main and Side; if panel isn't visible, show it + focus Side

  Plus overloaded cursor commands: when `focus == Side`,
  `CursorUp`/`CursorDown`/`CursorPageUp`/`CursorPageDown`/
  `CursorFileStart`/`CursorFileEnd` move the selection, not the
  cursor. `CursorLeft`/`CursorRight` route to
  `CollapseDir`/`ExpandDir`. This overload happens inside
  `run_command` — no separate "SideMoveUp" commands needed.

- **Default bindings**:
  ```rust
  // global
  m.bind("ctrl+b",   Command::ToggleSidePanel);
  m.bind("alt+tab",  Command::ToggleFocus);

  // browser context
  m.bind_browser("up",        Command::CursorUp);        // selection up
  m.bind_browser("down",      Command::CursorDown);
  m.bind_browser("left",      Command::CollapseDir);
  m.bind_browser("right",     Command::ExpandDir);
  m.bind_browser("enter",     Command::OpenSelected);
  m.bind_browser("alt+enter", Command::OpenSelectedBg);
  m.bind_browser("pageup",    Command::CursorPageUp);
  m.bind_browser("pagedown",  Command::CursorPageDown);
  m.bind_browser("ctrl+home", Command::CursorFileStart);
  m.bind_browser("ctrl+end",  Command::CursorFileEnd);
  m.bind_browser("ctrl+q",    Command::CollapseAll);
  ```

- **Preview tab** — `Tab` grows `preview: bool`. At most one
  preview tab at a time (invariant enforced on write). Opening
  a file from the browser either:
  - replaces the existing preview (same slot);
  - creates a new preview if none exists.
  `OpenSelected` on the preview's path sets `preview = false`
  (promotes). `OpenSelected` on another file creates a fresh
  preview in the same slot.

  Preview tabs render identically to regular tabs in the tab bar
  (italics / different color is M15 theming territory). M11 does
  not visually distinguish them.

- **Side panel render** — `Layout` struct carves `side_area` +
  `editor_area` from `dims`. `render_frame` composes a
  `SidePanelModel` whenever `browser.visible` is true; the
  painter draws it inside `side_area` (25 cols wide) and a
  `│` border at col 25. Body starts at col 26 with the 2-col
  gutter then text.

  Auto-hide: when `dims.cols < 25 + 25` (min editor width
  threshold), the panel hides regardless of `visible`. Matches
  legacy `min_editor_width` semantics.

  Side panel rows:
  ```
  <indent><chevron><name>
  ```
  Where indent is `depth * 2` spaces, chevron is:
  - `▷ ` for collapsed dir (`\u{25b7}`),
  - `▽ ` for expanded dir (`\u{25bd}`),
  - `  ` (two spaces) for files.

  Rows past the end render blank. The selected row is drawn with
  `ReverseVideo` attribute (no theming yet).

- **FS listing emission** — runtime memo diffs
  `{root} ∪ expanded_dirs` against `dir_contents.keys()` and
  emits one `ListCmd` per path that isn't yet listed. The
  execute phase feeds those into `FsListDriver::execute`.
  Completion round-trips through the ingest phase, updating
  `dir_contents` and calling `browser.rebuild_entries()`.

- **Open behaviour** — `OpenSelected` on a file:
  1. If the selected file's path matches the current preview,
     flip `preview = false` (promote). Focus → Main.
  2. If it's a different file, replace the preview tab's path
     (keeping `preview = true`). Focus → Main.
  3. If there's no preview tab, create one.

  `OpenSelected` on a directory toggles its expansion state —
  equivalent to pressing `Right`/`Left`.

- **Scroll clamping** — after selection change, clamp
  `scroll_offset` so `selected` is within the visible range.
  Visible range = side-panel row height = body rows (same as the
  editor area — the panel occupies full body height).

### Out

Per `ROADMAP.md` and legacy-spec review:

- **`.gitignore` awareness** → M14 (file search reuses the
  `ignore` crate; the browser can adopt it then if we want).
- **Hidden files toggle** → not scheduled. Legacy filters out
  leading-`.` with no toggle; we match.
- **File rename / delete / new file** from the browser → not
  scheduled. Users rename via SaveAs (M12) or delete externally.
- **Reveal-active-file** (auto-expand ancestors on tab
  activation) → M21 (session / persistence). The scaffolding to
  accept a `pending_reveal: Option<CanonPath>` can land later
  without changing M11 shape.
- **Preview auto-close on focus loss** → not scheduled. Legacy
  closes the preview in `close_preview` when focus returns from
  Side to Main without committing; we defer that nuance. M11
  keeps the preview open until a new one replaces it or the
  tab is killed.
- **Visual distinction for preview tabs** (italics / muted
  style) → M15 (theming). For M11 they look identical to real
  tabs.
- **Workspace root detection via `.git` walk** → M19 (git). For
  M11 the root is the process's CWD; `led foo.rs` started from
  the project directory Just Works.
- **FS watcher** (inotify / FSEvents for auto-refresh on
  external changes) → M26 (external file change detection). M11
  re-lists only on user `Right` / `CollapseAll`-then-reopen.
- **Mouse input** → not scheduled.

## Key design decisions

### D1 — `state-browser` owns `visible` and `focus`

Both are UI-chrome state that drive side-panel layout and
dispatch routing. Separate "session-state" or "focus-state"
crates would fragment something that conceptually lives
together. When M14 adds a file-search overlay, `Focus` grows a
variant (`Overlay`) and the same crate absorbs it.

### D2 — Context keymap is additive, not layered

Legacy has a richer modal-keymap system; for M11 we only need
one additional table (`browser_direct`). `lookup_direct` gains
a two-step fallback: if focus is Side, try `browser_direct`
first, then fall through to `direct`. This is O(2×hash) on the
hot path — still trivial.

When M12 (find-file), M13 (isearch), M17 (completion) land, each
adds an overlay context. Extending this scheme linearly (a `Vec`
of context tables keyed by a Focus / OverlayState enum) keeps
the dispatch single-digit ns per lookup.

### D3 — Selection/cursor command overloading, not separate commands

Having `SidebarMoveUp` + `CursorUp` + a keymap that picks which
would mean duplicated arms + more commands. Instead, `run_command`
reads `focus` once at the top and routes each cursor command to
the matching browser action. Net change: `+9 lines in run_command`
vs `+9 new enum variants` + `+9 run_command arms` + `+9 parse
cases`. Net-negative complexity.

### D4 — Single preview slot

Legacy confirms: exactly one preview tab exists at a time. M11
enforces this as an invariant at the three write sites (Open on
preview, Open on different file, force-kill). No "preview chain"
accumulating — matches the golden where a live-preview scan
doesn't spam the tab bar.

Wait — the golden at `goldens/scenarios/keybindings/browser/
right/frame.snap` DOES show three tabs in the bar: `alpha.txt
beta.txt inner.txt`. That's because the harness passed all three
as CLI args (they all appear as real tabs at startup), and the
browser's Right / Down didn't add any more. The preview slot
discipline is correct; the golden's three tabs are from the
initial `--files` arguments, not from preview scanning.

### D5 — `Layout` struct finally introduced

M9 deferred it. M11 carves `side_area` + `editor_area` out of
`Dims`. Everything that previously used `dims.cols` for body
width switches to `layout.editor.cols`. Status-bar row stays
full width. Tab bar stays inside `editor_area`, not extending
into the side panel.

```rust
pub struct Layout {
    pub dims:        Dims,
    pub side_area:   Option<Rect>,   // cols 0..25, rows 0..body_rows
    pub editor_area: Rect,           // cols panel_width..cols, rows 0..body_rows
    pub tab_bar:     Rect,           // editor_area x, row body_rows
    pub status_bar:  Rect,           // full width, last row
}
```

`Layout::compute(dims, browser_visible)` is a pure function;
everything downstream is a function of it.

### D6 — Dir listings cached in imbl, not HashMap

imbl's structural sharing makes `BrowserState::clone()` cheap
(pointer copy), which matters because the drv-input pattern
projects over that field. A `std::HashMap` clone would be
O(n_dirs × n_entries). imbl wins.

### D7 — FS listing drives via the query layer

Consistent with every other I/O path. `file_list_action(...)`
memo diffs expected listings (`{root} ∪ expanded_dirs`) against
`dir_contents.keys()`, emits `ListCmd` values for absent paths,
runtime feeds them to `FsListDriver::execute`. No sync mutation
of `dir_contents` from dispatch — only `expanded_dirs` mutates
there; the listing fills in asynchronously.

This means `Right` on an unlisted dir is *not instantaneous*:
selection stays, dir flips to expanded, rebuild_entries runs
(no children yet → empty indent), then seconds/ms later the
listing arrives and rebuild_entries runs again with children.
Matches legacy — and with `driver-fs-list` on a local disk,
the delay is normally <5ms.

### D8 — Workspace root is CWD, workspace detection is M19

Legacy has a workspace driver that walks up for `.git`. M11
sidesteps that: `browser.root = Some(canonicalize(cwd))` at
boot. Users who run `led` outside the project root get their
CWD as the "workspace" — matches the `no_workspace = true`
goldens setup flag.

### D9 — Rendering order: side panel first, then body

The painter paints side panel first (cols 0..25), then body
(cols 26..), then tab bar (inside editor area), then status
bar (full width). Cursor draws last as before. Painter grows
a `paint_side_panel` step; body's `paint_body` takes a
`content_x_start: u16` for the left column to start writing at
(was 0; becomes 26 when panel is visible, 0 when hidden).

### D10 — Tree ordering matches legacy golden: dirs first, alpha

Golden `right/frame.snap` shows `▽ subdir / inner.txt / alpha.txt
/ beta.txt` after expanding `subdir` — `subdir` comes before the
alphabetically-earlier `alpha.txt`. So it's **dirs first, then
files, each group sorted alphabetically** (case-insensitive).

## Types

### `state-browser` (new crate)

```rust
use imbl::{HashMap, HashSet, Vector};
use led_core::CanonPath;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: CanonPath,
    pub kind: DirEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Main,
    Side,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BrowserState {
    pub root:          Option<CanonPath>,
    pub dir_contents:  HashMap<CanonPath, Vector<DirEntry>>,
    pub expanded_dirs: HashSet<CanonPath>,
    pub entries:       Arc<Vec<TreeEntry>>,
    pub selected:      usize,
    pub scroll_offset: usize,
    pub visible:       bool,
    pub focus:         Focus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path:  CanonPath,
    pub name:  String,
    pub depth: usize,
    pub kind:  TreeEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeEntryKind {
    File,
    Directory { expanded: bool },
}

impl BrowserState {
    pub fn rebuild_entries(&mut self);
    pub fn expand(&mut self, path: CanonPath);
    pub fn collapse(&mut self, path: CanonPath);
    pub fn collapse_all(&mut self);
    pub fn move_selection(&mut self, delta: isize);
    pub fn page_selection(&mut self, delta: isize, page: usize);
    pub fn select_first(&mut self);
    pub fn select_last(&mut self);
    pub fn selected_entry(&self) -> Option<&TreeEntry>;
}
```

### `driver-fs-list` (new crates)

`core`:
```rust
pub struct FsListDriver { ... }
pub struct ListCmd { pub path: CanonPath }
pub struct ListDone { pub path: CanonPath, pub result: Result<Vec<DirEntry>, String> }
pub trait Trace { fn list_start(&self, _: &CanonPath); fn list_done(&self, _: &CanonPath, _: &Result<Vec<DirEntry>, String>); }
```

`native`:
```rust
pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier) -> (FsListDriver, FsListNative);
```

Worker uses `std::fs::read_dir`, maps entries, filters hidden
(leading `.`), sorts legacy-style, and sends `ListDone`.

### Runtime additions

- `query::file_list_action` memo — emits `ListCmd` per
  unlisted expected path.
- `query::side_panel_model` memo — builds a paintable
  `SidePanelModel` from `BrowserState`.
- `query::Layout` + `query::Rect` types.
- `render_frame` composes side panel + body using Layout.
- `driver-terminal/core::Frame` gains `side_panel:
  Option<SidePanelModel>`.
- `driver-terminal/native::paint` gains `paint_side_panel`;
  `paint_body` takes an `x_offset`.
- `Dispatcher` grows `browser: &mut BrowserState` and
  `fs_list: &FsListDriver` (execute path).

## Crate changes

```
crates/
  state-browser/             NEW — BrowserState, DirEntry, TreeEntry
  driver-fs-list/core/       NEW — FsListDriver, ListCmd/Done, Trace
  driver-fs-list/native/     NEW — std::fs::read_dir worker
  runtime/src/
    query.rs                 + Layout, Rect, SidePanelInput,
                               side_panel_model, file_list_action;
                               render_frame composes via Layout
    dispatch/browser.rs      NEW — expand/collapse/open primitives
                               + selection move
    dispatch/mod.rs          Dispatcher.browser; run_command routing
                               for focus-side cursor commands,
                               ExpandDir/CollapseDir/CollapseAll/
                               OpenSelected/OpenSelectedBg/
                               ToggleSidePanel/ToggleFocus
    keymap.rs                + browser_direct table; bind_browser;
                               + Command variants; + parse cases;
                               + default bindings
    lib.rs                   run() threads BrowserState + FsListDriver
                               through; ingest applies ListDone;
                               execute fires ListCmd
  driver-terminal/core/      Frame.side_panel, SidePanelModel,
                               Rect geometry types (if not in runtime)
  driver-terminal/native/    paint_side_panel; paint_body takes
                               x_offset; body_rows accounts for
                               editor_area.cols (not dims.cols)
```

New workspace members: `led-state-browser`,
`led-driver-fs-list-core`, `led-driver-fs-list-native`.

## Testing

### `state-browser`
- `rebuild_entries` builds from root + expanded + dir_contents.
- Ordering: dirs first, files second, each alpha.
- Hidden (`.git`, `.env`) filtered out.
- `expand` + `collapse` + `collapse_all` mutate state; rebuild
  reflects them.
- `move_selection` clamps at 0 / len().
- `select_first`/`select_last` land as expected.
- Scroll clamping across selection moves.

### `driver-fs-list`
- `spawn` + list a real temp directory → returns entries.
- Hidden filter applied.
- Error path: non-existent path → `Err`.

### `runtime::query::file_list_action`
- Empty expanded set + no root → no commands.
- Root set, no dir_contents → one command for root.
- Expanded dirs with missing listings → one command each.
- All expanded dirs listed → no commands.

### `runtime::query::side_panel_model`
- Rebuilt after expand shows expanded chevron.
- Selected row index carries through.

### `runtime::query::render_frame`
- With `browser.visible = true` → frame has `side_panel: Some`
  and body_x_offset applied.
- With `browser.visible = false` → body uses full width.
- Layout respects auto-hide threshold (small terminals).

### `runtime::dispatch::browser`
- `ToggleSidePanel` flips `visible`.
- `ToggleFocus` flips focus + sets `visible = true`.
- `ExpandDir` on selected dir adds to `expanded_dirs`.
- `CollapseDir` on selected dir removes from `expanded_dirs`.
- `CollapseDir` on a leaf collapses the parent.
- `CollapseAll` empties `expanded_dirs`, resets selection.
- `OpenSelected` on a file creates preview tab.
- `OpenSelected` on the preview path promotes (preview=false).
- `OpenSelected` on a different file replaces preview's path.
- `OpenSelected` on a dir toggles expansion.
- Focus-side `CursorUp/Down` move selection, not cursor.

Expected: +40 tests.

## Done criteria

- All existing tests pass.
- New state-browser / driver-fs-list / nav tests pass.
- Clippy unchanged from post-M10 (10).
- Interactive smoke:
  - `cd ~/dev/led-rewrite && cargo run`. Side panel shows the
    workspace tree. `Alt-Tab` focuses it; arrow keys scan with
    live preview; `Enter` promotes; `Ctrl-b` toggles; `Ctrl-q`
    collapses all.
- Goldens: a concrete target. Pre-M11 was 0 / 257. Post-M11
  should move the smoke + keybindings/browser + basic
  features/* scenarios to green — rough estimate 30–50
  goldens.

## Growth-path hooks

- **Git integration** (M19): workspace-root walks up for `.git`;
  browser's gutter per-file mark reflects git status. M11's
  browser doesn't care — the new column is additive.
- **Reveal-active-file** (M21): adds `pending_reveal:
  Option<CanonPath>` + `complete_pending_reveal` plumbing.
- **Find-file** (M12) and **isearch** (M13): each registers a
  new Focus variant. The context-keymap mechanism grows from
  "browser OR main" to "overlay | browser | main" without a
  rewrite.
- **Theme** (M15): preview tabs get a muted style, the selected
  row gets a proper accent colour.
- **External watcher** (M26): `FsWatchNotify` events trigger a
  fresh listing for the affected parent dir.
- **Mouse** (not scheduled): selection-on-click is a
  straightforward addition once terminal mouse events land.
