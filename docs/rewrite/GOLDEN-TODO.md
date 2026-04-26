# Golden TODO

Snapshot for the next handover. Counts move by ±1 between parallel
runs because of test-load flakiness; check individual tests with
`cargo test --manifest-path goldens/Cargo.toml --test <name>` to
confirm a real failure.

## Current state (post-M22, 2026-04-26)

| Suite          | Pass | Fail |
|----------------|------|------|
| actions        | 54  | 3   |
| config_keys    | 7   | 0   |
| driver_events  | 25  | 2   |
| edge           | 26  | 3   |
| features       | 24  | 2   |
| keybindings    | 108 | 3   |
| smoke          | 4   | 1   |
| **Total**      | **248** | **14** |

Counts can swing ±1 per run from test-load flakiness on the
parallel runner; the clipboard isolation flag knocked out the
previous worst offenders (yank tests racing the system
pasteboard).

## What's solid (recent fixes)

- **scroll_margin**: `crates/runtime/src/dispatch/cursor.rs` —
  3-row margin from each viewport edge, clamped to `body_rows /
  2`.
- **Diagnostic gutter**: `crates/runtime/src/query.rs`
  `merged_gutter_category` no longer accepts LSP severity (bar is
  git/PR only); popover anchor now includes `GUTTER_WIDTH`; the
  `W:N` warning count is gone from the position string.
- **Browser cluster**: ancestor reveal persisted into
  `browser.expanded_dirs` on file_load completion (mirrors
  legacy `reveal_active_buffer`); initial `GitScan` waits for
  pending CLI loads to land.
- **Session / undo flush wiring**:
  - `--config-dir` CLI plumbed through `World.cli_config_dir` →
    `SessionCmd::Init` so the goldens harness's per-test
    `<tmpdir>/config/` is honoured (was hardcoded to
    `~/.config/led`, so every test fought over the same flock).
  - `WorkspaceFlushUndo` trace emission turned on in the session
    driver core (was an explicit TODO awaiting the debounce).
  - Per-buffer **200ms debounce** for flush dispatch
    (`undo_flush_debounce` map on `Atoms`, deadline plumbed into
    `nearest_deadline`). Mirrors legacy's `KeepExisting` 200ms
    timer — short edit-then-quit scripts settle before it fires.
  - Dropped the `session.primary` gate on flush: legacy emits
    in standalone too. The session driver's `FlushUndo` handler
    already skips the SQLite write when not primary.
  - Moved `clipboard.execute` ahead of the flush block so the
    trace order reads `ClipboardWrite … WorkspaceFlushUndo …
    ClipboardRead`.
- **LSP completion popup — `prefix_start_col` backtracking**:
  `LspEvent::Completion.prefix_start_col` is now `Option<u32>`;
  the protocol parser only fills it when an item carried a
  `textEdit.range`. Runtime ingest backtracks through identifier
  characters from the cursor on `prefix_line` when `None`
  (`identifier_start_col` in `runtime/src/lib.rs`). Without this,
  servers that return bare-label items (the fake-lsp does this in
  every completion test) defaulted `prefix_start_col` to `0`, the
  refilter prefix became the whole line, fuzzy match returned
  empty, and `dismiss()` hid the popup before paint. Mirrors
  legacy `convert_completion_response`.
- **Code-action "no actions" alert removed**: legacy
  `Mut::LspCodeActions` clears the picker silently when the
  server returns `[]`; we used to surface a transient
  `"No code actions available"` info alert that broke any frame
  golden where the test happened to hit empty.
- **Save without LSP**: `save_with_optional_format` now skips
  the format round-trip when no LSP server has emitted any
  events. Mirrors legacy `save_of.rs::has_active_lsp(s)` —
  saves on plain `.txt` (or any unconfigured language) go
  straight through `request_save_active`, leaving a clean
  `"Saved <name>"` alert instead of being stuck on
  `"Formatting…"` until the 2-s TTL expires.
- **`file_save_action` no-dirty-filter**: removed the
  `if !eb.dirty() { continue; }` guard. The dispatch helpers
  already gate `Save` (writes-always-fire) vs `SaveAll` (only
  dirty buffers), so the duplicate filter was silently
  dropping clean-buffer saves the user explicitly asked for.
- **isearch failed wording**: status bar now reads
  `"Failing search: <query>"` when the query has no match
  (matches legacy `display.rs:761`), not the previous
  `"Search: <query>  [No match]"`.
- **`L1:C1` position fallback**: `position_string` now returns
  `L1:C1` when no tab is active. Without this, post-kill
  status bars dropped the position entirely, breaking the
  `ctrl_x_k` / `confirm_kill_*` goldens.
- **Centered file-search preview trim**: the side-panel
  preview now only trims when the raw text doesn't fit the
  per-row column budget, and trims by centering the match
  window in the visible cell (legacy
  `display.rs::file_search_hit_spans`). Replaces the old
  always-on left-trim with a leading ellipsis.
- **Replace-all alert format**: now `"Replaced N occurrence(s)"`
  matching legacy `Mut::FileSearchReplaceComplete`.
- **`Alt-Enter` no-op without LSP**: `lsp_goto_definition` is
  a silent no-op when no server has emitted anything (legacy
  parity — no request, no `"No definition found"` alert).
- **Auto-indent on Enter**: `insert_newline` copies the
  current line's leading whitespace into the new line so the
  cursor lands at the same indent column. Mirrors legacy
  `editing_of.rs::insert_newline_s` (without the syntax-tree
  request_indent path — simple "match previous line"
  covers the goldens).
- **Clipboard preview length**: `ClipboardWrite` trace preview
  now keeps 40 chars (legacy `led/src/lib.rs:201`); previously
  truncated at 14, which caused trace mismatches whenever
  yanked text exceeded that length.
- **Tab character expansion**: body lines now expand `\t` to
  4 spaces in `body_model` (matches legacy
  `core/src/wrap.rs::expand_tabs`). Previously the raw `\t`
  bytes flowed through to the terminal painter, where vt100
  jumped the cursor to the next 8-col tab stop and shifted
  everything after the tab by one column.
- **Wide-character cell handling**: `Buffer::put_str` now
  consults `unicode-width` and advances the cell column by 2
  for CJK / east-asian-wide chars, leaving the continuation
  cell untouched for the terminal's automatic wide-glyph
  drawing. Without this, sequential consecutive cell writes
  trampled the continuation cell of the prior wide char and
  the line ended up one or more codepoints short.
- **Combining-mark side map**: zero-width chars (combining
  accents, ZWJ joiners) now attach to the most recently
  written base cell via a `combiners` side map on `Buffer`.
  The diff renderer emits them as a follow-on `Print(ch)`
  immediately after the base char so the terminal attaches
  them to the right glyph (covers `unicode_combining` —
  café / niño / ZWJ-emoji family).
- **Tab-bar scroll-to-active**: `paint_tab_bar` advances the
  visible window when the active tab would fall off the right
  edge, matching the implicit "active tab is always visible"
  behaviour the legacy goldens require.
- **Chord-prefix shadows direct binding**: the config loader
  now drops the default direct binding for any key the user
  remaps to a chord table (`[keys."ctrl+y"]`). Without this,
  the merged keymap had both, `is_prefix` returned false, and
  the user's chord table was never consulted. Fixes the
  `keys_chord_prefix_remap` flake (which was leaking the
  default Yank's clipboard contents into the next test).
- **Hermetic clipboard for goldens**: `--test-clipboard-isolated`
  CLI flag (always passed by the goldens harness) swaps the
  `arboard`-backed clipboard worker for an in-memory cell so
  parallel tests can't trample each other through the OS
  pasteboard. Pre-fix, `yank_empty_kill_ring` and `main_ctrl_y`
  were "passing" by yanking whichever string the previous
  parallel test happened to leave on the system clipboard;
  now both correctly observe an empty kill ring and become
  no-ops.

## Remaining failures by cluster

### A. M23 — Auto-indent / reflow / sort-imports — 7 tests

Tests gated on the M23 milestone (`ROADMAP.md` § "M23"):

- `actions/insert_tab` — `Tab` key behavior in editor.
- `actions/reflow_paragraph` — `Ctrl-q` paragraph rewrap.
- `actions/sort_imports` — `Ctrl-x i` import-block sort.
- `keybindings/ctrl_x_i` — sort_imports binding alert.
- `keybindings/main_ctrl_q` — reflow_paragraph binding.
- `keybindings/main_tab` — insert_tab binding (currently
  reserved for `next_tab` placeholder).
- `features/editing_type_delete_reflow` — narrative auto-indent
  scenario.

All three commands lean on M15 (syntax) for language-aware
logic. Out of scope until M23.

### B. M26 — External file change + cross-instance sync — 5 tests

Tests gated on M26 (file-watch driver + `SessionCmd::CheckSync`):

- `smoke/external_change`
- `edge/external_change_while_dirty`
- `edge/external_delete_open_file`
- `driver_events/docstore_external_change`
- `driver_events/workspace_workspace_changed`

The rewrite's session driver has no `CheckSync` command — it's
the cross-instance sync feature documented in
`docs/drivers/workspace.md` §"Inputs": `$config/notify/<hash>`
touch files watched by a non-recursive notify watcher,
debounced 100ms; on Modify, the model bumps
`pending_sync_check`, derived dispatches `CheckSync`, the
driver runs `db::load_undo_after`. Pretending to emit the
trace without the underlying machinery would mask that the
feature isn't there.

### C. Completion / rename / code-action overlay rendering

Most of this cluster is now green — the popup-painting
fixes plus a refresh of 14 capture-variance / stale-legacy
goldens (see "What's solid"). The remaining frame failures
in this neighbourhood are unrelated to the popups:

- `lsp_completion/enter` — auto-indent missing on Enter:
  expected cursor at col 5 after the popup commit, ours
  at col 1.
- `lsp_completion/backspace` — the dirty-marker E2 issue
  (content-hash vs distance-from-save).
- `lsp_completion_popup_types_pr` — golden uses an old
  `LspServerStarted server=...` trace category that the
  rewrite never emitted; either drop the test or refresh
  with the current trace shape.

### D. Chord prefix display (cleared)

Previously a mixed bag — now resolved:

- **Position-string drop** (FIXED): `ctrl_x_k`,
  `confirm_kill_char_*` were missing the trailing `L1:C1`
  because `position_string` returned an empty `Arc` when no
  active tab existed. It now defaults to `L1:C1` (mirrors
  legacy `display.rs` reading the zero-init cursor row/col).
- **Kbd-macro chord bindings** (FIXED in M22):
  `ctrl_x_e` / `ctrl_x_lparen` / `ctrl_x_rparen` shipped
  with M22.
- **`ctrl_x_i`**: still pending; M23 (sort_imports alert).
  Tracked in Cluster A above.

### E. (folded into D)

The dirty-marker divergence (`EditedBuffer::dirty()` uses
content-hash equality while legacy uses
`distance_from_save() != 0`) is now treated as a deliberate
design choice — refresh affected goldens (e.g.
`lsp_completion_backspace`) when they otherwise match.

### F. File-search overlay (mostly FIXED)

The previously-truncated previews were a wrong-trim bug:
`trimmed_preview` always left-trimmed when the match was past
the 4-char context window. It now (a) skips trimming when the
raw preview fits in the per-row column budget, (b) centers the
match window when it doesn't (legacy
`display.rs::file_search_hit_spans`). Replace-all dropped its
old "Replaced N occurrences in M files." alert in favour of
legacy's `"Replaced {N} occurrence(s)"` shape.

### G. (cleared) Config-keys parsing

Goldens were captured with stale "Formatting…" alert text
left over from when every save queued a format. Once
`save_with_optional_format` started skipping the format round-
trip when no LSP server has run (matches legacy
`save_of.rs::has_active_lsp(s)`), the goldens needed a refresh
and now all seven config-keys tests pass.

### H. Long-tail — current breakdown

The earlier rendering-bug list is now cleared:

- `mixed_tabs_spaces` — fixed via tab expansion in body_model.
- `unicode_cjk` — fixed via wide-char cell handling.
- `unicode_combining` — fixed via combining-mark side map.
- `find_file_no_matches` — refreshed (golden's
  `<TMPDIR>/<active>/<typed>` shape was a legacy quirk; our
  parent-dir seed is the documented behaviour).
- `find_file_many_matches` — fixed via tab-bar scroll-to-active.
- `keys_chord_prefix_remap` — fixed via "user chord table
  shadows default direct binding" in the config loader.
- `yank_empty_kill_ring` — fixed via the
  `--test-clipboard-isolated` flag (in-memory clipboard per
  spawned `led`).

Two remaining long-tail items, both deliberate divergences
or known flakes:

- `edge/lsp_rebase_after_insert` — deliberate divergence:
  legacy rebases stale diagnostics onto the new line numbers,
  the rewrite hides them entirely until the next pull (memory
  `feedback_lsp_no_smear.md`). Don't refresh.
- `features/git_workspace_open_file` — flaky under parallel
  test load; passes individually. Investigate when it bites
  twice in a row.

### I. M22 — Keyboard macros (SHIPPED, 2026-04-26)

All seven previously-failing kbd_macro goldens are now green
following M22:

- `actions/kbd_macro_{start,end,execute}` (refreshed: `wait
  500ms` added to make undo-flush debounce deterministic).
- `keybindings/ctrl_x/{lparen,rparen,e}`.
- `keybindings/kbd_macro/e_replay` (refreshed: cursor at
  L4:C1 — legacy bug had it at L2:C1; see
  `POST-REWRITE-REVIEW.md` § "Macro replay does not dispatch
  editor cursor moves").
- `features/macros/{record_and_replay,record_insert_play_twice}`
  (refreshed for the same legacy macro-replay bug).

## Recommended next pass order

1. **M23 (auto-indent / reflow / sort-imports)** — Cluster A.
   7 failing tests gated on the milestone. All three commands
   share a syntax-tree dependency that already exists (M15);
   the work is wiring + per-language indent queries.
2. **M26 (file-watch + cross-instance sync)** — Cluster B.
   5 tests. Adds `driver-file-watch/` (notify-based), the
   `SessionCmd::CheckSync` command, and the client-side
   `workspace/didChangeWatchedFiles` LSP notification.
3. **`features/git_workspace_open_file` flake** — investigate
   if it bites twice in a row. Likely a parallel-test-load
   issue that a deterministic seed in the harness fixture
   would resolve.

## Don't refresh blindly

The 10 goldens refreshed in the recent pass were all
**capture-variance** cases: legacy goldens for the
identically-scripted `actions/yank` and
`driver_events/clipboard/text` disagreed on whether
`WorkspaceFlushUndo` fires within the test settle window. Once
the rewrite became deterministic, exactly one of the two
captures was correct; the other got refreshed to agree.

Refreshing a golden where the rewrite is **missing
functionality** (e.g. cross-instance sync, completion popup)
would mask the gap. Always check whether the diff is order /
capture-variance vs. missing trace events / missing overlay.
