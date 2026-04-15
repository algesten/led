# ui-chrome

## Summary

This doc covers the screen furniture that wraps the buffer area: the
**status bar** (one row along the bottom), the **gutter** (two fixed
columns on the left of the buffer, one for git change marks, one for
diagnostic severity), the **tab bar** (one row above the buffer), the
**side panel / file browser** (fixed-width column on the left when
visible), **alerts** (info and warn only — there is no error level in
code; displayed in the status bar), and the **confirm-kill dialog** (an
inline status prompt, not a popup).

Overlays (completion popup, code-action picker, rename input, diagnostic
hover, PR comment hover) render above the buffer but are covered in the
relevant feature-area specs (`lsp.md`, `find-file.md`, `search.md`). This
doc references them only where they affect the chrome layout (e.g. the
status bar switches to a prompt while find-file is open).

Out of scope for this doc:

- Alert timing rules (how long each kind displays) — covered in
  `docs/extract/config-keys.md` / `docs/rewrite/GOLDENS-PLAN.md`.
- Buffer content rendering (syntax highlighting, bracket matching,
  selection, inlay hints) — covered in `editing.md` / `syntax.md`.

## Behavior

### Screen layout

```
┌──────────────────────────────────────────────────────────────────────┐
│ tab-bar  row (height 1)                                              │
│                                                                      │
│  ┌───────┬────────────────────────────────────────────────────────┐  │
│  │       │ ▎●                                                     │  │
│  │ side  │  1 │ buffer content (tab_stop = 4)                     │  │
│  │ panel │ ▎●│  ...                                               │  │
│  │ (25)  │    │                                                   │  │
│  │       │    │                              ruler at col 110     │  │
│  └───────┴────┴───────────────────────────────────────────────────┘  │
│                                                                      │
│ status-bar row (height 1)                                            │
└──────────────────────────────────────────────────────────────────────┘

^ tab bar: 1 row, above the buffer, inside the editor area
^ side panel: 25 cols when visible, left edge, with a right border
^ gutter:   2 cols between side panel and buffer text (change mark, diagnostic mark)
            NO line numbers
^ ruler:    thin vertical mark at column 110 (configurable style via editor.ruler,
            position is hardcoded in Dimensions::new)
^ status:   single row at the bottom, full width
```

Layout composition (`crates/ui/src/render.rs:9-50`):

1. Vertical split: top block = `main_area` (everything except the status
   row), bottom = `status_area` (height = `dims.status_bar_height`, always
   1).
2. If `dims.side_panel_visible()` (browser shown AND editor has at least
   `min_editor_width = 25` cols to spare): horizontal split of
   `main_area` into `side_area` (width = `dims.side_width()` = 25) and
   `editor_area`. Otherwise `editor_area` takes the whole `main_area`.
3. Inside `editor_area`: vertical split, top = `buffer_area` (min 1 row),
   bottom = `tab_area` (height = `dims.tab_bar_height`, always 1).
   **Note**: the tab bar is rendered at the *bottom* of the editor area,
   not the top — reading `Constraint::Min(1)` then `Constraint::Length(1)`.
   [unclear — worth confirming visually; goldens should pin this.]
4. Overlays render last, above everything else.

### Status bar

Single row, always visible, full terminal width. Layout logic in
`crates/ui/src/display.rs:840` (`build_status_content`). Rendering in
`render.rs:332` (`render_status_bar`).

The status bar has **one of four modes**, chosen with clear precedence:

1. **Find-file / Save-as prompt** — if `state.find_file.is_some()`:
   `" Find file: <input>"` or `" Save as: <input>"` (full width padded).
   Normal (non-warn) style. Cursor position is at the input; no L:C
   indicator.
2. **Isearch prompt** — if `state.active_buffer.isearch.is_some()`:
   `" <search prompt>"` (full width padded). Normal style. No L:C
   indicator.
3. **Alert** — if `s.alerts.info` is present (transient info), show it.
   Otherwise if `s.alerts.warn()` (persistent first-arrived warn),
   show it with a warn style (white-on-red, bold).
4. **Default** — `" {branch}{dirty-dot}{pr-tag}{lsp-status}" ... "L{row}:C{col} "`
   right-aligned position.

The default-mode components:

| Component | Source | Shown when | Format |
|---|---|---|---|
| `branch` | `AppState.git.branch` | git workspace detected | plain name, e.g. `main` |
| dirty dot | active buffer's `is_dirty` | any unsaved change | `" \u{25cf}"` (●) |
| PR tag | `AppState.git.pr` | PR state known | `(PR #N)`, `(PR #N, merged)`, or `(PR #N, closed)` |
| LSP name + spinner + detail | `lsp.server_name`, `lsp.busy`, `lsp.progress` | LSP active | `"  {spinner} {name}  {spinner2} {detail}"` — spinner chars from a 10-frame braille set, frame chosen by wall-clock ms |
| macro-recording indicator | `kbd_macro.recording` | replaces the default-left when recording | `" Defining kbd macro..."` |
| position | cursor row/col | always | `L<row+1>:C<col+1> ` — right-aligned, trailing space |

The LSP progress comes from `lsp.progress.title` + optional `message`.
The spinner's tick is derived from wall-clock time, so status re-renders
don't need to be scheduled — every render samples the current frame.

### Alerts — `AlertState` (`crates/state/src/lib.rs:1648`)

Two levels in code: `Alert::Info` and `Alert::Warn`. **There is no
`Error` variant** (contrary to any doc that mentions three levels). If
a spec mentions `error`, it's either referring to terminology from
goldens plans or conflating diagnostic severity with alert severity.

- `AlertState.info: Option<String>` — single slot. Newer `Info` alerts
  replace the previous one. Clearing is driven by a timer:
  `alert_timer` in `derived.rs:287-292` schedules a `"alert_clear"`
  timer whenever `alerts.has_alert()` is true. The timer's duration is
  out of scope for this doc; see extracts.
- `AlertState.warns: Vec<(key, message)>` — keyed list. `set_warn(key,
  msg)` either replaces the entry with that key or appends a new
  (key, message) pair. `clear_warn(key)` removes by key. `warn()`
  returns the *first-arrived* message (the head), so older warns take
  priority over newer ones. Warns are **persistent** — no timer
  clears them. They remain until the producer calls `clear_warn`.

Display interaction (status bar):

- If `info.is_some()`: show info, `is_warn = false`, rendered with the
  theme's `status_bar.style`.
- Else if `warn().is_some()`: show warn, `is_warn = true`, rendered
  with a hardcoded **bg Red / fg White / bold** style (note: this is
  not themeable today; see `render.rs:338-345`).
- Else: normal status line.

"Until-ack" vs "timed":

- `Info`: **timed**. Cleared by the `alert_clear` timer.
- `Warn`: **until-ack**. Only the producer clearing the specific key
  can remove it. The user cannot dismiss a warn interactively —
  `Esc`/`Ctrl-g` do not affect alerts.

### Gutter

Exactly **two columns** wide (`dims.gutter_width = 2`). Between the
side panel (if visible) and the buffer text. Built per-line in
`display.rs:322-366`. Line numbers are **not rendered** — only the two
status markers.

| Col | Content | Source |
|---|---|---|
| 1 | Git change mark — a vertical bar (`\u{258E}` `▎`) styled by the highest-precedence category (`IssueCategory::Unstaged`, `StagedModified`, `StagedNew`, `PrComment`, `PrDiff`). Blank if no category. | `git::best_category_at`, `AppState.git.line_annotations` |
| 2 | Diagnostic severity mark — a filled circle (`\u{25CF}` `●`) styled for the most-severe `DiagnosticSeverity` on the line. Only `Error` and `Warning` get a mark; `Info` and `Hint` do **not** — they are blank. | `AppState.lsp.diagnostics` filtered to the cursor's buffer |

Both marks are drawn only on the *first chunk* of a line (soft-wrap
sub-lines repeat the blanks). Style resolution goes through
`category_style` / the diagnostics theme.

Note: the line gutter differs from the **file browser status column**,
which uses a letter (or `•` for directories) — see below.

### Tab bar

One row above the buffer (or the bottom of the editor area depending on
layout order — see note in "Screen layout"). Renders each tab as a
labeled segment separated by one-space gaps, starting at `area.x +
gutter_width - 1` (`render.rs:389`). Truncates when it would overflow
the editor width — no horizontal scrolling.

Tab styles come from `theme.tabs`:

| Tab kind | Theme key |
|---|---|
| Active regular | `tabs.active` |
| Inactive regular | `tabs.inactive` |
| Active preview | `tabs.preview_active` |
| Inactive preview | `tabs.preview_inactive` |

Preview tabs are the "opened by single-click / selected in browser"
convention — not a separate UI, just different styling. See `buffers.md`
for the preview-tab semantics.

### Side panel / file browser

25 cols wide when `side_panel_visible()`. Rendered in
`render.rs:352-365`:

- A right border drawn via `Block::default().borders(Borders::RIGHT)`
  styled from `theme.browser.border`.
- Background style from `theme.browser.selected_unfocused_style` (via
  `LayoutInfo.side_bg_style`).
- Content: a list of `Line`s built by `display.rs:1339`
  (`build_browser_lines`) from `AppState.side_panel` entries.

Per-row format:

```
<indent><icon><name>                  <status-char>
```

- Indent: depth * 1 space.
- Icon: directory collapsed/expanded marker (not covered here).
- Name: file/directory basename.
- Status char (right-aligned 1 col): letter from
  `IssueCategory::browser_letter` for files (`M` modified, `S` staged,
  `U` untracked, etc.); `•` for directories.
- Selected row: different style when panel focused
  (`browser.selected`) vs unfocused (`browser.selected_unfocused`).

Auto-hide: when the editor area would shrink below `min_editor_width =
25`, the side panel is hidden by the dimensions layer even if the user
toggled it on. Toggling back on at small terminals has no effect until
the terminal is resized.

### Confirm-kill dialog

Not a popup. When the user tries to kill a dirty buffer (`Ctrl-x k` on a
modified file), `AppState.confirm_kill` is set to `true`. There is no
separate dialog UI — the prompt is rendered into the status bar as an
`Alert::Info` with text describing the choice (the exact string is
built by the action handler; [unclear — grep didn't surface it here;
confirm in Phase D]).

Key handling while `confirm_kill` is true (`model/mod.rs:270-282`):

- `InsertChar('y')` or `InsertChar('Y')` → `Mut::ForceKillBuffer` (close
  without saving).
- Any other migrated action → `Mut::DismissConfirmKill` (cancel prompt,
  then run the action normally — so pressing `Esc` or a movement key
  dismisses *and* moves).

The prompt is not modal in the overlay sense: other keymap behaviour
continues unchanged. The dismiss-on-first-keystroke behaviour is
intentional and matches common Emacs-style prompts.

### Overlays (pointer to feature specs)

These all render on top of the buffer, above the tab bar and side
panel, below the status bar. Listed here only for layout context.

| Overlay | Driven by | Rendered by |
|---|---|---|
| Completion popup | `lsp.completion` | `render_overlay / OverlayContent::Completion` |
| Code-action picker | `lsp.code_actions` | `OverlayContent::CodeActions` |
| Rename input | `lsp.rename` + `focus == Overlay` | `OverlayContent::Rename` |
| Diagnostic hover | cursor on a line with error/warning diagnostics | `OverlayContent::Diagnostic` — positioned above the cursor when possible, below otherwise |
| PR comment hover | PR review mode | `OverlayContent::PrComment` |

Overlay height caps at 10–15 rows or `area.height / 2`; overlay width
caps at `area.width`. Anchor x/y are computed from the buffer cursor
position in absolute terminal coordinates.

## User flow

1. **Plain editing**: user opens a file. Tab bar shows one tab (active
   style). Gutter is mostly blank. Status bar shows
   `" main  rust-analyzer L1:C1"`. Side panel shows the workspace rooted
   at the file's directory, file highlighted in the `selected_unfocused`
   style (panel isn't focused).
2. **User edits a line**: dirty dot `●` appears in the status bar after
   the branch. Gutter col 1 shows a modified bar on that line (git
   unstaged style). If LSP reports a warning on the line after a pull,
   gutter col 2 shows the warning circle.
3. **Save**: dirty dot disappears. An `Info` alert `saved {path}`
   replaces the default-left until the clear timer fires.
4. **External change**: docstore detects the change, a `Warn` alert
   appears (red bar, persistent). User acknowledges (by action, not by
   pressing a key) — producer calls `clear_warn`.
5. **LSP progress**: spinner appears after the server name with a
   message like `"Indexing  1/50 crates"`. Updates every ~80ms via
   wall-clock tick.
6. **Trigger completion**: popup opens near the cursor. Tab bar and
   status bar unchanged. Arrow keys navigate inside the popup.
7. **`Ctrl-x k` on a dirty file**: `confirm_kill = true`, status bar
   shows the `y/N` prompt. Pressing `y` kills; anything else dismisses.

## State touched

- `AppState.alerts` (`info`, `warns`) — status-bar content.
- `AppState.dims` (`Dimensions`) — drives layout (side panel
  visibility, gutter width, status/tab heights, ruler column).
- `AppState.focus` — affects side-panel highlighting and some chrome
  behavior.
- `AppState.git.{branch, pr, line_annotations}` — status-bar left and
  gutter col 1.
- `AppState.lsp.{server_name, busy, progress, spinner_tick,
  diagnostics}` — status-bar LSP block and gutter col 2.
- `AppState.kbd_macro.recording` — status-bar macro indicator.
- `AppState.active_tab` / `active_buffer` — position indicator, dirty
  dot, isearch prompt.
- `AppState.find_file`, `file_search` — status-bar prompt mode.
- `AppState.confirm_kill` — prompt mode.
- `AppState.tabs` — tab bar content.
- `AppState.side_panel` — side panel content.
- `LayoutInfo` (derived, not stored) — styles and geometry snapshot per
  render.

## Extract index

- Status bar build: `crates/ui/src/display.rs:740-902`
  (`status_inputs`, `build_status_content`, `StatusContent`).
- Status bar render: `crates/ui/src/render.rs:332-350`.
- Gutter render: `crates/ui/src/display.rs:322-366`.
- Tab bar render: `crates/ui/src/render.rs:389-413`.
- Side panel render: `crates/ui/src/render.rs:352-365`.
- Browser line builder: `crates/ui/src/display.rs:1339-1437`.
- Overlay render: `crates/ui/src/render.rs:52-296`.
- Alert state: `crates/state/src/lib.rs:1646-1687`.
- Alert levels: `crates/core/src/alert.rs`.
- Alert timer: `led/src/derived.rs:287-292`.
- Confirm-kill dispatch: `led/src/model/mod.rs:270-282`.
- Dimensions: `crates/state/src/lib.rs:234`.
- Themes referenced: `tabs`, `status_bar`, `browser`, `editor.ruler`,
  `editor.gutter`, `diagnostics`, `git.gutter_*` — see
  `docs/extract/config-keys.md`.

## Edge cases

- **Terminal narrower than `min_editor_width + side_width`**: side panel
  auto-hides regardless of toggle. Toggling on has no visible effect
  until resize.
- **Terminal narrower than status content**: `build_status_content`
  uses `saturating_sub` for padding — the right-side position indicator
  can collide with/overlap the left content. Truncation is
  character-based.
- **Spinner when LSP is busy but has no server_name**:
  `format_lsp_status` returns empty string. No spinner shown.
- **Empty buffer**: line count 0, gutter and buffer area drawn as
  `Block::default().style(text_style)` with no per-line gutter. Position
  indicator shows `L1:C1`.
- **Cursor past EOF / EOL**: handled by the buffer layer; gutter and
  status bar reflect the clamped cursor.
- **Soft-wrapped long lines**: gutter markers only on the *first*
  sub-chunk. Continuation sub-lines have blank gutter.
- **Diagnostic on line 0** (as in `edge/lsp_diagnostic_line_zero`): the
  gutter mark appears on the top visible line as expected.
- **Multiple diagnostics on one line**: most severe wins in the gutter
  (Error > Warning); Info/Hint produce no mark.
- **Persistent warn + new info**: info takes priority in the status bar
  while active (transient), reverts to warn when info clears.
- **Two concurrent warns**: head-of-list wins (first-arrived). The
  other remains hidden until the head is cleared.
- **Long alert text**: truncated at the viewport width; no wrapping.
- **Confirm-kill on a non-dirty buffer**: no prompt — the kill proceeds
  directly.
- **Tab bar overflow**: tabs past the right edge are simply not
  rendered. No `...` indicator or horizontal scroll.

## Error paths

- **Theme with an unresolvable `$name` in a chrome style**: [unclear —
  behavior depends on `style::resolve_cached`; likely returns a fallback
  and continues.]
- **`Dimensions` not yet set** (e.g. pre-resize): status-bar and overlay
  builders early-return `OverlayContent::None` / skip content;
  `build_layout` returns `None` and the frame draws a near-empty screen.
- **LSP progress with missing title**: formatted as just the server
  name.
- **`alert_clear` timer firing when `info` has already been cleared**:
  no-op; the timer handler reads current state.
- **Warn producer never calls `clear_warn`**: warn stays forever, even
  after restart if it's re-emitted during session restore. This is a
  real hazard — no dead-warn GC exists.
- **No active tab**: status-bar file-related fields are blank; position
  indicator still renders with the last known row/col (which may be
  stale if the last active buffer was closed).
- **`render_status_bar` when `is_warn == true`**: uses a hardcoded
  red/white bold style rather than `theme.status_bar.style`.
  Intentional (makes warnings unmissable) but not themeable today —
  flag for the rewrite.
