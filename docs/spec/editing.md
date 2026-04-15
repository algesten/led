# Editing

## Summary

Editing is the set of text-mutating operations on an active,
materialized buffer: insert char, insert newline with auto-indent,
indent via Tab, delete forward and backward, undo/redo with grouped
edits persisted across sessions, mark + region, kill line / kill
region into the kill ring, yank from the system clipboard with
kill-ring fallback, sort imports, and reflow paragraph. Every
mutation produces a `Mut::BufferUpdate(path, buf)` that replaces the
buffer's `Rc` in the reducer, so edits are copy-on-write. Almost every
edit also resets the cursor's column affinity, re-runs `adjust_scroll`
so the cursor stays visible, and calls `update_matching_bracket` so
the renderer's bracket highlight follows the cursor. Undo groups are
closed implicitly on edit-kind transitions (typing after deleting,
whitespace after a word boundary) and explicitly on move-like
operations via `close_group_on_move`. Editing actions are gated by
`has_blocking_overlay`, `has_any_input_modal`, `is_indent_in_flight`,
and `confirm_kill` — so the find-file dialog, LSP completion popup,
rename overlay, and confirm-kill prompt receive keystrokes instead.
See [`buffers.md`](buffers.md) for how dirty state is observed and
persisted once content is edited.

## Behavior

### Insert char

`Action::InsertChar(ch)` (any unbound `KeyCode::Char`) flows through
`editing_of.rs`. The parent stream filters out blocked states; a
small chain decides whether the undo group needs to close before this
character is added: if the last edit was not an `Insert`, or if the
new character is whitespace following a word boundary, the group
closes. The handler clones the active buffer, clears any mark
(because typing cancels a region), inserts the char via
`edit::insert_char`, advances the cursor, resets column affinity,
optionally queues a reindent request when the character is in the
buffer's `reindent_chars` set (language-specific — e.g. `}` in Rust),
adjusts scroll, touches the buffer, and updates the matching-bracket
highlight. A parallel child stream watches alphanumerics and
underscores and — when no completion popup is open and the language
has completion triggers — emits `Mut::LspRequestPending(Complete)` so
completions are pulled on a debounced cycle.

The same `InsertChar` action also routes every modal input: when
`confirm_kill` is on, 'y'/'Y' becomes `Mut::ForceKillBuffer`; when a
modal is open, the char feeds its query. `editing_of.rs` filters
itself out via `has_any_input_modal`, so those paths live in the
corresponding `_of.rs` files.

### Delete backward / forward

`Action::DeleteBackward` (Backspace) and `Action::DeleteForward`
(Ctrl-d, Delete) share a shape: filter blocked states, close the
undo group if the last edit was not a Delete, clear any mark, call
`edit::delete_backward` or `edit::delete_forward`, update cursor and
affinity, adjust scroll, update matching bracket, touch. Deleting
past BOF / EOF is a no-op (the edit helpers return `None`). Word and
line deletion are not distinct actions in the current enum. [unclear
— `Action::DeleteWordBackward` and similar don't appear; confirm
absence.]

### Newline with auto-indent

`Action::InsertNewline` (Enter, also the submit for every modal)
calls `edit::insert_newline`, closes the undo group before and after,
and issues `request_indent(row, false)`. The `pending_indent_row`
flag gates further edits via `is_indent_in_flight`: the syntax driver
sees the buffer change, computes tree-sitter indent for the row, and
replies via `SyntaxIn`. The model then emits `Mut::ApplyIndent` when
the version matches, or `Mut::SetReindentChars` when version mismatch
(the newline was superseded). Buffers without a recognized language
never receive a `SyntaxIn` reply; the pending flag is cleared on the
next edit. [unclear — verify exact clear rule; risk of stuck
indent-in-flight on unknown-language files.]

### Tab / indent

`Action::InsertTab` does not literally insert a tab; it calls
`request_indent(cursor_row, tab_fallback=true)`. The syntax driver
computes the target indent (tree-sitter queries plus the `tab_stop`
from `Dimensions`) and replies. If the language has no indent
support, the fallback flag allows a plain tab-stop insertion of
spaces (or tabs per config); see edge golden `mixed_tabs_spaces`.
Inside the LSP completion popup, Tab accepts the selected item;
inside file-search with replace_mode, Tab toggles between search and
replace inputs.

### Undo / redo

`Action::Undo` (Ctrl-/, Ctrl-_, Ctrl-7) calls `buf.undo()` on a
clone; if it returns a char offset the cursor moves there. `Redo`
is the mirror. Undo groups are the unit of coalescing: every insert,
delete, kill, yank, or format edit appends to an open group;
`close_group_on_move` and `close_undo_group` end the group so the
next edit starts a new one. Typing a word is one group; a space
closes it; Backspace afterwards opens a Delete group; arrow keys
close on move. Redo is not bound in `default_keys.toml` (emacs
tradition replays Undo), but the action exists for macros.

Undo persists across sessions: every 500 ms of edit quiescence the
`undo_flush` timer fires and derived dispatches
`WorkspaceOut::FlushUndo` which serializes new entries to sqlite. On
startup, if the file's content hash matches the stored hash, the
persisted history is re-attached. Cross-instance sync uses the same
mechanism via `NotifyEvent` + `CheckSync`.

### Mark and region

`Action::SetMark` (Ctrl-space) calls `buf.set_mark()` which records
the current cursor as the region anchor, then emits a "Mark set"
alert. Subsequent cursor moves do not clear the mark — only
`clear_mark` or an edit does. The mark is consumed by
`Action::KillRegion` and by the `OpenFileSearch` selection-seed
path. `Action::Abort` (Esc / Ctrl-g) clears the mark when no modal
is open.

### Kill line

`Action::KillLine` (Ctrl-k) kills from the cursor to end-of-line (or
the newline itself if the line is empty) into the kill ring.
Consecutive `KillLine`s accumulate into a single kill-ring entry via
`KillRingState::accumulate`. The guard stream `kill_ring_break_s`
fires `Mut::BreakKillAccumulation` on every other migrated action,
so a single intervening keystroke resets the accumulator.

### Kill region

`Action::KillRegion` (Ctrl-w) kills the text between mark and cursor
into the kill ring via `KillRingState::set` (not accumulate — a
kill region always replaces). If the mark is unset the handler emits
a "No region" alert and does nothing else. Endpoints are normalized,
so selection direction doesn't matter. Mark is cleared after the
kill.

### Yank

`Action::Yank` (Ctrl-y) dispatches `Mut::PendingYank`, which derived
turns into `ClipboardOut::Read`. The clipboard driver replies with
`ClipboardIn::Text(text)`; the model's clipboard chain falls back to
`kill_ring.content` when the system clipboard is empty. The yank
inserts at the cursor: `close_group_on_move`, `clear_mark`,
`edit::yank`, cursor advances to the end of inserted text, undo
group closes. Because the yank is asynchronous (routed through a
driver), the chain samples state at reply-time — so a tab switch
between Ctrl-y and the reply yanks into whatever is active when the
clipboard responds. [unclear — is there a cancellation for a
superseded request?]

### Bracket matching

`BufferState.update_matching_bracket()` runs after every edit and
cursor move. It consults `bracket_pairs` (set by the syntax driver)
and stores `matching_bracket: Option<(Row, Col)>` for the renderer.
`Action::MatchBracket` (Alt-]) jumps the cursor to
`matching_bracket` if one is set; see `navigation.md` for the
movement-side story.

### Paragraph reflow

`Action::ReflowParagraph` (Ctrl-q) runs the bundled dprint engine on
the paragraph (or doc-comment block like Rust `///`) at the cursor.
`reflow_of.rs` calls `reflow::reflow_buffer(buf, file_path)` which
returns `Some(new_buf)` when reflow would change the text and `None`
otherwise. The success branch emits `Mut::BufferUpdate`; the `None`
branch emits `Alert("Nothing to reflow")`. Reflow is synchronous —
dprint is bundled in the binary. In the sidebar (browser) context,
Ctrl-q is rebound to `CollapseAll`; the same chord means different
things by focus.

### Sort imports

`Action::SortImports` (Ctrl-x i) uses tree-sitter: `SyntaxState`
extracts the import block, `import::sort_imports_text` produces a
replacement. `sort_imports_buf_s` builds a `BufferUpdate` with the
replacement applied via `buf.edit_at`; a parallel
`sort_imports_alert_s` emits "Imports sorted" or "Imports already
sorted". The language must have a syntax config that identifies
imports — Rust, TypeScript, JavaScript, Python, Go, and a few
others are supported; other buffers are no-ops.

## User flow

The user opens `main.rs`, places the cursor mid-function, and types
`let x = 3;`. Each keystroke is an `InsertChar`: the buffer clones,
the char inserts, cursor and scroll update, matching bracket
refreshes. The undo group coalesces the word; the space closes it;
so Undo unwinds `3;` as one unit and `let x =` as the previous. The
user hits Enter — `InsertNewline` fires, the group closes, the
syntax driver auto-indents the new line. Ctrl-space sets the mark;
the user moves down three lines and hits Ctrl-w to kill the region
into the kill ring. They navigate elsewhere and press Ctrl-y; led
reads the (empty, in headless) clipboard, falls back to the kill
ring, and yanks at the new cursor. Repeating Ctrl-k four times in a
row accumulates all four line-kills into one ring entry — but any
intervening key resets the accumulator. Ctrl-q on a long markdown
paragraph rewraps it; Ctrl-/ a few times walks back through
history.

## State touched

- `state.buffers[path]` — replaced wholesale via
  `Mut::BufferUpdate(path, buf)` on every edit; COW via
  `Rc::make_mut`.
- `BufferState.version`, `.saved_version`, `.last_edit_kind` —
  bumped per edit; saved_version only changes on save/reload.
- `BufferState.cursor_row/_col/_affinity`,
  `.scroll_row/_sub_line` — interior-mutable Cells; `set_cursor` /
  `set_scroll` don't require `&mut`.
- `BufferState.mark: Option<(Row, Col)>` — set by `SetMark`,
  cleared by edit and by `Abort` (when no modal is open).
- `BufferState.undo: UndoHistory` — append-only with grouping.
- `BufferState.pending_indent_row`, `.pending_tab_fallback` —
  indent-request in-flight.
- `BufferState.matching_bracket` — recomputed after every cursor
  move or edit.
- `state.kill_ring.content`, `.accumulator`, `.pending_yank` —
  kill ring state and versioned yank request.
- `state.lsp.pending_request` —
  `Some(LspRequest::Complete)` when an insert triggers completion.
- `state.alerts.info` — "Mark set", "No region", "Imports
  sorted", "Imports already sorted", "Nothing to reflow".
- `state.confirm_kill` — gates whether 'y' is a kill-confirm or
  normal insert.

## Extract index

- Actions: `InsertChar`, `InsertNewline`, `InsertTab`,
  `DeleteBackward`, `DeleteForward`, `Undo`, `Redo`, `SetMark`,
  `KillLine`, `KillRegion`, `Yank`, `SortImports`,
  `ReflowParagraph`, `MatchBracket`, `Abort` (mark clear) — see
  `docs/extract/actions.md`.
- Muts: `BufferUpdate`, `KillRingAccumulate`, `KillRingSet`,
  `BreakKillAccumulation`, `PendingYank`, `LspRequestPending`,
  `ApplyIndent`, `SetReindentChars`, `SyntaxHighlights`,
  `LspEdits`.
- Driver events: `ClipboardIn::Text`, `SyntaxIn`, `LspIn::Completion`,
  `LspIn::Edits`, workspace `UndoFlushed`,
  `SyncResultKind::SyncEntries` — see
  `docs/extract/driver-events.md`.
- Timers: `undo_flush` (500 ms) — see `docs/extract/timers.md`.

## Edge cases

- Insert char at EOL — cursor advances into virtual space; affinity
  tracks the preferred column for subsequent vertical moves.
- Insert at EOF with no trailing newline — char appends without
  synthesizing a newline; see edge golden `no_trailing_newline`.
- Unicode combining / CJK / emoji / RTL — char-based movement means
  combining sequences may require multiple moves to cross; see edge
  goldens `unicode_*`.
- Delete backward at BOL — joins to previous line; join closes the
  undo group via row change.
- Delete at BOF / forward at EOF — no-op.
- Undo with empty history — `buf.undo()` returns `None`;
  idempotent.
- Redo after a post-undo edit — redo history is discarded (standard
  undo-tree behavior).
- Kill line at EOL — kills the newline, joining with the next line.
- Kill line on empty buffer — no-op.
- Kill region with mark == cursor — zero-length region; would
  clobber kill ring with empty string. [unclear — does
  `edit::kill_region` short-circuit?]
- Yank with empty clipboard and empty kill ring — filter drops the
  event; nothing happens. See edge golden `yank_empty_kill_ring`.
- Yank into buffer with active mark — mark is cleared before
  inserting.
- Reflow on non-reflowable content (code, not a comment/markdown) —
  `reflow_buffer` returns `None`; "Nothing to reflow" alert.
- Reflow on already-wrapped content — same.
- SortImports on a file with no imports or an unknown language —
  "Imports already sorted".
- Indent-in-flight gates editing — between `InsertNewline` and the
  `ApplyIndent` reply, subsequent `InsertChar` is suppressed.
- Undo persistence vs content hash mismatch — if the file was
  edited externally between sessions and the hash no longer matches,
  undo history is discarded on reopen.
- Kill accumulation across buffer switches — accumulator resets on
  any non-`KillLine` action; killing in A then B produces two ring
  entries.
- Rapid type-then-undo — coalescing unwinds word-by-word (typing)
  and char-by-char (deleting); see edge golden
  `insert_then_undo_chain`.

## Error paths

- Clipboard read fails — arboard errors are not surfaced; the
  handler silently produces no yank. A golden needing a paste
  should prime the kill ring first. [unclear — confirm the driver
  drops errors instead of emitting a variant.]
- Syntax driver panic (bad tree-sitter query) — logs;
  `pending_indent_row` may remain set forever, blocking edits.
  [unclear — is there a watchdog?]
- LSP format returns garbage edits (wrong byte offsets) —
  `apply_text_edits` likely produces nonsensical buffer state; no
  guard against bad server behavior.
- Reflow dprint panic — `reflow_buffer` is expected to return
  `None` gracefully; a panic would crash the thread.
- SortImports on a partial or broken import block — tree-sitter
  returns fewer items than expected; the rewriter either refuses
  (None → "already sorted") or partially rewrites. The latter would
  be a silent bug. [unclear — worth a dedicated golden.]
- Undo after external reload — reload resets history; Undo behaves
  as if the buffer is fresh.
