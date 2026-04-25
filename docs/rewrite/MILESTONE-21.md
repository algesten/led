# Milestone 21 — Persistence (session)

After M21, exiting led with `Ctrl-X Ctrl-C` writes the open
tabs + cursors + scroll positions into a SQLite session row,
and the next launch in the same workspace restores them. The
same pending-cursor primitive M21 introduces also unblocks
Alt-Enter into library code and Alt-./Alt-, into unopened
files — both deferred from earlier milestones.

This is the milestone the rewrite has been deferring to a lot.
Once it lands, several "skip silently" branches become real.

Prerequisite reading:

1. `docs/spec/persistence.md` — full file. Authoritative
   reference for what gets persisted and the recovery flow.
2. `docs/spec/lifecycle.md` § "Quit" — the gate condition
   (`session.saved || !needs_save`) M20 left as a placeholder.
3. Legacy `led/crates/workspace/src/db.rs` (832 LOC) and
   `led/crates/workspace/src/lib.rs` (545 LOC) — the
   reference port. Schema, save / load, flock primary
   acquisition, undo persistence.
4. `MILESTONE-20.md` § "Out — Session flush on Quit" —
   confirms M21 fills the placeholder.
5. `goldens/scenarios/driver_events/workspace/*` — the
   target scenarios.

---

## Goal

```
$ cd ~/project && led src/main.rs src/lib.rs
# Edit, scroll, switch tabs, Ctrl-X Ctrl-C.
$ cd ~/project && led
# Both tabs reopen at the cursor positions and scroll
# offsets where you left them. Active tab is whatever
# was active before quit.
```

Plus three smaller wins:

```
# In a Rust file, position cursor on `vec![…]`, Alt-Enter:
# rust-analyzer's std-library macro location loads in a
# new tab, cursor lands on the def, scroll recenters.

# A diagnostic exists on a file you don't have open:
# Alt-. opens that file, applies the cursor at the
# diagnostic's line/col, scrolls to centre.
```

## Scope

### In

- **`state-session` crate** — new workspace member.

  ```rust
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct SessionState {
      /// `true` once a Save round-trip completed, OR there's
      /// nothing to save. The Quit gate consults this.
      pub saved: bool,
      /// Set on startup once the primary flock acquisition
      /// settles. `false` means another led owns this
      /// workspace; we run read-only and don't persist on quit.
      pub primary: bool,
      /// The persisted SessionData we restored on startup
      /// (or built from the live state on quit). Lives here
      /// so `derived` can diff against last_saved without
      /// re-reading SQLite.
      pub last_saved: Option<SessionData>,
  }

  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct SessionData {
      pub active_tab_idx: Option<usize>,
      pub show_side_panel: bool,
      pub tabs: Vec<SessionTab>,
  }

  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct SessionTab {
      pub path: CanonPath,
      pub cursor: Cursor,
      pub scroll: Scroll,
  }
  ```

- **`Tab.pending_cursor: Option<Cursor>`** + **`Tab.pending_scroll: Option<Scroll>`**
  — set when a tab is opened with a target location that
  needs a load before the cursor can apply (session restore,
  goto-def into unopened file, issue-nav into unopened
  file). Cleared when applied.

- **`Phase::Resuming`** — new variant on `state-lifecycle`'s
  `Phase` enum, between `Starting` and `Running`. Active
  while at least one restored buffer is still materializing.
  Render runs in `Resuming` (so the user sees the placeholder
  blank body), but commands that require a fully-settled
  buffer (LSP requests, format-on-save) gate on `is_running()`.

- **`driver-session/{core,native}` crates** — new workspace
  members.

  ```rust
  // core
  #[derive(Debug, Clone)]
  pub enum SessionCmd {
      /// Open the SQLite DB, attempt primary flock, and load
      /// the restored session. Carries the workspace root +
      /// config dir.
      Init { root: CanonPath, config_dir: CanonPath },
      /// Persist `SessionData` into the workspace's row.
      Save { data: SessionData },
      /// Graceful close — flush any pending writes, drop the
      /// flock.
      Shutdown,
  }

  #[derive(Debug, Clone)]
  pub enum SessionEvent {
      /// First message after `Init`. `restored` is `None` for
      /// non-primary instances or when no prior session
      /// exists.
      Restored {
          primary: bool,
          restored: Option<SessionData>,
      },
      /// `Save` completed.
      Saved,
      /// Save / load failed with a non-fatal error.
      Failed { message: String },
  }
  ```

  Native: `rusqlite` (bundled feature already in workspace
  deps) + `fs2`-equivalent flock via direct `libc::flock`.
  Schema mirrors legacy:

  ```sql
  CREATE TABLE workspaces (
      root_path       TEXT PRIMARY KEY,
      active_tab      INTEGER NOT NULL DEFAULT 0,
      show_side_panel INTEGER NOT NULL DEFAULT 1
  );
  CREATE TABLE buffers (
      root_path       TEXT NOT NULL REFERENCES workspaces(root_path) ON DELETE CASCADE,
      file_path       TEXT NOT NULL,
      tab_order       INTEGER NOT NULL,
      cursor_row      INTEGER NOT NULL DEFAULT 0,
      cursor_col      INTEGER NOT NULL DEFAULT 0,
      scroll_row      INTEGER NOT NULL DEFAULT 0,
      scroll_sub_line INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (root_path, file_path)
  );
  ```

  No `session_kv` and no `undo_*` tables for M21 — both
  defer to follow-ups (browser state restore + undo
  persistence).

- **Primary flock** — at `<config>/primary/<hash(root)>`,
  opened with `LOCK_EX | LOCK_NB`. Failure → run as
  secondary (`primary = false`); session is read-only and we
  don't write on quit.

- **Pending-cursor plumbing** — when the load completion
  ingest seeds an `EditedBuffer` for a path, check every
  open tab for that path with `pending_cursor` set and
  apply (clamping to rope bounds). Same hook handles all
  three triggers (session restore, goto-def, issue-nav).

- **Quit gate** — M20 left:

  ```rust
  Command::Quit => {
      lifecycle.phase = Phase::Exiting;
      quit = true;
      break;
  }
  ```

  M21 changes to:

  ```rust
  Command::Quit => {
      lifecycle.phase = Phase::Exiting;
      // Don't break here — the next iteration's execute
      // dispatches SaveSession; the iteration after that
      // checks session.saved.
  }
  // … later, after the execute phase:
  if lifecycle.phase == Phase::Exiting
      && (session.saved || !session.primary)
  {
      break Ok(());
  }
  ```

- **Alt-Enter / Alt-./Alt-, into unopened files** — the
  silent-skip branches in `apply_goto_definition` and
  `nav_issue` are removed. Now route through
  `dispatch::open_or_focus_tab`, stash `pending_cursor` on
  the new tab, and the load-completion hook applies it.
  Recenter scroll the same way the in-memory paths do.

### Out

Per the roadmap and the legacy spec, deferred from M21:

- **Undo persistence** — legacy carries `buffer_undo_state` +
  `undo_entries` in the same DB; M21 leaves `eb.history`
  in-memory only (already correct post-format-on-save fix).
  A follow-up adds the schema + flush-debounce machinery.
- **Cross-instance notify files** — couples to M26's file
  watcher, deferred until then.
- **`session_kv` blob** — browser state, jump list,
  isearch state, etc. Restore-on-startup just doesn't
  reach those — user sees a fresh browser. Adding the kv
  table is a small follow-up.
- **`Phase::Resuming` gating non-trivial commands** — for
  M21 we just transition through the phase; nothing
  meaningful gates on it yet (LSP requests already gate on
  `lsp_init_sent`). Later milestones can tighten.
- **External `git checkout` triggering rescan** — M26.

## Key design decisions

### D1 — `Init` carries both `root` and `config_dir`

Legacy reads them from a global startup struct. The rewrite
keeps drivers explicit about their inputs; passing both into
`SessionCmd::Init` means the driver doesn't reach into
`Atoms` and is testable in isolation.

### D2 — Pending cursor + scroll are tab-local

Storing on `Tab` (not in a side-table indexed by path) keeps
the load-completion hook simple — just walk `tabs.open`. A
buffer can have multiple tabs (preview + real, multi-pane
when M27 lands), each carrying its own pending position.

### D3 — Restore happens through the same Tab.pending mechanism

When `SessionEvent::Restored` arrives, dispatch creates one
`Tab` per restored buffer (using `open_or_focus_tab`) and
sets `pending_cursor` + `pending_scroll`. The load
completion path then applies them buffer-by-buffer. No
special "restore" code path — same mechanism as goto-def.

### D4 — Standalone (`--no-workspace`) skips the session driver

`SessionCmd::Init` is the only outbound command; if the CLI
sets `no_workspace`, the runtime never dispatches it. The
session driver thread spawns regardless (the cost is one
idle channel) but never receives a command. `session.saved`
defaults to `true` for standalone mode so the Quit gate
clears immediately.

### D5 — Schema migrations are drop-and-recreate

Same as legacy: `user_version` mismatch wipes the DB and
recreates. M21 starts at `SCHEMA_VERSION = 1` (a fresh
schema, not the legacy v3 — fewer tables for now). Future
milestones bump the version and the rewrite handles its own
migration.

### D6 — Phase::Resuming is observable but not gating

Legacy hides the first paint until restore completes. The
rewrite renders during Resuming (the buffers show as blank
loading frames already, post-recent-fix), so there's no
flicker to suppress. Phase::Resuming is bookkeeping —
"we're between session-load and all-tabs-materialized". The
status bar memo can highlight it later if useful.

### D7 — Save fires on the Exiting transition only

Not on every dirty-buffer event. Legacy debounces to a
period of inactivity; M21 takes the simpler "save on quit"
path. Cross-instance sync, autosave, etc. are later.

### D8 — Primary flock is a soft signal

Failure to acquire isn't fatal — secondary instance still
runs, just doesn't restore session and doesn't save on quit.
The flock file at `<config>/primary/<hash(root)>` releases
when the primary process dies (kernel cleanup), so a crash
doesn't permanently lock out future starts.

## Types

### `state-session` (new crate)

```rust
use led_core::CanonPath;
use led_state_tabs::{Cursor, Scroll};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    pub saved: bool,
    pub primary: bool,
    pub last_saved: Option<SessionData>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionData {
    pub active_tab_idx: Option<usize>,
    pub show_side_panel: bool,
    pub tabs: Vec<SessionTab>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTab {
    pub path: CanonPath,
    pub cursor: Cursor,
    pub scroll: Scroll,
}
```

### `state-tabs` additions

```rust
pub struct Tab {
    // existing fields …
    pub pending_cursor: Option<Cursor>,
    pub pending_scroll: Option<Scroll>,
}
```

### `state-lifecycle` additions

```rust
pub enum Phase {
    Starting,
    Resuming,    // NEW
    Running,
    Suspended,
    Exiting,
}
```

### `driver-session/core` (new crate)

`SessionCmd`, `SessionEvent`, `SessionDriver` handle, `Trace`.

### `driver-session/native` (new crate)

`SessionNative` lifetime marker + `spawn(...)`. SQLite
worker on `std::thread`. Schema + save/load helpers ported
from `led/crates/workspace/src/db.rs`.

## Crate changes

```
crates/
  state-session/             NEW — SessionState + SessionData + SessionTab
  state-tabs/                + Tab.pending_cursor/scroll
  state-lifecycle/           + Phase::Resuming
  driver-session/core/       NEW — SessionCmd / SessionEvent / Trait / Handle
  driver-session/native/     NEW — rusqlite worker + flock
  runtime/src/
    lib.rs                   + Drivers.session, Atoms.session, ingest
                              SessionEvent, dispatch SessionCmd::Save on
                              Exiting, hold quit until session.saved.
                              Apply pending_cursor on file_completions.
    dispatch/save.rs / mod.rs Quit arm no longer breaks immediately.
    dispatch/nav.rs          next_issue / prev_issue route unopened
                              targets through open_or_focus_tab + pending.
    (lib.rs)                 apply_goto_definition same.
    trace.rs                 + session_load_start / session_save_start /
                              session_save_done. dispatched.snap names:
                              `WorkspaceLoad`, `WorkspaceSaveSession`,
                              `WorkspaceSessionSaved`.
```

New workspace members: `led-state-session`,
`led-driver-session-core`, `led-driver-session-native`.

## Testing

### `state-session`
- `SessionState::default()` — saved=false, primary=false.
- Round-trip a `SessionData` through Clone/Eq.

### `driver-session/core`
- `SessionDriver::execute` forwards a batch.
- `SessionDriver::process` drains events.

### `driver-session/native`
- `Init` on a tempdir creates `db.sqlite`, returns
  `Restored { primary: true, restored: None }` for a fresh
  workspace.
- `Save` then `Init` on the same tempdir restores the saved
  state exactly.
- A second `Init` while the first holds the flock returns
  `Restored { primary: false, .. }`.

### `runtime` integration
- Pending-cursor applies on load completion (clamped).
- Phase::Starting → Resuming on Init dispatch.
- Phase::Resuming → Running once all restored tabs
  materialize.
- Quit holds until session.saved (primary path).
- Quit fires immediately when not primary or standalone.
- Goto-def into unopened file: opens tab, applies cursor on
  load complete, recenters.
- Next-issue into unopened file: same.

Expected: +25 tests.

## Done criteria

- All existing tests pass.
- New tests green.
- Clippy: net delta ≤ +2 from post-M20a.
- Interactive smoke:
  - Open `cd led-rewrite && cargo run -p led -- src/main.rs src/lib.rs`,
    edit, scroll, Ctrl-X Ctrl-C. Re-launch — both files
    reopen at the same cursors.
  - Same project, second terminal: `cargo run -p led` — runs
    as secondary; quit doesn't overwrite primary's session.
  - Alt-Enter on a `vec!` macro — opens the std lib file
    with cursor on the def.
- Goldens:
  - `driver_events/workspace/session_saved` — green.
  - `driver_events/workspace/session_restored_none` — green.
  - `keybindings/confirm_kill/*` — closer (the
    `WorkspaceFlushUndo` line still missing, that's the
    undo-persistence follow-up).
  - `features/persistence/*` — best-effort; some need
    undo persistence.

## Growth-path hooks

- **Undo persistence** — schema gains `buffer_undo_state` +
  `undo_entries` tables. `History::flush` runs after every
  edit-burst debounce; restore on Init populates each
  buffer's `History.past`. The `WorkspaceClearUndo` trace
  line finally binds to a real `DELETE FROM undo_entries
  WHERE …`.
- **`session_kv` for browser / jump list / isearch state**.
- **Cross-instance notify** — M26 file watcher subscribes to
  `<config>/notify/<hash(root)>/`; primary writes one notify
  file per state-change, secondaries pick them up.
- **Reveal-active-file on session restore** — once browser
  state restores, the active tab's path can drive the
  initial expand-ancestors pass.
- **Mid-startup quit safety** — already handled (the Quit
  arm sets Exiting regardless of phase).
