# file-browser

## Summary

The **file browser** is a persistent tree-view of the workspace rooted
at the workspace root, rendered in the left side panel. It shows
directories and files, supports per-directory expand/collapse, keeps
its own selection and scroll state, and lets the user open a file
either by previewing (arrow-browse, live preview in main pane) or
by committing (Enter promotes preview to a real tab and moves focus
to the editor). Unlike the `find-file` overlay, the browser is a
long-lived UI surface with its own focus context (`PanelSlot::Side`)
and its own keymap table.

## Behavior

### Structure

State lives on `AppState.browser: FileBrowserState`
(`crates/state/src/lib.rs:1532-1594`):

- `root: Option<CanonPath>` — the workspace root, set on workspace
  load.
- `dir_contents: HashMap<CanonPath, Vec<DirEntry>>` — per-directory
  cache of raw listings from the FS driver.
- `expanded_dirs: HashSet<CanonPath>` — which directories are
  currently expanded.
- `entries: Rc<Vec<TreeEntry>>` — the flattened, rendered tree built
  from `dir_contents` + `expanded_dirs` via `walk_tree`.
- `selected: usize` — index into `entries`.
- `scroll_offset: usize` — first visible entry.
- `pending_reveal: Option<CanonPath>` — deferred reveal target (see
  "reveal-active-file" below).

Rebuild is driven by `rebuild_entries()`, which walks from the root
and emits a `TreeEntry { path, name, depth, kind }` per visible line.
`EntryKind::Directory { expanded }` drives the chevron/arrow rendering;
`EntryKind::File` is a leaf.

### Focus context

The browser has focus when `state.focus == PanelSlot::Side`. The
keymap lookup resolves the `"browser"` context first
(`actions_of.rs:151-153`), so the browser's bindings override global
ones. Editor-facing actions (`InsertChar`, `InsertNewline`,
`InsertTab`, `DeleteBackward/Forward`, `KillLine/Region`, `Yank`,
`Undo/Redo`, `SortImports`) are filtered out while focus is Side
(`actions_of.rs:159-174`) so that keystrokes meant for typing don't
escape into a buffer.

### Navigation keys

All browser navigation goes through the `Mut::Action` mega-dispatcher
— the browser is one of the remaining residents (see
`docs/extract/actions.md` findings and `CLAUDE.md` Principle 9).
Handlers live in `led/src/model/action/browser.rs`.

| Key | Action | Effect |
|---|---|---|
| Up / Down | `MoveUp` / `MoveDown` | selection +/- 1, clamped; auto-scroll; preview selected file |
| PageUp / PageDown | selection by buffer-height |
| Ctrl-Home / Ctrl-End | `FileStart` / `FileEnd` | top / bottom |
| Left | `CollapseDir` | collapse directory at selection, or parent if selection is a leaf inside an expanded dir |
| Right | `ExpandDir` | expand directory at selection; triggers fresh FS listing |
| Enter | `OpenSelected` | file → open (preview promote or `request_open`); dir → expand/collapse toggle |
| Alt-Enter | `OpenSelectedBg` | **dead** (see below) |
| Ctrl-q | `CollapseAll` | collapse every expanded dir; reset selection and scroll to 0 |

Selection scrolling: after selection changes,
`handle_browser_nav` clamps `scroll_offset` so `selected` is within
the visible range, using `dims.buffer_height()` as the visible count
(`browser.rs:37-41`).

Preview side effects on nav: when the post-nav selection is a file,
`set_preview` opens/updates a preview tab; when it's a directory, any
existing preview is closed. This is what makes `Up`/`Down` act as a
"live preview" scan through the workspace (`browser.rs:44-58`).

### Expansion

`Right` on a collapsed directory:
1. Inserts the path into `expanded_dirs`.
2. Rebuilds entries if we already have the contents cached.
3. **Always** sets `state.pending_lists` to request a fresh listing
   from the FS driver — even if cached — so changes made on disk
   while the directory was collapsed become visible.

`Left` on an expanded directory collapses that directory. `Left` on
a leaf whose parent is expanded collapses the **parent** (so `Left`
in a deep tree "zooms out" one level). The selection moves to the
collapsed directory's row (`browser.rs:92-95`).

`CollapseAll` (`Ctrl-q` in browser context) wipes `expanded_dirs`,
rebuilds, and resets selection and scroll to 0.

Initial listing for the root is handled by the workspace driver on
startup (see driver-events / workspace docs); every subsequent
`Right` adds a sub-directory listing request. Listings that arrive
back as `FsIn::DirListed` update `dir_contents` and trigger a
rebuild (handled in `buffers_of` / `mod.rs`).

### Open behavior

`Enter` on a file (`handle_browser_open`, `browser.rs:106-135`):

1. If that path is the current preview, `promote_preview` unpins it
   into a real tab. Focus switches to Main. No `request_open` is
   needed — the buffer is already materialized.
2. Otherwise, any existing preview is closed, `request_open` is
   called with the entry's CanonPath wrapped as a `UserPath`, the
   active tab is set, and focus moves to Main.

`Enter` on a directory toggles its expansion state — equivalent to
`Right`/`Left` depending on current state. This makes Enter on a
dir a natural "drill in".

### Reveal-active-file

`FileBrowserState::reveal(path)` expands every ancestor directory of
`path` up to (but not including) the root, sets
`pending_reveal = Some(path)`, rebuilds entries, and calls
`complete_pending_reveal` which tries to position `selected` on the
revealed path. If some ancestor's contents haven't been listed yet,
`complete_pending_reveal` is a no-op — the `pending_reveal` is
retried each time a new listing arrives, eventually landing on the
entry once the full path is materialized in the tree. Returns the
list of ancestor directories that need fresh listings so the caller
can dispatch them (`crates/state/src/lib.rs:1557-1594`).

Reveal is triggered in a few places — e.g. activating a tab that
came from elsewhere (`reveal_active_buffer` is called from the fold
tail and from close-preview logic). Direct user-facing invocation
from a key: [unclear — confirm whether there's a bound
"reveal in tree" key or whether it's purely automatic].

### Hidden files

`crates/fs/src/lib.rs:143-168` `list_dir` filters out any entry
whose name starts with `.`. The browser therefore never shows
hidden files. This is unlike the `find-file` overlay, which toggles
hidden-visibility when the user types a leading `.`. There is no
user-visible "show hidden" toggle in the browser. [unclear — this
asymmetry may be intentional or an omission].

## User flow

Workspace opens, browser shows root-level entries. User presses
`Right` on `src/`, the directory expands and its contents appear.
User presses `Down` repeatedly; each file selection spawns a
preview tab and they flash by in the main pane. User lands on the
target file, presses `Enter`; the preview promotes, focus moves
back to Main, typing is now editing. `Alt-Tab` (`toggle_focus`)
bounces focus back to the browser; `Ctrl-q` collapses the whole
tree for a fresh start.

## State touched

- `AppState.browser` (every field above).
- `AppState.pending_lists: Versioned<Vec<CanonPath>>` — set on
  `ExpandDir` for the fresh listing request.
- `AppState.tabs` / `active_tab` — written by `OpenSelected` for
  file entries.
- `AppState.focus` — set to Main on file open.
- `AppState.buffers` — `request_open` creates a placeholder; the
  docstore materializes it.
- `AppState.dims` — read to compute visible height for scroll
  clamping and page-nav.

## Extract index

- Actions: `MoveUp`, `MoveDown`, `PageUp`, `PageDown`, `FileStart`,
  `FileEnd`, `ExpandDir`, `CollapseDir`, `CollapseAll`,
  `OpenSelected`, `OpenSelectedBg` **(dead)**, `ToggleFocus`,
  `ToggleSidePanel` — `docs/extract/actions.md`.
- Keybindings: see `docs/extract/keybindings.md` §"Context: file
  browser sidebar".
- Driver events: `FsOut::ListDir`, `FsIn::DirListed` —
  `crates/fs/src/lib.rs`; workspace-driver root listing — see
  driver docs.
- Config: none specific — side panel width is hardcoded at
  `Dimensions::side_panel_width = 25` (see
  `docs/extract/config-keys.md` and
  `docs/rewrite/POST-REWRITE-REVIEW.md` §"Hardcoded settings").

## Edge cases

- **Empty directory expand**: no children under that entry; chevron
  still flips to "expanded".
- **Disk changes while collapsed**: next `Right` re-fetches a fresh
  listing; [unclear — whether watcher also rebuilds; confirm in
  driver docs].
- **Selection past end after listing shrinks**: user-driven nav is
  clamped; listing-driven rebuilds may momentarily leave
  `selected >= entries.len()` — [unclear — verify renderer
  clamps].
- **Symlink directories**: workspace driver canonicalizes before
  emitting; cycles don't appear in `entries`.
- **Very deep trees**: no depth limit in `walk_tree`; performance
  scales with visible entries.
- **Hiding panel while focus is Side**: `Ctrl-b` hides the panel but
  focus stays `Side`; [unclear — verify focus auto-swap in
  `ui_actions_of`].

## Error paths

- **FS driver fails to list**: logged, empty entries returned;
  the directory appears empty in the tree with no error surfacing
  to the user.
- **Workspace not yet loaded when Ctrl-b opens the panel**: browser
  is empty; selection stays at 0; all nav is no-op until the
  workspace arrives.
- **Opening a file whose path has been deleted**: `request_open`
  routes through docstore; see `buffers.md` for the error handling.
  From the browser's perspective the entry simply stays in the tree
  until the next listing removes it.

## Dead / absent features

- **`Action::OpenSelectedBg` (Alt-Enter in browser context)** is
  declared in `crates/core/src/lib.rs` and bound in
  `default_keys.toml`, but has no match arm in `handle_action` and
  no combinator. Pressing Alt-Enter in the sidebar does nothing.
  Presumed intent: "open in background tab without stealing focus
  or moving active-tab." See
  `docs/rewrite/POST-REWRITE-REVIEW.md` §"Dead code".
- **No file rename / delete / new-file operations** from the browser
  tree. Users rename via SaveAs and delete via external shell.
  There is no context menu (or mouse input) in the browser.
- **No `.gitignore` awareness in the browser**: unlike `file-search`
  which walks with ignore rules, the browser lists every non-hidden
  entry. [unclear — verify in `workspace` crate for the root
  crawl].
- **No multi-select / drag-to-reorder** — the browser is a single-
  selection read-only tree.
