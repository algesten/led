# search

## Summary

`led` has two distinct search surfaces: an **in-buffer incremental
search** (isearch, `Ctrl-s`) that navigates the current buffer's matches
interactively, and a **file-search overlay** (`Ctrl-f`) that runs a
ripgrep-style search across the entire workspace with optional regex,
case, and replace modes. The two share nothing at the code level — the
in-buffer version lives on `BufferState.isearch`, the overlay version
lives on `AppState.file_search` and is driven by a dedicated
`file-search` crate.

## Behavior

### In-buffer isearch

Starting isearch is `Ctrl-s`. The binding resolves to
`Action::InBufferSearch`, which is routed through the `Mut::Action`
mega-dispatcher (explicitly carved out of `isearch_of.rs` —
`isearch_of.rs:31` and `mod.rs:262-264`) because starting and advancing
must share a sync state write. `search::start_search(buf)` snapshots the
current cursor row/col and scroll as the **origin**, clears any mark,
and if there's a selection, seeds the query with the selected text
(`search.rs:46-65`). The buffer now carries a `Some(ISearchState)` and
the editor is in isearch mode.

**Typing** appends to the query. On each character, `update_search`
recomputes the full match list via a case-insensitive scan of the
whole document (`search::find_all_matches`, `search.rs:6-35`) and
jumps the cursor to the first match at or after the current cursor
position. If the query produces no forward match (matches exist but
all before the cursor, or no matches at all) the buffer enters a
"failed" state flag. Typing is always consumed by `isearch_of` — see
`isearch_of.rs:59-71`.

**Backspace** pops the last query character and re-runs
`update_search`. Popping the final character restores the cursor to
the origin and clears the matches list — this is how the user can
"undo" into a clean state without losing their position
(`isearch_of.rs:73-96`).

**Ctrl-s again** (`InBufferSearch` while isearch is active) advances
to the next match via `search::search_next`. Semantics:

- If the query is empty, recall `buf.last_search` (if any), populate
  the query, re-run `update_search`.
- If currently in `failed` state, **wrap to match index 0** and clear
  the failed flag. This is the wrap-around — it takes a second
  `Ctrl-s` press after hitting the end.
- Otherwise advance `match_idx`; if that would walk past the last
  match, set `failed = true` (which makes the *next* `Ctrl-s` wrap).

**Any arrow key / printable key / editing action** while isearch is
active emits `Mut::SearchAccept` (which clears `buf.isearch`) **and
also runs its normal handler on the same tick** — see
`isearch_of.rs:147-157`. So pressing `Up` during isearch accepts the
current match and moves the cursor up. `Resize`, `Quit`, `Suspend`
and `InBufferSearch` itself are carved out of this "accept on
passthrough" rule.

**Enter** accepts the search — keeps the cursor where it is, stashes
the query into `buf.last_search` for future `Ctrl-s`-on-empty
recalls, clears the isearch state. Additionally, if the cursor moved
from its origin, `isearch_of` emits `Mut::JumpRecord` with the origin
position so the user can `Alt-Left` back (`isearch_of.rs:114-131`).

**Esc / Ctrl-g** cancels: restores cursor and scroll to the origin,
saves `last_search`, clears `isearch` (`search.rs:161-167`).

### File-search overlay

`Ctrl-f` opens the overlay in the left sidebar. The activation path
is `find_file_of::open_file_search_parent_s` which seeds the query
from the current selection if any, sets `show_side_panel = true`,
focuses the side slot, and (when seeded) emits `TriggerFileSearch`
to kick off the initial search. On first open without a selection
the input is empty and no results are shown
(`find_file_of.rs:57-107`).

The overlay contains a **search input row**, an optional **replace
input row** (only when replace mode is on), and a **results tree**
grouped by file. The selection model (`FileSearchSelection` —
`crates/state/src/file_search.rs:37-42`) is a single cursor that
moves through `SearchInput → [ReplaceInput →] Result(i)` via the
unified vertical navigation. `file_search.rs:128-173` implements the
up/down transitions that make the input rows and the hit list feel
like one list.

**Typing** into the search input (`InsertChar` when selection is
`SearchInput`) mutates `fs.query`, resets the scroll offset, and
triggers a new driver request via `trigger_search` — which is
coalescing: the file-search driver drains the request queue and only
processes the latest (`crates/file-search/src/lib.rs:51-70`). Empty
query clears results without dispatching.

**Toggles** (`Alt-1 case`, `Alt-2 regex`, `Alt-3 replace`) flip the
relevant flags on `FileSearchState` and, for case / regex, re-trigger
the search. `Alt-3` additionally clears `replace_stack` when leaving
replace mode (`file_search.rs:362-384`).

**Enter on an input row** advances the selection: from `SearchInput`
to `ReplaceInput` if replace mode is on, otherwise to `Result(0)`
(`file_search.rs:411-433`). **Enter on a result row** either opens
the file at the hit location (via `confirm_selected` →
`promote_preview` or `request_open`), or in replace mode deactivates
the overlay (the per-hit replacements were already applied one by
one as the user navigated).

**Left / Right on a result row** (replace mode only) perform a
**single replace** on the current hit (right) or **undo the last
replace** (left) — a manual stepped flow for curated replacements.
Each individual replace gets its own undo group
(`file_search.rs:296-309, 539-630`). Hits that are replaced are
removed from the results; hits that are undone are reinserted,
re-sorted within the group.

**Alt-Enter (`ReplaceAll`)** performs the bulk replace: every hit in
`flat_hits` is replaced at once. Hits in already-open buffers are
replaced in-buffer (one undo group per file); hits in not-yet-open
files are stashed on `AppState.pending_replace_all`, the tabs are
pre-created and buffers pre-materialized, and when each buffer
arrives the stashed replacements are applied from
`apply_pending_replace` (`file_search.rs:706-802, 806+`). After
dispatch the overlay deactivates.

**Escape** closes the overlay via `deactivate` which clears
`state.file_search`, closes any preview tab, and returns focus to
Main (`file_search.rs:78-82`).

### Scope

File search operates on the **workspace root** (`state.workspace.loaded().root`,
falling back to the startup dir). There is no per-glob or per-path
scope filter in the UI — the ripgrep-style walker underlies it via the
`led_file_search` driver and respects `.gitignore`. Replace also
operates on the workspace root, with `ReplaceScope::Single` (one hit)
or `ReplaceScope::All` (bulk), and carries a `skip_paths` list for
"files we already replaced in-buffer and don't need to write from
disk" (`crates/state/src/file_search.rs:59-79`).

## User flow

**In-buffer**: user presses `Ctrl-s`, types `foo`, sees the cursor
jump to the first occurrence; presses `Ctrl-s` again, jumps to the
next; at end-of-doc presses `Ctrl-s` once more, which sets `failed`;
presses `Ctrl-s` a third time, wraps to the first occurrence; presses
`Enter`, accepts. A `JumpRecord` for the starting position is pushed;
`Alt-Left` returns the user.

**File search**: user selects a symbol, presses `Ctrl-f`, the overlay
opens with query pre-filled and results already populating. Arrow
`Down` drops the selection into the first hit; Enter opens it as a
preview tab. Back to the overlay, `Alt-3` enters replace mode,
`Tab` toggles to the replace input, types replacement text,
`Alt-Enter` replaces everywhere. Overlay closes.

## State touched

- `BufferState.isearch: Option<ISearchState>` — owned by the buffer,
  set by `start_search`, cleared by `search_accept` / `search_cancel`
  / `Mut::SearchAccept`.
- `BufferState.last_search` — persisted across isearch sessions
  within the buffer, recalled on `Ctrl-s` with empty query.
- `AppState.file_search: Option<FileSearchState>` —
  `query`, `cursor_pos`, `case_sensitive`, `use_regex`, `results`,
  `flat_hits`, `selection`, `scroll_offset`, `replace_mode`,
  `replace_text`, `replace_cursor_pos`, `replace_stack`.
- `AppState.pending_file_search` — set by `trigger_search`, read by
  derived to emit `FileSearchOut::Search`.
- `AppState.pending_file_replace` — set by `replace_selected` /
  `unreplace_selected`, read by derived to emit
  `FileSearchOut::Replace`.
- `AppState.pending_replace_all: Option<PendingReplaceAll>` — per-file
  stashed hits, applied on buffer materialization.
- `AppState.focus`, `AppState.show_side_panel` — written by overlay
  activation / deactivation.
- `AppState.jump` — pushed by isearch accept when the cursor moved.

## Extract index

- Actions: `InBufferSearch`, `OpenFileSearch`, `CloseFileSearch`,
  `ToggleSearchCase`, `ToggleSearchRegex`, `ToggleSearchReplace`,
  `ReplaceAll`, plus the overlay's repurposed `InsertChar`,
  `DeleteBackward`, `DeleteForward`, `InsertNewline`, `InsertTab`,
  `LineStart`, `LineEnd`, `KillLine`, `MoveLeft`, `MoveRight`,
  `MoveUp`, `MoveDown`, `PageUp`, `PageDown`, `FileStart`, `FileEnd`,
  `OpenSelected`, `Abort` — see `docs/extract/actions.md`.
- Keybindings: `Ctrl-s`, `Ctrl-f`, `Alt-1`, `Alt-2`, `Alt-3`,
  `Alt-Enter` (file_search context), `Enter`, `Esc/Ctrl-g`, and all
  the editing chords re-routed inside the two overlays —
  `docs/extract/keybindings.md` §"Context: file-search sidebar" and
  §"Context: isearch".
- Driver events: `FileSearchOut::Search`, `FileSearchOut::Replace`,
  `FileSearchIn::Results`, `FileSearchIn::ReplaceComplete` —
  `crates/file-search/src/lib.rs`.
- Persisted: isearch `last_search` is in-process only; file-search
  state is ephemeral (nothing persisted).

## Edge cases

- **Isearch empty query + `Ctrl-s`**: recalls `buf.last_search`
  (`search.rs:101-113`); still empty → no-op.
- **Isearch failed then typing**: `update_search` recomputes from
  current cursor, so the failed flag may clear without wrap.
- **Unicode in queries**: both query and line are lowercased and
  byte-scanned; column reported as char count (`search.rs:20-32`).
- **Invalid regex**: [unclear — confirm in file-search crate].
- **Replace-all across many files**: in-buffer files replace
  synchronously; others are opened lazily. No progress UI.
- **Unreplace after buffer was saved**: `replace_stack` still carries
  the entry; unreplace re-dirties the buffer but does not re-edit
  disk.
- **`Ctrl-f` with no workspace loaded**: `trigger_search` falls back
  to `startup.start_dir` (`file_search.rs:110-114`).

## Error paths

- **No match found in isearch**: sets `failed = true`, leaves cursor
  in place; the UI presumably styles the query differently but
  behavior-wise nothing else happens.
- **File-search driver dropped / panicked**: the `tokio::spawn` is
  fire-and-forget; no retry, no alert. [unclear — confirm in
  driver docs].
- **Replace on a file that was deleted between search and replace**:
  the driver write fails silently (channel send succeeded, disk op
  did not). The in-buffer hit is already gone from `results`.
  No recovery path. [unclear — confirm error handling in
  `file-search` crate].
