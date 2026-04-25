# Golden TODO

State at end of the stabilization sweep: **36 pass / 21 fail**.

Lib test suite is green (365/365), so the goldens are the only
remaining signal. Each cluster below is a separate fix front;
size estimates are rough.

## 1. LSP wire-level tracing — 6 tests, ~half a day

Tests: `jump_back`, `jump_forward`, `lsp_goto_definition`,
`lsp_rename` (trace half), `match_bracket`, plus the trace half
of `next_issue` / `prev_issue` when LSP-driven.

Need `LspSend` / `LspRecv` trace lines for every JSON-RPC message
the manager exchanges with a server (`initialize`, `initialized`,
`textDocument/didOpen`, `textDocument/definition`, response
correlations by id). Current trace surfaces only
`LspServerStarted` once per language. Plumbing is mechanical but
touches the protocol layer.

## 2. Unimplemented commands — 5 tests, real M22 work

Tests: `kbd_macro_start`, `kbd_macro_end`, `kbd_macro_execute`,
`reflow_paragraph`, `sort_imports`, `insert_tab`.

Each is a feature on its own:

- `kbd_macro_*` — record/replay state machine + replay
  coalescing. Largest of the bunch.
- `reflow_paragraph` — paragraph rewrap to `text_width`.
- `sort_imports` — language-specific (Rust `use` ordering);
  same trigger surface as format-on-save.
- `insert_tab` — Tab key behavior (currently reserved by the
  keymap; need to decide tab vs spaces).

Out-of-scope for stabilization. Tracked here so they don't get
forgotten when the M22 cycle starts.

## 3. Browser cluster — 5 tests, root cause unclear

Tests: `find_file`, `expand_dir`, `collapse_dir`, `collapse_all`,
`toggle_side_panel`.

`expand_dir` was the deepest investigation. The script focuses
the side panel, presses Down, then Right. Expected: `sub/` is
expanded. Actual: still collapsed. Tracing through:

- entries = `[sub (dir), a.txt (file)]` in legacy's
  dirs-first-then-alphabetical order — same as ours.
- Active tab is `a.txt` (first CLI arg).
- Our active-tab snap pins selection to `a.txt` (idx 1).
  Legacy's `reveal_active_buffer` does the same on
  `Mut::ActivateBuffer`.
- Down clamps to last entry (still 1 = `a.txt`).
- Right calls `expand_dir`, which is a no-op on a file.

The expected golden shows `sub/` expanded, but tracing legacy's
own paths produces the same selection state we have. The golden
was captured against `Prep for rewrite` (commit `244e824`) and
may reflect a startup state we can't reproduce — possibly an
auto-reveal of all open tabs' ancestors, or a different default
selection on side-panel focus.

`find_file`'s prompt seed is similar territory: legacy's spec
says "active buffer's *directory*", but the golden shows
`<TMPDIR>/a.txt/` (the full active path) — we seed with
`<TMPDIR>/`, matching the spec.

Resolution likely needs either (a) running legacy from the
matching tag and recapturing, or (b) accepting that the goldens
were captured against pre-spec behavior and refreshing via
`UPDATE_GOLDENS=1` once we're sure our state is correct.

## 4. `scroll_margin` for page_down — 1 test, isolated

`page_down` already has the legacy `body_rows - 1` step (committed
in `a859723`), but the scroll adjustment doesn't apply legacy's
3-row scroll margin. Result: cursor lands at the last visible
row instead of 3 rows from the bottom.

Add a `scroll_margin: usize` field to `Terminal::dims` (default
3) and have `adjust_scroll` keep the cursor at least `margin`
rows from each edge. Isolated change, additive — should be a
short fix once the dim plumbing is clear.

## 5. Diagnostic gutter mark — 2 tests, visual rendering

Tests: `next_issue`, `prev_issue` (frame halves).

Our render emits a `▎` glyph in the gutter on diagnostic-bearing
rows; legacy's golden has just space + bullet. The bullet itself
matches; it's the leading `▎` we add that doesn't match.

Trace through `query::body_model` / paint to find where the
gutter row's category gets the extra mark and either suppress
it or update the goldens (the rewrite's mark may be deliberate
M19 visual polish).

## 6. LSP UI rendering — 2 tests

Tests: `lsp_rename` (frame), `lsp_code_action` (frame),
`lsp_toggle_inlay_hints` (frame).

`lsp_rename` shows the rename popup `Rename: foo` overlaid on the
buffer in expected; our render shows the buffer without the
overlay. The overlay state likely exists but doesn't stamp into
the frame at the same column / on the right tick.

`lsp_toggle_inlay_hints` shows inlay hint markers in the buffer
that we either don't render or render in a different style.

Each needs its own dispatch trace through to see whether the
state landed and the renderer just isn't picking it up. Likely
small fixes once located, but they're individual investigations.

## Recommended order

1. **`scroll_margin`** — single additive change, +1 test.
2. **Diagnostic gutter mark** — small render fix, +2 tests.
3. **LSP wire-level tracing** — biggest single unlock at +6 tests.
4. **LSP UI rendering** — peel them off one at a time, +2-3 tests.
5. **Browser cluster** — needs decision: refresh goldens or
   re-capture from legacy. +5 tests.
6. **Unimplemented commands** — defer to M22.

Reaching ~50/57 (≈90%) green is realistic without M22; the
remaining gap is the unimplemented-command bucket.
