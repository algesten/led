# Milestone 25 — Grapheme-aware column math

After M25, the cursor's `col` field stops being a char index and
becomes a **grapheme-cluster index**. Wide characters (CJK, emoji)
and combining-mark / ZWJ sequences behave the way users expect:
Right moves one cluster at a time, Backspace deletes the whole
cluster (not just its trailing combiner), End lands at the visible
end-of-line, and the rendered cursor sits at the correct display
column.

This is the milestone the M2 / M3 cursor docs flagged ("M2: char
index; revisit for unicode widths when syntax work comes online").
The render path already handles cell-level width and combining
marks (M-late: `Buffer::put_str` consults `unicode-width`,
combiners attach via a side map). What's missing is the
**buffer-coordinate** side: cursor arithmetic, edit primitives,
status bar, popup anchoring, wrap geometry — anywhere the code
currently treats one `char` as one column.

Prerequisite reading:

1. [`MILESTONE-2.md`](MILESTONE-2.md) — established `Cursor { line,
   col, preferred_col }` with the explicit char-index caveat
   ([state-tabs/src/lib.rs:14-23](../../crates/state-tabs/src/lib.rs)).
2. [`MILESTONE-3.md`](MILESTONE-3.md) §"D5" — established that edit
   primitives index the rope by `rope.line_to_char(line) + col`. M25
   keeps the contract but inserts a grapheme→char conversion step.
3. [`POST-REWRITE-REVIEW.md`](POST-REWRITE-REVIEW.md) — context for
   why legacy mostly ignores this. M25 is a behavioural improvement
   over legacy, not a regression fix.
4. `core/src/wrap.rs` — soft-wrap geometry. Currently treats every
   char as one display cell; the M25 refactor teaches it to walk
   grapheme clusters and accumulate display widths.
5. `crates/driver-terminal/native/src/buffer.rs:181-208` — the
   already-shipped `put_str` that advances the cell column by 2 on
   wide chars and attaches combiners via a side map. M25's
   buffer-side changes have to land in the column system that
   feeds this painter.
6. `goldens/scenarios/edge/{unicode_cjk,unicode_combining,
   unicode_emoji,unicode_rtl}/` — the four scenarios M25 must turn
   green (or keep green, in the cases where rendering-side fixes
   already passed them by accident).

---

## Goal

```
$ cargo run -p led -- emoji.txt
# (file contains "emoji line 🎉🚀✨🔥")
# Press End. Cursor lands at column 20 on screen — the visible end
# of the line — because four emoji each occupy 2 cells.
# Status bar reads `L1:C16` (16 graphemes: 11 ascii + 4 emoji + 1
# past-end).
# Press Backspace. The whole 🔥 cluster disappears (not just its
# tail surrogate); cursor steps back one grapheme.

$ cargo run -p led -- combining.txt
# (file contains "café" written as `cafe\u{0301}`)
# Press End. Cursor at C5 (4 graphemes + past-end).
# Press Left twice. Cursor at C2 (between 'c' and 'a').
# Press Backspace at the end. The whole 'é' cluster (e +
# combining acute) disappears in one go.

$ cargo run -p led -- cjk.txt
# (file contains "你好世界")
# Press Right. Cursor visually advances 2 cells (one CJK glyph).
# Press End. Cursor at the visible right edge.
# preferred_col is preserved across Up/Down using DISPLAY width:
# moving from a 4-CJK line (8 cells) up to a 10-ASCII-char line
# lands at the 8th display column, not the 4th char column.
```

## Scope

### In

- **`crates/core/src/grapheme.rs`** — new module. Pure rope-walk
  helpers built on `unicode-segmentation`'s `UnicodeSegmentation::
  graphemes()` and `unicode-width::UnicodeWidthStr/Char`.

  ```rust
  use ropey::RopeSlice;

  /// How many grapheme clusters a logical line contains, NOT
  /// counting the trailing newline (`\n` or `\r\n`). Mirrors the
  /// existing `line_char_len` shape, but in grapheme units.
  pub fn line_grapheme_len(line: RopeSlice<'_>) -> usize;

  /// Convert a grapheme-cluster index into the matching char index
  /// inside the line. Saturating: an out-of-range grapheme idx
  /// returns the line's char length (excluding newline).
  pub fn grapheme_col_to_char(line: RopeSlice<'_>, grapheme_col: usize) -> usize;

  /// Inverse of [`grapheme_col_to_char`]. A char index that lands
  /// inside a multi-codepoint cluster snaps to the cluster's
  /// starting grapheme (matches the conventional "cursor between
  /// graphemes" model).
  pub fn char_to_grapheme_col(line: RopeSlice<'_>, char_idx: usize) -> usize;

  /// Sum of `unicode-width` cell widths of the first
  /// `grapheme_count` clusters on the line. The painter feeds the
  /// result to crossterm `cursor::MoveTo`. Tabs (`\t`) expand to
  /// the next 4-col tab-stop, mirroring `body_model::expand_tabs`.
  pub fn prefix_display_width(line: RopeSlice<'_>, grapheme_count: usize) -> usize;

  /// Inverse of [`prefix_display_width`]: the grapheme col whose
  /// `prefix_display_width(...)` is the largest value `<= cells`.
  /// Used to translate `preferred_col` (a display position) back
  /// into a cluster index when moving Up/Down.
  pub fn display_col_to_grapheme(line: RopeSlice<'_>, cells: usize) -> usize;

  /// Cell width of a single grapheme cluster. Wide CJK / emoji
  /// → 2; combining marks alone → 0; tab → next-tab-stop delta;
  /// printable ASCII → 1.
  pub fn grapheme_display_width(cluster: &str, prior_cells: usize) -> usize;
  ```

  No `tree-sitter` dep, no `state-*` dep. `core/` already pulls
  `ropey` for the rest of M2/M3; `unicode-segmentation` and
  `unicode-width` are added (the latter is already a direct dep
  of `driver-terminal/native`, just promoted to a `core/` dep too).

  Why `core/` and not a new `text-grapheme/` crate: the helpers
  are rope-shaped utilities that every state crate already
  consuming `core::SubLine` will want to use. Keeps the dep graph
  flat.

  Performance: each helper materialises the line into an owned
  `String` exactly once (`line.to_string()`) and walks it with the
  segmenter. M25's hot paths (cursor move, body_model) call them
  at most a handful of times per keystroke / paint, so the per-
  call overhead is fine. The allocation discipline is preserved
  because `body_model` is memoised — re-rendering on identical
  input cache-hits without entering the helper at all.

- **`crates/core/src/wrap.rs`** — refactor to a rope-aware API.
  Existing functions keep their names but their signatures shift:

  ```rust
  // Before:
  pub fn sub_line_count(line_char_len: usize, content_cols: usize) -> usize;
  pub fn sub_line_range(sub: SubLine, line_char_len: usize, content_cols: usize) -> (usize, usize);
  pub fn col_to_sub_line(col: usize, line_char_len: usize, content_cols: usize) -> (SubLine, usize);
  pub fn sub_line_col_to_line_col(sub, col_within, line_char_len, content_cols) -> usize;
  pub fn is_continued(sub: SubLine, line_char_len: usize, content_cols: usize) -> bool;

  // After:
  pub fn sub_line_count(line: RopeSlice<'_>, content_cols: usize) -> usize;
  /// Returns the [start_char, end_char) slice of the rope line
  /// covered by sub, plus the display-cell width that slice
  /// occupies. Painter consumes the char range; geometry callers
  /// consume the cell width.
  pub fn sub_line_range(sub: SubLine, line: RopeSlice<'_>, content_cols: usize)
      -> SubLineRange;

  pub struct SubLineRange {
      pub char_start: usize,
      pub char_end: usize,
      pub cells: usize,
  }

  /// Given a cursor at grapheme col `gcol` on `line`, return which
  /// sub-line the cursor sits on and its cell offset within that
  /// sub-line.
  pub fn col_to_sub_line(gcol: usize, line: RopeSlice<'_>, content_cols: usize)
      -> (SubLine, usize /* cells_within */);

  pub fn is_continued(sub: SubLine, line: RopeSlice<'_>, content_cols: usize) -> bool;

  /// Inverse: for a given `(SubLine, cells_within)`, the matching
  /// grapheme col on `line`. Used by Up/Down to land at the
  /// closest cluster to `preferred_col` on the destination row.
  pub fn sub_line_cells_to_grapheme_col(
      sub: SubLine,
      cells_within: usize,
      line: RopeSlice<'_>,
      content_cols: usize,
  ) -> usize;
  ```

  Internally each function walks the line's grapheme clusters,
  accumulating cell width until the running total would exceed
  `wrap_width(content_cols)`. At each break point a new sub-line
  starts. The `\` continuation glyph still occupies the rightmost
  cell on non-last sub-lines.

  The two existing free helpers `wrap_width(content_cols)` and the
  degenerate `content_cols <= 1` short-circuit stay verbatim.

- **`crates/state-tabs/src/lib.rs`** — `Cursor` doc-comment shift.
  No struct shape change.

  ```rust
  /// Buffer-coordinate cursor position. Stored on [`Tab`] so two
  /// tabs viewing the same file can hold independent cursors.
  ///
  /// `line` and `col` are zero-based. `col` indexes **grapheme
  /// clusters** on the line (not chars, not display cells): one
  /// position past `c` on the line `c` `combining-acute` is
  /// `col=1`, even though the rope holds two chars. Right moves
  /// `col` by one cluster; End sets `col = line_grapheme_len`.
  ///
  /// `preferred_col` is the user's horizontal goal in **display
  /// cells**. Up / Down / PageUp / PageDown walk to the cluster
  /// whose `prefix_display_width` is closest to (but not greater
  /// than) `preferred_col` on the destination line; the displayed
  /// cursor stays vertically aligned even when the destination
  /// line has a different grapheme density. Any explicit
  /// horizontal move (Left / Right / Home / End / WordLeft /
  /// WordRight / FileStart / FileEnd) resets `preferred_col` to
  /// match the post-move display position.
  pub struct Cursor {
      pub line: usize,
      pub col: usize,
      pub preferred_col: usize,
  }
  ```

  Implementations of `Default + PartialEq + Eq + Hash + Copy +
  Clone + drv::Input + serde::*` stay derived; the type's wire
  shape doesn't change so no migration is needed.

- **`crates/runtime/src/dispatch/shared.rs`** — `line_char_len`
  stays for rope-byte / rope-char operations (the dozens of
  call sites that index the rope). A new sibling lives next to
  it:

  ```rust
  /// Grapheme-cluster count for the line. Use this when measuring
  /// against `cursor.col`; use [`line_char_len`] for everything
  /// that converts cursor.col → rope char index.
  pub(crate) fn line_grapheme_len(rope: &Rope, line: usize) -> usize {
      if line >= rope.len_lines() { return 0; }
      led_core::grapheme::line_grapheme_len(rope.line(line))
  }
  ```

  Plus a small adapter pair the dispatch helpers reach for:

  ```rust
  pub(crate) fn cursor_to_char(c: &Cursor, rope: &Rope) -> usize {
      let line_char = rope.line_to_char(c.line);
      let line_slice = rope.line(c.line);
      line_char + led_core::grapheme::grapheme_col_to_char(line_slice, c.col)
  }

  pub(crate) fn char_to_cursor(rope: &Rope, char_idx: usize) -> (usize, usize) {
      let line = rope.char_to_line(char_idx);
      let line_char = rope.line_to_char(line);
      let line_slice = rope.line(line);
      let gcol = led_core::grapheme::char_to_grapheme_col(line_slice, char_idx - line_char);
      (line, gcol)
  }
  ```

  These already exist as ad-hoc helpers in `dispatch/mark.rs`,
  `dispatch/isearch.rs`, etc. (each builds the conversion inline
  via `rope.line_to_char(line) + col`). M25 lifts them to
  `shared.rs`, replaces the inline forms, and adds the grapheme
  step. **Every** existing inline `rope.line_to_char(c.line) + c.col`
  is migrated.

- **`apply_move` (`runtime/src/dispatch/cursor.rs`)** — every
  Move variant rewritten to walk grapheme clusters and consult
  display widths:

  ```rust
  Move::Left  → if col > 0: col -= 1, preferred_col = display_width
                else if line > 0: line -= 1, col = line_grapheme_len(prev),
                                  preferred_col = prefix_display_width(prev, col)
                else: stay.
  Move::Right → if col < line_grapheme_len(cur): col += 1, preferred_col = …
                else if line + 1 < line_count: line += 1, col = 0,
                                                preferred_col = 0
                else: stay.
  Move::LineStart → col = 0, preferred_col = 0.
  Move::LineEnd   → col = line_grapheme_len(cur), preferred_col = display_width.
  Move::Up        → step one visual sub-line up. Within the same logical
                    line we move to the prior sub-line; at the top sub-line
                    we cross into the previous logical line. The new col is
                    sub_line_cells_to_grapheme_col(target_sub,
                        preferred_col_within_sub, target_line, content_cols).
  Move::Down      → mirror.
  Move::PageUp / Move::PageDown → step body_rows visual rows; same cell-
                    based landing logic as Up/Down but aggregated.
  Move::FileStart → (0, 0), preferred_col = 0.
  Move::FileEnd   → (last_line, line_grapheme_len(last)), preferred_col = …
  Move::WordLeft / Move::WordRight → walk grapheme clusters. Word-boundary
                    classifier (`is_word_grapheme(&str)`) treats clusters
                    starting with an ASCII alphanumeric / underscore as
                    word-clusters; everything else is non-word. Refines the
                    char-walking version so combining marks attached to a
                    word base count with the base.
  ```

  Every variant updates `preferred_col` consistently:

  - Vertical moves (Up/Down/PageUp/PageDown) **preserve**
    `preferred_col`, then re-derive `col` on the destination line
    via `display_col_to_grapheme(target_line, preferred_col)`.
  - Horizontal moves (Left/Right/Home/End/Word*/File*) **set**
    `preferred_col` to the post-move `prefix_display_width`.

  This is the same invariant the current code maintains, just
  expressed in the cell domain instead of the char domain.

- **Edit primitives (`runtime/src/dispatch/edit.rs`)**:

  - **`insert_char`** uses `cursor_to_char` to find the rope
    insertion point. Cursor advancement: insert the char, then
    re-derive `cursor.col` via `char_to_grapheme_col` for the
    line slice **after** the edit. This handles the rare case
    where the inserted char extends a preceding cluster (typing
    a combining mark after a base) — `col` stays put. In the
    common case (printable char that starts a new cluster) `col`
    advances by one as today.

  - **`insert_newline`** — char-index logic stays char-shaped
    (split point is a char position, not a grapheme position).
    The post-newline indent length conversion (`indent.chars().
    count()`) becomes `indent.graphemes(true).count()` so the
    cursor lands at grapheme col `indent_len_in_graphemes`.

  - **`delete_back`** — find the char range of the **grapheme
    before** the cursor (NOT just one char). Implemented as:

    ```rust
    let cur_char = cursor_to_char(&tab.cursor, &rope);
    if cur_char == 0 { return; }
    let line_slice = rope.line(tab.cursor.line);
    let line_char_start = rope.line_to_char(tab.cursor.line);
    let prev_grapheme_char_in_line = if tab.cursor.col > 0 {
        led_core::grapheme::grapheme_col_to_char(line_slice, tab.cursor.col - 1)
    } else {
        // At column 0: the "previous grapheme" is the newline at the end
        // of the previous line. Delete just the \n to join lines.
        rope.line_to_char(tab.cursor.line) - 1 - line_char_start
        // (handled by the line-join branch below)
    };
    ```

    For a multi-codepoint cluster like `e\u{0301}` at col 1, this
    selects the 2-char range `[line_char_start, line_char_start+2)`,
    removes it as one operation, records one `EditOp::Delete`,
    and decrements `cursor.col` by 1. Cursor lands cleanly between
    clusters; nothing dangling.

    The line-join branch (col 0, line > 0) stays single-char
    (just the `\n`). After the join the new cursor lives at the
    grapheme col equal to the previous line's `line_grapheme_len`
    before the join.

  - **`delete_forward`** — mirror of `delete_back`. Find the
    char range of the cluster **at** the cursor, delete it.
    Inside-line: cursor stays put; at-EOL: delete the `\n` to
    join with the next line.

  - **`insert_tab`** — the tree-driven path replaces the line's
    leading whitespace with the suggested indent and lands the
    cursor at `indent.graphemes(true).count()` (was `chars().
    count()`). The fallback (no language / no tree) stays
    cell-based: insert spaces from the cursor's display column
    up to the next 4-col tab stop, then re-derive `cursor.col`
    via `char_to_grapheme_col` after the insertion. The
    `tab_fallback=true` semantics are unchanged; only the unit
    of measurement (cells, not chars) catches up.

- **`body_model` + `visible_cursor` (`runtime/src/query.rs`)** —
  cursor placement reroutes through display width. Sketch:

  ```rust
  fn visible_cursor(c: Cursor, s: Scroll, dims: Dims, rope: &Rope) -> Option<(u16, u16)> {
      let body_rows = ... ;
      let line_slice = rope.line(c.line);
      let (cur_sub, cells_within) = wrap::col_to_sub_line(
          c.col, line_slice, content_cols);
      // Translate (line, sub) into a body-relative row using the
      // per-line sub_line_count walk that scroll already does.
      // ... (existing logic; the row math is unchanged) ...
      let display_col = (cells_within + GUTTER_WIDTH).min(max_col) as u16;
      Some((row, display_col))
  }
  ```

  `render_content` similarly switches its slice extraction from
  `chars().skip(col_start).take(...)` to a char-range slice
  `rope.slice(line_char_start + char_start_in_line ..
  line_char_start + char_end_in_line).to_string()`, where
  `char_start_in_line` / `char_end_in_line` come from the new
  `SubLineRange`. The painter has always rendered character-by-
  character; what changes is which character range each visual
  row carries.

  Side effect: `BodyLine` (the model carrying one painted row)
  no longer needs a `cells: usize` field if one isn't already
  there — the painter computes the running cell column itself
  via `Buffer::put_str`. (Verified with the explore agent's
  finding: `put_str` already does cell accounting.)

- **Status bar (`runtime/src/query.rs::position_string`)** —
  `L<line+1>:C<col+1>` continues to display the **grapheme** col,
  1-indexed. Conceptually a UX choice; matches the convention in
  most modern editors (Zed, VS Code, Sublime). Display cells would
  be misleading on lines with mixed widths (col jumping by 2 per
  CJK glyph). No format change; only semantics tighten.

- **Completion / code-action popup anchors (`runtime/src/query.rs`)**
  — replace `tab.cursor.col as u16` with the cursor's display
  column on its sub-line. Computed once in `body_model`'s
  `visible_cursor` and stashed on the `BodyModel::Content` struct
  so the popup memo can read it without redoing the walk:

  ```rust
  pub enum BodyModel {
      // ...
      Content {
          lines: Arc<Vec<BodyLine>>,
          cursor: Option<(u16, u16)>,
          /// Cursor's display column inside the body grid (sub-line
          /// relative). Same units as `cursor.0`. Memoised here so
          /// completion / code-action / rename popup memos don't
          /// re-walk the rope.
          cursor_display_col: Option<u16>,
      },
  }
  ```

  Existing popup memos (`completion_overlay_model`, `code_action_
  overlay_model`, `rename_overlay_model`) read this cached
  display col instead of `tab.cursor.col`. Misalignment on wide-
  char lines disappears.

- **Workspace deps (`Cargo.toml`)** — add:
  - `unicode-segmentation = "1.11"` to `[workspace.dependencies]`.
  - `unicode-width = "0.2"` promoted to a `[workspace.dependencies]`
    entry (was a direct dep of `driver-terminal/native`).

- **`led-core` Cargo.toml** — pull `unicode-segmentation` and
  `unicode-width` as new deps. `ropey` is already there.

### Out

- **LSP UTF-16 ↔ grapheme col conversion** — LSP positions are
  UTF-16 code units; led's diagnostics / completion ingest
  currently treats them as char indices, which is wrong for any
  non-BMP codepoint. Diagnostics and completions still appear at
  approximately the right column, but exact alignment with the
  LSP server's notion of position is broken on
  wide-codepoint files. Tracked separately — needs an audit of
  every LSP position consumer and converters at the protocol
  boundary. **Not in M25.** A failing scenario authored under
  M25 (e.g., `edge/lsp_position_emoji_buffer`) would correctly
  show a divergence; that scenario can be authored once we tackle
  the LSP-position story. Filed as the M25-followup `LSP
  UTF-16 column accuracy` orphan.

- **Bidirectional text rendering (RTL beyond byte order)** — led
  renders RTL strings left-to-right because the terminal does. A
  proper RTL display would need a logical/visual reordering pass
  (FriBidi or similar). The `unicode_rtl` golden encodes the
  current "byte-preserving" behaviour; M25 doesn't change it. The
  scenario's expected end state with cursor on the End of an RTL
  line lands on the visible right edge of those glyphs in the
  buffer, which is what the goldens already assert.

- **Mouse click → grapheme col** — no mouse input wired yet;
  irrelevant. When mouse lands (post-rewrite), the conversion
  will read `display_col_to_grapheme(line, click_col -
  GUTTER_WIDTH)`.

- **Tab-stop config** — `TAB_STOP = 4` stays hardcoded, same
  caveat as M23. M25's grapheme width helper consults the same
  constant when measuring `\t`'s display width.

- **`reflow_at` / `sort_imports` / `suggest_indent`** — these
  helpers operate on rope chars; they don't read `cursor.col`
  for their own arithmetic. M25 doesn't touch them. The dispatch
  glue (`reflow_paragraph`, `sort_imports`) reads cursor.col only
  to pick a row; that row is grapheme-agnostic.

- **Expand-tabs in `body_model`** — `expand_tabs` is already
  cell-aware (it pads with spaces to the next 4-col tab stop).
  Stays as-is; it's consumed by the painter as ASCII spaces, no
  grapheme issue.

- **Hardening against malformed UTF-8** — `ropey` already
  enforces UTF-8; the segmenter assumes valid UTF-8. M25 inherits
  those guarantees and adds nothing.

## Architecture conformance

M25 is a refactor inside the existing rewrite arch. No new
sources, no new drivers, no new memos cross crates. Mapped
against the `EXAMPLE-ARCH.md` axes:

- **Sources** — unchanged. `Tabs.open[i].cursor` is the same
  field; only the unit it counts changes.
- **Drivers** — unchanged. The terminal painter's existing wide-
  char + combiner handling is the **other half** of M25; the
  wire it joins to (cursor display col on the body model) is the
  only update on the painter side.
- **Queries** — `body_model` and the popup overlays are existing
  memos; their computation gains a grapheme walk. Output value
  identity is preserved (idle ticks still cache-hit on Frame
  equality).
- **Main loop phases** — dispatch reads grapheme widths from
  `core::grapheme` (a pure helper); render reads from the same.
  No new phase, no new ingest hook.
- **Crate layout** — `core/` gains a module + two deps; no new
  crate. Strict driver isolation preserved (`core/` has zero
  driver imports).
- **Guideline 8 (driver ignorance)** — `core::grapheme` has zero
  driver imports. Drivers never import it directly anyway; only
  `runtime` and the state crates do.
- **Guideline 11 (consumer declares inputs)** — N/A; no new
  cross-crate memo.
- **Guideline 14 (zero alloc on idle)** — the grapheme helpers
  allocate one `String` per call (rope-line materialisation).
  All call sites are gated by user keystroke or by `body_model`
  recompute (which only fires when an input changed). Idle ticks
  cache-hit on memos and never enter `core::grapheme`. The
  allocation discipline holds.

## Key design decisions

### D1 — `cursor.col` is a grapheme-cluster index; `preferred_col` is a display cell

The two units are different on purpose. `col` is what the user
moves with arrow keys (one Right = one cluster). `preferred_col`
is what the user sees; vertical moves keep the cursor visually
straight. Carrying the two together is standard practice in
modern editors (Zed, VS Code, Sublime, JetBrains all do this).

The alternative — col in display cells — would mean Right at
the start of a CJK glyph advances by 2, which feels broken when
the cursor "stops in the middle" of a glyph. The alternative —
preferred_col in graphemes — would mean Up/Down on a mixed
line jumps the cursor visually left-or-right when it shouldn't.

Both fields keep `usize` representation. The semantics are
documented; the field types are unchanged.

### D2 — `wrap.rs` becomes rope-aware; char-count callers move to display-cells

The legacy wrap API took `line_char_len: usize`, treating each
char as one cell. That's a lie on any wide-char content; lines
that should wrap at 90 chars don't, lines that shouldn't wrap
do. The fix needs to feed the actual line content to the
geometry helpers so they can walk grapheme clusters and sum
display widths.

The new API takes a `RopeSlice`. It's slightly heavier per call
(you can't precompute `len_chars()` and forget about the line);
the same memo gating that protected idle-tick performance still
applies, and the per-keystroke / per-paint cost is one extra
walk over the affected line (already the cost paid by the
painter).

The unit returned by `sub_line_range` flips from
`(char_start, char_end)` to `SubLineRange { char_start, char_end,
cells }`. Cells is what the painter and cursor-positioner need;
char range is what the rope-slicer needs. Bundling them avoids a
second walk to compute one from the other.

### D3 — Conversion happens at the boundary; rope ops stay char-indexed

The rope and ropey's API speak in chars. Converting cursor.col
to a rope char index every time we touch the rope keeps the
interface narrow. The conversion is a single helper
(`shared::cursor_to_char`) and a single inverse
(`shared::char_to_cursor`); every dispatch arm that mutates the
rope routes through them.

This isolates the change. If future work introduces a new edit
primitive, it doesn't have to know about graphemes — it operates
on `cursor_to_char` results and lets the helpers do the
translation.

### D4 — `delete_back` deletes the whole prior cluster

The user's mental model: Backspace removes the visible thing to
the left. That's a grapheme cluster, not a code unit. Deleting
just the trailing combining mark leaves a half-character on
screen, which is incoherent.

This is a behavioural improvement over legacy. Legacy's
`delete_back` removes one char per press, so a cluster like
`é` (e + combining acute) takes two Backspaces to disappear.
M25 makes one press do the right thing.

The matching `delete_forward` follows the same rule.

### D5 — `insert_char` re-derives `col` from the post-edit char index

The natural-feeling alternative — "increment col by 1 per
keypress" — breaks for combining marks. Typing `e` then a
combining acute should leave the cursor at col 1, not col 2:
the cluster `é` only has one cluster boundary after it. The fix
is post-hoc: after the rope edit, look up `char_to_grapheme_col`
on the new line slice. Cluster-extending inserts naturally pin
col; cluster-starting inserts naturally advance col by 1.

The cost is one extra `char_to_grapheme_col` per insert. That's
one segmenter walk over the line — fast at typical line lengths.
Hot enough to matter only if a user pastes a 10K-char line, in
which case the paste path (a future M-something) will batch.

### D6 — Word-boundary helpers walk graphemes, not chars

`is_word_grapheme(&str)` treats clusters whose first scalar is
an ASCII alphanumeric or `_` as word clusters. Combining marks
attached to a word base count with the base. Wide CJK glyphs
are non-word (legacy parity — alt+f / alt+b stop at every CJK
glyph individually). Future polish: a Unicode-class-aware
classifier (Word_Break property); not required for M25.

### D7 — Up/Down land on the cluster nearest preferred_col

The visual experience: the cursor stays in a vertical column.
The math: on the destination line, find the largest grapheme
col `g` such that `prefix_display_width(line, g) <=
preferred_col`. If `prefix_display_width(line, g+1) <
preferred_col` (the line ends short of the goal) and there's no
`g+1`, land at line end. If `prefix_display_width(line, g) ==
preferred_col` exactly, that's the column. Cluster width 2
means the cursor "snaps" to the cluster start when
`preferred_col` falls in the middle of a wide glyph.

`display_col_to_grapheme` encapsulates this. Its result is
deterministic and idempotent.

### D8 — Status bar shows grapheme col, not display col

UX: `L1:C5` should mean "the 5th visible thing on line 1". Cells
would be misleading (jumping by 2 per CJK glyph reads as
"buggy"); chars would also be misleading on combining-mark
content (cluster of 3 chars showing as col 3).

Graphemes match the cursor's notion of position and match what
modern editors do. Some editors show a chained `Lx:Cy:Dz` with
display col separately; out-of-scope for M25.

### D9 — Popup anchors use display col, cached on `BodyModel::Content`

Popups (completion / code-action / rename) anchor at the cursor's
visible column. Reading `tab.cursor.col` and trusting it as a
terminal column is wrong on wide-char lines. The body_model
already computed the cursor's display col when it placed the
cursor; caching that value on the model is cheaper than
re-walking the line in three more memos.

The alternative — adding a `display_col(line, col)` memo — would
fan in the same input via three different paths. Caching on
`BodyModel::Content` keeps the dataflow linear: cursor col →
body model → popup model.

### D10 — Tabs (`\t`) keep their cell-stop semantics

`\t` already expands to "spaces to the next 4-col tab stop" in
`body_model::expand_tabs`. The new grapheme helper agrees: a
`\t` cluster has display width `4 - (prior_cells % 4)`, equal to
what the painter already produces. Tabs and graphemes coexist
without special-casing.

### D11 — No M25-followup for legacy bug forwarding

Per `ROADMAP.md` § "Golden-review discipline", M25 is described
as a behavioural improvement, not a regression. New goldens
authored here land on `main` first — except: `main` (legacy)
doesn't have grapheme-aware col math. Its captures will have
the buggy positions. Authoring on `main` first means recording
the buggy frame, which then never matches the rewrite.

The pragmatic resolution (consistent with the doc's note that
"M25 is a behaviour improvement over legacy, not a regression
fix"): author the new `unicode_*` scenarios directly on
`rewrite`, since legacy doesn't have a meaningful "correct"
answer. The four existing `edge/unicode_*` scenarios already
came over from `main` and capture rendering-only behaviour;
M25 needs to either:

- Update those captures in place (they're already on `rewrite`),
  documenting that the rewrite's cursor is grapheme-aware.
- Author additional `edge/unicode_grapheme_*` scenarios that
  exercise specifically what M25 introduces (Backspace on a
  combining-mark cluster, Up/Down preserving display col on
  mixed-width lines, etc.).

We do both. The four legacy-derived scenarios get refreshed
where their current snapshots reflect the still-buggy cursor;
new scenarios cover the M25-specific behaviours that have no
legacy counterpart.

## Types

### `core` additions

```rust
// crates/core/src/lib.rs
pub mod grapheme;       // new module
pub use grapheme::{
    line_grapheme_len, grapheme_col_to_char, char_to_grapheme_col,
    prefix_display_width, display_col_to_grapheme,
    grapheme_display_width,
};

// crates/core/src/wrap.rs (signature changes per § In, above)
pub struct SubLineRange {
    pub char_start: usize,
    pub char_end: usize,
    pub cells: usize,
}
```

### `state-tabs`

`Cursor` doc-comment updated; struct unchanged. No migration,
serialised shape stable.

### `runtime/src/dispatch/shared.rs`

```rust
pub(crate) fn line_grapheme_len(rope: &Rope, line: usize) -> usize;
pub(crate) fn cursor_to_char(c: &Cursor, rope: &Rope) -> usize;
pub(crate) fn char_to_cursor(rope: &Rope, char_idx: usize) -> (usize, usize);
```

The existing `line_char_len(rope, line)` keeps its signature and
its 50-odd call sites. Migration to `line_grapheme_len` is per
call site, on a code-by-code basis: cursor-bound call sites
(clamp, paint cursor, popup anchor) move to `line_grapheme_len`;
rope-bound call sites (slice ranges, kill range, edit log
boundaries) stay on `line_char_len`.

### `runtime/src/query.rs::BodyModel`

```rust
pub enum BodyModel {
    Empty,
    Pending { path_display: String },
    Error { path_display: String, message: Arc<str> },
    Content {
        lines: Arc<Vec<BodyLine>>,
        cursor: Option<(u16, u16)>,
        cursor_display_col: Option<u16>,    // NEW
    },
}
```

The new field is `Option<u16>`; popup memos read it. Default
`None` keeps any not-yet-migrated path safe.

## Crate changes

```
crates/
  core/
    Cargo.toml             + unicode-segmentation,
                           + unicode-width.
    src/lib.rs             + pub mod grapheme;
                           + pub use grapheme::*;
    src/grapheme.rs        NEW — rope-walk helpers.
    src/wrap.rs            REFACTOR — rope-aware geometry.
  state-tabs/
    src/lib.rs             Cursor doc-comment update only.
  runtime/
    Cargo.toml             (no change — core::grapheme is reachable)
    src/dispatch/shared.rs + line_grapheme_len,
                           + cursor_to_char,
                           + char_to_cursor.
    src/dispatch/cursor.rs apply_move + word-boundary rewrite.
    src/dispatch/edit.rs   insert_char / delete_back / delete_forward
                           grapheme-aware; insert_tab indent length
                           in graphemes; insert_newline indent length
                           in graphemes.
    src/dispatch/{mark,kill,nav,isearch,mod}.rs
                           inline cursor-to-char conversions →
                           shared::cursor_to_char calls.
    src/query.rs           BodyModel.Content.cursor_display_col;
                           visible_cursor reads grapheme col +
                           emits display col;
                           render_content slices via SubLineRange;
                           position_string keeps grapheme col;
                           completion / code-action / rename
                           overlay memos read cursor_display_col.
goldens/scenarios/edge/
  unicode_emoji/           (refresh — cursor lands at correct cell)
  unicode_rtl/             (refresh — cursor lands at correct cell)
  unicode_cjk/             (verify — already passes via cell-rendering)
  unicode_combining/       (verify — already passes via combiner side map)
  unicode_grapheme_backspace/    NEW (Backspace on é cluster)
  unicode_grapheme_up_down/      NEW (Up/Down preserves display col)
```

No new workspace members. `core/` gains two deps; the workspace
manifest gains two `[workspace.dependencies]` entries.

## Testing

### `core::grapheme` (unit)

- `line_grapheme_len` — empty line → 0; pure ASCII line → char
  count; CJK line → glyph count (half of char count); combining-
  mark line `cafe\u{0301}` → 4; ZWJ family emoji as a single
  cluster → 1; trailing `\n` not counted; trailing `\r\n` not
  counted.
- `grapheme_col_to_char` — round-trip with `char_to_grapheme_col`
  on every grapheme col of a fixture; out-of-range col saturates
  to `len_chars()` minus newline.
- `prefix_display_width` — pure ASCII line: prefix == col. CJK:
  prefix == 2 * col. Combining: prefix == base widths only. Tab:
  contributes `4 - (prior_cells % 4)` cells.
- `display_col_to_grapheme` — round-trip with `prefix_display_
  width` for a few col values; "in the middle of a wide glyph"
  cells snap to cluster start.
- `grapheme_display_width("\t", 0)` → 4. `("\t", 3)` → 1.
  `("\t", 4)` → 4. `("a", 0)` → 1. `("é", 0)` → 1 (composite
  combining cluster). `("你", 0)` → 2. `("\u{0301}", 0)` → 0
  (combining alone — pathological; wraps as zero-width).

### `core::wrap` (unit)

Existing tests rewritten against the new API. New tests:

- `wide_chars_wrap_at_cell_boundary` — line `aaaaaaaaaa你`
  (10 ASCII + 1 CJK) at content_cols=12 wraps because 10+2 = 12,
  exceeds wrap_width=11. Sub-line 0 covers chars [0, 10); sub-
  line 1 covers chars [10, 11) (the CJK glyph alone, 2 cells).
- `combining_marks_dont_force_wrap` — line `cafe\u{0301}` at
  content_cols=5 fits in one sub-line (4 cells, 5 chars).
- `tab_in_wrap` — line `\t\t\t\t\t\t` at content_cols=10 wraps
  after 2 tabs (8 cells, sub 0); sub 1 is the next 2 tabs.

### `runtime::dispatch::cursor` (unit)

- `right_advances_one_grapheme` — cursor at (0, 0) on `é`-only
  line → after Right: (0, 1). char index after = 2.
- `right_at_eol_wraps_to_next_line` — current behaviour preserved.
- `left_decrements_one_grapheme` — cursor at (0, 1) on
  `cafe\u{0301}` → after Left: (0, 0).
- `end_lands_at_grapheme_count` — `cafe\u{0301}` → after End:
  (0, 4).
- `up_preserves_display_col` — cursor at (1, 4) (display col 4)
  on a line whose row 0 is `你好世界` (4 CJK glyphs, 8 cells). After
  Up: (0, 2) (col 2 is display 4 cells, the closest cell to
  preferred_col=4 ≤).
- `down_preserves_display_col_clamping_short` — Down onto a
  shorter line lands at end of line; preferred_col preserved
  for the next move.
- `word_left_skips_whole_grapheme` — cursor at end of `café`
  → WordLeft → (0, 0).
- `home_resets_preferred_col` — checked.

### `runtime::dispatch::edit` (unit)

- `delete_back_removes_combining_cluster` — cursor at (0, 1)
  on rope `e\u{0301}` (2 chars, 1 grapheme) → delete_back →
  rope is empty, cursor at (0, 0).
- `delete_back_removes_zwj_emoji_family` — cursor past family
  emoji → delete_back removes all 7 chars (👨‍👩‍👧‍👦) as one
  cluster, cursor backs up by 1 grapheme col.
- `delete_forward_at_cluster_start` — cursor at (0, 0) on
  rope `é-rest` (3 chars, 2 graphemes) → delete_forward → rope
  is `-rest` (4 chars, 4 graphemes) — wait, let me re-check.
  Actually `é` = `e + combining acute` = 2 chars 1 grapheme,
  + `-rest` (5 chars 5 graphemes) = 7 chars 6 graphemes total.
  After delete_forward at (0, 0): the `é` cluster is removed.
  Rope = `-rest` (5 chars 5 graphemes). Cursor stays at (0, 0).
- `insert_char_extends_cluster` — cursor at (0, 1) on rope
  `e` → insert_char(combining acute) → rope is `e\u{0301}` (2
  chars 1 grapheme), cursor at (0, 1) (unchanged — the new
  char extended the prior cluster, not a new one).
- `insert_char_starts_new_cluster` — cursor at (0, 0) on empty
  rope → insert_char('h') → rope is `h`, cursor at (0, 1).
- `insert_tab_uses_grapheme_count_for_indent` — Rust file with
  parsed tree at `let x = 1;` → after Tab: cursor at (line, 4)
  (graphemes), display col 4.
- `insert_newline_indent_in_graphemes` — Rust file with parsed
  tree, Enter at end of `fn main() {` → cursor on next line at
  col 4 (4 graphemes).

### `runtime::query::body_model` (unit)

- `cursor_display_col_in_emoji_line` — line `emoji line 🎉🚀✨🔥`
  (15 graphemes, 19 cells), cursor at col 15 (end) → body
  model's `cursor_display_col` = Some(19).
- `cursor_display_col_in_cjk_line` — line `你好世界`, cursor at
  col 2 → display col = 4.
- `cursor_display_col_with_tabs` — line `\thello`, cursor at
  col 1 → display col = 4.
- `position_string_uses_grapheme_col` — cursor on line at
  grapheme col 15 of `emoji line 🎉🚀✨🔥` → status bar
  reads `L1:C16`.

### `runtime::query::completion_overlay_model` (unit)

- `popup_anchors_at_display_col_in_cjk_line` — cursor at col 2
  on line `你好世界` → popup x = editor_area.x + GUTTER_WIDTH + 4.

### Integration

- `unicode_emoji` golden — Down + End on emoji line lands at
  display col 19 (the visible end), `L2:C16` in status bar.
- `unicode_rtl` golden — End on RTL line lands at the correct
  display col (matches existing capture if cursor was off; or
  capture refreshed).
- `unicode_grapheme_backspace` (new) — type `cafe\u{0301}`,
  press End, press Backspace twice. After first Backspace the
  rope is `caf` (3 chars 3 graphemes), cursor at (0, 3). After
  second: `ca` (2 chars 2 graphemes), cursor at (0, 2). Frame
  captures `ca | L1:C3`, then `ca | L1:C3` after the second
  Backspace... no, after second is `ca` cursor at C3? Let me
  re-derive. After first Backspace: `caf`, cursor (0, 3); after
  second: `ca`, cursor (0, 2). Status bar `L1:C3`.
- `unicode_grapheme_up_down` (new) — cursor on a line of CJK
  glyphs at col 4 (display 8); Up onto an ASCII line — lands at
  col 8 (display 8); Down back — lands at col 4 (display 8).

Expected delta: ~25 unit tests, ~2 new goldens.

## Done criteria

- All existing tests pass (unit + integration).
- All new tests green.
- `cargo clippy --all-targets`: net delta ≤ +2 from post-M23.
- Goldens:
  - `edge/unicode_cjk` — green (preserved).
  - `edge/unicode_combining` — green (preserved).
  - `edge/unicode_emoji` — green (was failing).
  - `edge/unicode_rtl` — green (was failing).
  - `edge/unicode_grapheme_backspace` — green (new, authored on
    `rewrite` per D11).
  - `edge/unicode_grapheme_up_down` — green (new, authored on
    `rewrite` per D11).
  - No regressions in any other suite.
- Interactive smoke:
  - `cargo run -p led -- emoji.txt` — End lands cursor at
    visible right edge; Backspace removes whole emoji cluster.
  - `cargo run -p led -- cjk.txt` — Up/Down preserves display
    column across ASCII / CJK mixes.
  - `cargo run -p led -- combining.txt` — Backspace on `é`
    deletes both code points; cursor moves one cluster left.
- `GOLDEN-TODO.md` updated: total moves from 254/8 to 256/6
  (or wherever the new totals land), M25 entry added under
  "What's solid".

## Growth-path hooks

- **LSP UTF-16 column** — when M16-followup audits LSP position
  conversion, `core::grapheme` already has the helpers needed.
  The conversion is "char idx → UTF-16 code unit count" (added
  to `core::grapheme`) and "UTF-16 code unit count → char idx"
  for the inverse.
- **Configurable tab stop** — `grapheme_display_width` reads a
  hardcoded `TAB_STOP = 4`. When a `settings.toml` exposes the
  knob, it becomes a parameter or a thread-local context.
- **Word-break property** — `is_word_grapheme` could consult
  `unicode_properties::WordBreak` for languages where word
  boundaries don't align with ASCII alphanumerics. Defer until
  someone files a real concern.
- **Mouse click → grapheme col** — `display_col_to_grapheme`
  is the conversion. Mouse landing post-rewrite plugs in here.
- **Bidirectional text rendering** — would require a logical/
  visual reorder pass before painting. Out of scope for M25;
  flagged for a possible future RTL milestone.
- **Paste-large-text optimisation** — bulk insert currently
  walks the segmenter once per char. A future M-something
  paste path could batch the post-edit `char_to_grapheme_col`
  call to one walk per paste, not per char.
