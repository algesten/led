# Implementation Plan: Rewriting `_old` into FRP Architecture

This document describes every piece of functionality from the old codebase and how it maps into the new cyclic FRP architecture: **State -> Derived -> Drivers -> Model -> State**. Each chunk is ordered by dependency so it can be conquered sequentially. Nothing is optional; this is a full parity plan.

The fundamental rule: **every side effect lives in a Driver**. The Model is a pure function `(State, DriverOutput) -> State`. Derived selects and transforms State fields into Driver inputs.

---

### 1A. Action Enum

The old codebase defines 48 `Action` variants in `core/types.rs`. These are the user-intent signals that flow from the terminal driver into the model. Port the full enum into `led_core`:

- **Movement**: `MoveUp`, `MoveDown`, `MoveLeft`, `MoveRight`, `LineStart`, `LineEnd`, `PageUp`, `PageDown`, `FileStart`, `FileEnd`
- **Insert/Delete**: `InsertChar(char)`, `InsertNewline`, `DeleteBackward`, `DeleteForward`, `InsertTab`, `KillLine`
- **File**: `Save`, `SaveForce`, `KillBuffer`
- **Navigation**: `PrevTab`, `NextTab`, `JumpBack`, `JumpForward`, `Outline`, `MatchBracket`
- **Search**: `InBufferSearch`, `OpenFileSearch`, `CloseFileSearch`, `ToggleSearchCase`, `ToggleSearchRegex`
- **Find**: `FindFile`
- **Edit**: `Undo`, `SetMark`, `KillRegion`, `Yank`, `SortImports`
- **LSP**: `LspGotoDefinition`, `LspRename`, `LspCodeAction`, `LspFormat`, `LspNextDiagnostic`, `LspPrevDiagnostic`, `LspToggleInlayHints`
- **UI**: `ToggleFocus`, `ToggleSidePanel`, `ExpandDir`, `CollapseDir`, `CollapseAll`, `OpenSelected`, `OpenSelectedBg`, `OpenMessages`, `Abort`
- **Lifecycle**: `Quit`, `Suspend`

These are pure data. No side effects, no Driver involvement. They go into `crates/core/src/types.rs` and re-export from `lib.rs`.

### 1C. PanelSlot

Port `PanelSlot` (Main, Side, StatusBar, Overlay),

---

## Phase 2: Terminal Driver

The terminal is the primary I/O boundary. In the old code, `main.rs` owns a crossterm `EventStream` and a ratatui `Terminal`. In FRP, this becomes a Driver that consumes a stream of "what to render" and produces a stream of "what the user did".

### 2A. Input Driver

Create `crates/terminal/src/input.rs`. This driver:

- Takes no derived input (it is a source)
- Spawns a task that reads `crossterm::event::EventStream`
- Converts `KeyEvent` to our `KeyCombo` type (stripping SHIFT from character keys, as the old `KeyCombo::from_key_event` does)
- Converts resize events to viewport size updates
- Handles `FocusGained` / `FocusLost` terminal events
- Outputs a stream of `TerminalInput` enum: `Key(KeyCombo)`, `Resize(u16, u16)`, `FocusGained`, `FocusLost`

The driver also handles entering/exiting raw mode, alternate screen, mouse capture, and bracketed paste. On drop (or stream end), it restores the terminal. The panic hook that restores terminal state lives here too.

### 2B. Render Driver

Create `crates/terminal/src/render.rs`. This driver:

- Takes a `LatestStream<Arc<State>>` as input (only the latest state matters for rendering, we never need to paint stale frames)
- Owns the ratatui `Terminal`
- On each new state, calls a pure `render(state) -> Frame` function and flushes to screen
- Outputs nothing (or a unit stream for backpressure)

The render function itself is pure: it reads the State and produces ratatui widget calls. It lives in `led/src/ui.rs` as it does today, but expanded to cover all panels. The render driver just orchestrates the paint loop.

### 2C. Keymap Resolution in Model

The Model receives `TerminalInput` from the terminal driver. When it gets a `Key(KeyCombo)`, it must resolve it against the keymap (including chord state) to determine the `Action`. This logic comes from `shell.rs`'s `handle_key_event`:

- Check if we're in a chord state (a prefix key like `Ctrl+X` was pressed)
- If chord pending: look up `(prefix, key)` in `keymap.chords`. If found, produce the action. If not, cancel chord.
- If no chord: look up `key` in `keymap.direct`. If it returns `ChordPrefix`, enter chord state. If it returns `Action`, produce it.
- Context-sensitive keymaps: check `keymap.contexts[current_context]` first (e.g., "browser", "picker", "file_search" contexts have their own bindings)

The chord timeout (500ms) and chord state are part of `State`. This is pure model logic, no Driver needed.

### 2D. Action Routing in Model

In the old code, the Shell routes actions to the "focused component". In FRP, the Model simply pattern-matches on the current `focus` slot and the action to decide which part of State to mutate. There is no dynamic dispatch. Each action handler is a function `(State, Action) -> State`.

The model file will grow large. Organize it into sub-modules: `model/buffer.rs`, `model/browser.rs`, `model/search.rs`, etc. The top-level model merges all driver outputs into a single stream and dispatches.

---

## Phase 3: Document Model

The text editing engine is the heart of the editor. Port the document storage layer first, then the editing operations.

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

In the old code, `DocStore` is a `HashMap<PathBuf, TextDoc>` that lives on `Context`. In FRP, the docs live directly inside `State.buffers` — each `BufferState` owns its `TextDoc`. There is no separate DocStore; the State *is* the store. Any model function that needs to read or mutate a doc accesses `state.buffers[path].doc`.

---

## Phase 4: Buffer Core

### 4A. BufferState

Define the `BufferState` struct that replaces the old `Buffer` component's 56 fields. Group them logically:

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

### 4B. File I/O Driver

Create `crates/file-io/`. This driver handles all file read/write operations:

**Input (from Derived):** a stream of file operation requests:
- `ReadFile(PathBuf)` — load file from disk
- `WriteFile { path, content: String }` — save content to disk

**Output:** a stream of results:
- `FileRead { path, content: Rope, hash: u64 }` — successful read with content hash
- `FileReadError { path, error: String }` — binary file, permission error, etc.
- `FileWritten { path, hash: u64 }` — successful write with new content hash
- `FileWriteError { path, error: String }`

The driver performs binary detection (null byte check on first 8KB), path canonicalization, and xxHash computation. The model receives these results and updates `BufferState` accordingly.

### 4C. Cursor Movement

Port all cursor movement functions from `buffer/editing.rs` as pure functions that take `(&BufferState, &TextDoc) -> (usize, usize)` and return the new cursor position. The model calls these and writes the result to state:

- `compute_move_up`, `compute_move_down` — these are already pure functions in the old code, handling soft-wrap sub-line navigation via `expand_tabs`, `compute_chunks`, `find_sub_line`
- `move_left`, `move_right` — wrapping across line boundaries
- `move_to_line_start`, `move_to_line_end`
- `page_up`, `page_down` — viewport-sized jumps
- `move_to_file_start`, `move_to_file_end`

Port the wrap utilities from `buffer/wrap.rs`: `expand_tabs`, `visual_line_count`, `compute_chunks`, `find_sub_line`, `display_col_to_char_idx`. These are already pure.

### 4D. Text Editing in Model

Port the editing operations from `buffer/editing.rs`. Each takes the current BufferState + TextDoc mutably and performs the edit:

- `insert_char` — insert character, update cursor, record undo, update dirty flag
- `insert_newline` — insert newline + auto-indent (two-pass tree-sitter with regex fallback), compound undo entry
- `delete_char_backward`, `delete_char_forward` — delete with undo recording
- `kill_line` — kill to end of line (or join with next line), accumulates in kill ring
- `kill_region` — delete selection between mark and cursor
- `yank_text` — paste from clipboard/kill-ring
- `indent_line` — reindent current line via tree-sitter/regex
- `apply_text_edits` — apply LSP edits in reverse document order with compound undo

The undo grouping logic (`PendingGroup` with 1000ms timeout, merging consecutive same-type edits) is pure state manipulation that lives in the model.

### 4E. Scroll Clamping

After every model update that changes cursor position, the viewport must be adjusted. Port the scroll clamping logic: if cursor is above `scroll_offset`, scroll up; if cursor is below `scroll_offset + viewport_height`, scroll down. This accounts for soft-wrapped lines via `visual_line_count`.

---

## Phase 5: Tab Management

### 5A. TabState

Define `TabState` in core:

```
TabState {
    order: Vec<PathBuf>,           // tab order (paths into State.buffers)
    active: usize,                 // index into order
    pre_preview: Option<usize>,    // tab to return to when preview closes
}
```

### 5B. Tab Operations in Model

Port from `shell.rs`'s `TabManager`:

- `next_tab`, `prev_tab` — cyclic navigation
- `activate_buffer_by_path(path)` — find tab by path, switch to it
- `open_file(path)` — if buffer exists, activate it; otherwise, request file read from driver. When file read completes, create `BufferState`, add to `order`
- `kill_buffer` — remove from `order`, adjust `active` index. If buffer is dirty, set modal confirmation
- `preview_file(path, row, col, match_len)` — open in preview mode (reuses existing preview tab). On any navigation action, the preview tab is auto-promoted or killed
- `preview_promote` — convert preview tab to permanent
- `preview_close` — close preview, return to `pre_preview` tab

Tab descriptors (label, dirty indicator, read_only flag, preview flag) are computed at render time from `BufferState`, not stored separately.

---

## Phase 6: Configuration System

### 6A. Keymap

Port `led/src/config.rs`:

- `KeyCombo` struct (`code: KeyCode`, `modifiers: KeyModifiers`) with `from_key_event`, `display_name`
- `Keymap` struct with `direct`, `chords`, and `contexts` maps
- `KeymapLookup` enum: `Action`, `ChordPrefix`, `Unbound`
- TOML parsing: `parse_key_combo`, `toml_to_keymap`, `parse_flat_table`
- Default keybindings embedded as `DEFAULT_KEYS_TOML` constant
- `load_or_create_config()` — reads `~/.config/led/keys.toml` or writes default

The keymap itself is pure data that lives in `State.keymap`. The model uses it during action resolution.

### 6B. Theme System

Port `led/src/theme.rs`:

- `Theme` struct with `styles: HashMap<String, ElementStyle>`
- `ElementStyle`: `fg`, `bg`, `bold`, `reversed`
- Color resolution: hex (`#fff`, `#ffffff`), ANSI names (`ansi_black`, `ansi_bright_red`), aliases (`$color_name`), style objects (`{fg = "...", bg = "...", bold = true}`)
- `rgb_to_indexed(r, g, b)` — maps to nearest xterm-256 color
- Truecolor detection via `COLORTERM` env var
- Default theme embedded as TOML constant
- Loading from `~/.config/led/theme.toml`

Theme lives in `State.theme`. Used by the render function (pure).

### 6C. Config Watch Driver

Create a config watcher driver. In the old code, `main.rs` spawns a `notify` watcher on `~/.config/led/`. In FRP:

**Input:** None (this is a source driver, always watching)

**Output:** `ConfigChanged { keymap: Option<Keymap>, theme: Option<Theme> }`

The driver watches `keys.toml` and `theme.toml` with `notify`. On change, it re-parses the file and emits the new config. The model receives this and replaces `State.keymap` / `State.theme`. If parsing fails, the driver logs the error and emits nothing.

---

## Phase 7: File Browser

### 7A. FileBrowserState

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

### 7B. File Browser Driver

The file browser needs to read directories. This is a side effect.

**Input (from Derived):** stream of `BrowseRequest` — either a rebuild request (when `expanded_dirs` changes) or an explicit refresh request (when workspace changes)

**Output:** `BrowseResult { entries: Vec<TreeEntry> }` — the flattened, sorted tree

The driver walks the directory tree from `root`, respecting `expanded_dirs`. It skips dotfiles, sorts directories before files, both alphabetically. This is the `rebuild` + `walk_dir` logic from `file-browser/lib.rs`, moved to a blocking task.

### 7C. Browser Actions in Model

- `MoveUp`, `MoveDown`, `PageUp`, `PageDown` — update `selected` with clamping
- `ExpandDir` — add to `expanded_dirs`, trigger rebuild
- `CollapseDir` — remove from `expanded_dirs`, trigger rebuild
- `CollapseAll` — clear `expanded_dirs`, trigger rebuild
- `OpenSelected` — if file: emit file-open intent; if directory: toggle expand
- `OpenSelectedBg` — open file without switching focus
- `reveal(path)` — expand all ancestors, set `selected` to the entry. Used when opening a file to show it in the browser

### 7D. Browser Rendering

The file browser is rendered in the Side panel slot. The render function reads `State.file_browser` and `State.git.file_statuses` to draw:

- Tree with indentation (2 spaces per depth level)
- Directory/file icons
- Git status indicators (M/A/U) with theme colors
- Diagnostic severity indicators per file
- Selected entry highlight
- Scroll viewport

---

## Phase 8: Workspace & File Watching

### 8A. Workspace Watcher Driver (already exists)

The current `led_workspace::driver` takes a stream of `PathBuf` and outputs the git root. This is already implemented. It will also be extended with the old `WorkspaceWatcher` functionality:

**Additional output:** `WorkspaceChanged` events when files are created/removed in the workspace. Uses `notify::recommended_watcher` in recursive mode on the root, filtering out `.git` internal changes. This was the `workspace-watcher` crate's job.

### 8B. File Watcher Driver

Individual buffer files need to be watched for external changes. Create `crates/file-watcher/`:

**Input (from Derived):** stream of `WatchSet` — the set of open file paths that should be watched

**Output:** `FileChanged { path, kind }` where kind is Created/Modified/Removed

The driver maintains a set of `notify` watchers. When the watch set changes, it adds/removes watchers. It watches parent directories (not files directly) to catch renames. It includes the `self_notified` debounce logic to avoid reacting to our own saves.

The model receives `FileChanged` and sets `disk_modified` / `disk_deleted` flags on the affected `BufferState`. If the buffer is not dirty, it can auto-reload. If dirty, it shows a warning in the status bar.

---

## Phase 9: Git Integration

### 9A. GitState

```
GitState {
    branch: Option<String>,
    file_statuses: HashMap<PathBuf, HashSet<FileStatus>>,
    line_statuses: HashMap<PathBuf, Vec<LineStatus>>,
}
```

### 9B. Git Driver

Port `git-status/lib.rs`. The old code uses two worker tasks (file status and line status) with `Notify`-based triggering. In FRP:

**Input (from Derived):** a stream of `GitRequest`:
- `ScanFiles` — triggered on file save, workspace change, resume
- `ScanLines(PathBuf)` — triggered on tab activation, file save

**Output:** a stream of `GitResult`:
- `FileStatuses { statuses: HashMap<PathBuf, HashSet<FileStatus>>, branch: Option<String> }`
- `LineStatuses { path: PathBuf, statuses: Vec<LineStatus> }`

The driver uses `git2` on `spawn_blocking` tasks:

- `scan_file_statuses(root)` — opens repo, queries `repo.statuses()`, maps to `FileStatus` enum (GitModified, GitAdded, GitUntracked), extracts branch from HEAD
- `scan_line_statuses(root, path)` — computes diff between HEAD blob and disk file using `git2::Patch::from_buffers`, produces `LineStatus` entries with row ranges and kind (GitAdded/GitModified)

Both include 50ms coalescing delay.

### 9C. Derived Git Triggers

Derived selects from State to trigger git operations:

- When a file save occurs (detected by comparing `base_content_hash` changes), emit `ScanFiles` + `ScanLines(path)`
- When the active tab changes, emit `ScanLines(new_path)`
- When `WorkspaceChanged` is set, emit `ScanFiles`

---

## Phase 10: Syntax Highlighting

### 10A. Syntax Driver

Port `buffer/syntax.rs`. Tree-sitter parsing is CPU-intensive and must happen off the main thread.

**Input (from Derived):** a stream of `SyntaxRequest`:
- `Parse { path, rope: Rope }` — full parse (on file open)
- `IncrementalParse { path, rope: Rope, edits: Vec<InputEdit> }` — incremental re-parse after edit

**Output:** `SyntaxResult { path, highlights: Vec<HighlightSpan> }` — the highlight spans for rendering

The driver:
- Detects language from file extension (the `language_id_for_extension` registry)
- Uses `tree_sitter` to parse on `spawn_blocking`
- Supports cancellation via `AtomicBool` flag (if a new parse request arrives for the same path, cancel the old one)
- Computes highlight spans by walking the tree with highlight queries
- Returns flat `(row, col_start, col_end, highlight_name)` spans for the renderer

The model stores the highlight spans in `BufferState.syntax_state`. The renderer uses them to colorize text according to `State.theme.styles["syntax.*"]`.

### 10B. Auto-Indent Support

The old code uses tree-sitter for auto-indent via `suggest_indent` and `closing_bracket_indent`. These operate on the parse tree to determine indent level. Since the tree lives in the driver, there are two approaches:

1. **Synchronous fallback:** Use regex-based indent when tree is not available. The regex patterns (`increase_indent_pattern`, `decrease_indent_pattern`) are per-language and pure.
2. **Request-response:** The indent request goes to the syntax driver, which computes indent level from the current tree and returns it. This adds a round-trip but is more accurate.

For the initial implementation, use regex fallback with the `detect_indent_unit`, `get_line_indent`, `apply_indent_delta`, and `find_prev_nonempty_line` pure functions from `editing.rs`. Tree-sitter indent can be added as an enhancement once the basic loop works.

---

## Phase 11: In-Buffer Search (ISearch)

### 11A. ISearchState

Already defined above in BufferState. Port the struct:

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

### 11B. Search Logic in Model

Port from `buffer/search.rs`. All search logic is pure (operates on TextDoc in memory):

- `start_search` — snapshot current position as origin, initialize empty query
- `find_all_matches(doc, query)` — case-insensitive substring scan, returns `(row, col, char_len)` triples
- `update_search` — recompute matches after query change, jump to first match at or after cursor
- `search_next` — advance to next match; if failed, wrap to first match; if query empty, recall `last_search` (Emacs C-s C-s behavior)
- `search_cancel` — restore origin position, save query to `last_search`
- `search_accept` — keep current position, clear search state

### 11C. Search Rendering

The renderer draws the search state: "Search: {query}" or "Failing search: {query}" in the status area. Match highlights are rendered as background spans on the buffer text. The current match gets a distinct highlight color.

---

## Phase 12: File Search (Ripgrep)

### 12A. FileSearchState

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

With `FileGroup { path, relative, hits: Vec<SearchHit> }`, `SearchHit { row, col, line_text, match_start, match_end }`, `FlatHit { group_idx, hit_idx }`.

### 12B. Search Driver

Port `file-search/search.rs`. The search worker is a side effect (disk I/O + CPU):

**Input (from Derived):** a stream of `SearchRequest { query, root, case_sensitive, use_regex }`

**Output:** `SearchResult(Vec<FileGroup>)` — grouped search hits

The driver:
- Uses `grep` crate (ripgrep-compatible) with `RegexMatcherBuilder`
- Uses `ignore::WalkBuilder` for traversal (respects `.gitignore`)
- Binary detection (quit on null byte)
- Per-file search with byte-offset-to-char-col conversion
- Groups by file, sorts by path
- Caps at 1000 total hits
- Drains request queue, processes only latest (debounce)

### 12C. Search Actions in Model

- `OpenFileSearch` — set `active = true`, focus side panel. If there is selected text in the active buffer, pre-fill the query with it (via the `FileSearchOpened { selected_text }` pattern from the old code)
- `CloseFileSearch` — set `active = false`, return focus to main
- `ToggleSearchCase` / `ToggleSearchRegex` — flip flags, re-trigger search
- Character input — modify query, re-trigger search
- `MoveUp` / `MoveDown` — navigate selected hit in flat list
- `OpenSelected` (Enter) — emit file-open + goto position for the selected hit

### 12D. Search Rendering

Rendered in the Side panel when active:
- Top bar: query input field with cursor, case/regex toggle indicators
- Below: file groups with hit count per file
- Each hit: line number, line text with match highlighted
- Selected hit has distinct background

---

## Phase 13: Find File Panel

### 13A. FindFileState

```
FindFileState {
    active: bool,
    input: String,
    cursor: usize,
    completions: Vec<Completion>,
    selected: Option<usize>,
}
```

### 13B. Find File Driver

Directory listing for path completion is a side effect:

**Input (from Derived):** a stream of `CompletionRequest { dir: PathBuf, prefix: String }`

**Output:** `CompletionResult(Vec<Completion>)` with `Completion { name, full_path, is_dir }`

The driver reads the directory, filters by prefix, separates dirs from files, sorts both alphabetically, and returns. It handles tilde expansion (`~` to `$HOME`) and path normalization (`.` and `..` resolution).

### 13C. Find File Actions in Model

- `FindFile` — activate panel, set focus to status bar
- Character input — modify path, recompute completions via driver
- `MoveUp` / `MoveDown` — navigate completions (wrapping)
- Enter — if selected is file: open it; if directory: expand input to that directory and recompute
- Escape — close panel

### 13D. Find File Rendering

Rendered as a status bar input area plus a side panel showing completions list. Each completion shows filename with directory/file indicator.

---

## Phase 14: LSP Integration

This is the most complex subsystem. It involves multiple side effects (process management, JSON-RPC I/O, file watching) and substantial state.

### 14A. LspState

```
LspState {
    servers: HashMap<String, ServerStatus>,    // language_id -> status
    opened_docs: HashSet<PathBuf>,
    pending_code_actions: HashMap<PathBuf, Vec<CodeAction>>,
    progress: HashMap<String, ProgressState>,
    status: Option<LspStatus>,
    completion_triggers: HashMap<String, Vec<String>>,  // extension -> triggers
}
```

`ServerStatus` tracks whether a server is starting, running, or failed.

### 14B. LSP Driver

Port `lsp/server.rs`, `lsp/transport.rs`, `lsp/manager.rs`. This is a complex driver:

**Input (from Derived):**
- `DidOpen { path, language_id, content, version }` — when tab is activated for the first time
- `DidChange { path, changes: Vec<EditorTextEdit>, version }` — when doc changes (from `TextDoc.drain_changes()`)
- `DidSave { path }` — on file save
- `DidClose { path }` — on buffer close
- `GotoDefinition { path, row, col }` — request
- `Rename { path, row, col, new_name }` — request
- `CodeAction { path, range }` — request
- `CodeActionResolve { path, index }` — resolve selected action
- `Format { path, generation }` — request
- `Completion { path, row, col }` — request
- `ResolveCompletion { path, lsp_item_json }` — resolve completion detail
- `InlayHints { path, range }` — request

**Output:**
- `GotoDefinitionResult { locations: Vec<Location> }`
- `RenameEdits { file_edits: Vec<FileEdit> }`
- `CodeActions { path, actions: Vec<CodeAction> }`
- `FormatEdits { path, edits: Vec<EditorTextEdit>, generation }`
- `CompletionItems { path, items, prefix_start_col }`
- `CompletionResolved { path, additional_edits }`
- `InlayHints { path, hints: Vec<EditorInlayHint> }`
- `Diagnostics { path, diagnostics: Vec<EditorDiagnostic> }`
- `ProgressUpdate { server_name, busy, detail }`
- `CompletionTriggers { extensions, triggers }`
- `ServerStarted { language_id }`
- `ServerError { error }`

The driver manages the full server lifecycle:

1. **Server Start:** On `DidOpen`, if no server exists for the language, start one. Spawn process via `tokio::process::Command`, set up stdin/stdout pipes, run `initialize` handshake with client capabilities (textDocument: synchronization, definition, rename, codeAction, formatting, inlayHint, diagnostic, completion with resolve).

2. **Transport:** Two tasks per server — `spawn_writer` (mpsc → JSON-RPC → stdin) and `spawn_reader` (stdout → JSON-RPC → route responses/notifications). Content-Length header framing per LSP spec.

3. **Request/Response:** Each request gets a unique ID. Responses matched via handler map. Results translated from LSP types to editor types using the convert module.

4. **Notifications from server:** `textDocument/publishDiagnostics` → diagnostic conversion. `$/progress` → progress tracking (Begin/Report/End states). `window/logMessage` → log.

5. **File Watching:** If server registers `workspace/didChangeWatchedFiles` capability, extract glob patterns and watch matching files. Send `workspace/didChangeWatchedFiles` notification on changes.

6. **UTF-16 Conversion:** Port `lsp/util.rs` functions `lsp_pos` and `from_lsp_pos` that convert between editor UTF-8 char positions and LSP UTF-16 byte offsets.

### 14C. LSP Registry

Port `lsp/registry.rs` — the language-to-server-command mapping. Pre-configured entries for: rust-analyzer, pylsp, typescript-language-server, clangd, gopls, etc. Maps file extensions to language IDs and language IDs to server commands. Pure data, lives in core.

### 14D. Derived LSP Triggers

Derived selects from State to trigger LSP operations:

- **DidOpen:** When `active_buffer` changes and the buffer's path hasn't been opened yet, emit `DidOpen` with current content
- **DidChange:** When a buffer's `doc.version` changes, `drain_changes()` and emit `DidChange`
- **DidSave:** When a buffer's `base_content_hash` changes (save detected), emit `DidSave`
- **DidClose:** When a buffer is removed from `State.buffers`, emit `DidClose`

### 14E. LSP Response Handling in Model

When the model receives LSP driver output:

- `GotoDefinitionResult` → open file at location (same as file-open + goto), record jump
- `RenameEdits` → apply `EditorTextEdit` to each affected buffer's TextDoc
- `CodeActions` → populate `picker` state with action list, set focus to Overlay
- `FormatEdits` → apply text edits to buffer, handle `pending_save_after_format` flow
- `CompletionItems` → populate `BufferState.completion` with filtered items
- `Diagnostics` → update `BufferState.diagnostics`
- `InlayHints` → update `BufferState.inlay_hints`
- `ProgressUpdate` → update `State.lsp.status`

---

## Phase 15: Completion

### 15A. CompletionState

```
CompletionState {
    items: Vec<EditorCompletionItem>,
    filtered: Vec<usize>,       // indices into items, fuzzy-sorted
    selected: usize,            // index into filtered
    prefix_start_col: usize,    // start of word being completed
}
```

### 15B. Completion Logic in Model

- On `SetCompletions` from LSP driver: build `CompletionState` with fuzzy filtering via `nucleo_matcher`
- `MoveUp` / `MoveDown` — navigate filtered list
- Character input while completion active — re-filter
- Enter — accept selected: replace prefix with completion text, apply additional edits (auto-imports), dismiss
- Escape — dismiss completion menu
- Trigger: when user types a trigger character (per-language from LSP capabilities), or manually invokes completion

### 15C. Completion Rendering

Rendered as a floating popup near the cursor:
- Show filtered items with selected highlight
- Show documentation/detail for selected item if available
- Position popup relative to cursor, avoiding screen edges

---

## Phase 16: Jump List

### 16A. JumpListState

```
JumpListState {
    list: Vec<JumpPosition>,
    index: usize,
}
```

With `JumpPosition { path, row, col, scroll_offset }`.

### 16B. Jump Logic in Model

Port from `jump-list/lib.rs`. All pure:

- `record_jump(pos)` — truncate forward history, push position, cap at 100 entries
- `jump_back(current_pos)` — if at tip, save current position first; move index back; open file at position
- `jump_forward` — move index forward; open file at position

These are triggered by:
- `RecordJump` — before any goto-definition, file-open, or search-confirm
- `JumpBack` — `Alt+B` / `Alt+Left`
- `JumpForward` — `Alt+F` / `Alt+Right`

---

## Phase 17: Picker / Overlay

### 17A. PickerState

```
PickerState {
    active: bool,
    title: String,
    items: Vec<String>,
    selected: usize,
    source_path: PathBuf,
    kind: PickerKind,
}
```

With `PickerKind::CodeAction` or `PickerKind::Outline { rows: Vec<usize> }`.

### 17B. Picker Actions in Model

- `ShowPicker` — populate state, set focus to Overlay
- `MoveUp` / `MoveDown` — navigate items
- Enter — confirm: for `CodeAction`, emit resolve request; for `Outline`, goto row
- Escape — dismiss, return focus to Main

### 17C. Picker Rendering

Rendered as a centered modal overlay on top of everything:
- Border with title
- Item list with selected highlight
- Auto-sized to content (capped at terminal dimensions)

---

## Phase 18: Messages Panel

### 18A. MessagesState

```
MessagesState {
    active: bool,
    doc: TextDoc,     // read-only text doc showing log entries
    cursor_row: usize,
    scroll_offset: usize,
    last_synced: usize,
}
```

### 18B. Log Driver

Port `led/src/logger.rs` and `core/logging.rs`:

**Output:** `LogEntry { elapsed, level, message }` — a stream of log entries

The driver implements the `log::Log` trait, capturing all `log::info!()` / `log::warn!()` etc. calls. It writes to a `SharedLog` ring buffer (capacity 10,000) and optionally to a file. The model receives entries and appends formatted lines to `MessagesState.doc`.

### 18C. Messages Logic in Model

- `OpenMessages` — set `active = true`, sync new log entries into doc
- Tick / sync — append new entries since `last_synced`, auto-scroll if cursor was at end
- `KillBuffer` — set `active = false`
- Navigation — forward standard movement actions to the read-only buffer

The `should_auto_scroll` and `compute_auto_scroll_position` pure helpers from `messages/lib.rs` determine whether to track the tail.

---

## Phase 19: Session Persistence

### 19A. Session Driver

Port `led/src/session.rs`. Database operations are side effects:

**Input (from Derived):** a stream of `SessionOp`:
- `SaveSession(SessionData)` — persist workspace state
- `SaveKV(HashMap<String, String>)` — persist component key-value pairs
- `LoadSession` — request workspace restore
- `LoadKV` — request KV restore
- `SaveBufferSession { path, cursor_row, cursor_col, scroll_offset }` — per-buffer position
- `RestoreBufferSession(PathBuf)` — request per-buffer restore

**Output:**
- `SessionLoaded(SessionData)` — restored state
- `KVLoaded(HashMap<String, String>)` — restored KV pairs
- `BufferSessionRestored { path, cursor_row, cursor_col, scroll_offset }` — restored position

The driver manages the SQLite database at `~/.config/led/db.sqlite`:

**Schema (3 tables):**
- `workspaces(root_path PK, active_tab, focus, show_side_panel)`
- `buffers(id AUTOINCREMENT, root_path FK, tab_index, file_path UNIQUE per root, cursor_row, cursor_col, scroll_offset)`
- `session_kv(root_path, key, value, PK(root_path, key))`

Includes the migration logic from old UNIQUE(root_path, tab_index) to UNIQUE(root_path, file_path).

### 19B. Primary Lock

Port the workspace lock mechanism:
- `~/.config/led/primary/{hash}` lock file using `libc::flock(LOCK_EX | LOCK_NB)`
- Hash is 64-bit hash of workspace root path
- Only the primary editor restores full workspace; secondary shows only CLI-specified file

This check happens once at startup in `main.rs` (before the FRP loop starts) and is not a driver — it's a one-shot initialization step.

### 19C. Session Save Triggers

Derived watches for session-relevant state changes:
- Periodically (every ~30s), snapshot current state and emit `SaveSession`
- On graceful quit, emit final `SaveSession`
- Component KV data: file browser expanded dirs, jump list entries, search state

### 19D. Session Restore in Model

On startup (if primary):
1. Load session data → set `active_tab`, `focus`, `show_side_panel`
2. Load buffer paths → request file reads for each
3. As file reads complete, restore cursor/scroll from saved positions
4. Load KV → restore file browser expanded dirs, jump list, etc.

---

## Phase 20: Modal Dialogs

### 20A. ModalState

```
ModalState {
    prompt: String,
    input: String,
    pending_action: PendingAction,
}

RenameModalState {
    prompt: String,
    input: String,
    path: PathBuf,
    row: usize,
    col: usize,
}
```

### 20B. Modal Logic in Model

Modals intercept all keyboard input when active:
- Characters → append to `input`
- Backspace → delete last character
- Enter → for confirmation modal, check if input == "yes"; for rename, execute rename
- Escape / Ctrl+G → cancel

Triggered by:
- `ConfirmAction { prompt, action }` — dirty buffer kill, quit with unsaved changes
- `PromptRename { prompt, initial, path, row, col }` — LSP rename refactoring

### 20C. Rename Execution

When a rename modal is confirmed, the model emits the `LspRename` request to the LSP driver. This is an effect that flows through the model into derived → LSP driver. The file system rename for the file browser (old code's `Effect::PromptRename`) is a separate path.

### 20D. Modal Rendering

Both modal types render as centered bordered boxes overlaying the main content. The confirmation modal shows prompt + input. The rename modal shows current name + editable input.

---

## Phase 21: Clipboard Driver

### 21A. Clipboard Driver

The old code wraps `arboard::Clipboard` behind a trait. In FRP:

**Input (from Derived):** `SetClipboard(String)` — when a kill/copy operation occurs

**Output:** `ClipboardContent(String)` — when a yank/paste is requested

The driver wraps `arboard::Clipboard` with `Mutex` for thread safety (same as `ArboardClipboard` in `shell.rs`). The model triggers clipboard reads on `Yank` action and clipboard writes on `KillRegion` / `KillLine`.

---

## Phase 22: Process Signals

### 22A. Suspend/Resume

`Ctrl+Z` (`Suspend` action) needs to:
1. Restore terminal to normal mode
2. `libc::raise(SIGTSTP)` to suspend the process
3. On resume, re-enter raw mode + alternate screen
4. Emit `FocusGained`-equivalent to trigger git refresh

This is handled by the terminal driver. When the model sets a "suspend requested" flag in State, the terminal driver sees it (via derived), performs the suspend, and emits a `Resumed` output when the process continues.

---

## Phase 23: Rendering (Complete)

### 23A. Full Layout

Port `led/src/ui.rs` render function. The layout:

1. **Status bar** (bottom 1 line): filename, dirty indicator (`*`), git branch, LSP status (spinner + detail), cursor position (L:C)
2. **Main area** (everything above status bar):
   a. If side panel visible and width > 50: split into sidebar (25 chars) + editor area
   b. Editor area: tab bar (bottom 1 line of editor area) + buffer content (rest)
3. **Tab bar**: truncated filenames with dirty/preview indicators, active tab highlighted
4. **Overlay**: if picker/modal active, render centered on top of everything

### 23B. Buffer Rendering

The buffer renderer is the most complex view. It draws:

- **Gutter**: line numbers (right-aligned), git line status indicators (colored bar), diagnostic severity icons
- **Text area**: syntax-highlighted text with soft wrapping (backslash at wrap point)
- **Cursor**: positioned at `(cursor_row, cursor_col)`, accounting for scroll offset and soft wrap
- **Selection**: highlighted region between mark and cursor
- **Search matches**: highlighted background spans, current match distinct
- **Diagnostics**: underline spans with severity color
- **Inlay hints**: inline virtual text (dimmed) after expressions
- **Completion popup**: floating menu near cursor
- **Scroll**: respect `scroll_offset` and `scroll_sub_line`

### 23C. Color Hints

Port `buffer/color_hint.rs` — renders inline color swatches for hex color literals in the text (e.g., `#ff0000` gets a small colored block). This is a rendering-only feature that reads hex patterns from the current line and draws them.

---

## Phase 24: Remaining Buffer Features

### 24A. Match Bracket

The old `MatchBracket` action finds the matching bracket (`()`, `[]`, `{}`) and jumps to it. This can be implemented as a pure function scanning the document, or via tree-sitter if available. Lives in model.

### 24B. Sort Imports

The `SortImports` action sorts contiguous import/use lines at the top of the file. A pure text transformation in the model that reads lines, detects import blocks, sorts them, and replaces in the TextDoc.

### 24C. Outline

The `Outline` action shows document symbols (functions, structs, etc.) in the picker. Two paths:
1. **LSP:** Request `textDocument/documentSymbol` from LSP driver, populate picker with symbol names and row numbers
2. **Regex fallback:** Scan document for patterns like `fn `, `struct `, `impl `, `class `, etc.

### 24D. Format on Save

When `LspFormat` is requested with `pending_save_after_format = true`:
1. Model sets `format_generation` and `pending_save_after_format` on the buffer
2. LSP driver receives format request, returns edits with generation
3. Model applies edits, then triggers save (via the file I/O driver)
4. The compound undo includes both format edits and save-time cleanup (trailing whitespace strip, final newline)

---

## Phase 25: CLI & Startup

### 25A. CLI Arguments

Port the `clap` CLI from `main.rs`:
- `path: Option<String>` — file or directory to open
- `--reset-config` — write default configs, clear DB
- `--debug` — set log level to Debug, show key presses
- `--log-file <path>` — file logging
- `--script <path>` — headless script execution

### 25B. Startup Sequence

1. Parse CLI args
2. Resolve starting directory, canonicalize path
3. Acquire primary workspace lock
4. Build initial `Config` and `State`
5. Start all drivers
6. Wire up the FRP cycle (Derived → Drivers → Model → hoist → Derived)
7. If primary: request session restore from session driver
8. If `--script`: run script driver instead of terminal driver
9. Run hoist loop until quit

### 25C. Script Driver

The old `--script` mode reads a file with one JSON action per line and feeds them as if they were keyboard input. This is an alternative input driver:

**Output:** stream of `Action` (parsed from JSON lines), with `wait <ms>` commands as delays

This enables headless testing and automation.

### 25D. Graceful Shutdown

When the model processes a `Quit` action:
- Check for unsaved buffers → if any, show confirmation modal
- If confirmed: save session, stop LSP servers, restore terminal, exit
- LSP servers are killed on drop (the driver handles cleanup)

---

## Implementation Order Summary

The phases above are already in dependency order, but here is the critical path:

1. **Phase 1** (types) — everything depends on this
2. **Phase 2** (terminal driver) — need to see things on screen
3. **Phase 3** (TextDoc) — need text storage for buffers
4. **Phase 4** (buffer core) — need editing to be useful
5. **Phase 6** (config/keymap) — need keybindings to interact
6. **Phase 5** (tabs) — need to manage multiple files
7. **Phase 7** (file browser) — need to navigate files
8. **Phase 8** (file/workspace watching) — need external change detection
9. **Phase 9** (git) — needed for status display
10. **Phase 10** (syntax) — needed for readable code
11. **Phase 11-13** (search features) — navigating large codebases
12. **Phase 14-15** (LSP + completion) — the heavy lift
13. **Phase 16-18** (jump list, picker, messages) — supporting features
14. **Phase 19** (session) — persistence
15. **Phase 20-24** (modals, clipboard, remaining) — polish
16. **Phase 25** (CLI, startup, shutdown) — production readiness

Each phase should result in a working (if incomplete) editor. Phase 2 gives a blank screen that reads keys. Phase 4 gives a functional single-file editor. Phase 6 adds proper keybindings. Phase 7 adds file navigation. And so on.
