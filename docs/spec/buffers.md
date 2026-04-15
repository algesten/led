# Buffers

## Summary

A buffer is led's in-memory representation of a file's text, its
cursor and scroll position, its undo history, and the per-file
annotations (diagnostics, inlay hints, git line statuses) that must
stay in sync with content. Buffers are opened from four sources — CLI
arguments, find-file dialog, session restore, and jump-to-definition
— and they persist in memory until explicitly killed or evicted when
the tab disappears. Tabs are a separate concept: `state.tabs:
VecDeque<Tab>` names which buffers the user wants on screen. A tab may
exist without a materialized buffer (briefly, during startup
materialization). The *preview tab* is a transient one-entry slot
used for sidebar-driven file peeking that collapses back to the
previously active tab on dismissal. See [`editing.md`](editing.md)
for how editing actions mutate a buffer; the flows here only move
buffers in and out of the materialized set.

## Behavior

### Opening a file

Every open is an async request to the docstore driver: the model
sets `pending_open` (or `pending_opens`, for session bursts) and
derived issues `DocStoreOut::Open { path, create_if_missing }`. The
docstore acknowledges with `DocStoreIn::Opening` (currently inert —
the model drops it), reads the file, and replies `Opened { path, doc
}` or `OpenFailed { path }`. `create_if_missing = true` (CLI,
find-file, jump-to-def) turns `NotFound` into an empty `TextDoc` so
typing a fresh path always works. Session restore passes `false` so a
deleted session file becomes `SessionOpenFailed` and is dropped from
tabs + buffers.

The `Opened` handler in `buffers_of.rs` is where materialization
happens. It refuses to materialize a buffer for a tab that has been
killed meanwhile, it refuses to re-materialize an already-materialized
buffer (docstore does not deduplicate — duplicate opens each produce
an `Opened`), and it reconciles the incoming doc with any
`session.positions[path]` entry: saved cursor and scroll are applied,
and persisted undo history is re-attached if the doc's `content_hash`
matches the stored `PersistedContentHash`. Activation is decided
here: a CLI-arg file activates only if it was the last path on the
command line; a session file activates only if its tab index matches
`session.active_tab_order`; all other opens activate immediately. The
resulting `Mut::BufferOpen` carries every pre-computed field into the
reducer, which just assigns.

### Find-file picker

`Action::FindFile` (Ctrl-x Ctrl-f) opens the find-file overlay seeded
from the active buffer's directory (or the startup dir if none) and
fires `Mut::SetPendingFindFileList`, which triggers
`FsOut::FindFileList`. The `FsIn::FindFileListed` reply fills the
completion menu; Enter either opens an existing file or creates a new
one (create-if-missing). Tab expands the selected entry.

### Session restore

When the workspace driver emits `WorkspaceIn::SessionRestored { session
}`, `session_of.rs` fans out a dozen child streams that populate
tabs, `active_tab_order`, `show_side_panel`, jump list, browser
expansion, and the `resume` list — a per-tab tracker that
transitions each entry from `Pending` to `Open` or `Failed` as
docstore replies stream back. Once all entries leave `Pending`,
`resume_complete_s` flips `phase` from `Resuming` to `Running` and
activates the resolved active tab. Standalone (`--no-workspace`) and
fresh workspaces skip restore entirely. [unclear — precise sqlite
schema for `session.positions`; see persistence.md.]

### Saving

`Action::Save` (Ctrl-x Ctrl-s) has two branches, gated on whether an
LSP server is attached. Without LSP, `save_of.rs` synthesizes a
`BufferUpdate` that calls `begin_save`, `touch`,
`apply_save_cleanup`, and `record_diag_save_point` on a clone, then
emits `Mut::SaveRequest`; derived dispatches `DocStoreOut::Save`.
With LSP, the model emits `BufferUpdate` (begin_save + touch only),
`SetPendingSaveAfterFormat`, `LspRequestPending(Format)`, and an
`Alert("Formatting...")`. When the LSP returns `LspIn::Edits` and
`pending_save_after_format` is true with edits that apply cleanly,
`lsp_of.rs` emits `Mut::LspFormatDone` which applies the remaining
cleanup and finally emits `SaveRequest`. The docstore writes via
tempfile-plus-rename, returns `Saved { path, doc }`, and
`buffers_of.rs` emits `Mut::BufferSaved`.

`Action::SaveNoFormat` (Ctrl-x Ctrl-d) skips the LSP format step. It
emits `begin_save + record_diag_save_point` plus a `SaveRequest`
directly — useful when the formatter is broken or would damage
content.

`Action::SaveAs` (Ctrl-x Ctrl-w) opens the find-file overlay in
SaveAs mode. Its Enter handler emits `Mut::SaveRequest` targeting
the typed path, which derived converts into `DocStoreOut::SaveAs`.
The docstore writes the file, updates its parent-dir watch
registration (new parent in, old out), and replies `SavedAs`;
`buffers_of.rs` emits `Mut::BufferSavedAs` carrying the old path,
new path, and post-save buffer clone. The reducer replaces the
buffer's identity and re-keys tabs and notify_hash.

`Action::SaveAll` (Ctrl-x Ctrl-a) scans every buffer, `begin_save`s
each dirty one, and emits one `Mut::SaveAllRequest`. Derived
dispatches `WorkspaceOut::FileSaveAll`.

### Tabs — open, close, switch, preview

A `Tab` names its path, an optional `preview: Some(PreviewTab {
previous_tab })` slot, and an optional `pending_cursor` set by
jump-to-def or session restore so the cursor is positioned once the
buffer materializes. `Action::NextTab` (Ctrl-right) and
`Action::PrevTab` (Ctrl-left) cycle `active_tab` across non-preview
materialized tabs. Switching activates only — buffers stay
materialized.

The preview tab is at most one entry in the `VecDeque`. Sidebar
navigation over a file entry calls `action::preview::set_preview`
which replaces any existing preview and stashes `previous_tab`.
Pressing Enter on a previewed file promotes it (`unpreview`); moving
to another file swaps the preview target. `close_preview` removes
the preview tab and restores `active_tab` to `previous_tab`. If all
tabs disappear, focus falls back to `PanelSlot::Side`.

### Kill buffer (with confirm)

`Action::KillBuffer` (Ctrl-x k) dispatches through the
`Mut::Action` mega-handler to `tabs::kill_buffer`. If the buffer is
dirty, it sets `state.confirm_kill = true` and installs an alert
("Buffer X modified; kill anyway? (y or n)"). The next
`Action::InsertChar('y' | 'Y')` routes through
`confirm_kill_accept_s` to emit `Mut::ForceKillBuffer`; any other
migrated action clears `confirm_kill` via `Mut::DismissConfirmKill`.
A non-dirty kill, or a force kill, executes `do_kill_buffer`: if the
target is the preview tab it delegates to `close_preview`; otherwise
it picks the adjacent tab (next, falling back to previous) as the
new active, removes the target, and installs a "Killed X" alert.
The orphaned buffer is dematerialized by the post-reducer invariant
pass.

### External filesystem change

The docstore driver watches each parent directory of every opened
file. A Modify or Create event on a watched path triggers an on-disk
re-read and a `DocStoreIn::ExternalChange { path, doc }`.
`buffers_of.rs` branches three ways: same content-hash with a dirty
buffer that has no `has_local_edits` means an external save of our
own content — mark externally saved, take a diag save point.
Different content-hash with `has_local_edits` means the user has
unsaved changes; led silently ignores the external write to protect
them. Different content-hash with no local edits means a clean
reload. `DocStoreIn::ExternalRemove` is currently dropped (the
buffer stays open with stale content and no indicator; flagged as a
gap).

### Dirty tracking

A buffer is dirty when `version != saved_version`. Every edit bumps
`version`; `save_completed` advances `saved_version` to match the
saved doc's version. `has_local_edits` is stricter — true only when
the user has typed since last save/reload — and guards the
external-change clobber. `is_dirty` is the broad check used for the
confirm-kill prompt and `SaveAll` filtering.

## User flow

The user launches `led src/foo.rs src/bar.rs`. Both files open;
`bar.rs` activates (last on the CLI). The user types in `bar.rs` —
cursor moves, undo accumulates, version bumps, dirty=true. After 500
ms of quiet the `undo_flush` timer serializes new undo entries to
sqlite. Ctrl-s: because LSP is attached, rust-analyzer formats,
edits apply via `LspEdits`, the post-format cleanup runs, docstore
writes `bar.rs`. `BufferSaved` clears dirty state; a "Saved" alert
appears. The user switches to `foo.rs` (Ctrl-left), hits Ctrl-x k —
it's clean, the tab disappears, `bar.rs` reactivates. Meanwhile a
teammate edits `bar.rs` on disk; the watcher fires, docstore reads,
and because the local buffer is clean the external content replaces
it.

## State touched

- `state.buffers: Rc<HashMap<CanonPath, Rc<BufferState>>>` — the
  materialized buffer set; written by `BufferOpen`, `BufferUpdate`,
  `BufferSaved`, `BufferSavedAs`; cleaned up by the post-reducer
  orphan sweep.
- `state.tabs: VecDeque<Tab>`, `state.active_tab: Option<CanonPath>`
  — written by `SetActiveTab`, `ActivateBuffer`, `AddTab`, and the
  imperative `tabs::kill_buffer` / preview paths.
- `state.pending_open`, `state.pending_opens`, `state.save_request`,
  `state.save_all_request`, `state.save_done`,
  `state.pending_save_as`, `state.pending_find_file_list` —
  versioned request slots drained by derived.
- `state.session.positions`, `state.session.active_tab_order`,
  `state.session.resume` — session restore reconciliation.
- `state.notify_hash_to_buffer` — path-hash index for cross-instance
  notify events.
- `state.confirm_kill`, `state.alerts.info` — the dirty-kill prompt.
- `state.focus: PanelSlot` — falls back to `Side` when tabs become
  empty.
- `BufferState.version`, `.saved_version`, `.save_in_flight`,
  `.materialization`, `.content_hash`, `.path_chain`, `.last_used`
  — per-buffer identity and dirty bookkeeping.

## Extract index

- Actions: `FindFile`, `Save`, `SaveNoFormat`, `SaveAs`, `SaveAll`,
  `KillBuffer`, `PrevTab`, `NextTab`, `OpenSelected`,
  `InsertChar('y'|'Y')` (confirm-kill accept), `Abort` (dismiss) —
  see `docs/extract/actions.md`.
- Driver events: docstore `Opening`, `Opened`, `Saved`, `SavedAs`,
  `ExternalChange`, `ExternalRemove`, `OpenFailed`, `Err(Alert)`;
  workspace `SessionRestored`, `SessionSaved`, `WatchersReady`,
  `WorkspaceChanged`; fs `FindFileListed` — see
  `docs/extract/driver-events.md` §docstore, §workspace, §fs.
- Timers: `undo_flush`, `tab_linger`, `alert_clear` — see
  `docs/extract/timers.md`.

## Edge cases

- Duplicate `Opened` for the same path — filtered via
  `is_materialized()`.
- `Opened` arrives after tab was killed — filtered by the `tabs`
  membership check.
- Session file missing on disk — `OpenFailed` →
  `SessionOpenFailed`; other resume entries continue.
- Save while another save is in-flight — `save_in_flight` flag
  avoids stacking; not surfaced to the user.
- Save to a read-only parent — docstore returns `Err(Alert)`;
  buffer stays dirty; no retry.
- Save-as to an existing path — overwrites silently. No confirm.
  [unclear — intentional?]
- External change on darwin can race the save rename (FSEvents fires
  in <3ms); content-hash compare usually no-ops.
- External change to dirty buffer with local edits — silently
  dropped. [unclear — should we surface "file changed on disk"?]
- External delete — buffer stays open with stale content; no
  indicator. Flagged gap.
- Preview tab across session quit — preview is not persisted.
- Kill preview via Ctrl-x k — delegates to `close_preview`.
- Empty tabs after kill — focus falls back to `Side`.
- CLI arg list mixing existing and missing paths — missing ones
  open as empty (via `create_if_missing`); last path activates
  regardless.
- CRLF, no trailing newline, unicode CJK/combining/emoji/RTL — see
  edge goldens `crlf_line_endings`, `no_trailing_newline`,
  `unicode_*`, `very_long_line`.

## Error paths

- Disk read fails, `create_if_missing = false` → `OpenFailed` →
  `SessionOpenFailed`: tab + buffer removed, resume marked `Failed`,
  no user-visible alert.
- Disk read fails, `create_if_missing = true` → empty `TextDoc`
  fallback; user can save to create the file.
- Save tempfile write or rename fails → `Err(Alert::Warn(...))`;
  tempfile is cleaned up; buffer stays dirty.
- Save parent-dir creation fails → `Err(Alert)` before write.
- Save serialization (`Doc::write_to`) fails → `Err(Alert)`.
- LSP format times out or crashes mid-save →
  `pending_save_after_format` stays true; the save stalls. [unclear
  — no visible timeout; worth a dedicated golden.]
- External watcher registration fails — silently skipped; no
  external-change notifications for that file.
- Kill-buffer on the last tab — `focus = Side`, `active_tab = None`;
  no crash.
