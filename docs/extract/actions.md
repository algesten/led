# Action inventory

Total: 59 variants. Variants marked `[dead?]` appear to have no handler despite being
defined (and, in some cases, bound in `default_keys.toml`).

Enum at `/Users/martin/dev/led/crates/core/src/lib.rs:223-314`.

Conventions for each stanza:
- **State read** lists the AppState fields a handler consults when computing its
  output (often `active_tab`, `buffers`, `dims`, `focus`).
- **State written** names the `Mut` variants the handler emits (the reducer
  assigns them mechanically — see `led/src/model/mod.rs` fold_into).
- **Triggered by** gives the default keybinding(s). The main keymap lives in
  `[keys]`; sub-tables `[keys."ctrl+x"]`, `[keys."ctrl+h"]`, `[browser]`,
  `[file_search]` are noted explicitly.
- **Dispatched** describes any derived-side driver output the action eventually
  causes (clipboard reads, FS list dir requests, LSP requests, file save). In
  almost all cases this is indirect: the action writes a pending-field or Mut,
  and `derived.rs` turns it into a driver Out on the next cycle.
- **Source** is the primary handler location.
- **Default scenario hint** is minimum initial state needed for the action to
  have an observable effect.

---

## Movement

Movement actions all follow the same shape: filter `has_blocking_overlay` /
`has_any_input_modal`, require `focus == PanelSlot::Main`, require a
materialized active buffer + `dims`, then emit `Mut::BufferUpdate(path, buf)`
with a new cursor + scroll + matching bracket. They have a browser-side
branch (MoveUp/Down/Left/Right/PageUp/PageDown/FileStart/FileEnd) handled
imperatively via `Mut::Action` in `action/browser.rs` when `focus == Side`.

### Action::MoveUp
- Purpose: Move cursor up one display line; or move browser selection up when side panel focused.
- State read: `dims`, `active_tab`, `buffers[active]` (editor); `browser.entries`, `browser.selected`, `browser.scroll_offset`, `dims` (side); `lsp.completion` when popup open (handled separately in `handle_completion_action`); `lsp.code_actions` likewise.
- State written: `Mut::BufferUpdate` (editor); `Mut::Action` → mutates browser state directly (side); completion/code-action pickers absorb via `Mut::LspCompletionAction` / `Mut::LspCodeActionPickerAction`.
- Triggered by: `up` (main mode). Re-used inside completion picker, code-action picker, and as a fanout into `compute_navigation` cycles.
- Dispatched: none directly. Selecting a file entry in the browser triggers a `Mut::Action` path that sets preview (`action::set_preview`) — this eventually requests an open through docstore.
- Source: `led/src/model/movement_of.rs:93-113` (editor); `led/src/model/action/browser.rs:6-59` (side); `led/src/model/action/lsp.rs:9-17` (completion).
- Default scenario hint: buffer with ≥3 lines and cursor on row ≥1.

### Action::MoveDown
- Purpose: Move cursor down; side-panel selection down; completion/code-action selection down.
- State read: same as MoveUp; plus `buf.doc().line_count()` for editor.
- State written: `Mut::BufferUpdate` (editor); `Mut::Action` (side); completion/picker muts for overlays.
- Triggered by: `down` (main mode).
- Dispatched: browser-side selection can set a preview.
- Source: `led/src/model/movement_of.rs:115-135`; `led/src/model/action/browser.rs:18-20`; `led/src/model/action/lsp.rs:19-28`.
- Default scenario hint: buffer with ≥3 lines; cursor above bottom.

### Action::MoveLeft
- Purpose: Move cursor one character left; in browser, collapses current directory.
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate` (editor); `Mut::Action` → `handle_browser_collapse` for side focus.
- Triggered by: `left` (main mode).
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:137-162`; `led/src/model/action/mod.rs:122-124`.
- Default scenario hint: buffer with cursor at col ≥1.

### Action::MoveRight
- Purpose: Move cursor right; in browser, expand current directory.
- State read: active buffer; `browser.entries`, `browser.expanded_dirs`, `browser.dir_contents`.
- State written: `Mut::BufferUpdate` (editor); browser expansion sets `pending_lists`.
- Triggered by: `right` (main mode).
- Dispatched: browser expand emits an FS list-dir request via `pending_lists` -> `FsOut::ListDir` in derived.
- Source: `led/src/model/movement_of.rs:164-189`; `led/src/model/action/browser.rs:61-77`.
- Default scenario hint: buffer with cursor not at EOL.

### Action::LineStart
- Purpose: Move cursor to start of line.
- State read: `dims`, active buffer; `focus == Main`.
- State written: `Mut::BufferUpdate`.
- Triggered by: `ctrl+a`, `home` (main). In find-file modal, re-bound to move the input cursor to 0 (find_file.rs:423).
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:16-41`.
- Default scenario hint: buffer with cursor at col > 0.

### Action::LineEnd
- Purpose: Move cursor to end of line.
- State read: same as LineStart.
- State written: `Mut::BufferUpdate`.
- Triggered by: `ctrl+e`, `end` (main). In find-file modal moves input cursor to end.
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:43-68`.
- Default scenario hint: buffer line with length > 0, cursor not already at end.

### Action::PageUp
- Purpose: Scroll a page up in buffer, or page in browser.
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate` (editor); browser via `Mut::Action`.
- Triggered by: `pageup`, `alt+v` (main).
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:191-211`; `led/src/model/action/browser.rs:22-24`.
- Default scenario hint: buffer with lines > page height; scroll > 0.

### Action::PageDown
- Purpose: Scroll a page down; also bound to `ctrl+v`.
- State read: same as PageUp.
- State written: `Mut::BufferUpdate`.
- Triggered by: `pagedown`, `ctrl+v` (main).
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:213-233`; `led/src/model/action/browser.rs:25-27`.
- Default scenario hint: buffer with lines > page height, cursor above last page.

### Action::FileStart
- Purpose: Move to row 0, col 0.
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate`.
- Triggered by: `ctrl+home`, `alt+<` (main).
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:235-259`; `led/src/model/action/browser.rs:28-30`.
- Default scenario hint: buffer with cursor not on row 0.

### Action::FileEnd
- Purpose: Move to last row / last column.
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate`.
- Triggered by: `ctrl+end`, `alt+>` (main).
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:261-285`; `led/src/model/action/browser.rs:31-33`.
- Default scenario hint: multi-line buffer with cursor above last row.

### Action::MatchBracket
- Purpose: Jump to the matching bracket if one is highlighted.
- State read: active buffer’s `matching_bracket()`.
- State written: `Mut::BufferUpdate`.
- Triggered by: `alt+]` (main).
- Dispatched: none.
- Source: `led/src/model/movement_of.rs:70-91`.
- Default scenario hint: cursor on a bracket character inside matched pair (syntax must have set bracket pairs; e.g. Rust source).

---

## Insert / Delete

### Action::InsertChar(char)
- Purpose: Insert a single character at cursor; also drives modal input (isearch query, rename, find-file, file-search, completion filter, confirm-kill “y”).
- State read: `dims`, `active_tab`, `buffers[active]`, `lsp.completion` (triggers + gating), `confirm_kill`, `file_search`, `find_file`, `isearch`, kbd_macro.recording.
- State written: `Mut::BufferUpdate` (+ `Mut::LspRequestPending(Complete)` when identifier char triggers completion); inside modals, uses `Mut::LspCompletionAction / LspRenameAction / FileSearchAction / FindFileAction / Mut::Action` as the router; `Mut::ForceKillBuffer` for 'y'/'Y' when `confirm_kill`.
- Triggered by: fallback when a `KeyCode::Char` is unbound AND focus allows char insert; see `actions_of.rs:127-131`. Fanout target for arbitrary typed characters.
- Dispatched: auto-complete may cause `LspOut::Complete` via `LspRequest::Complete` on pending.
- Source: `led/src/model/editing_of.rs:20-67` (editor); `led/src/model/action/mod.rs:46-56,140-175` (confirm-kill + action); `led/src/model/isearch_of.rs:60-71` (isearch); `led/src/model/action/lsp.rs:171-178` (rename); `led/src/model/find_file.rs` (Open/SaveAs); `led/src/model/file_search.rs`.
- Default scenario hint: active materialized buffer, focus Main, no overlay.

### Action::InsertNewline
- Purpose: Insert newline (with indent request) in editor; submit in rename / find-file / file-search / completion (accept item); accept isearch.
- State read: `dims`, active buffer, various modal fields.
- State written: `Mut::BufferUpdate`; in modals: `Mut::LspCompletionAction` → accept item, `Mut::LspRenameAction` → rename (then `LspRequest::Rename`), `Mut::FindFileAction`, `Mut::FileSearchAction` (open selected); isearch accept emits `Mut::BufferUpdate` + possibly `Mut::JumpRecord`.
- Triggered by: `enter` (main), `enter` (browser context → `open_selected`), `enter` (file_search context → `open_selected`).
- Dispatched: find-file SaveAs path emits `Mut::SaveRequest`; rename accept triggers LSP rename.
- Source: `led/src/model/editing_of.rs:77-103`; `led/src/model/action/lsp.rs:30-113,144-158,194-211`; `led/src/model/find_file.rs` (enter handling); `led/src/model/file_search.rs:411-444`; `led/src/model/isearch_of.rs:114-141`.
- Default scenario hint: editor — buffer with text, cursor inside line.

### Action::DeleteBackward
- Purpose: Delete character before cursor in editor; pop char from modals (rename, find-file, file-search, isearch query).
- State read: `dims`, active buffer, modal query fields.
- State written: `Mut::BufferUpdate`; modal variants via their routing Muts.
- Triggered by: `backspace` (main).
- Dispatched: none directly.
- Source: `led/src/model/editing_of.rs:129-166`; modals per their files.
- Default scenario hint: buffer with cursor at col > 0.

### Action::DeleteForward
- Purpose: Delete character at cursor.
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate`.
- Triggered by: `ctrl+d`, `delete` (main).
- Dispatched: none.
- Source: `led/src/model/editing_of.rs:168-205`.
- Default scenario hint: buffer with cursor not at EOL.

### Action::InsertTab
- Purpose: Request an indent at cursor row; in completion popup accept current item; in file-search switch between search/replace input when replace mode is on.
- State read: `dims`, active buffer; `lsp.completion`; `file_search` (selection, replace_mode).
- State written: `Mut::BufferUpdate` (queues indent via `buf.request_indent`); `Mut::LspCompletionAction` accept path; `Mut::FileSearchAction`.
- Triggered by: `tab` (main).
- Dispatched: indent request resolves via Mut::ApplyIndent on a later cycle (driven from `syntax_in`).
- Source: `led/src/model/editing_of.rs:105-127`; `led/src/model/action/lsp.rs:30-113`; `led/src/model/file_search.rs:397-408`.
- Default scenario hint: active buffer, focus Main. A language-aware syntax state (Rust/TS/etc.) gives observable indentation.

### Action::KillLine
- Purpose: Kill from cursor to end of line into the kill ring (accumulating on repeat).
- State read: `dims`, active buffer, `kill_ring`.
- State written: `Mut::BufferUpdate` + `Mut::KillRingAccumulate`. Also: every non-KillLine action emits `Mut::BreakKillAccumulation` via a guard stream.
- Triggered by: `ctrl+k` (main). Also re-bound inside find-file modal to truncate the input.
- Dispatched: none.
- Source: `led/src/model/kill_of.rs:13-52`; `led/src/model/find_file.rs:435-440` (modal).
- Default scenario hint: buffer with text past the cursor on current line.

---

## File / Save

### Action::Save
- Purpose: Save active buffer; when LSP is attached, requests formatting first.
- State read: `active_tab`, active buffer, `lsp.server_name` (via `has_active_lsp`).
- State written: with LSP → `Mut::BufferUpdate` (begin_save+touch) + `Mut::SetPendingSaveAfterFormat` + `Mut::LspRequestPending(Format)` + `Mut::Alert("Formatting...")`. Without LSP → `Mut::BufferUpdate` (cleanup applied) + `Mut::SaveRequest`.
- Triggered by: `ctrl+x ctrl+s` (chord).
- Dispatched: `SaveRequest` → `WorkspaceOut::FileSave` via derived. LSP path triggers `LspOut::Format` then once `Edits{all empty}` arrive with `pending_save_after_format=true`, `Mut::LspFormatDone` + save cleanup follow in `lsp_of.rs`.
- Source: `led/src/model/save_of.rs:11-44`.
- Default scenario hint: dirty buffer with a path.

### Action::SaveAs
- Purpose: Open the find-file overlay in SaveAs mode.
- State read: active buffer (for initial prefix), start dir.
- State written: `Mut::SetFindFile(ff)` + `Mut::SetPendingFindFileList(dir, prefix, show_hidden)`.
- Triggered by: `ctrl+x ctrl+w` (chord).
- Dispatched: `pending_find_file_list` in derived → `FsOut::FindFileList`. After the user selects a path and hits Enter, `Mut::SaveRequest` is emitted; docstore then issues `DocStoreIn::SavedAs`.
- Source: `led/src/model/find_file_of.rs:37-55`; finalization in `find_file.rs` (enter handler).
- Default scenario hint: active buffer open.

### Action::SaveForce
- `[dead?]` No handler. Listed in `is_migrated` (`mod.rs:1254`) but no combinator filters for it and `handle_action` does not match it (falls through to `_ => {}`). Not bound in `default_keys.toml`.
- Source: declaration only, `crates/core/src/lib.rs:247`.

### Action::SaveNoFormat
- Purpose: Save without requesting LSP format.
- State read: active buffer.
- State written: `Mut::BufferUpdate` (begin_save + diag save point) + `Mut::SaveRequest`.
- Triggered by: `ctrl+x ctrl+d` (chord).
- Dispatched: `WorkspaceOut::FileSave` via `save_request` in derived.
- Source: `led/src/model/save_of.rs:66-86`.
- Default scenario hint: dirty buffer, LSP formatting would otherwise alter it (not strictly required, but meaningful for golden coverage).

### Action::SaveAll
- Purpose: Save every dirty buffer.
- State read: all buffers.
- State written: a `Mut::BufferUpdate(path, buf)` per dirty buffer + one `Mut::SaveAllRequest`.
- Triggered by: `ctrl+x ctrl+a` (chord).
- Dispatched: `save_all_request` → `WorkspaceOut::FileSaveAll`.
- Source: `led/src/model/save_of.rs:88-116`.
- Default scenario hint: ≥2 dirty buffers.

### Action::KillBuffer
- Purpose: Close the active tab; if dirty, prompt for confirmation; if it’s the preview tab, close preview instead.
- State read: `active_tab`, buffers, tabs (preview?), `dirty`.
- State written: goes through `Mut::Action` → `handle_action` → `tabs::kill_buffer` → imperatively edits `state.tabs`, `state.active_tab`, `state.confirm_kill`, `state.alerts.info`, `state.focus`. When dirty prompt is answered with 'y'/'Y' the pre-match stream emits `Mut::ForceKillBuffer` which triggers `action::force_kill_buffer`.
- Triggered by: `ctrl+x k` (chord).
- Dispatched: none.
- Source: `led/src/model/action/tabs.rs:8-79`.
- Default scenario hint: ≥2 tabs, active buffer clean (for direct kill) OR dirty (for confirm flow).

---

## Navigation (tabs, jumps)

### Action::PrevTab
- Purpose: Cycle active tab to previous non-preview, materialized tab.
- State read: `tabs`, `buffers`, `active_tab`.
- State written: `Mut::ActivateBuffer(path)`.
- Triggered by: `ctrl+left` (main).
- Dispatched: none.
- Source: `led/src/model/ui_actions_of.rs:81-87`, helper `compute_cycle_tab` at `mod.rs:1210-1227`.
- Default scenario hint: ≥2 materialized non-preview tabs.

### Action::NextTab
- Purpose: Cycle active tab to next.
- State read: same as PrevTab.
- State written: `Mut::ActivateBuffer`.
- Triggered by: `ctrl+right` (main).
- Dispatched: none.
- Source: `led/src/model/ui_actions_of.rs:73-79`.
- Default scenario hint: ≥2 tabs.

### Action::JumpBack
- Purpose: Move back in the jump list.
- State read: `jump.index`, `jump.entries`, buffers.
- State written: fan-out: `Mut::JumpRecord` (if at head), `Mut::SetJumpIndex(index-1)`, then either `Mut::BufferUpdate` + `Mut::ActivateBuffer` (existing buffer) or `Mut::RequestOpen` + `Mut::ActivateBuffer` + `Mut::SetTabPendingCursor` (not yet open).
- Triggered by: `alt+b`, `alt+left` (main).
- Dispatched: `RequestOpen` → docstore-driven materialization in derived.
- Source: `led/src/model/jump_of.rs:11-93`.
- Default scenario hint: a jump list with index > 0 (e.g. after an LspGotoDefinition).

### Action::JumpForward
- Purpose: Move forward in the jump list.
- State read / written: mirror of JumpBack but using `index+1`.
- Triggered by: `alt+f`, `alt+right` (main).
- Dispatched: possibly `RequestOpen`.
- Source: `led/src/model/jump_of.rs:95-160`.
- Default scenario hint: jump list with index + 1 < entries.len().

### Action::Outline
- `[dead?]` Declared at `crates/core/src/lib.rs:257` and bound to `alt+o` in `default_keys.toml`. No match arm, no filter, no combinator — it falls through `handle_action`'s `_ => {}` catch-all. Syntax layer exposes `OutlineItem` helpers but nothing in `led/src/` wires them.
- Source: declaration only.

---

## Search (in-buffer)

### Action::InBufferSearch
- Purpose: Start isearch; if already in isearch, advance to next hit.
- State read: `active_tab`, active buffer, `isearch`.
- State written: routed via `Mut::Action` → `handle_action` calls `search::start_search(buf)` or `search::search_next(buf)` directly on AppState (imperative).
- Triggered by: `ctrl+s` (main). Explicitly carved out of `isearch_of.rs` accept/ignore logic (mod.rs line 153 / isearch_of.rs line 31).
- Dispatched: none (pure in-buffer).
- Source: `led/src/model/action/mod.rs:90-103,262-264`; `led/src/model/search.rs` for the search logic.
- Default scenario hint: buffer with known match substring.

---

## Search (file search overlay)

### Action::OpenFileSearch
- Purpose: Open the project-wide file search overlay; seed from selection if any; focus Side panel.
- State read: `active_tab`, buffers, selection text, `show_side_panel`.
- State written: `Mut::SetFileSearch(fs)` + `Mut::SetShowSidePanel(true)` + `Mut::SetFocus(Side)`; if text was selected, also `Mut::BufferUpdate` (clear_mark) + `Mut::TriggerFileSearch`.
- Triggered by: `ctrl+f` (main).
- Dispatched: `TriggerFileSearch` causes `FileSearchOut::Search` in derived.
- Source: `led/src/model/find_file_of.rs:57-107`.
- Default scenario hint: workspace loaded; optionally a buffer with a selection.

### Action::CloseFileSearch
- Purpose: Close the file-search overlay. (Same as Abort within file_search context.)
- State read: `file_search.is_some()`.
- State written: `Mut::FileSearchAction(CloseFileSearch)` → calls `deactivate(state)` which clears `state.file_search` and unfocuses.
- Triggered by: Not bound directly in `default_keys.toml` (Abort / `esc` serves); reachable via macro or explicit Action injection.
- Dispatched: none.
- Source: `led/src/model/file_search.rs:447-450`.
- Default scenario hint: file_search open.

### Action::ToggleSearchCase
- Purpose: Toggle case sensitivity of file-search query; re-triggers search.
- State read: `file_search`.
- State written: `Mut::FileSearchAction(ToggleSearchCase)` → mutates `fs.case_sensitive` imperatively, sets `pending_file_search_trigger`.
- Triggered by: `alt+1` (file_search context).
- Dispatched: `FileSearchOut::Search`.
- Source: `led/src/model/file_search.rs:362-367`.
- Default scenario hint: file_search overlay open with non-empty query.

### Action::ToggleSearchRegex
- Purpose: Toggle regex mode of file search.
- State read: `file_search`.
- State written: like ToggleSearchCase but `fs.use_regex`; triggers search.
- Triggered by: `alt+2` (file_search context).
- Dispatched: `FileSearchOut::Search`.
- Source: `led/src/model/file_search.rs:368-373`.
- Default scenario hint: file_search open.

### Action::ToggleSearchReplace
- Purpose: Toggle replace input visibility/mode.
- State read: `file_search` (selection, replace_stack).
- State written: `Mut::FileSearchAction(ToggleSearchReplace)` flips `fs.replace_mode` and resets selection.
- Triggered by: `alt+3` (file_search context).
- Dispatched: none.
- Source: `led/src/model/file_search.rs:374-384`.
- Default scenario hint: file_search open.

### Action::ReplaceAll
- Purpose: Execute bulk replace for current query; deactivates the overlay.
- State read: `file_search`.
- State written: routes through `Mut::FileSearchAction(ReplaceAll)` → `replace_all(state)` (emits a pending driver command) + `deactivate(state)`.
- Triggered by: `alt+enter` (file_search context).
- Dispatched: `FileSearchOut::ReplaceAll`.
- Source: `led/src/model/file_search.rs:387-394`.
- Default scenario hint: file_search open in replace_mode with results.

---

## Find (find-file overlay)

### Action::FindFile
- Purpose: Open the find-file overlay (for opening a path by typing its name).
- State read: active buffer’s dir, start dir, selection.
- State written: `Mut::SetFindFile(ff)` + `Mut::SetPendingFindFileList(dir, prefix, show_hidden)`.
- Triggered by: `ctrl+x ctrl+f` (chord).
- Dispatched: `pending_find_file_list` → `FsOut::FindFileList`.
- Source: `led/src/model/find_file_of.rs:13-35`.
- Default scenario hint: workspace loaded.

---

## Edit (undo/redo, marks, yank, sort, reflow)

### Action::Undo
- Purpose: Undo last edit group on active buffer; restore cursor.
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate`.
- Triggered by: `ctrl+/`, `ctrl+_`, `ctrl+7` (main).
- Dispatched: none.
- Source: `led/src/model/editing_of.rs:207-234`.
- Default scenario hint: buffer with prior undoable edit.

### Action::Redo
- Purpose: Redo.
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate`.
- Triggered by: not bound in default_keys.toml (emacs-tradition redo sits on undo repeat). Reachable via macro / programmatic Action injection.
- Dispatched: none.
- Source: `led/src/model/editing_of.rs:236-263`.
- Default scenario hint: buffer after Undo.

### Action::SetMark
- Purpose: Set selection mark at cursor (start of region).
- State read: `dims`, active buffer.
- State written: `Mut::BufferUpdate` + `Mut::Alert { info: "Mark set" }`.
- Triggered by: `ctrl+space` (main).
- Dispatched: none.
- Source: `led/src/model/editing_of.rs:266-291`.
- Default scenario hint: buffer open; no prior mark needed.

### Action::KillRegion
- Purpose: Kill text between mark and cursor into kill ring; or alert "No region".
- State read: `dims`, active buffer’s mark.
- State written: `Mut::BufferUpdate` + `Mut::KillRingSet(text)`; or `Mut::Alert { info: "No region" }` if no mark.
- Triggered by: `ctrl+w` (main).
- Dispatched: none.
- Source: `led/src/model/kill_of.rs:54-107`.
- Default scenario hint: buffer with a mark set and cursor moved.

### Action::Yank
- Purpose: Paste from system clipboard (fallback to kill ring if clipboard empty).
- State read: kill ring (fallback), `dims`, active buffer.
- State written: `Mut::PendingYank`. When clipboard driver reports `ClipboardIn::Text`, a chain in `mod.rs:447-477` emits `Mut::BufferUpdate`.
- Triggered by: `ctrl+y` (main).
- Dispatched: `ClipboardOut::Read`.
- Source: `led/src/model/ui_actions_of.rs:59-63`; clipboard chain `mod.rs:447-477`.
- Default scenario hint: clipboard or kill ring has content; active buffer exists.

### Action::SortImports
- Purpose: Sort import block at top of buffer (via tree-sitter syntax helpers).
- State read: `active_tab`, buffer, syntax chain.
- State written: either `Mut::BufferUpdate` + `Mut::Alert("Imports sorted")` or just `Mut::Alert("Imports already sorted")`.
- Triggered by: `ctrl+x i` (chord).
- Dispatched: none.
- Source: `led/src/model/editing_of.rs:293-354`.
- Default scenario hint: buffer with out-of-order imports in a language whose syntax config recognizes them (Rust, TS, Python, etc.).

### Action::ReflowParagraph
- Purpose: dprint-based reflow of markdown paragraph / doc-comment block at cursor.
- State read: `active_tab`, buffer (doc + path).
- State written: `Mut::BufferUpdate` or `Mut::Alert("Nothing to reflow")`.
- Triggered by: `ctrl+q` (main). Also bound to `ctrl+q` in `[browser]` for `collapse_all` — different context.
- Dispatched: none (reflow is synchronous via the bundled dprint).
- Source: `led/src/model/reflow_of.rs:12-47`.
- Default scenario hint: buffer containing a long markdown paragraph (or a Rust `///` comment block) wider than target column.

---

## LSP

### Action::LspGotoDefinition
- Purpose: Request go-to-definition at cursor.
- State read: active buffer (implicitly, when the request later resolves via `LspIn::Navigate`).
- State written: `Mut::LspRequestPending(GotoDefinition)`. The `lsp_of.rs` driver-input chain later emits a cascade of `Mut::JumpRecord`, `Mut::BufferUpdate` (existing) OR `Mut::RequestOpen` + `Mut::SetTabPendingCursor`, + `Mut::ActivateBuffer`.
- Triggered by: `alt+enter` (main).
- Dispatched: `LspOut::GotoDefinition`.
- Source: `led/src/model/ui_actions_of.rs:43-47`; nav result handling `led/src/model/lsp_of.rs:13-71`.
- Default scenario hint: test-LSP configured, symbol under cursor resolves.

### Action::LspRename
- Purpose: Open rename overlay seeded with word under cursor.
- State read: active buffer; word at cursor.
- State written: `Mut::SetLspRename(RenameState)` + `Mut::SetFocus(Overlay)`.
- Triggered by: `ctrl+r` (main).
- Dispatched: submission → `LspRequest::Rename { new_name }` → `LspOut::Rename`.
- Source: `led/src/model/find_file_of.rs:109-138`.
- Default scenario hint: test-LSP configured with a renameable symbol at cursor.

### Action::LspCodeAction
- Purpose: Request code actions at cursor.
- State read: active buffer.
- State written: `Mut::LspRequestPending(CodeAction)`. Driver reply routes through `Mut::LspCodeActions { actions }` and opens a picker.
- Triggered by: `alt+i` (main).
- Dispatched: `LspOut::CodeAction`.
- Source: `led/src/model/ui_actions_of.rs:53-56`.
- Default scenario hint: test-LSP configured; position with code actions available.

### Action::LspFormat
- Purpose: Request LSP document formatting (without saving).
- State read: none before dispatch.
- State written: `Mut::LspRequestPending(Format)`. Driver reply → `Mut::LspEdits { edits }`.
- Triggered by: Not bound by default. Reachable programmatically.
- Dispatched: `LspOut::Format`.
- Source: `led/src/model/ui_actions_of.rs:48-52`.
- Default scenario hint: test-LSP configured.

### Action::NextIssue
- Purpose: Cycle to next diagnostic/git-hunk/PR issue.
- State read: `buffers[*].status().diagnostics`, `git.file_statuses`, per-buffer git line statuses, `git.pr`, active tab, cursor.
- State written: `Mut::Alert` + (same-buffer) `Mut::BufferUpdate` OR (other buffer open) `Mut::BufferUpdate` + `Mut::SetActiveTab` OR (new file) `Mut::RequestOpen` + `Mut::SetActiveTab` + `Mut::SetTabPendingCursor`.
- Triggered by: `alt+.` (main).
- Dispatched: potential open via docstore.
- Source: `led/src/model/nav_of.rs:37-128`.
- Default scenario hint: buffer with ≥1 LSP diagnostic OR ≥1 git hunk OR PR line comment.

### Action::PrevIssue
- Purpose: Cycle backward.
- State read / written: mirror of NextIssue.
- Triggered by: `alt+,` (main).
- Source: `led/src/model/nav_of.rs:37-128` (shared).
- Default scenario hint: same as NextIssue.

### Action::LspToggleInlayHints
- Purpose: Toggle inlay hints for the active buffer / session.
- State read: `lsp.inlay_hints_enabled`.
- State written: `Mut::ToggleInlayHints(!current)`.
- Triggered by: `ctrl+t` (main).
- Dispatched: subsequent pulls of `LspOut::InlayHints` are gated by the flag (in derived).
- Source: `led/src/model/ui_actions_of.rs:90-95`.
- Default scenario hint: test-LSP active.

---

## UI

### Action::ToggleFocus
- Purpose: Toggle focus between Main and Side panels.
- State read: `focus`.
- State written: `Mut::SetFocus(next)`.
- Triggered by: `alt+tab` (main).
- Dispatched: none.
- Source: `led/src/model/ui_actions_of.rs:19-30`.
- Default scenario hint: workspace loaded, side panel visible.

### Action::ToggleSidePanel
- Purpose: Show/hide the side panel.
- State read: `show_side_panel`.
- State written: `Mut::SetShowSidePanel(!current)`.
- Triggered by: `ctrl+b` (main).
- Dispatched: none.
- Source: `led/src/model/ui_actions_of.rs:12-17`.
- Default scenario hint: any state.

### Action::ExpandDir
- Purpose: Expand the directory at browser selection.
- State read: `browser.entries[selected]`, `browser.dir_contents`, `browser.expanded_dirs`.
- State written: routed via `Mut::Action` → `handle_browser_expand` → mutates `browser.expanded_dirs` + sets `pending_lists`.
- Triggered by: `right` in `[browser]` context.
- Dispatched: `FsOut::ListDir`.
- Source: `led/src/model/action/browser.rs:61-77`.
- Default scenario hint: browser focused; selection on a collapsed directory entry.

### Action::CollapseDir
- Purpose: Collapse directory at selection (or parent).
- State read: `browser.entries[selected]`, `browser.expanded_dirs`.
- State written: `Mut::Action` → `handle_browser_collapse` imperative.
- Triggered by: `left` in `[browser]`.
- Dispatched: none.
- Source: `led/src/model/action/browser.rs:79-96`.
- Default scenario hint: browser selection on an expanded directory.

### Action::CollapseAll
- Purpose: Collapse all expanded directories; reset selection/scroll.
- State written: `Mut::Action` → `handle_browser_collapse_all`.
- Triggered by: `ctrl+q` in `[browser]`.
- Source: `led/src/model/action/browser.rs:98-104`.
- Default scenario hint: browser focused; ≥1 expanded directory.

### Action::OpenSelected
- Purpose: Open file under selection (editor), or enter-key on a file-search result opens the selected hit.
- State read: `browser.entries`, preview tabs, `file_search` (when in file_search context).
- State written: for browser: `Mut::Action` → `handle_browser_open` → promote preview or `request_open` + set `active_tab` + `focus = Main`. For file_search: `Mut::FileSearchAction(OpenSelected)` advances selection or confirms + closes.
- Triggered by: `enter` in `[browser]`; `enter` in `[file_search]`.
- Dispatched: can trigger docstore open.
- Source: `led/src/model/action/browser.rs:106-135`; `led/src/model/file_search.rs:411-444`.
- Default scenario hint: browser with a file entry selected.

### Action::OpenSelectedBg
- `[dead?]` Declared at `crates/core/src/lib.rs:297` and bound to `alt+enter` in `[browser]`. No handler/match. Presumed intent: open a file in a background tab without activating. Falls through `handle_action`’s `_ => {}`.

### Action::OpenMessages
- `[dead?]` Declared at `crates/core/src/lib.rs:298`. Bound to `ctrl+h e` (chord). No handler/match/combinator; dead.

### Action::OpenPrUrl
- Purpose: Open GitHub PR URL, or the URL of a PR-comment on the current line.
- State read: `git.pr`, active buffer’s cursor, comment line.
- State written: `Mut::SetPendingOpenUrl(url)`.
- Triggered by: `ctrl+x ctrl+p` (chord).
- Dispatched: `UiOut::OpenUrl` / system `open` via `pending_open_url` in derived (`derived.rs:892-895`).
- Source: `led/src/model/gh_pr_of.rs:80-114`.
- Default scenario hint: a PR loaded with comments; cursor on a commented line (or not, for fallback to PR url).

### Action::Abort
- Purpose: Context-dependent cancellation: exits isearch, closes completion / code-action picker / rename overlay / file-search / find-file / confirm-kill; clears the mark in the main buffer.
- State read / written: multiple paths — see sources.
- Triggered by: `esc`, `ctrl+g` (main).
- Dispatched: none.
- Source: `led/src/model/action/mod.rs:54-56,129-131`; `led/src/model/action/lsp.rs:114-117,160-164,212-216`; `led/src/model/isearch_of.rs:103-111`; `led/src/model/find_file.rs:442-445`; `led/src/model/file_search.rs:447-450`.
- Default scenario hint: any modal or mark set.

---

## Macros

### Action::KbdMacroStart
- Purpose: Begin recording a keyboard macro; clears current buffer.
- State written (via `Mut::Action` → handle_action): `state.kbd_macro.recording = true`; `state.kbd_macro.current.clear()`; alert "Defining kbd macro...".
- Triggered by: `ctrl+x (` (chord).
- Dispatched: none.
- Source: `led/src/model/action/mod.rs:269-273`. Also special-cased at line 32-35 for “start while already recording”.
- Default scenario hint: any state.

### Action::KbdMacroEnd
- Purpose: End recording, stash macro into `kbd_macro.last`.
- State written: `state.kbd_macro.recording = false`, `state.kbd_macro.last = take(current)`, alert "Keyboard macro defined". When not recording, returns `"Not defining kbd macro"` alert (line 274-276).
- Triggered by: `ctrl+x )` (chord).
- Source: `led/src/model/action/mod.rs:26-31,274-276`.
- Default scenario hint: either recording or not — both paths exist.

### Action::KbdMacroExecute
- Purpose: Replay the last-defined macro, optionally N times (chord digits between `ctrl+x e` re-fires set the count via `Mut::KbdMacroSetCount`). `e` alone after an execute replays again (macro_repeat mode in actions_of).
- State read: `kbd_macro.last`, `kbd_macro.execute_count`, `kbd_macro.playback_depth`.
- State written: alerts (no macro / recursion limit) or imperative replay via recursive `handle_action` calls (`mod.rs:277-305`).
- Triggered by: `ctrl+x e` (chord) + the `e`-repeat mode.
- Dispatched: any actions inside the recorded macro dispatch normally.
- Source: `led/src/model/action/mod.rs:277-305`; `actions_of.rs:95-106`.
- Default scenario hint: a previously defined macro.

---

## Lifecycle / test

### Action::Quit
- Purpose: Begin clean shutdown.
- State written: `Mut::SetPhase(Phase::Exiting)`.
- Triggered by: `ctrl+x ctrl+c` (chord).
- Dispatched: derived responds to Phase transition (workspace teardown, session save, then exit).
- Source: `led/src/model/ui_actions_of.rs:32-35`.
- Default scenario hint: any state.

### Action::Suspend
- Purpose: Unix job-control suspend (SIGTSTP) then resume.
- State written: `Mut::SetPhase(Phase::Suspended)`. `process_of.rs:11-16` inspects the phase and runs `libc::raise(SIGTSTP)` in an `inspect` side-effect, then emits `Mut::Resumed`.
- Triggered by: `ctrl+z` (main).
- Dispatched: terminal mode changes + SIGTSTP (side effect in `process_of::suspend`).
- Source: `led/src/model/ui_actions_of.rs:37-40`; `led/src/model/process_of.rs:11-27,77-97`.
- Default scenario hint: unlikely useful in golden PTY tests (probably skip or treat as a no-op observation).

### Action::Wait(u64)
- Purpose: Test harness sleep in milliseconds.
- State read / written: none. `should_record` (helpers.rs:100-106) explicitly excludes it from macro recording.
- Triggered by: not bound; injected via `drivers.actions_in` (golden runner).
- Dispatched: handled by the test harness driver (not by the model).
- Source: declaration only (`crates/core/src/lib.rs:312`). No match arm in `handle_action` → `_ => {}`.
- Default scenario hint: test-runner specific; scenarios may use it as a delay primitive.

### Action::Resize(u16, u16)
- Purpose: Terminal resize (from terminal driver or test harness).
- State read: none.
- State written: `Mut::Resize(w, h)` — resets `state.dims`.
- Triggered by: terminal event (`TerminalInput::Resize`) or test-injected `Action::Resize(w, h)` via `actions_in`.
- Dispatched: downstream redraw via the UI driver.
- Source: `led/src/model/actions_of.rs:21-27` (terminal); `led/src/model/ui_actions_of.rs:65-70` (action).
- Default scenario hint: scenarios that need to observe reflow at a specific size.

---

## Findings

1. **Dead variants.** Four action variants appear wired (enum + keybinding) but have no handler in the `led` crate:
   - `Action::Outline` — bound to `alt+o`, no combinator, no match. The `syntax::outline` module exists but isn’t consumed.
   - `Action::OpenMessages` — bound to `ctrl+h e`, no handler.
   - `Action::OpenSelectedBg` — bound to `alt+enter` in `[browser]`, no handler.
   - `Action::SaveForce` — listed in `is_migrated` but otherwise unused, and unbound. Generating a “default scenario” for these will produce a no-op snapshot unless a handler is planned.

2. **Redo has no default keybinding.** `Action::Redo` has a full handler in `editing_of.rs` but nothing in `default_keys.toml` binds it. Golden scenarios must either inject it via the test harness `actions_in`, bind it in a keymap override, or use a macro. Worth confirming with the maintainer whether this is intentional (Emacs tradition replays undo) or an omission.

3. **`Action::Suspend` performs a real SIGTSTP.** `process_of.rs` calls `libc::raise(SIGTSTP)` inside `inspect()` — running it in a PTY-based golden will actually suspend the process and require a `SIGCONT`. The goldens-per-variant axis should probably special-case this action.

4. **`Action::CloseFileSearch` is redundant with `Abort`.** Both end up at `deactivate(state)` in `file_search.rs`. No default binding exists — the action exists as a way to programmatically close. Scenario generator should not expect a keystroke.

5. **`Action::InBufferSearch` has dual semantics.** Starts isearch on first press; advances match on subsequent presses. Both are implemented imperatively inside `handle_action` via the `Mut::Action` mega-dispatch (explicitly called out in a comment), unlike the rest of the search modals. Any per-variant scenario should decide which phase it exercises.

6. **Macro repeat mode is chord-sensitive.** After a `KbdMacroExecute`, bare `e` keys replay — this state lives in the `actions_of` closure Cell. A default-state golden will never exercise it because there’s no recorded macro to replay; a meaningful scenario must first record one (`KbdMacroStart`/`KbdMacroEnd`).

7. **Context-swapped actions.** Several actions change meaning per focus/modal:
   - Movement actions (`MoveUp/Down/Left/Right/PageUp/PageDown/FileStart/FileEnd`) split between editor and browser handlers — six of those also re-route to completion picker and code-action picker when those overlays are open. Per-variant scenarios should pick a canonical context.
   - `InsertChar` is especially polymorphic (editor typing; file_search/find-file/isearch/rename query; completion filter; confirm-kill 'y').
   - `LineStart` / `LineEnd` / `KillLine` / `DeleteBackward` / `InsertNewline` re-map inside the find-file modal.

8. **`SaveAs` depends on the find-file modal.** `Action::SaveAs` by itself just opens the overlay with SaveAs mode — it does not save. To exercise the save path requires typing a filename and pressing Enter. The `Default scenario hint` reflects this; the caller may want a second, composite scenario.

9. **`ReflowParagraph` is bound to `ctrl+q` both in main and in `[browser]` (`collapse_all`).** Not a conflict at runtime (context-dependent keymap lookup) but worth noting: the meaning of `ctrl+q` flips on focus.

10. **`KillLine` accumulation is scenario-sensitive.** Multiple consecutive `KillLine`s accumulate into one kill-ring entry; any other action in between resets it (via the `kill_ring_break_s` guard stream at `mod.rs:284-290`). Goldens that chain `KillLine` twice exercise accumulation; a single-press scenario only covers set-fresh.

11. **Issue navigation (`NextIssue`/`PrevIssue`) fans out to ≥3 Muts per invocation.** Depending on whether the target is the same buffer, another open buffer, or a not-yet-open file. Per-variant scenarios should be honest about which branch they hit — probably the same-buffer case by default, since that’s one BufferUpdate + one Alert.

12. **`Mut::Action` is still the mega-dispatcher for a dozen actions.** Per CLAUDE.md Principle 9 this is explicitly called out as bad; the remaining residents include `KillBuffer`, `KbdMacroStart/End/Execute`, `InBufferSearch`, `Abort`, `ExpandDir`, `CollapseDir`, `CollapseAll`, `OpenSelected`, and browser movement overloads. The golden generator doesn’t need to care, but the inventory reflects that these write state via imperative `handle_action` rather than a fine-grained `Mut`.

13. **Fan-in streams that end up writing `Mut::BufferUpdate`.** The same `Mut::BufferUpdate(path, buf)` is emitted from at least: editing_of, movement_of, kill_of, jump_of, nav_of, lsp_of, buffers_of, reflow_of, isearch_of, find_file_of (clear-mark branch), clipboard chain, and save_of. A golden snapshot of `AppState.buffers` won’t distinguish source — the test must rely on alert text / cursor position / undo state.
