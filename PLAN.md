# Implementation Plan: FRP Text Editor

**State -> Derived -> Drivers -> Model -> State**

Every side effect lives in a Driver. The Model is a pure reduce: `(State, Mut) -> State`. Derived selects and transforms State fields into Driver inputs. Side effects that react to state live in `_of` functions (e.g. `process_of`).

---

## Phase 9: Session Persistence & Cross-Instance Sync

SQLite at config_dir/db.sqlite. Workspace, buffer positions, undo chains. Restore on startup if primary.

### Primary instance lock

One editor per workspace gets the primary lock via `flock` on `config_dir/primary/{workspace_hash}`. Primary saves session state (open tabs, cursor positions) on exit; secondary does not. Already partially implemented in `workspace::try_become_primary`.

### Session data

Primary saves on exit, restores on startup:
- Open buffer paths + tab order
- Cursor position (row, col) per buffer
- Scroll position per buffer
- Active tab index

### Undo persistence

#### Tables

- `buffer_undo_state`: `(root_path, file_path)` → `chain_id`, `content_hash`, `undo_cursor`, `distance_from_save`
- `undo_entries`: `(root_path, file_path, seq)` → `entry_data` (msgpack-serialized `UndoEntry`)

#### Flush

Each buffer tracks `persisted_undo_len`. On tick (or periodic timer), if `has_unpersisted_undo()`, flush new entries to SQLite. Generate `chain_id` on first flush (unique per editing session). After save, delete undo state from DB and reset `chain_id`.

### Cross-instance sync

When two led instances open the same file, they coordinate via SQLite + a notify directory.

#### Notify directory

`config_dir/notify/` — shared directory watched by all instances. When an instance writes undo entries to SQLite, it touches `notify/{file_path_hash}` to wake other instances watching the same file. The writing instance sets `self_notified = true` so it ignores its own touch.

#### Sync algorithm (`check_cross_instance_sync`)

Triggered by file watcher firing on the notify directory. Each buffer tracks `last_seen_seq` (highest undo entry sequence number it has processed).

1. Query SQLite for `buffer_undo_state` + any `undo_entries` with `seq > last_seen_seq`
2. If no rows and buffer is dirty → reload from disk (external save cleared the DB)
3. If rows exist, compare `chain_id`:
   - **Same chain** (same editing session, e.g. instance was restarted): replay new entries via `apply_remote_entries`, update `last_seen_seq`
   - **Different chain** (another instance started editing): if content hash differs → reload from disk. Then adopt new chain_id and replay all its entries from seq 0

#### External change detection (disk watcher)

Separate from cross-instance sync. File watcher on parent directory detects disk modifications (external tools, git checkout, etc).

Classification (`classify_disk_state`):
- **Unchanged**: disk hash matches base hash (covers own save)
- **Reloadable**: disk changed, buffer is clean → reload from disk, clamp cursor, show message
- **ConflictNew**: disk changed, buffer is dirty → warn user, don't reload
- **ConflictAlready**: already warned about this conflict
- **DeletedNew**: file deleted externally → warn user
- **DeletedAlready**: already warned about deletion

On reload: replace rope, re-parse syntax, reset undo history, clear dirty flag, update base content hash.

#### Save flow

On save:
1. Strip trailing whitespace from all lines
2. Ensure final newline
3. Record deferred compound undo for format-on-save (full before/after as single undo step)
4. Write to disk
5. On first save: canonicalize path, start watcher
6. Drain pending changes (so format edits aren't sent as LSP didChange)
7. Reset dirty, distance_from_save, base_content_hash
8. Delete undo state from SQLite (clean slate for next editing session)
9. Touch notify dir to wake other instances
10. Reset chain_id and last_seen_seq

### Color hint (theme file editing)
- `scan_hex_color()` for `#rrggbb` / `#rgb` in any file → color preview in gutter
- `parse_color_defs()` + `evaluate_theme_line()` for theme.toml files → full style preview in gutter

---

## Phase 10: Selection & Kill Ring

- `SetMark` — toggle mark at cursor
- `KillRegion` — delete selection, push to kill ring
- `Yank` — paste from kill ring
- Selection rendering in UI (highlight between mark and cursor using `editor.selection` style)
- Selection extends padding to line edge on last visual line of each selected row

---

## Phase 11: In-Buffer Search (ISearch)

ISearchState with query, origin, matches, match_idx, failed flag.

- Actions: InBufferSearch (open/next), character input, DeleteBackward, Abort (cancel), Enter (accept)
- `find_all_matches()` — case-insensitive substring search across all lines
- Visible matches rendered with `editor.search_match`, current match with `editor.search_current`
- Status bar switches to search prompt mode (Phase 6B)
- Cancel restores origin cursor/scroll position

---

## Phase 12: Syntax Highlighting

Tree-sitter driver for incremental parsing. Highlight spans stored per-buffer. Language detection from extension.

Old editor supports: Rust, Python, JS/TS/TSX, JSON, TOML, Markdown, Bash, C/C++, Swift, Make.

### Rendering pipeline

Per-visible-line, per-display-column style array:
1. Base: `editor.text`
2. Syntax captures applied (sorted by span size descending — inner overwrites outer)
3. Capture name resolution: `syntax.{capture_name}`, fallback to `syntax.{parent}`, fallback to text style
4. Rainbow bracket coloring (6 depth levels, wrapping)
5. Cursor bracket + matching bracket highlight (`brackets.match`)
6. Selection overlay
7. Diagnostic underlines (fg from `diagnostics.*`, underline modifier except for hints)
8. Search match overlay (applied last to ensure visibility)

Spans grouped by consecutive same-style runs.

### Auto-indent

Two-pass tree-sitter analysis for newline indentation, with regex fallback when tree is in error state.

---

## Phase 13: Git Integration

git2 driver for branch, file statuses, line statuses.

- **File status worker:** scans entire repo on save/resume (50ms coalescing)
- **Line status worker:** per-file diff on tab activation
- Status bar branch display
- File browser status icons (later)
- Gutter diff markers: `▎` colored by `git.gutter_added` / `git.gutter_modified`

---

## Phase 14: File Search (Ripgrep)

`grep_searcher` + `ignore::WalkBuilder`. Background worker with request coalescing (only process latest query).

### UI
- Claims Side panel (priority 20)
- Row 0: toggle buttons (case-sensitive, regex) with `file_search.toggle_on/off` styles
- Row 1: search input with cursor
- Rows 2+: results grouped by file with line numbers, highlighted matches (`file_search.match_`)
- Scroll-into-view, max 1000 hits

---

## Phase 15: Find File Panel

Directory completion, tilde expansion, path abbreviation.

- Status bar shows `Find file: {input}` with cursor
- Side panel shows completion list with directory icons
- Tab completion: single match → complete, multiple → longest common prefix
- Wrapping selection through completions

---

## Phase 16: LSP Integration

Full server lifecycle, JSON-RPC transport, request tracking.

### Server registry
Hardcoded configs: rust-analyzer, typescript-language-server, pyright, clangd, sourcekit-lsp, taplo, vscode-json-language-server, bash-language-server.

### Features
- `textDocument/didOpen`, `didChange` (incremental), `didSave`, `didClose`
- Goto definition
- Rename (workspace edits applied in reverse document order)
- Code actions with resolve
- Format (organize imports then format)
- Completion with fuzzy filtering (nucleo) and resolve for additional edits
- Inlay hints (ghost text at line end, `editor.inlay_hint` style)
- Diagnostics (pull + push, severity mapping)
- Progress tracking (spinner in status bar)
- File watching (notify crate, forwarded to servers)

### Completion popup
- Positioned below cursor (or above if near bottom)
- Width from longest label + detail, max 60 chars
- Max 10 visible items with scroll
- Fuzzy filtered via nucleo (case-insensitive, smart normalization)

---

## Phase 17: Remaining

- Modal dialogs (dirty buffer kill, quit with unsaved, LSP rename)
- Clipboard driver (arboard)
- Match bracket, sort imports, outline
- Format on save
- Jump list (record, back, forward — max 100 entries, session-persisted)
- Messages panel (log viewer — read-only buffer syncing from SharedLog)
- Workspace watcher (notify crate, recursive, skip .git, emit WorkspaceChanged on create/remove)
- CLI flags (--reset-config, --debug, --log-file)

