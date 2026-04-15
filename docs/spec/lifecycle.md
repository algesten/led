# lifecycle

## Summary

`led` is a single-threaded terminal editor with a pronounced startup ramp: the
process first resolves its launch context (CLI args, config dir, workspace
root), then opens the session database and restores prior tabs, then brings up
the rest of the reactive graph (watchers, LSP, syntax, git, file browser). A
small state machine (`AppState.phase`) tracks the lifecycle: `Init → Resuming
→ Running ⇄ Suspended`, with `Exiting` as the terminal transition. Because
the whole editor is driven by the FRP cycle described in `CLAUDE.md`, "startup"
is not a linear boot but a short storm of reactive events that must converge
before the user sees a first paint. Quit is likewise reactive — setting phase
to `Exiting` kicks off a session save, and the process is only allowed to exit
once the database write has been acknowledged.

## Behavior

### Phase state machine

Defined in `crates/state/src/lib.rs:33` (`pub enum Phase`):

```
Init ──┬── Resuming ──┐
       └──────────────┼── Running ⇄ Suspended
                      │
       (any phase) ───┴── Exiting ── (process exit)
```

- `Init` — default. Waiting for workspace resolution, session load, first
  paint.
- `Resuming` — session found with tabs to restore; buffers are being
  materialized from disk.
- `Running` — fully operational. Focus is resolved on entry
  (`process_of.rs:33-45`).
- `Suspended` — SIGTSTP: the process has been stopped by `Action::Suspend`.
  `process_of.rs:12-18` performs the re-init handshake when phase returns to
  `Running`.
- `Exiting` — set by `Action::Quit`. `derived.rs:67-117` fires one
  `WorkspaceOut::SaveSession`; `lib.rs:339-350` holds the quit signal until
  `session.saved == true` (or the workspace is non-primary / standalone, in
  which case no save is attempted and quit returns immediately).

### Startup sequence

The numbered flow from `main` to first paint (see `led/src/main.rs`,
`led/src/lib.rs`, `crates/workspace/src/lib.rs:189-294`,
`led/src/model/session_of.rs`):

1. **CLI parse** — `Cli::parse()` (`main.rs:94`). clap parses args: file paths,
   `--log-file`, `--reset-config`, `--no-workspace`, `--keys-file`,
   `--keys-record`, `--golden-trace`, `--config-dir`, `--test-lsp-server`,
   `--test-gh-binary`. Before runtime setup, `--reset-config` short-circuits:
   rewrites `keys.toml`, `theme.toml`, removes `db.sqlite`, and exits.
2. **Logger init** — if `--log-file` provided.
3. **Path resolution** — each CLI path is paired as `(UserPath, CanonPath)`
   (`main.rs:117-121`). The pairing preserves the user's symlink spelling for
   later language detection (`.profile` vs `dotfiles/profile`). Directories
   vs. files are disambiguated: a single directory arg seeds the browser (no
   files opened); any file-containing arg list opens those files.
4. **`Startup` struct assembly** (`main.rs:205-218`, definition in
   `crates/core/src/config.rs:8-73`) — freezes the immutable launch config:
   `arg_paths`, `arg_user_paths`, `start_dir`, `config_dir`, `arg_dir`,
   `no_workspace`, `golden_trace`, and test overrides. After construction
   this struct is never mutated.
5. **Tokio `LocalSet` entered** (`main.rs:226`). Everything in the model runs
   on the current thread.
6. **`led::run(startup, terminal_in, actions_in, quit_tx)`** — the reactive
   graph is built (`lib.rs:60-357`):
   1. `FileWatcher::new()` (or `inert()` when `enable_watchers == false`).
   2. `AppState::new(startup)` gives the initial state; `state: Stream<Rc<AppState>>`
      is created as an imitator.
   3. `derived(state, git_activity)` builds every driver output stream.
   4. Drivers are spawned: `led_terminal_in`, `led_ui`, `led_timers`, `led_fs`,
      `led_clipboard`, `led_syntax`, `led_git`, `led_gh_pr`, `led_file_search`,
      `led_lsp`, `led_workspace`, `led_docstore`, two `led_config_file` drivers
      (keys + theme). Each returns a `*In` stream.
   5. `model(drivers, init)` wires all the `_of` stream combinators and
      produces `real_state`; `real_state.forward(&state)` closes the cycle.
   6. `state.push(seed)` — the first emission. This is what triggers the whole
      derived side to fire its initial driver outputs.
7. **First derived sweep** — `state` seeded → derived emits:
   - `WorkspaceOut::Init { startup }` (deduped on `startup`, so only fires once).
   - `ConfigFileOut::ConfigDir { ... }` (deduped on config path + read-only
     flag) — config-file drivers begin reading `keys.toml` and `theme.toml`.
   - First `FsOut::ListDir` for browser root (seeded from session restore
     result, below).
   - Timers are set for `alert_clear`, `undo_flush`, spinner, etc. as their
     gating conditions come true.
8. **Workspace driver processes `Init`** (`crates/workspace/src/lib.rs:189-294`)
   on a spawned tokio task:
   1. If `startup.no_workspace` is true: emit `SessionRestored { session: None }`
      and `WatchersReady` and return. No git root, no flock, no DB, no
      watchers. (`lib.rs:199-205`.)
   2. Otherwise: `find_git_root(start_dir)` walks up from `start_dir`
      searching for the deepest `.git/` directory
      (`crates/workspace/src/lib.rs:461-475`). "Deepest" = the topmost match
      found while popping; falls back to `start_dir` if no `.git` exists in
      any ancestor.
   3. `try_become_primary(config, root)` (`lib.rs:477-496`) creates
      `<config>/primary/<hash(root)>`, opens it, and tries `flock(LOCK_EX |
      LOCK_NB)`. Success → this instance is primary; failure → another led
      is already primary for this workspace, we run read-only. Headless tests
      skip this and always mark primary (stale locks between parallel test
      runs otherwise).
   4. Emit `WorkspaceIn::Workspace { workspace }` — the model wraps it into
      `Mut::Workspace { workspace, initial_dirs }`, which seeds
      `pending_lists` (triggers `FsOut::ListDir` for root + expanded dirs),
      bumps the git file-scan counter, and sets `workspace =
      WorkspaceState::Loaded(...)` (`mod.rs:986-996`).
   5. `db::open_db(config)` at `<config>/db.sqlite` — runs schema migration
      (drops & recreates if `user_version != 3`, see persistence.md).
   6. `db::load_session(conn, root_str)` — primary only. Returns a
      `RestoredSession` or `None`. For each restored buffer, `db::load_undo_all`
      loads its undo entries and `chain_id`/`content_hash`/`undo_cursor`.
      Non-primary instances get `None` (they cannot safely own the session row).
   7. Emit `WorkspaceIn::SessionRestored { session }`.
   8. Create `<config>/notify/`, register a recursive watcher on the workspace
      root and a non-recursive watcher on `notify/` with the shared
      `FileWatcher`.
   9. Emit `WorkspaceIn::WatchersReady`.
9. **Model reacts to `SessionRestored`** (`session_of.rs:104-253`). A single
   parsed `SessionData` is fanned out into ~12 child streams, each producing
   one fine-grained `Mut` (CLAUDE.md Principle 2):
   - `SetActiveTabOrder`, `SetShowSidePanel` (suppressed in standalone mode),
     `SetSessionPositions`, `SetBrowserState` (selected + scroll + expanded dirs),
     `SetJumpState`, `SetPendingLists` (expanded dirs to list), optionally
     `SetPendingLists([start_dir])` for standalone.
   - If `pending_opens` is non-empty: `EnsureTab(buf)` per restored path (via
     `BufferState::new` when the user re-invoked with the same symlink, else
     `new_from_canon`), `SetResumeEntries(paths)`, `SetPhase(Resuming)`.
   - If `pending_opens` is empty: `SetPhase(Running)`, plus
     `EnsureTab(arg_user_path)` per CLI-arg file, `SetFocus(resolve_focus_slot())`,
     and optional `BrowserReveal(arg_dir)`.
10. **Buffer materialization** — the derived layer intersects `tabs` with
    non-materialized buffers (`derived.rs:188-231`) and emits
    `DocStoreOut::Open { path, create_if_missing }` for each. The docstore
    driver deduplicates and reads from disk; responses come back as
    `DocStoreIn::Opened { path, doc }` → `Mut::BufferOpen` applies the session
    cursor/scroll and attaches the restored undo history (`buffers_of.rs:18-91`).
    Failed opens produce `Mut::SessionOpenFailed`, which removes the tab and
    marks the resume entry `Failed`.
11. **Resume complete** — when every resume entry is no longer `Pending`
    (all session buffers either Opened or Failed), the `resume_complete_s`
    parent stream fires (`mod.rs:297-340`) and branches into
    `SetPhase(Running)`, `SetActiveTab(last_arg_or_first_tab)`,
    `EnsureTab` for any CLI arg not yet open, `SetFocus(...)`, and
    `BrowserReveal(arg_dir)` if one was passed. Phase is now `Running`.
12. **Post-resume activation** — `process_of::activate_arg_s` dedupes on
    `phase == Running` and activates the last CLI-arg tab; `touch_args_s`
    bumps `last_used` on arg buffers so they resist LRU eviction
    (`process_of.rs:31-67`).
13. **First UI frame** — the UI driver observes the state transitions and
    renders. `derived.rs:50-58` suppresses rendering while
    `pending_indent_row` is set to avoid a cursor flash; the first frame the
    user sees is `Phase::Running` with all tabs, gutters, and the status bar
    drawn.
14. **Background settle** — LSP servers (spawned per language on first buffer
    open), git scan (50ms debounce from `pending_file_scan`), and gh-pr
    (loaded on branch detection, polled every 15s) continue populating state
    asynchronously. None of these gate first paint.

### Suspend / resume

- `Action::Suspend` (default Ctrl-Z) → `Mut::SetPhase(Suspended)`
  (`ui_actions_of.rs:37-40`).
- `process_of::suspend_s` observes phase == Suspended
  (`process_of.rs:12-17`), runs a side effect that leaves the alt-screen,
  restores cooked mode, `raise(SIGTSTP)` (POSIX-stops the process), and on
  return re-enters the alt-screen and re-enables bracketed paste and raw
  mode. Then emits `Mut::Resumed` which the reducer interprets as
  `SetPhase(Running)`.
- `process_of::redraw_s` detects the `Suspended → Running` edge and bumps
  `force_redraw` so the UI driver repaints from scratch.

### Quit

- `Action::Quit` (default Ctrl-x Ctrl-c) → `Mut::SetPhase(Exiting)`
  (`ui_actions_of.rs:32-35`).
- `derived::session_save` (`derived.rs:67-117`) dedupes on `phase == Exiting`,
  requires `workspace.loaded().primary == true`, and emits one
  `WorkspaceOut::SaveSession { data }`. The session serializes: non-preview
  tabs (paths preserved as `UserPath` so symlink names survive),
  cursor/scroll per buffer, active tab order, `show_side_panel`, and a kv
  blob containing `browser.selected`, `browser.scroll_offset`,
  `browser.expanded_dirs` (newline-separated), and the jump list
  (`jump_list.entries` JSON + `jump_list.index`).
- Workspace driver writes to SQLite, replies with `WorkspaceIn::SessionSaved`
  → `Mut::SessionSaved` sets `s.session.saved = true`.
- `lib.rs:339-350` subscribes `state.on(...)`: when `phase == Exiting` and
  (`session.saved || !needs_save`), the oneshot `quit_tx` fires.
- `main.rs:277-297`: `rx.await` returns; terminal is restored (disable raw
  mode, leave alt-screen, disable bracketed paste, show cursor); then
  `std::process::exit(0)`. The hard exit is intentional — it avoids waiting
  for background `spawn_blocking` work (git scans, `gh` polls, LSP shutdown
  handshakes) that would otherwise stall the runtime's drop especially when
  quitting mid-startup.

Unsaved buffer confirmation is orthogonal: `Action::KillBuffer` on a dirty
buffer sets `confirm_kill`, displays a prompt; `y/Y` force-kills, anything
else cancels. This guards single-tab kills, not the overall quit path —
`Ctrl-x Ctrl-c` on a workspace with dirty buffers saves session and exits
without prompting; unflushed undo data remains in SQLite for recovery
(see Crash recovery below).

### Crash recovery

If led exits abnormally (SIGKILL, panic, power loss), the session row and
all flushed undo entries are still in SQLite. On next startup the workspace
driver walks the same Init path and the same `SessionRestored(Some(...))`
fires — tabs reopen at the saved cursor positions with their persisted undo
history intact. Entries flushed by the `undo_flush` timer (default 200ms
after an edit burst) are durable; edits in the 200ms tail window are lost.

The primary-instance flock is released by `flock` when the OS reaps the
process, so a second instance can claim primacy on next launch regardless
of how the previous one exited.

## User flow

- **Cold start in a project**: `cd ~/project && led` (or `led src/foo.rs`).
  User sees brief blank screen while session loads; then tabs reappear at the
  cursor positions they left, with the sidebar expanded as it was.
- **Opening the same project in a second terminal**: `led` again. First
  instance keeps the primary flock; second instance starts without a session
  restore (explicitly `None` for non-primary) but shares the LSP diagnostics
  picture via the cross-instance notify mechanism (see persistence.md).
- **Suspend**: mid-edit, Ctrl-Z. Shell prompt returns. `fg` → editor redraws
  in place.
- **Quit and reopen**: Ctrl-x Ctrl-c. Status may briefly show "Saving
  session" alert. Relaunch → prior state restored.
- **`$EDITOR` use**: `git commit` invokes `led --no-workspace
  .git/COMMIT_EDITMSG`. Standalone mode: no session, no flock, no watchers.
  Browser is rooted at CWD (the project), not the `.git` parent. Quit
  returns immediately (no session to save).

## State touched

- `state.phase` — written by `Mut::SetPhase` from `session_of.rs`,
  `ui_actions_of.rs`, `mod.rs:308-310`, and `process_of.rs`.
- `state.workspace` (`WorkspaceState::{Loading, Loaded, Standalone}`) — set
  by `Mut::Workspace`.
- `state.session.{saved, watchers_ready, resume, ...}` — gates quit.
- `state.startup` (immutable `Rc<Startup>`) — read by every `_of` module
  that needs arg paths, config dir, no-workspace flag.
- `state.tabs`, `state.buffers`, `state.active_tab`, `state.focus` — set up
  by session restore and by `resume_complete_s`.
- `state.browser` (root, selected, scroll_offset, expanded_dirs) — seeded
  from KV blob in session, then from `Mut::Workspace`'s `initial_dirs`.
- `state.jump.{entries, index}` — restored from `jump_list.entries` KV.
- `state.force_redraw` — bumped on Suspend→Running edge.

## Extract index

- Actions: `Quit`, `Suspend`, `Abort`, `Wait(_)`, `Resize(_,_)` → `docs/extract/actions.md`.
- Keybindings: `Ctrl-x Ctrl-c` (quit), `Ctrl-z` (suspend) →
  `docs/extract/keybindings.md`.
- Driver events consumed:
  - `WorkspaceIn::Workspace`, `WorkspaceIn::SessionRestored`,
    `WorkspaceIn::SessionSaved`, `WorkspaceIn::WatchersReady`,
    `WorkspaceIn::WorkspaceChanged`, `WorkspaceIn::GitChanged`
    → `docs/extract/driver-events.md` § workspace.
  - `DocStoreIn::Opened`, `DocStoreIn::OpenFailed` (session restore paths).
- Driver outputs: `WorkspaceOut::{Init, SaveSession}`,
  `DocStoreOut::Open` (materialization), `ConfigFileOut::ConfigDir`.
- Timers (none gate first paint; all fire after `Running`):
  `undo_flush`, `alert_clear`, `git_file_scan`, `tab_linger` →
  `docs/extract/driver-events.md` § timers.
- CLI flags: all flags in `Cli` struct (`main.rs:17-61`).
- Config keys: `kbd/quit`, `kbd/suspend`, `kbd/abort`.

## Edge cases

- **No git ancestor**: `find_git_root` falls back to `start_dir`. Workspace is
  "Loaded" but the root is just the start dir. Session is keyed by the
  start-dir path, so distinct non-git dirs each get their own session row.
- **Symlinked CLI arg (`led ~/.profile` where it links into `~/dotfiles`)**:
  `UserPath` preserves `~/.profile`; `CanonPath` resolves to the target.
  Session restore with the canonical path only reconstructs the user form if
  the user re-invoked with the same symlink as an arg
  (`session_of.rs:170-187`); otherwise falls back to `new_from_canon` and
  LSP/syntax detection uses the canonical filename.
- **Non-primary instance quit**: `session.saved` never flips (no save
  request), but `needs_save = false`, so quit fires immediately.
- **Standalone (`--no-workspace`) quit**: workspace is `Standalone`,
  `needs_save = false`, quit fires immediately.
- **Session has files that no longer exist on disk**: `DocStoreIn::OpenFailed`
  → `Mut::SessionOpenFailed` removes the tab and marks the entry `Failed`.
  If this was the only pending entry, `resume_complete_s` still fires and
  phase advances.
- **CLI args with a directory**: `arg_dir = Some(dir)`, no files opened;
  browser reveals the dir (`BrowserReveal(arg_dir)`).
- **Single directory CLI arg + session restore with tabs**: resume restores
  prior tabs; `BrowserReveal(arg_dir)` fires in `resume_complete_s`. Tabs
  still activate per `activate_arg_s` only if the user also named a file.
- **Mid-startup quit**: possible (user hits Ctrl-x Ctrl-c during Resuming).
  `session_save` in derived does not require `Running`; it fires on
  `Exiting`. The hard `std::process::exit(0)` in `main.rs` ensures we don't
  hang on unfinished LSP spawns.
- **Suspend during startup**: possible. `process_of::suspend_s` fires
  regardless of prior phase. The redraw bump on Suspended→Running handles
  the return.
- **`--reset-config` flag**: exits before the runtime ever starts. Rewrites
  `keys.toml`, `theme.toml` with baked defaults; deletes `db.sqlite`.

## Error paths

- **`db::open_db` fails**: logged warning; `session` is `None`; editor
  proceeds as if fresh. Undo persistence is disabled for this session.
- **Flock already held (non-primary)**: no DB load, `session: None`, no
  session save on quit. Cross-instance sync still works (reads SQLite for
  notify-driven diffs).
- **Watcher registration fails**: silent (uses inert watcher stub). Editor
  runs without external-change detection.
- **Terminal setup fails (`enable_raw_mode` etc.)**: errors are `.ok()`-ed.
  UX degrades to cooked-mode quirks but the editor still runs.
- **Panic during startup**: no save happens. SQLite retains whatever was
  flushed in the previous clean run. Next launch still restores that prior
  session.
- **`find_git_root` hits a permissioned ancestor**: `is_dir()` check returns
  false, loop continues; worst case falls back to `start_dir`.
- **Resume buffer load fails for all session files**: all marked `Failed`;
  `resume_complete_s` fires with all Failed; phase advances to Running with
  no tabs (unless a CLI arg was also given).

## Gaps

- `[unclear — hot-reload of keys.toml / theme.toml]` — `ConfigFileOut::Persist`
  is wired but per `POST-REWRITE-REVIEW.md:48-52` has no effect; runtime
  config edits are no-ops. Whether this should work in the rewrite is open.
- `[unclear — what happens if the session file path's parent no longer
  exists]` — `DocStoreIn::OpenFailed` fires for missing files, but the
  cleanup semantics for a whole-subtree deletion (and its impact on browser
  `expanded_dirs` restore) are not explicitly tested.
- `[unclear — standalone mode crash behavior]` — there is no session DB so
  there's nothing to recover. Not a defect, but worth stating.
- `[unclear — interaction between multi-instance and crash recovery]` — if
  the primary crashes while a secondary is running, does the secondary
  automatically promote? No code path rechecks flock mid-session; primary
  designation is once-at-startup.
