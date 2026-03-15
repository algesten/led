# Implementation Plan: FRP Text Editor

**State -> Derived -> Drivers -> Model -> State**

Every side effect lives in a Driver. The Model is a pure reduce: `(State, Mut) -> State`. Derived selects and transforms State fields into Driver inputs. Side effects that react to state live in `_of` functions (e.g. `process_of`).

---

## Next: Phase 5 — Tab Management

### 5A. BufferState additions

Add `preview: bool` for preview tabs that auto-close when navigating away.

### 5B. Tab Operations in Model

- `NextTab` / `PrevTab` — cyclic navigation through buffers sorted by tab_order
- `KillBuffer` — remove buffer, adjust active_buffer. If dirty, warn (or modal later)
- Opening a file that's already open → activate its tab instead of re-opening

### 5C. Derived: Open from file browser / find-file

Currently only CLI arg triggers file open. Need to support opening files from within the editor (Action::OpenFile or similar).

---

## Phase 6: File Browser

### 6A. FileBrowserState

```
FileBrowserState {
    entries: Vec<TreeEntry>,
    selected: usize,
    expanded_dirs: HashSet<PathBuf>,
    scroll_offset: usize,
}
```

### 6B. File Browser Driver

Walks directory tree from workspace root, respects expanded_dirs. Skips dotfiles, sorts dirs before files.

### 6C. Browser Actions

ExpandDir, CollapseDir, CollapseAll, OpenSelected, OpenSelectedBg — all wired to the existing Action variants.

### 6D. Browser Rendering

Tree with indentation in the Side panel. Selected entry highlight. Scroll viewport.

---

## Phase 7: Selection & Kill Ring

- `SetMark` — toggle mark at cursor
- `KillRegion` — delete selection, push to kill ring
- `Yank` — paste from kill ring
- Selection rendering in UI (highlight between mark and cursor)

---

## Phase 8: In-Buffer Search (ISearch)

ISearchState with query, origin, matches. Actions: InBufferSearch, character input, search_next, cancel, accept.

---

## Phase 9: Syntax Highlighting

Tree-sitter driver for incremental parsing. Highlight spans stored per-buffer. Language detection from extension.

---

## Phase 10: Git Integration

git2 driver for branch, file statuses, line statuses. Status bar branch display, file browser status icons, gutter diff markers.

---

## Phase 11: File Search (Ripgrep)

grep crate + ignore::WalkBuilder. Results panel with file groups. OpenSelected to jump to match.

---

## Phase 12: Find File Panel

Directory completion, tilde expansion, fuzzy matching.

---

## Phase 13: LSP Integration

Full server lifecycle, JSON-RPC, diagnostics, goto definition, rename, code actions, format, completion, inlay hints.

---

## Phase 14: Session Persistence

SQLite at config_dir/db.sqlite. Workspace, buffer positions, undo chains. Restore on startup if primary.

---

## Phase 15: Remaining

- Modal dialogs (dirty buffer kill, quit with unsaved, LSP rename)
- Clipboard driver (arboard)
- Match bracket, sort imports, outline
- Format on save
- Jump list
- Messages panel (log viewer)
- CLI flags (--reset-config, --debug, --log-file)

---

## Implementation Order

1. **Phase 5** (tabs) — manage multiple files
2. **Phase 6** (file browser) — navigate and open files
3. **Phase 7** (selection) — mark, kill, yank
4. **Phase 8** (search) — in-buffer find
5. **Phase 9** (syntax) — readable code
6. **Phase 10** (git) — status display
7. **Phase 11-12** (file search, find file) — navigating large codebases
8. **Phase 13** (LSP) — the heavy lift
9. **Phase 14** (session) — persistence
10. **Phase 15** (remaining) — polish and production readiness

Each phase should result in a working (if incomplete) editor.
