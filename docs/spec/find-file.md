# find-file

## Summary

The **find-file overlay** is `led`'s path-oriented opener. `Ctrl-x Ctrl-f`
opens it in Open mode (type a path, hit Enter, buffer opens);
`Ctrl-x Ctrl-w` opens the same overlay in SaveAs mode (type a path,
Enter writes the current buffer there). Completions come from a
filesystem driver that does prefix matching against the current
directory; navigation through completions is arrow-driven; directories
can be descended by Tab or by typing a trailing `/`. There is no fuzzy
matching inside the overlay — matching is a strict case-insensitive
prefix against the leaf name. "Recents" and "workspace files" are not
concepts the overlay exposes.

## Behavior

### Activation

Both entry points go through `find_file_of.rs`, which emits two Muts
per activation: `Mut::SetFindFile(FindFileState)` seeding the
initial state, and `Mut::SetPendingFindFileList(dir, prefix, show_hidden)`
which derived turns into `FsOut::FindFileList` to populate completions.

**Open mode (`Ctrl-x Ctrl-f`)** — initial input is the directory
containing the active buffer's path (or `start_dir` if none), with a
trailing `/`, home-abbreviated to `~/` when applicable. Cursor is at
end-of-input. `compute_activate` in `find_file.rs:232-261`.

**SaveAs mode (`Ctrl-x Ctrl-w`)** — initial input is the active
buffer's full path (if any) rather than just its parent, so the
user can tweak the filename and hit Enter to save in-place. If no
active buffer, falls back to start_dir with trailing `/`. No prefix
is seeded, so completions include every entry in that directory
(`find_file.rs:264-302`).

### Input editing

Keystrokes reach the overlay via the "any migrated action"
pass-through in `action/mod.rs:226-232` that routes everything to
`Mut::FindFileAction`, which then hits `handle_find_file_action`.
The overlay re-implements a tiny inline editor over `ff.input` +
`ff.cursor`:

| Action | Effect |
|---|---|
| `InsertChar(c)` | byte-insert at cursor, re-request completions |
| `DeleteBackward` | delete previous char boundary, re-request |
| `DeleteForward` | delete next char, re-request |
| `MoveLeft` / `MoveRight` | cursor to previous / next char boundary |
| `LineStart` (`Ctrl-a`, `Home`) | cursor to 0 |
| `LineEnd` (`Ctrl-e`, `End`) | cursor to input length |
| `KillLine` (`Ctrl-k`) | truncate input at cursor, re-request |

Every input-changing action calls `request_completions`
(`find_file.rs:111-137`) which:

1. Resets `ff.selected = None` and `ff.base_input = ff.input.clone()`.
2. Computes the expected listing directory: if input ends with `/`, the
   directory is the expanded path itself; otherwise it's the parent of
   the expanded path and the file name is the prefix.
3. If the prefix starts with `.`, `show_hidden = true`; otherwise dot-files
   are filtered out of the listing.
4. Sets `state.pending_find_file_list` so derived fires
   `FsOut::FindFileList { dir, prefix, show_hidden }`.

The FS driver (`crates/fs/src/lib.rs:93-141`) reads the dir,
prefix-filters case-insensitively on the display name, sorts
dirs-first then alphabetical, and returns each entry as
`FindFileEntry { name, full: CanonPath, is_dir }` where `name` for
directories has a trailing `/`.

### Arrow navigation through completions

`MoveUp` / `MoveDown` wrap through `ff.completions` via
`wrap_selection_up` / `wrap_selection_down` and rewrite `ff.input`
to the `input_dir_prefix(base_input) + selected.name`. So the user
sees the selected completion live in the input box. `show_side = true`
is set so the completions list is displayed in the sidebar
(`find_file.rs:371-405`).

Preview side effects: non-dir selections call
`super::action::set_preview` which opens a preview tab for the
highlighted file (`find_file.rs:313-324`). The overlay thereby
doubles as a lightweight file previewer — arrow through files and
see them in the main pane without committing.

### Tab completion

`Tab` invokes `tab_complete` (`find_file.rs:174-227`):

1. If input ends with `/` and there are completions, just show the
   side panel (user is exploring, don't auto-select).
2. If there's a single match which is a directory (no trailing `/`
   yet), append `/` (triggers a descent re-request).
3. If there's a single match of any kind, complete the input fully
   to the matched name.
4. If there are multiple matches, extend the input to the longest
   common prefix across their base names (case-insensitive matched)
   and show the side panel.

LCP is computed on the raw leaf names (trailing `/` stripped) —
`longest_common_prefix` in `find_file.rs:141-170`.

### Enter (commit)

`InsertNewline` dispatches to `handle_enter`, which routes by
`FindFileMode`:

**Open mode** (`handle_enter_open`, `find_file.rs:468-546`):
- **Path A**: a completion is selected (arrows). If it's a directory,
  descend into it by appending its name (plus `/`). If it's a file,
  either promote an existing preview of that file to a real tab, or
  `request_open` + set active tab.
- **Path B**: no selection, but the exact expanded input matches a
  completion entry. Same dir-or-file branching as A, except for
  directories the re-request only fires if input already ends with
  `/`.
- **Path C**: no selection, no exact match, input is non-empty and
  doesn't end with `/`. Open (or create) a new file at that path.

**SaveAs mode** (`handle_enter_save_as`, `find_file.rs:548-604`):
- Selection on a directory: descend (same as Open mode).
- Selection on a file: use its `full` as the save target.
- No selection, no matching entry: use `expanded_canon` as the save
  target, provided the input doesn't end with `/` and is non-empty.
- Execution: `state.buf_mut(&active).begin_save()` plus
  `state.pending_save_as = Some(target)`. The save flows through
  the workspace driver and back as `DocStoreIn::SavedAs`.

After any successful commit, `deactivate` closes the overlay and
clears any preview tab.

### Escape

`Abort` calls `deactivate` which clears `state.find_file` and closes
the preview. Cursor and focus are restored to the previous state
(which, for find-file, is always Main; find-file cannot run while
focused Side — `find_file` is not a keymap context).

### Path expansion and tilde

`expand_path` resolves `~` to `$HOME` and canonicalizes `..` / `.`
lexically. `abbreviate_home` goes the other way for display. Symbolic
links are NOT resolved by `expand_path` (it's a path-syntax operation
only); the `UserPath::canonicalize()` inside the driver's
`find_file_list` does the OS-level canonicalization.

## User flow

User presses `Ctrl-x Ctrl-f`. Input is prepopulated with
`~/project/src/`. Completions populate in the side panel within a
frame. User types `m`, the list narrows to entries starting with `m`;
types `o`, narrows further; presses `Tab`, which extends the input
to `~/project/src/model_of.rs` (the single remaining match), and
presses `Enter` — file opens as a tab.

Alternative: user presses `Ctrl-x Ctrl-f`, presses `Down` three times
to browse, sees each file as a preview in the main pane, finds the
right one, presses `Enter`. Committed; preview promoted to real tab.

SaveAs: user has a scratch buffer, presses `Ctrl-x Ctrl-w`; input is
seeded with current path (or start_dir). User edits the filename,
presses `Enter`, buffer saves to the new location; tab rebinds.

## State touched

- `AppState.find_file: Option<FindFileState>` —
  `mode`, `input`, `cursor`, `base_input`, `completions`,
  `selected`, `show_side` (`crates/state/src/lib.rs:1636-1644`).
- `AppState.pending_find_file_list` — set on every input change, read
  by derived.
- `AppState.pending_save_as` — set on SaveAs commit, read by derived.
- `AppState.tabs` / `active_tab` — written when a preview promotes or
  a new file opens.
- `AppState.focus` — not explicitly changed by find-file (overlay
  runs while focus remains Main).

## Extract index

- Actions: `FindFile`, `SaveAs`, plus every action re-bound inside
  the overlay (`InsertChar`, `DeleteBackward`, `DeleteForward`,
  `InsertTab`, `InsertNewline`, `MoveUp`, `MoveDown`, `MoveLeft`,
  `MoveRight`, `LineStart`, `LineEnd`, `KillLine`, `Abort`) —
  `docs/extract/actions.md`.
- Keybindings: `Ctrl-x Ctrl-f`, `Ctrl-x Ctrl-w`, and the overlay-
  context bindings — `docs/extract/keybindings.md` §"Context:
  find-file / save-as overlay".
- Driver events: `FsOut::FindFileList`, `FsIn::FindFileListed` —
  `crates/fs/src/lib.rs`.
- Persisted: none. Find-file state is purely ephemeral.

## Edge cases

- **Leading `~`**: `expand_path` substitutes `$HOME`; display
  abbreviates back to `~/` via `abbreviate_home`.
- **Trailing `/` with empty listing**: Enter is a no-op in Open mode;
  SaveAs refuses (`find_file.rs:588`).
- **Non-existent prefix**: completions empty; Tab no-ops; Enter in
  Open mode takes Path C and creates-or-opens the path.
- **Dotfile navigation**: a leading `.` in the leaf prefix flips
  `show_hidden = true` for that listing.
- **Unbound action while active**: catch-all `_` branch in
  `handle_find_file_action` deactivates the overlay
  (`find_file.rs:451-454`).
- **Stale listings**: `expected_dir` (`find_file.rs:30-37`) lets
  derived discard listings whose directory no longer matches the
  current input.

## Error paths

- **Directory unreadable** (permissions, deleted mid-session): the
  `fs` driver logs a warning and returns `Vec::new()`
  (`crates/fs/src/lib.rs:96-103`). Completions list goes empty; no
  alert in the UI.
- **SaveAs to an unwritable directory**: the docstore surfaces an
  error on the save attempt, which raises an alert through the
  save flow. The overlay has already `deactivate`d by then.
- **Open on a file that fails to materialize**: see `buffers.md`;
  the buffer goes into an error state. The overlay doesn't itself
  observe the failure.

## Dead / absent features

- **No fuzzy matching**: completions are strict prefix matches only.
  A user-visible "fuzzy find" does not exist.
- **No recents list**: the overlay does not surface a list of
  recently-opened files independent of the filesystem listing.
- **No workspace-file list**: the overlay does not pull from
  `state.workspace` or the tree crawl — it always goes through the
  FS driver directory by directory.
- **No mouse-click to commit**: overlay is keyboard-only.
