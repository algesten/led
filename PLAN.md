# Implementation Plan: FRP Text Editor

**State -> Derived -> Drivers -> Model -> State**

Every side effect lives in a Driver. The Model is a pure reduce: `(State, Mut) -> State`. Derived selects and transforms State fields into Driver inputs. Side effects that react to state live in `_of` functions (e.g. `process_of`).

---

## Done

The following are fully implemented and working:

- **Action enum** — 48 variants in `crates/core/src/lib.rs`
- **PanelSlot** — Main, Side, StatusBar, Overlay in `crates/core/src/lib.rs`
- **Input driver** (`crates/input/`) — crossterm EventStream, KeyCombo, raw mode, alternate screen, panic hook
- **Render driver** (`crates/ui/`) — LatestStream\<Arc\<AppState\>\>, ratatui Terminal, force_redraw support
- **UI skeleton** (`crates/ui/src/render.rs`) — status bar, side panel, tab bar, editor area layout
- **Theme resolution** (`crates/ui/src/style.rs`) — StyleValue → ratatui Style, $ref chains, ANSI/hex colors
- **Keymap system** (`crates/core/src/keys.rs`) — KeyCombo, Keymap, chords, context-sensitive lookup, TOML parsing
- **Theme system** (`crates/core/src/theme.rs`) — Theme struct with typed sub-themes, TOML deserialization
- **Config watch driver** (`crates/config-file/`) — watches keys.toml/theme.toml, hot reload, alert on parse error
- **Workspace driver** (`crates/workspace/`) — git root detection, primary lock via flock
- **Storage driver** (`crates/storage/`) — file open/close/save, notify-based file watching, atomic writes
- **Keymap resolution in model** (`led/src/model/actions_of.rs`) — TerminalEvent, chord state, context resolution, char insert fallback
- **Action routing in model** (`led/src/model/mod.rs`) — ToggleSidePanel, ToggleFocus, Quit, Suspend
- **Suspend/resume** (`led/src/model/process_of.rs`) — reactive via process_of, terminal restore/SIGTSTP/re-setup, force_redraw
- **Alert system** (`led/src/model/alerts_of.rs`) — split_result, Info/Warn routing
- **Keymap derivation** (`led/src/model/keymap_of.rs`) — Keys → Keymap compilation as derived stream
- **FRP cycle** (`led/src/main.rs`) — hoisting loop, initial state seed, quit handling, terminal cleanup
- **Derived** (`led/src/derived.rs`) — workspace, config_file_out, storage (placeholder)
- **CLI** — basic clap with `path` argument, start_dir computation
- **AppState** (`crates/state/`) — startup, workspace, config, keymap, focus, show_side_panel, viewport, quit, suspend, force_redraw, info, warn

---

## Next: Phase 3 — Document Model

### 3A. TextDoc and Rope

Port `core/doc.rs` with its `TextDoc` wrapper around `ropey::Rope`. TextDoc provides:

- `insert(char_idx, text)`, `insert_char(char_idx, ch)`, `remove(start, end)` — mutations that track pending LSP changes
- `line(idx)`, `line_len(idx)`, `line_count()` — accessors
- `char_idx(row, col)`, `char_to_line()`, `line_to_char()` — coordinate conversions
- `drain_changes()` — returns accumulated `EditorTextEdit` list for LSP `didChange`
- `version()` — monotonic counter incremented per edit
- `write_to(writer)`, `to_string()` — serialization
- `replace_rope(rope)` — for reload (clears changes, resets version)

TextDoc must be `Clone` to live inside `State`. Since `Rope` is already cheaply cloneable (structural sharing), this is fine. The `pending_changes` vec is the only part that accumulates; it gets drained by the LSP derived selector.

### 3B. DocStore

No separate DocStore. The docs live directly inside `State.buffers` — each `BufferState` owns its `TextDoc`. The State *is* the store.

---

## Phase 4: Buffer Core

### 4A. BufferState

Define the `BufferState` struct. Group fields logically:

**Cursor & Viewport:**
`cursor_row`, `cursor_col`, `scroll_offset`, `scroll_sub_line`, `text_width` (set from viewport width minus gutter)

**Document:**
`doc: TextDoc`, `path: PathBuf`

**Dirty Tracking:**
`dirty: bool`, `base_content_hash: u64` (xxHash of content at load/save), `distance_from_save: i32`

**Undo:**
`undo_history: Vec<UndoEntry>`, `undo_cursor: Option<usize>`, `pending_group: Option<PendingGroup>`, `save_history_len: usize`

**Selection:**
`mark: Option<(usize, usize)>`, `kill_accumulator: Option<String>`

**Search:**
`isearch: Option<ISearchState>`, `last_search: Option<String>`

**LSP Decoration:**
`diagnostics: Vec<EditorDiagnostic>`, `inlay_hints: Vec<EditorInlayHint>`, `inlay_hints_enabled: bool`

**Completion:**
`completion: Option<CompletionState>`, `completion_triggers: Vec<String>`

**Formatting:**
`pending_save_after_format: bool`, `format_generation: u64`, `pre_format_snapshot: Option<(String, (usize, usize))>`

**File State:**
`disk_modified: bool`, `disk_deleted: bool`, `preview: bool`, `read_only: bool`

**Syntax:**
`syntax_state: Option<SyntaxHighlightData>` (the highlight spans, not the tree-sitter Tree itself — the Tree lives in the syntax driver)

### 4B. File I/O

The storage driver (`crates/storage/`) already handles file open/close/save and file watching. Wire the derived storage stream to request file reads when opening buffers. The model receives `StorageIn::Opened(path)` and creates `BufferState` from the file content.

### 4C. Cursor Movement

Port all cursor movement functions from `buffer/editing.rs` as pure functions that take `(&BufferState, &TextDoc) -> (usize, usize)` and return the new cursor position:

- `compute_move_up`, `compute_move_down` — soft-wrap sub-line navigation via `expand_tabs`, `compute_chunks`, `find_sub_line`
- `move_left`, `move_right` — wrapping across line boundaries
- `move_to_line_start`, `move_to_line_end`
- `page_up`, `page_down` — viewport-sized jumps
- `move_to_file_start`, `move_to_file_end`

Port the wrap utilities from `buffer/wrap.rs`: `expand_tabs`, `visual_line_count`, `compute_chunks`, `find_sub_line`, `display_col_to_char_idx`. These are already pure.

### 4D. Text Editing in Model

Port the editing operations from `buffer/editing.rs`. Each takes the current BufferState + TextDoc mutably and performs the edit:

- `insert_char` — insert character, update cursor, record undo, update dirty flag
- `insert_newline` — insert newline + auto-indent (regex fallback initially), compound undo entry
- `delete_char_backward`, `delete_char_forward` — delete with undo recording
- `kill_line` — kill to end of line (or join with next line), accumulates in kill ring
- `kill_region` — delete selection between mark and cursor
- `yank_text` — paste from clipboard/kill-ring
- `indent_line` — reindent current line via regex
- `apply_text_edits` — apply LSP edits in reverse document order with compound undo

The undo grouping logic (`PendingGroup` with 1000ms timeout, merging consecutive same-type edits) is pure state manipulation that lives in the model.

### 4E. Scroll Clamping

After every model update that changes cursor position, the viewport must be adjusted. Port the scroll clamping logic: if cursor is above `scroll_offset`, scroll up; if cursor is below `scroll_offset + viewport_height`, scroll down. This accounts for soft-wrapped lines via `visual_line_count`.

---

## Phase 5: Tab Management

### 5A. TabState

```
TabState {
    order: Vec<PathBuf>,           // tab order (paths into State.buffers)
    active: usize,                 // index into order
    pre_preview: Option<usize>,    // tab to return to when preview closes
}
```

### 5B. Tab Operations in Model

- `next_tab`, `prev_tab` — cyclic navigation
- `activate_buffer_by_path(path)` — find tab by path, switch to it
- `open_file(path)` — if buffer exists, activate it; otherwise, request file read from driver. When file read completes, create `BufferState`, add to `order`
- `kill_buffer` — remove from `order`, adjust `active` index. If buffer is dirty, set modal confirmation
- `preview_file(path, row, col, match_len)` — open in preview mode (reuses existing preview tab). On any navigation action, the preview tab is auto-promoted or killed
- `preview_promote` — convert preview tab to permanent
- `preview_close` — close preview, return to `pre_preview` tab

Tab descriptors (label, dirty indicator, read_only flag, preview flag) are computed at render time from `BufferState`, not stored separately.

---

## Phase 6: File Browser

### 6A. FileBrowserState

```
FileBrowserState {
    root: PathBuf,
    entries: Vec<TreeEntry>,
    selected: usize,
    expanded_dirs: HashSet<PathBuf>,
    scroll_offset: usize,
}
```

`TreeEntry` holds `path`, `depth`, `kind` (File or Directory with expanded flag).

### 6B. File Browser Driver

**Input (from Derived):** stream of `BrowseRequest` — rebuild request (when `expanded_dirs` changes) or refresh (when workspace changes)

**Output:** `BrowseResult { entries: Vec<TreeEntry> }` — the flattened, sorted tree

The driver walks the directory tree from `root`, respecting `expanded_dirs`. Skips dotfiles, sorts directories before files, both alphabetically.

### 6C. Browser Actions in Model

- `MoveUp`, `MoveDown`, `PageUp`, `PageDown` — update `selected` with clamping
- `ExpandDir` — add to `expanded_dirs`, trigger rebuild
- `CollapseDir` — remove from `expanded_dirs`, trigger rebuild
- `CollapseAll` — clear `expanded_dirs`, trigger rebuild
- `OpenSelected` — if file: emit file-open intent; if directory: toggle expand
- `OpenSelectedBg` — open file without switching focus
- `reveal(path)` — expand all ancestors, set `selected` to the entry

### 6D. Browser Rendering

Rendered in the Side panel slot:
- Tree with indentation (2 spaces per depth level)
- Directory/file icons
- Git status indicators (M/A/U) with theme colors
- Diagnostic severity indicators per file
- Selected entry highlight
- Scroll viewport

---

## Phase 7: Workspace & File Watching

### 7A. Workspace Watcher

Extend `led_workspace::driver` with `WorkspaceChanged` events when files are created/removed. Uses `notify::recommended_watcher` in recursive mode on the root, filtering out `.git` internal changes.

### 7B. File Watcher

Individual buffer files are already watched by the storage driver (`crates/storage/`). It emits `StorageIn::Changed` / `StorageIn::Removed` when files change externally. The model sets `disk_modified` / `disk_deleted` flags on the affected `BufferState`.

---

## Phase 8: Git Integration

### 8A. GitState

```
GitState {
    branch: Option<String>,
    file_statuses: HashMap<PathBuf, HashSet<FileStatus>>,
    line_statuses: HashMap<PathBuf, Vec<LineStatus>>,
}
```

### 8B. Git Driver

**Input (from Derived):**
- `ScanFiles` — triggered on file save, workspace change, resume
- `ScanLines(PathBuf)` — triggered on tab activation, file save

**Output:**
- `FileStatuses { statuses, branch }`
- `LineStatuses { path, statuses }`

Uses `git2` on `spawn_blocking` tasks. Both include 50ms coalescing delay.

### 8C. Derived Git Triggers

- When a file save occurs (detected by comparing `base_content_hash` changes), emit `ScanFiles` + `ScanLines(path)`
- When the active tab changes, emit `ScanLines(new_path)`
- When `WorkspaceChanged` is set, emit `ScanFiles`

---

## Phase 9: Syntax Highlighting

### 9A. Syntax Driver

**Input (from Derived):**
- `Parse { path, rope }` — full parse (on file open)
- `IncrementalParse { path, rope, edits }` — incremental re-parse after edit

**Output:** `SyntaxResult { path, highlights: Vec<HighlightSpan> }`

The driver detects language from file extension, uses `tree_sitter` on `spawn_blocking`, supports cancellation, computes highlight spans by walking the tree with highlight queries.

### 9B. Auto-Indent Support

Start with regex fallback: `detect_indent_unit`, `get_line_indent`, `apply_indent_delta`, `find_prev_nonempty_line`. Tree-sitter indent added later once the basic loop works.

---

## Phase 10: In-Buffer Search (ISearch)

### 10A. ISearchState

```
ISearchState {
    query: String,
    origin: (usize, usize),
    origin_scroll: usize,
    origin_sub_line: usize,
    failed: bool,
    matches: Vec<(usize, usize, usize)>,  // (row, col, char_len)
    match_idx: Option<usize>,
}
```

### 10B. Search Logic in Model

All pure (operates on TextDoc in memory):
- `start_search` — snapshot current position as origin
- `find_all_matches(doc, query)` — case-insensitive substring scan
- `update_search` — recompute matches after query change, jump to first match at or after cursor
- `search_next` — advance to next match; wrap; recall `last_search` on empty query
- `search_cancel` — restore origin position
- `search_accept` — keep current position, clear search state

---

## Phase 11: File Search (Ripgrep)

### 11A. FileSearchState

```
FileSearchState {
    active: bool,
    query: String,
    cursor_pos: usize,
    case_sensitive: bool,
    use_regex: bool,
    results: Vec<FileGroup>,
    flat_hits: Vec<FlatHit>,
    selected: usize,
    scroll_offset: usize,
}
```

### 11B. Search Driver

**Input (from Derived):** `SearchRequest { query, root, case_sensitive, use_regex }`

**Output:** `SearchResult(Vec<FileGroup>)`

Uses `grep` crate with `ignore::WalkBuilder`, binary detection, groups by file, caps at 1000 hits, debounces.

### 11C. Search Actions in Model

- `OpenFileSearch` — activate, pre-fill from selection
- `CloseFileSearch` — deactivate, return focus
- `ToggleSearchCase` / `ToggleSearchRegex` — flip flags, re-trigger
- Character input, `MoveUp`/`MoveDown`, `OpenSelected`

---

## Phase 12: Find File Panel

### 12A. FindFileState

```
FindFileState {
    active: bool,
    input: String,
    cursor: usize,
    completions: Vec<Completion>,
    selected: Option<usize>,
}
```

### 12B. Find File Driver

**Input:** `CompletionRequest { dir, prefix }`
**Output:** `CompletionResult(Vec<Completion>)`

Reads directory, filters by prefix, handles tilde expansion and path normalization.

---

## Phase 13: LSP Integration

This is the most complex subsystem.

### 13A. LspState

```
LspState {
    servers: HashMap<String, ServerStatus>,
    opened_docs: HashSet<PathBuf>,
    pending_code_actions: HashMap<PathBuf, Vec<CodeAction>>,
    progress: HashMap<String, ProgressState>,
    status: Option<LspStatus>,
    completion_triggers: HashMap<String, Vec<String>>,
}
```

### 13B. LSP Driver

Full server lifecycle management: spawn, initialize handshake, JSON-RPC transport (Content-Length framing), request/response matching, notification routing (diagnostics, progress), file watching for `workspace/didChangeWatchedFiles`, UTF-16 position conversion.

**Input:** DidOpen, DidChange, DidSave, DidClose, GotoDefinition, Rename, CodeAction, Format, Completion, InlayHints

**Output:** definition results, rename edits, code actions, format edits, completion items, diagnostics, inlay hints, progress updates, server status

### 13C. LSP Registry

Language-to-server-command mapping: rust-analyzer, pylsp, typescript-language-server, clangd, gopls, etc. Pure data in core.

### 13D. Derived LSP Triggers

- **DidOpen:** When active_buffer changes and path not yet opened
- **DidChange:** When doc.version changes, drain_changes()
- **DidSave:** When base_content_hash changes
- **DidClose:** When buffer removed

---

## Phase 14: Completion

### 14A. CompletionState

```
CompletionState {
    items: Vec<EditorCompletionItem>,
    filtered: Vec<usize>,
    selected: usize,
    prefix_start_col: usize,
}
```

### 14B. Completion Logic

Fuzzy filtering via `nucleo_matcher`, navigation, accept (replace prefix + apply additional edits), trigger on trigger character.

---

## Phase 15: Jump List

```
JumpListState {
    list: Vec<JumpPosition>,
    index: usize,
}
```

Pure: `record_jump`, `jump_back`, `jump_forward`. Cap at 100 entries.

---

## Phase 16: Picker / Overlay

```
PickerState {
    active: bool,
    title: String,
    items: Vec<String>,
    selected: usize,
    source_path: PathBuf,
    kind: PickerKind,  // CodeAction or Outline
}
```

Rendered as centered modal overlay.

---

## Phase 17: Messages Panel

```
MessagesState {
    active: bool,
    doc: TextDoc,
    cursor_row: usize,
    scroll_offset: usize,
    last_synced: usize,
}
```

Log driver implements `log::Log` trait, writes to ring buffer. Model appends formatted lines.

---

## Phase 18: Session Persistence

Session driver manages SQLite at `~/.config/led/db.sqlite`. Three tables: workspaces, buffers, session_kv. Save periodically and on quit. Restore on startup if primary.

---

## Phase 19: Modal Dialogs

Confirmation modals (dirty buffer kill, quit with unsaved) and rename modals (LSP rename). Intercept all keyboard input when active.

---

## Phase 20: Clipboard Driver

**Input:** `SetClipboard(String)` on kill/copy
**Output:** `ClipboardContent(String)` on yank/paste

Wraps `arboard::Clipboard` with Mutex.

---

## Phase 21: Remaining Features

- **Match Bracket** — pure function scanning document for matching `()[]{}`
- **Sort Imports** — sort contiguous import/use lines
- **Outline** — LSP documentSymbol or regex fallback, shown in picker
- **Format on Save** — format → apply edits → save chain
- **Color Hints** — inline color swatches for hex color literals

---

## Phase 22: CLI & Startup Polish

- `--reset-config`, `--debug`, `--log-file`, `--script` flags
- Script driver for headless testing (JSON action per line)
- Graceful shutdown: check unsaved, save session, stop LSP servers

---

## Implementation Order

1. **Phase 3** (TextDoc) — need text storage for buffers
2. **Phase 4** (buffer core) — need editing to be useful
3. **Phase 5** (tabs) — need to manage multiple files
4. **Phase 6** (file browser) — need to navigate files
5. **Phase 7** (workspace watching) — external change detection
6. **Phase 8** (git) — status display
7. **Phase 9** (syntax) — readable code
8. **Phase 10-12** (search features) — navigating large codebases
9. **Phase 13-14** (LSP + completion) — the heavy lift
10. **Phase 15-17** (jump list, picker, messages) — supporting features
11. **Phase 18** (session) — persistence
12. **Phase 19-21** (modals, clipboard, remaining) — polish
13. **Phase 22** (CLI, startup, shutdown) — production readiness

Each phase should result in a working (if incomplete) editor.
