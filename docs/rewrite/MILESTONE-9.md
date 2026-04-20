# Milestone 9 — UI chrome: status bar, gutter, alerts

Ninth vertical slice. After M9 the editor looks like an editor:
a three-region layout (body + tab bar + status bar), a reserved
2-column gutter for future git/lsp marks, tilde rows for
past-EOF, a default status line (dirty dot + cursor position),
and a pluggable alert surface that powers both transient success
notices and the confirm-kill prompt.

Prerequisite reading:

1. `docs/spec/ui-chrome.md` — every section (status bar, gutter,
   tab bar, alerts, confirm-kill). This is the authoritative
   reference for layout + precedence.
2. `MILESTONE-7.md` § "D2 — kill / yank interaction with the
   existing mark" — kill_buffer on dirty currently no-ops; M9
   replaces that with a confirm prompt.
3. `goldens/scenarios/smoke/{open_empty_file,type_and_save}/frame.snap`
   — the chrome shape we match (tab bar at row N-2, status bar
   at row N-1, gutter 2 cols wide).

---

## Goal

```
$ cargo run -p led -- draft.txt
# Initial frame (120×40):
#   row 0..37  : body (2-col gutter, tilde rows past EOF)
#   row 38     : tab bar ` draft.txt `
#   row 39     : status bar `                      ... L1:C1 `
# Type "hello"
#   tab bar becomes ` ●draft.txt `
#   status bar becomes `  ●  ... L1:C6 `
# Ctrl-x Ctrl-s
#   status bar flashes ` Saved draft.txt  ... L1:C6 ` for 2s
#   tab bar snaps back to ` draft.txt ` (saved)
# Ctrl-x k  (kill active)
#   clean → buffer closes immediately
#   dirty → status bar shows ` Kill buffer 'draft.txt'? (y/N) `
#           y/Y       → kill
#           any other → dismiss prompt and run that key normally
```

## Scope

### In

- **Three-region layout** at the `Frame` level:
  - `body` (existing) now carries gutter-and-content rows.
  - `tab_bar` (existing) is painted at `rows - 2`, not `row 0`.
  - `status_bar` (NEW) painted at `rows - 1`, always full width.
- **2-column gutter** on the left of every body row. Empty for
  M9 (future: git marks col 1, diagnostic marks col 2). The
  gutter is **not** a separate region — it is a per-line prefix
  of the `BodyModel::Content` lines so one paint pass still
  covers the whole body.
- **Past-EOF tilde rows**: any body row past the last buffer
  line renders `~ ` in the gutter area and nothing after. The
  `BodyModel::Content::lines` vec grows to always be
  `body_rows` long; entries past EOF are a sentinel the painter
  recognises.
- **`state-alerts` crate** — new workspace member. Owns:
  ```rust
  pub struct AlertState {
      pub info: Option<String>,
      pub info_expires_at: Option<Instant>,
      pub warns: Vec<(String, String)>, // (key, msg)
      pub confirm_kill: Option<TabId>,
  }
  ```
  - `info` / `info_expires_at`: transient, timer-cleared.
  - `warns`: keyed, persistent until `clear_warn(key)`.
  - `confirm_kill`: set while the dirty-buffer prompt is live.
- **Info alert TTL**: 2 seconds. On every tick the runtime
  clears `info` + `info_expires_at` if `Instant::now() >
  info_expires_at`. No driver: the check is one `Instant::now()`
  compare per tick. Matches legacy `alert_clear` timer duration.
- **Status bar content** via a new memo `status_bar_model`:
  - Priority 1 — **confirm-kill prompt** (if `confirm_kill` set):
    left = `" Kill buffer '<name>'? (y/N) "`, no position on
    right. Rendered with the default (non-warn) style.
  - Priority 2 — **info alert** (if `info` set): left =
    `" <info>"`, right = position.
  - Priority 3 — **warn** (if `warns` non-empty): left =
    `" <first-warn-msg>"`, right = position, `is_warn = true`
    (painter uses red/white bold background).
  - Priority 4 — **default**: left = `"  ●"` if active tab
    dirty else `"   "`, right = `"L<row>:C<col> "` (1-indexed,
    trailing space).
- **Tab bar `●` prefix for dirty** (replaces M4's `*` prefix):
  per label, format is `<space><dirty-or-space><name><space>`
  where dirty-or-space = `●` when dirty, else an extra space.
- **Kill active tab on dirty buffer** flow:
  1. `Ctrl-x k` when `active`'s buffer is dirty →
     - `alerts.confirm_kill = Some(active_id)`.
     - Does NOT remove the tab yet.
  2. Next keystroke:
     - `y` / `Y` → force-kill (remove tab, drop edit buffer,
       clear prompt). Does not run any other command.
     - Anything else → clear `confirm_kill`, then dispatch the
       keystroke normally (so `Ctrl-g`/`Esc` just cancels;
       arrow keys cancel + move; letters cancel + insert).
- **Saved alert**: when a save completes successfully (i.e.
  `FileWriteDriver` reports `Ok`), the runtime sets an
  `info = format!("Saved {basename}")` with a 2s TTL. Matches
  legacy.

### Out

Per `ROADMAP.md`:

- **Side panel / file browser** → M11. Without it, M9 does
  not split horizontally; body takes the full terminal width.
  Most `features/*` goldens will still fail because their
  snapshots include the side panel. M9 focuses on the chrome
  that's scheduled for it; the golden counter starts moving
  materially at M11.
- **Branch / PR / LSP blocks** in the default status line →
  M16 (LSP) / M19 (git) / M20 (gh pr). M9's default-left is
  just the dirty dot.
- **Macro recording indicator** → M14.
- **Find-file / isearch prompt modes** (status-bar overrides 1
  and 2 in the spec) → M12 / M13.
- **Ruler** (thin vertical mark at col 110) → M15. The ruler's
  style is themeable, and M15 is the milestone that grows the
  painter to per-cell styles + introduces the theme parser.
- **Theming** (`theme.status_bar.style`, `theme.git.gutter_*`,
  `theme.diagnostics`, …) → M15. For M9 the painter uses
  crossterm basic colors only; `status_bar_model` reports
  `is_warn: bool` and the painter picks white-on-red-bold by
  hand. The semantic signal stays the same when themes land.
- **Per-overlay rendering**: diagnostic hover → M16,
  completion popup → M17, code-actions / rename → M18. M9
  lays no groundwork for these beyond the region split.

## Key design decisions

### D1 — Alerts are a state source, not a driver

No async backing. `AlertState` lives in `crates/state-alerts/`
alongside `state-tabs` / `state-kill-ring`. Dispatch writes to
it directly (setting `info`, `warns`, `confirm_kill`); the
runtime's tick loop clears expired `info` before the query
phase.

### D2 — Confirm-kill is a bit in `AlertState`, not a separate source

Two reasons. First, the prompt *displays* through the alert
surface — co-locating the state keeps the status-bar memo
reading from one source. Second, there is at most one
in-flight confirm-kill (it blocks the tab it targets), so a
single `Option<TabId>` suffices. No separate "dialog atom"
needed.

### D3 — Tab bar at `rows - 2`, status bar at `rows - 1`

The spec notes this as `[unclear]`; the goldens pin it. Legacy
led constructs the layout with `Constraint::Min(1)` (body) then
`Constraint::Length(1)` (tab bar), which ratatui interprets as
"body on top, tab bar at the bottom of the editor area." Then
the bottom-most row of the whole terminal is the status bar.

Concretely, for rows = 40:
- Body: rows 0..=37 (38 rows).
- Tab bar: row 38.
- Status bar: row 39.

### D4 — Gutter is a per-line prefix, not a separate column region

Painting the gutter separately would need two passes over the
body (one for gutter, one for text) or extra positioning state
in the painter. Since the gutter content is derivable per-line
from the same sources as the text, the memo prepends `"  "`
(2 spaces) to each body line, and past-EOF lines render `"~ "`
(tilde + space). One loop in the painter covers everything.

The painter still gets `dims.cols`; the memo truncates each
line to `cols - 2` *before* prepending the 2-col gutter, so
the final string length equals `cols`.

### D5 — Past-EOF rendered as `~ ` sentinel in `lines`

Today `BodyModel::Content::lines` is shorter than `body_rows`
when the buffer ends early. The painter fills the remainder
with blanks. For M9 we make `lines` always `body_rows` long so
the painter has a uniform loop; past-EOF entries start with
`"~"` (tilde in the first gutter col).

Concretely the memo now produces one of these per body row:
- A real buffer line (gutter `"  "` + content).
- A past-EOF marker (`"~ "` only; remaining cells left to the
  painter's clear-to-EOL).

The painter's job is unchanged from M8 aside from looping
`body_rows` times always.

### D6 — `status_bar_model` is a sibling memo to `tab_bar_model`

```rust
#[drv::memo(single)]
pub fn status_bar_model<'a, ...>(
    alerts: AlertsInput<'a>,
    tabs:   TabsActiveInput<'b>,
    edits:  EditedBuffersInput<'c>,
    dims:   Dims,
) -> StatusBarModel { ... }
```

Emits:

```rust
pub struct StatusBarModel {
    pub left:    Arc<str>,
    pub right:   Arc<str>,
    pub is_warn: bool,
}
```

`left` and `right` are `Arc<str>` so cache-hit clones are a
pointer copy. The painter writes `left` from col 0, clears the
gap, then writes `right` right-aligned.

### D7 — Saved-alert is set by the runtime's write-completion branch

The save happens across ticks: `FileWriteDriver::execute` is
fire-and-forget; its completion round-trips through
`FileWriteDriver::process` during the next ingest phase. That
branch already exists (it round-trips saved ropes into
`BufferStore`). M9 extends it: on a successful completion, set
`alerts.info = Some(format!("Saved {basename}"))` with
`info_expires_at = Instant::now() + 2s`. Errors already trace;
M9 additionally sets a `warns` entry keyed by the path so the
user sees a persistent warning.

### D8 — Info-expiry check is unconditional per tick

```rust
if let Some(exp) = alerts.info_expires_at
    && Instant::now() >= exp {
    alerts.info = None;
    alerts.info_expires_at = None;
}
```

At most one `Instant::now()` per tick; cheap. The check runs
*before* the query phase so the status-bar memo's input is
already up-to-date for this tick's render.

All Info alerts share the same 2 s TTL (legacy matches).
No per-alert TTL control: the call sites don't need it, and
adding a `Duration` arg to every `set_info` call would
complicate the dispatch sites without solving a real problem.
If a future feature needs a different duration, extend
`set_info` then — the shape already takes `ttl`, just no caller
varies it yet.

### D9 — Confirm-kill gates the next keystroke, not the whole tick

`Dispatcher::dispatch_key` grows an early branch:

```rust
if let Some(target) = self.alerts.confirm_kill {
    match k {
        Char('y') | Char('Y') => {
            force_kill(self.tabs, self.edits, target);
            self.alerts.confirm_kill = None;
            return DispatchOutcome::Continue;
        }
        _ => {
            self.alerts.confirm_kill = None;
            // fall through to normal resolve
        }
    }
}
// ... resolve_command / run_command as today
```

This matches legacy semantics: the prompt is dismiss-on-first-
keystroke, and the dismissing key still runs if it has a
binding (so `Esc` clears the mark *and* the prompt, a letter
both inserts and dismisses).

Subtle: `y`/`Y` must not insert themselves when the prompt is
live — so we `return` after force-kill, not fall through.

### D10 — Tab bar `●` prefix, not `*` prefix

The M4 `*foo.rs` shape was a stand-in. Goldens pin `●` as the
dirty marker (see `features/save_flows/save_then_edit_again`).
M9 also drops the M7 mark-indicator suffix (` ●` after the
active label): once alerts + the status bar exist, the user
gets visible feedback on `Ctrl-Space` via the status-bar
position column (the cursor still blinks; M14 may add explicit
region highlighting). The stand-in is no longer paying rent.

### D11 — Dims helpers don't grow yet

Legacy's `Dimensions` struct encodes `side_panel_visible`,
`gutter_width`, `tab_bar_height`, etc. For M9 we keep `Dims`
minimal (cols, rows) and compute everything in the memos.
When M11 introduces the side panel, we'll factor out a
`Layout` struct.

## Types

### `state-alerts` (new crate)

```rust
use std::time::Instant;
use led_state_tabs::TabId;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlertState {
    pub info:              Option<String>,
    pub info_expires_at:   Option<Instant>,
    pub warns:             Vec<(String, String)>,
    pub confirm_kill:      Option<TabId>,
}

impl AlertState {
    pub fn set_info(&mut self, msg: String, ttl: Duration);
    pub fn clear_info(&mut self);
    pub fn expire_info(&mut self, now: Instant); // idempotent
    pub fn set_warn(&mut self, key: String, msg: String);
    pub fn clear_warn(&mut self, key: &str);
    pub fn first_warn(&self) -> Option<&(String, String)>;
}
```

`PartialEq` on `Instant` is fine — `Instant` implements it.
Drv cache invalidation still works via pointer equality on the
`Vec`/`Option` fields.

### Runtime grows

```rust
// runtime/src/query.rs
#[drv::input]
pub struct AlertsInput<'a> {
    pub info:              &'a Option<String>,
    pub warns:             &'a Vec<(String, String)>,
    pub confirm_kill:      &'a Option<TabId>,
}

pub struct StatusBarModel {
    pub left:    Arc<str>,
    pub right:   Arc<str>,
    pub is_warn: bool,
}

#[drv::memo(single)]
pub fn status_bar_model<'a, 'b, 'c>(
    alerts: AlertsInput<'a>,
    tabs:   TabsActiveInput<'b>,
    edits:  EditedBuffersInput<'c>,
    dims:   Dims,
) -> StatusBarModel;
```

`Frame` grows a `status_bar: StatusBarModel` field.
`render_frame` composes it.

### Dispatch

`Dispatcher<'a>` grows `alerts: &'a mut AlertState`. The
confirm-kill branch lives at the top of `dispatch_key`. The
`KillBuffer` arm in `run_command` switches from a no-op on
dirty to "set `confirm_kill = Some(active_id)`".

### Painter

`paint` grows a `paint_status_bar` step:

```rust
fn paint_status_bar(
    s: &StatusBarModel, dims: Dims, out: &mut impl Write,
) -> io::Result<()>;
```

Position: `cursor::MoveTo(0, dims.rows - 1)`.
Content: `left`, padding, `right` right-aligned.
Style: if `is_warn`, wrap in red-bg / white-fg / bold.

`paint_body` loops `body_rows = dims.rows - 2` times (tab bar
takes one, status bar takes one) rather than today's
`rows - 1`.

`paint_tab_bar` moves to `cursor::MoveTo(0, dims.rows - 2)`.

## Crate changes

```
crates/
  state-alerts/        NEW — AlertState + tests
  runtime/src/
    query.rs           + AlertsInput, StatusBarModel,
                         status_bar_model; render_frame
                         composes status_bar; body_model
                         prepends gutter; past-EOF sentinel
    dispatch/mod.rs    + Dispatcher.alerts, confirm-kill
                         gate in dispatch_key, KillBuffer
                         arm updated
    dispatch/tabs.rs   + force_kill split out from
                         kill_active; kill_active on dirty
                         sets confirm_kill
    lib.rs             + AlertState threaded into run();
                         expire_info() before query phase;
                         save-completion sets info; save
                         error sets warn
  driver-terminal/
    core/src/lib.rs    + StatusBarModel; Frame.status_bar
    native/src/lib.rs  + paint_status_bar; body loop uses
                         rows - 2; tab bar at rows - 2
```

New workspace member: `led-state-alerts`.

## Testing

### `state-alerts`
- `set_info` stores msg + expiry.
- `expire_info` clears when `now >= expires_at`, no-op
  otherwise.
- `set_warn` appends on new key, replaces on existing key.
- `clear_warn` removes by key.
- `first_warn` returns head.
- `confirm_kill` round-trip (set, read, clear).

### `runtime::query::status_bar_model`
- No-tab, no-alert, no-dirty → left = `"   "`, right =
  `"L1:C1 "` (or `"L0:C0 "`? see spec — 1-indexed for human
  display).
- Dirty active → left contains `●`.
- Info set → left = `" <info>"`.
- Warn set, no info → left = `" <warn>"`, `is_warn = true`.
- Warn + info → info wins (transient > persistent).
- Confirm-kill set → left = `" Kill buffer '<name>'? (y/N) "`,
  right empty.

### `runtime::query::render_frame` + `body_model`
- Body shorter than viewport → trailing rows render as
  `~ ` sentinel.
- Content line + 2-col gutter prefix → final string length
  matches viewport cols (no overflow).
- `rows = 40` → tab bar at row 38 (inside `Frame.tab_bar`,
  painter resolves), body is 38 rows tall.

### `runtime::dispatch`
- `KillBuffer` on clean active → tab gone, no prompt.
- `KillBuffer` on dirty active → prompt set, tab still open.
- Prompt + `Char('y')` → force-kill, prompt cleared.
- Prompt + `Char('Y')` → force-kill.
- Prompt + `Char('n')` → prompt cleared; `n` inserts into
  buffer (dismiss + run normal).
- Prompt + `Esc` → prompt cleared; mark cleared (Esc's normal
  effect).

### `runtime::run` (integration)
- Save completion sets info alert with the file basename.
- Info expires after TTL.
- Second save before expiry replaces the existing info.

Expected: +25 tests.

## Done criteria

- All existing tests pass.
- New alerts / chrome / confirm-kill tests pass.
- Clippy unchanged from post-M8 (13).
- Interactive smoke:
  - Open a file at 120×40. Tab bar at row 39 (1-indexed).
    Status bar at row 40. Gutter 2 cols. Tildes past EOF.
  - Type a char. `●` appears on tab bar and status bar.
  - Save. `Saved <name>` flashes in status bar for 2s.
  - Edit, `Ctrl-x k`. Prompt appears. `n` dismisses and
    inserts `n`.
  - Edit, `Ctrl-x k`, `y`. Tab closes without saving.
- Goldens baseline: expected unchanged at 0 / 257 — chrome
  matches body/tab/status but the side panel is still
  missing, so no snapshot will fully equal. M11 is the
  milestone that starts moving the counter.

## Growth-path hooks

- **Side panel** (M11): add a `Layout` struct that carves
  `side_area` + `editor_area` from `dims`. Everything that
  currently uses `dims.cols` switches to `layout.editor.cols`.
- **Ruler** (later): a hardcoded col-110 mark painted after
  body. Stashed as an M-later nice-to-have.
- **Theming** (M15): replace hardcoded paint styles with
  theme lookups. `is_warn` stays as the semantic signal.
- **LSP status in default status line** (M16): extend
  `status_bar_model` to take an `LspInput` with server name,
  busy flag, progress message.
- **Branch / PR tags** (M19 / M20): same pattern —
  `status_bar_model` grows optional `GitInput` / `PrInput`.
- **Warn dead-letter cleanup**: `clear_warn` today requires
  the producer to know the key. A later pass may add a
  `clear_warns_for(path)` for bulk cleanup on tab close.
- **Info timer driver**: if `Instant::now()` polling becomes
  a hot spot (unlikely at a 10ms tick), a tiny timer driver
  could emit `expire_info` events instead. Currently the
  poll is O(1) and allocation-free — no need.
