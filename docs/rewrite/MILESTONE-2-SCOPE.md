# Milestone 2 — scope (not a design)

This is a short **scope bookmark**, not a detailed design doc like
MILESTONE-1.md. It exists so a fresh session can pick up the next
vertical slice without re-deriving what's obvious, while leaving
concrete design decisions open.

Write the full design (source fields, input shapes, memo signatures,
crate-level changes) as the first step of M2 work, committed as
MILESTONE-2.md. Keep this file as the initial scoping input.

## Goal

Give the rewrite binary a **moving cursor + viewport scrolling**. After
M2, opening a file and pressing arrow keys should move the cursor
visibly, and the body should scroll when the cursor leaves the visible
rows.

## In scope

- Cursor state per tab (line + column). Persistent across tab switches.
- Arrow keys (Up / Down / Left / Right). Home / End / PageUp / PageDown
  optional if cheap to add — full feature-level keybindings wait for M4
  (config/keymap).
- Viewport scroll offset per tab. Scroll when cursor leaves the visible
  rows; scroll margin (a couple of rows of padding at top/bottom) is
  nice but not required.
- Render updates:
  - `body_model` reads the active tab's cursor and scroll offset; emits
    a scrolled slice of lines instead of the first N.
  - Cursor drawn in the frame (either as a `Frame`-level position the
    painter honours, or painted directly via a terminal cursor move —
    design decision open).
- Dispatch extensions: arrow keys mutate cursor / scroll on the active
  `Tabs` source.

## Out of scope

- Editing (insert / delete / undo) — that's M3.
- Word-wise / line-wise movement primitives beyond the basic arrows.
- Cursor invariants tied to edits (clamping after delete, etc.). Until
  M3 the buffer is read-only, so cursor.line/col can trust the rope.
- Search, jump list, mark / region, selection.
- Multi-cursor.
- Mouse input.

## Design questions to resolve at the start of M2

1. **Where does cursor state live?**
   - On the `Tab` struct in `state-tabs` (`Tab { id, path, cursor }`).
     Cleanest — M1 already treats `Tab` as per-view state. Cursor is
     per-view (two tabs on the same file can have independent
     cursors).
   - Or in a separate `state-cursors` crate (map `TabId → Cursor`).
     More modular, but splits data that belongs together.
   - **Lean toward on-Tab.** Same argument MILESTONE-1 used for
     preview/cursor.
2. **Where does scroll offset live?** Same question. Probably on `Tab`
   too — scroll is per-view just like the cursor.
3. **Does `Tabs` become a source that dispatch mutates a lot (cursor
   moves per arrow key)?** Yes. That's fine — `single` memo caching on
   `render_frame` still works because `TabsActiveInput` only invalidates
   when `open` or `active` change, not cursor (once we refine the input
   projection to exclude cursor from what the tab-bar / path list
   cares about). Be careful about input granularity: a cursor-only
   input for `body_model`, no cursor on the tab-bar input.
4. **Does `driver-terminal/native` need new keys?** Arrow keys are
   already translated in M1; no translation work needed. PageUp / Home
   etc. likewise. Dispatch is the only thing to extend.
5. **Testing:** can we unit-test cursor dispatch deterministically
   without spinning up a PTY? Yes — runtime's `dispatch` tests already
   do this for Tab/Shift-Tab; extend the same pattern. Goldens verify
   the visual output at the PTY level.

## Crates expected to change

- `state-tabs/` — `Tab` struct gains `cursor: Cursor` (and probably
  `scroll: Scroll`). Add `Cursor` + `Scroll` types here or pull them
  into `core/`.
- `runtime/src/query.rs` — new `#[drv::input]` projection on `Tabs` for cursor/scroll-bearing
  fields (consumed by `body_model`); `body_model` signature grows to
  read them. `render_frame` composition unchanged.
- `runtime/src/dispatch.rs` — match arrow keys, update cursor on the
  active tab, adjust scroll if cursor leaves viewport.
- `driver-terminal/core/` — `Frame` may need a `cursor: Option<(u16,
  u16)>` field so `paint()` can place the terminal cursor. Alternative:
  painter computes cursor position from body model — less clean.
- `driver-terminal/native/` — `paint()` honours the new cursor field:
  emits `cursor::Show` + `cursor::MoveTo` at the right screen coords.
  RawModeGuard already restores visibility on exit.

**No new driver crates.** No new sources outside `state-tabs`. M2 is a
pure within-existing-drivers extension.

## Done criteria

- `cargo run -p led -- FILE` with a multi-line file: arrow keys move
  the cursor visibly; the cursor doesn't leave the viewport (scroll
  follows).
- All M1 tests still pass; new dispatch tests for arrow keys pass.
- Ctrl-C still exits cleanly.
- (Stretch) Goldens that cover cursor-movement scenarios under
  `goldens/scenarios/actions/move_{up,down,left,right}/` run and pass.
  Current goldens depend on `--test-clock` which isn't wired; may need
  Phase-0-style catch-up first. Acceptable to defer this until M2 code
  works interactively, then wire `--test-clock` as its own small task.

## Growth path hook (from MILESTONE-1.md)

MILESTONE-1.md's Growth-path table has:
- M2: cursor + movement
- M3: editing
- M4: saving

MILESTONE-1 was inconsistent internally about M2 vs M3 for cursor.
This doc pins M2 = cursor + movement + scroll. Editing moves to M3.
