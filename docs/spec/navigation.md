# navigation

## Summary

`led`'s in-buffer navigation covers cursor motion (character, line, page, file
extremes), bracket-matching jumps, issue cycling (LSP / git / PR comments),
tab cycling, and a persistent jump list that records "interesting"
positions so the user can return to them. Horizontal scrolling is not a
thing — the editor wraps soft. Vertical scrolling is implicit: every
cursor move re-solves scroll position via a single `mov::adjust_scroll`
that keeps the cursor within a fixed `scroll_margin` of the viewport
edges. There is no word-granularity motion, no paragraph motion, and no
working outline jumper.

## Behavior

### Character, line, file movement

The primitives are `Action::MoveUp`, `MoveDown`, `MoveLeft`, `MoveRight`,
`LineStart`, `LineEnd`, `FileStart`, `FileEnd`. Each is a stream in
`movement_of.rs` that gates on `focus == PanelSlot::Main`, rejects any
active input modal or blocking overlay, clones the active buffer,
computes the new `(row, col, affinity)` via a pure helper in
`led/src/model/mov.rs`, then emits a single `Mut::BufferUpdate`. Every
combinator chain follows the identical shape (filter → sample_combine →
filter_map to pull `dims` + `active_tab` + buffer → map to
cursor + affinity + scroll recompute + bracket update + touch).

Left/right move by character; display-column affinity (`reset_affinity`)
is recomputed on every move so the cursor snaps to the visual column
after a line transition. Up/down move one display row, honouring soft
wrap: they pass through `mov::move_up` / `mov::move_down` which consult
the wrap state so that a wrapped logical line counts as multiple
visual rows. File-start/file-end jump to row 0 col 0 and last-row
last-col respectively.

`LineStart` / `LineEnd` are handled separately from the arrow streams
because they're also bindable inside find-file and file-search overlays
(see find-file / search docs). The editor versions are gated by a
stricter `has_input_modal` check (the full-input-modal check would block
find-file's re-binding).

### Page scrolling

`PageUp` / `PageDown` move the cursor by one buffer-height in display
rows, again through `mov::page_*`. There is no "scroll without moving
cursor" primitive — the viewport is always cursor-driven. The
`scroll_margin` (hardcoded to 3 rows in `Dimensions::new` —
`crates/state/src/lib.rs:244`) is the minimum distance from either
horizontal edge of the viewport at which the cursor will force a
scroll. `adjust_scroll` computes a sub-line-accurate target in
`mov.rs:11-98`, so soft-wrapped lines at the top of the viewport can
partially-scroll rather than jumping a full logical line.

### Match-bracket

`Action::MatchBracket` (`Alt-]`) jumps the cursor to the matching bracket
when one is currently highlighted. The highlight is maintained by the
syntax layer on every buffer mutation (`update_matching_bracket()` is
called at the tail of every movement and edit stream). If no bracket is
currently highlighted the action is a no-op — the stream has a
`filter(|(_, _, buf)| buf.matching_bracket().is_some())` guard
(`movement_of.rs:79`). There is no "push corresponding bracket onto
jump list" — bracket jumps are not jump-list records.

### Tab cycling

`PrevTab` / `NextTab` (`Ctrl-Left` / `Ctrl-Right`) cycle the active tab
through the set of non-preview materialized tabs. See `buffers.md` for
the tab model; from navigation's perspective the important thing is
that the emitted `Mut::ActivateBuffer` triggers `reveal_active_buffer`
in the fold tail, which re-scrolls the just-activated buffer to its
saved cursor using the current dims and `scroll_margin`.

### Issue navigation (next/prev issue)

`Action::NextIssue` (`Alt-.`) / `Action::PrevIssue` (`Alt-,`) is the
unified cycle across LSP diagnostics, git hunks, and PR comments. The
implementation lives in `nav_of.rs`. Levels are walked in order
(`IssueCategory::NAV_LEVELS`) — LSP errors first, then warnings, then
git categories, then PR categories — and the first non-empty level's
positions are sorted by `(path, row, col)`, deduplicated (two
categories at the same position collapse to one target), and then
either the next or previous position relative to the cursor is picked,
wrapping on overshoot (`pick_target_index`, `nav_of.rs:196-217`).

The outcome decomposes into up to four Muts per invocation: one
`Mut::Alert` (`"Jumped to <label> X/N"`), plus either (a) one
`BufferUpdate` for the same-buffer case, (b) `BufferUpdate` +
`SetActiveTab` for the other-already-open case, or (c) `RequestOpen`
+ `SetActiveTab` + `SetTabPendingCursor` for the not-yet-materialized
case. The pending-cursor path places the cursor so the target row is
half a buffer-height below the top of the viewport once the buffer
materializes.

### Jump list (push / back / forward)

A `JumpListState` holds a `VecDeque<JumpPosition>` plus an `index`
pointing at "where the user currently sits in their history". Jumps
get recorded implicitly by:

- LSP goto-definition (`lsp_of.rs:26`),
- isearch accept with a moved cursor (`isearch_of.rs:124`),
- jumping back from head (`jump_of.rs:27` — saves the present
  position before moving into history).

`JumpBack` (`Alt-b` / `Alt-Left`) and `JumpForward` (`Alt-f` / `Alt-Right`)
consult the list and emit a fan of fine-grained Muts from `jump_of.rs`:
a `SetJumpIndex`, then either (a) a `BufferUpdate` when the target
buffer is already materialized, or (b) `RequestOpen` +
`SetTabPendingCursor` when it's not, plus an `ActivateBuffer`. Saving
the current position only happens when the user back-jumps from the
very head of the list (`s.jump.index == s.jump.entries.len()`). The
jump list is capped and pruned by buffers-of logic (consult
`crates/state` for the capacity).

Jumps are not record in the undo history or the session DB — they are
ephemeral to the editor process.

### Scroll margin behavior

All movement streams tail-call `mov::adjust_scroll(&buf, &dims)` after
updating the cursor. The margin is clamped to `height / 2` so that in
a very short viewport the margin doesn't exceed half the height
(`mov.rs:19`). When the cursor is already within the middle band the
scroll offset is left unchanged — movement inside the comfortable
region does not perturb the viewport.

## User flow

Typical editing: user opens a file, cursor is at origin. `Down` moves
one row; if the cursor approaches the bottom margin, the viewport
scrolls automatically. `PageDown` jumps ~one screenful. `Ctrl-End`
lands at last-row/last-column. `Alt-]` inside a bracketed pair jumps
to the counterpart. `Alt-.` hops to the next LSP error and shows
"Jumped to Error 3/7" in the status bar. After a goto-definition
(`Alt-Enter`), `Alt-Left` returns to where the user was before the
jump.

## State touched

- `BufferState.cursor_row / cursor_col / affinity` — written by every
  movement stream.
- `BufferState.scroll_row / scroll_sub_line` — written by
  `mov::adjust_scroll` via every movement stream.
- `BufferState.matching_bracket` — updated by `update_matching_bracket`
  at the tail of every movement and edit stream.
- `AppState.dims` — read for `scroll_margin`, `buffer_height`,
  `text_width` on every movement.
- `AppState.jump` (`JumpListState`) — written by goto-definition,
  isearch accept, and `JumpBack` from head.
- `AppState.tabs` / `active_tab` — read by tab cycling and issue
  navigation.
- `AppState.buffers` — read to compute the target cursor; written via
  `Mut::BufferUpdate`.
- `AppState.git.file_statuses` / `git.pr` / per-buffer diagnostics
  and line statuses — read by issue navigation.
- `AppState.alerts.info` — written by issue navigation for the status
  line message.

## Extract index

- Actions: `MoveUp`, `MoveDown`, `MoveLeft`, `MoveRight`, `LineStart`,
  `LineEnd`, `PageUp`, `PageDown`, `FileStart`, `FileEnd`,
  `MatchBracket`, `JumpBack`, `JumpForward`, `NextIssue`, `PrevIssue`,
  `PrevTab`, `NextTab`, `Outline` **(dead)** —
  `docs/extract/actions.md`.
- Keybindings: `Up/Down/Left/Right`, `Home/End`, `Ctrl-a/Ctrl-e`,
  `PageUp/PageDown`, `Ctrl-v/Alt-v`, `Ctrl-Home/Ctrl-End`,
  `Alt-</Alt->`, `Alt-]`, `Alt-b/Alt-Left`, `Alt-f/Alt-Right`,
  `Alt-./Alt-,`, `Ctrl-Left/Ctrl-Right`, `Alt-o` **(dead)** —
  `docs/extract/keybindings.md`.
- Config: `scroll_margin` (hardcoded at 3 —
  `docs/extract/config-keys.md`; see
  `docs/rewrite/POST-REWRITE-REVIEW.md` §"Hardcoded settings").

## Edge cases

- **Empty buffer**: movement actions are no-ops; no alerts.
- **Wrapped long lines**: up/down traverse display rows, not logical
  rows; cursor affinity is preserved across wrapped transitions.
- **Viewport smaller than `2 * scroll_margin`**: margin is clamped to
  `height / 2`.
- **Bracket at end of document**: `MatchBracket` is a no-op when
  `buf.matching_bracket()` is `None`.
- **Jump list entry at a stale row**: `clamp_row_to_buffer` caps the
  target to the materialized buffer's last line
  (`nav_of.rs:185-192`). Zombie entries survive in the deque.
- **Issue navigation with zero items at every level**:
  `compute_navigation` returns `None` and nothing fires. [unclear —
  whether a "no issues" alert was intended].

## Error paths

- **Docstore fails to open a jump target / issue target**: covered in
  `buffers.md` / docstore driver docs. From navigation's side the
  `SetTabPendingCursor` state sits forever until another activation
  overwrites it.
- **Dims unavailable (pre-first-resize)**: every movement stream has a
  `filter_map` that returns early on `s.dims?`, so the action is
  silently dropped. `SetTabPendingCursor` defaults the scroll offset
  to `target_row - 10` when dims are missing (`nav_of.rs:109`,
  `jump_of.rs` does similar).

## Dead / absent features

- **`Action::Outline` (Alt-o)** — declared in `crates/core/src/lib.rs`
  and bound in `default_keys.toml`, but no handler, combinator, or
  filter references it. The `syntax::outline` module exists and emits
  `OutlineItem`s that nothing consumes. Pressing `Alt-o` is a no-op.
  See `docs/rewrite/POST-REWRITE-REVIEW.md` §"Dead code".
- **Word-granularity motion** — no `MoveWordLeft` / `MoveWordRight`
  action exists. Users get char-level or line-level movement; that is
  the complete set.
- **Mouse / scroll-wheel navigation** — [unclear — terminal driver
  doesn't seem to produce scroll events; confirm in driver docs].
