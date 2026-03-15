# Implementation Plan: FRP Text Editor

**State -> Derived -> Drivers -> Model -> State**

Every side effect lives in a Driver. The Model is a pure reduce: `(State, Mut) -> State`. Derived selects and transforms State fields into Driver inputs. Side effects that react to state live in `_of` functions (e.g. `process_of`).

---

## Next: Phase 5 — Editor Styling, Gutter & Line Wrapping

The current render loop uses theme colors for text and gutter but does not match the old editor's gutter design or fill behaviors. Line wrapping must come now because it fundamentally changes how every line is rendered, how the cursor is positioned, and how scrolling works — every later phase (selection, search, syntax) builds on top of the wrap-aware display model.

### 5A. Line Wrapping

Line wrapping is not cosmetic — it defines the mapping between logical lines and screen rows that all rendering and cursor logic depends on.

- `expand_tabs()` — convert tabs to 4 spaces, produce `char_map` (logical char index → display column)
- `compute_chunks()` — split display line into `(start, end)` display-column ranges at `text_width - 1`
- Continuation indicator `\` in gutter style at end of non-last chunks
- Gutter shows `"  "` (spaces) on continuation lines (only first chunk gets the real gutter content)
- `scroll_sub_line` on BufferState for partial-line scroll when first visible line is wrapped
- Cursor movement (up/down) navigates visual sub-lines, preserving display column
- `visual_line_count()` computes how many screen rows a logical line occupies
- `adjust_scroll()` keeps cursor visible accounting for wrapped lines

### 5B. Gutter Redesign

Old editor uses a 2-char fixed gutter (no line numbers in the gutter itself). Each row has two columns:

- **Left column (1 char):** Git line-status indicator — `▎` colored by `git.gutter_added` / `git.gutter_modified`, or space with `editor.gutter` background when no status.
- **Right column (1 char):** Diagnostic indicator `●` colored by `diagnostics.error/warning/info`, OR color preview block (hex color / theme value), OR space with `editor.gutter` background.
- **Continuation lines** (wrapped): 2 spaces with `editor.gutter` background.
- **Past-EOF lines:** `~ ` (tilde + space) in `editor.gutter` style.

For now (no git, no diagnostics): render 2-char gutter as `"  "` in `editor.gutter` style on first chunk of each line, `"  "` on continuation chunks. Remove the line-number display. Past-EOF rows show `~ `.

### 5C. Text Area Background Fill

Old editor fills the entire text area with `editor.text` background via `Paragraph::new(display_lines).style(text_style)` rendered into the full area. Current code uses per-line `Paragraph` widgets — switch to building a `Vec<Line>` (one entry per screen row, accounting for wrapped chunks) rendered as a single `Paragraph` so the background covers the full region uniformly.

### 5D. Tab Bar Gutter Alignment

Old editor starts tab labels at `x + GUTTER_WIDTH - 1` (1 char into the gutter). Tab bar background uses `tabs.inactive` style. Currently correct but verify alignment matches old behavior once gutter width changes to 2.

---

## Phase 6 — Status Bar & Input Bar

The old editor's status bar is driven by the active buffer component and doubles as an input bar during incremental search.

### 6A. Default Status Bar Content

When no special mode is active, the status bar shows a single line:

```
 {filename}{dirty_dot}{branch}{lsp_status}              L{row}:C{col}
```

- **Left:** ` {filename}` — space-prefixed filename from active buffer
- **Dirty indicator:** ` ●` (U+25CF) if buffer is modified
- **Branch:** ` ({branch})` from git status (empty string if no branch yet)
- **LSP status:** `  {spinner}{server_name}  {spinner} {detail}` — spinner is braille animation when busy (skip for now, leave room in format)
- **Right:** `L{row+1}:C{col+1} ` — cursor position, right-aligned with trailing space
- **Style:** `status_bar.style` from theme

### 6B. Search Prompt Mode (ISearch — later phase)

When incremental search is active, the status bar becomes an input bar:

```
 Search: {query}|
```

or `Failing search: {query}|` if no matches. Cursor is positioned after the query text. This replaces the normal status bar content entirely.

### 6C. Transient Messages

Shell-level messages (warnings, info) take priority over the normal status bar when present. Show ` {message}` left-aligned, padded to full width, in `status_bar.style`. Messages auto-clear after a timeout (handled by model).

### 6D. Implementation

Currently `render_status_bar` shows `led: {workspace}` on the left and `{file} L:C` on the right. Change to match the old format: filename-first left side, no workspace name in status bar. The workspace name belongs in the tab bar or title — not the status bar.

---

## Phase 7 — Tab Management

### 7A. BufferState additions

Add `preview: bool` for preview tabs that auto-close when navigating away.

### 7B. Tab Operations in Model

- `NextTab` / `PrevTab` — cyclic navigation through buffers sorted by tab_order
- `KillBuffer` — remove buffer, adjust active_buffer. If dirty, warn (or modal later)
- Opening a file that's already open → activate its tab instead of re-opening

### 7C. Tab Rendering Refinements

Old editor has richer tab labels:
- Prefix: `●` (dirty), `#` (read-only), or space (clean)
- Filename truncated to 15 chars + ellipsis if needed
- Preview tabs use `tabs.preview_active` / `tabs.preview_inactive` styles
- Tab labels rendered via direct buffer writes (`set_string`), not `Paragraph` widgets — 1 char gap between tabs

### 7D. Derived: Open from file browser / find-file

Currently only CLI arg triggers file open. Need to support opening files from within the editor (Action::OpenFile or similar).

---

## Phase 8: File Browser

### 8A. FileBrowserState

```
FileBrowserState {
    entries: Vec<TreeEntry>,
    selected: usize,
    expanded_dirs: HashSet<PathBuf>,
    scroll_offset: usize,
}
```

### 8B. File Browser Driver

Walks directory tree from workspace root, respects expanded_dirs. Skips dotfiles, sorts dirs before files.

### 8C. Browser Actions

ExpandDir, CollapseDir, CollapseAll, OpenSelected, OpenSelectedBg — all wired to the existing Action variants.

### 8D. Browser Rendering

Tree with indentation in the Side panel. Selected entry highlight. Scroll viewport. `▼`/`▶` icons for expanded/collapsed dirs. Status indicators from git/diagnostics once those exist.

---

## Phase 9: Selection & Kill Ring

- `SetMark` — toggle mark at cursor
- `KillRegion` — delete selection, push to kill ring
- `Yank` — paste from kill ring
- Selection rendering in UI (highlight between mark and cursor using `editor.selection` style)
- Selection extends padding to line edge on last visual line of each selected row

---

## Phase 10: In-Buffer Search (ISearch)

ISearchState with query, origin, matches, match_idx, failed flag.

- Actions: InBufferSearch (open/next), character input, DeleteBackward, Abort (cancel), Enter (accept)
- `find_all_matches()` — case-insensitive substring search across all lines
- Visible matches rendered with `editor.search_match`, current match with `editor.search_current`
- Status bar switches to search prompt mode (Phase 6B)
- Cancel restores origin cursor/scroll position

---

## Phase 11: Syntax Highlighting

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

## Phase 12: Git Integration

git2 driver for branch, file statuses, line statuses.

- **File status worker:** scans entire repo on save/resume (50ms coalescing)
- **Line status worker:** per-file diff on tab activation
- Status bar branch display
- File browser status icons (later)
- Gutter diff markers: `▎` colored by `git.gutter_added` / `git.gutter_modified`

---

## Phase 13: File Search (Ripgrep)

`grep_searcher` + `ignore::WalkBuilder`. Background worker with request coalescing (only process latest query).

### UI
- Claims Side panel (priority 20)
- Row 0: toggle buttons (case-sensitive, regex) with `file_search.toggle_on/off` styles
- Row 1: search input with cursor
- Rows 2+: results grouped by file with line numbers, highlighted matches (`file_search.match_`)
- Scroll-into-view, max 1000 hits

---

## Phase 14: Find File Panel

Directory completion, tilde expansion, path abbreviation.

- Status bar shows `Find file: {input}` with cursor
- Side panel shows completion list with directory icons
- Tab completion: single match → complete, multiple → longest common prefix
- Wrapping selection through completions

---

## Phase 15: LSP Integration

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

## Phase 16: Session Persistence

SQLite at config_dir/db.sqlite. Workspace, buffer positions, undo chains. Restore on startup if primary.

### Undo persistence
- `buffer_undo_state` table: chain_id, content_hash, undo_cursor, distance_from_save
- `undo_entries` table: msgpack-serialized entries
- Cross-instance sync via chain_id detection
- Flush on tick

### Color hint (theme file editing)
- `scan_hex_color()` for `#rrggbb` / `#rgb` in any file → color preview in gutter
- `parse_color_defs()` + `evaluate_theme_line()` for theme.toml files → full style preview in gutter

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

