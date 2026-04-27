# Golden TODO

Snapshot for the next handover. Counts move by ±1 between parallel
runs because of test-load flakiness; check individual tests with
`cargo test --manifest-path goldens/Cargo.toml --test <name>` to
confirm a real failure.

## Current state (post-M26, 2026-04-27)

| Suite          | Pass | Fail |
|----------------|------|------|
| actions        | 57  | 0   |
| config_keys    | 7   | 0   |
| driver_events  | 25  | 2   |
| edge           | 30  | 1   |
| features       | 25  | 1   |
| keybindings    | 111 | 0   |
| smoke          | 5   | 0   |
| **Total**      | **260** | **4** |

(Single-threaded `cargo test --manifest-path goldens/Cargo.toml
--test <suite> -- --test-threads=1`. Counts can swing ±1–3 per
parallel run from test-load flakiness; single-threaded is the
authoritative baseline.)

The 4 single-threaded failures break down as:

- **`edge/external_change_while_dirty`** — frame mismatch
  (NOT trace mismatch): the script runs `press End` immediately
  after spawn, before the buffer has materialised, so the
  keystroke lands on an empty rope and the typed text ends up
  at column 0 instead of after the original line. Same
  pre-existing harness race documented below — confirmed
  reproducible on rewrite HEAD without M26 by stashing
  changes. M26's *trace* side passes (FlushUndo + CheckSync
  fire correctly); only the frame check fails.
- **Pre-existing harness flake (3)** — `wait_ready` exits on the
  first PTY byte (often a raw-mode-setup byte) rather than on
  the first painted body row, so "no script, capture initial
  frame" scenarios race the file load: `driver_events/
  docstore_opened`, `driver_events/syntax_buffer_parsed`,
  `features/git_workspace_open_file`. All three pass
  individually 70–100 % of the time. A goldens-harness fix
  (poll for actual content rather than first byte) is the
  proper resolution; this is what
  `edge/external_change_while_dirty` is also ultimately
  blocked on.

## What's solid (recent fixes)

- **M26 — File-watch + cross-instance sync** (2026-04-27):
  Per [`MILESTONE-26.md`](MILESTONE-26.md). New
  `crates/driver-file-watch/{core,native}` crate pair wraps
  `notify` (FSEvents on macOS, inotify on Linux). Goldens that
  moved to green: `smoke/external_change`,
  `edge/external_delete_open_file`,
  `driver_events/docstore/external_change`,
  `driver_events/workspace/workspace_changed`,
  `features/editing/type_delete_reflow` (script gained a
  trailing `wait 500ms` for the FlushUndo + CheckSync
  debounce settle). M26 ships:
  - `FileWatchState` driver-owned source (registry +
    recent_events queue + backend status). Imbl-backed for
    pointer-equality on idle ticks.
  - Three watch intents keyed by `WatcherId`: workspace root
    recursive (id `u64::MAX`), `<config>/notify/`
    non-recursive 100 ms-debounced (id `u64::MAX-1`), per-buffer
    parent dirs (allocated via `watch_id_seq`; skipped when the
    parent is already covered by ROOT to avoid notify's
    "double-watch" rejection).
  - `SessionCmd::CheckSync` + `SessionEvent::SyncResult` (with
    `SyncResultKind::SyncEntries|ExternalSave|NoChange`). The
    session driver's native worker reads back from
    `undo_entries` via `seq > last_seen_seq`. `FlushUndo` and
    `ClearUndo` now `touch_notify_file($config/notify/<hash>)`
    so peers' notify-dir watchers fire and dispatch CheckSync.
  - `FileReadCmd::Reread` + `FileReadEvent::RereadDone` (with
    `kind: ReadKind = Initial|Reread`). Reread uses a strict
    read that returns Err on NotFound rather than empty rope
    so an external delete doesn't silently clobber the buffer.
  - Three-branch external-change reconcile in ingest (per
    `EXAMPLE-ARCH.md` § "Invariant enforcement"):
    - Clean + new content: replace rope, push one EditGroup
      so undo restores prior content, refresh
      `disk_content_hash`, advance version + saved_version.
    - Dirty + new content: silent drop (legacy parity).
    - Hash matches: no-op.
    Both clean and dirty branches refresh the workspace tree
    (clear `fs.dir_contents`, set `git_scan_pending`) since
    the disk side moved either way.
  - Memo-driven dispatch helpers: `compute_watch_actions`
    (desired/actual diff → Watch/Unwatch cmds),
    `compute_external_reread_targets`,
    `compute_sync_check_targets`,
    `compute_workspace_tree_refresh`. Watch-actions stays in
    execute (output-side); event-fan-out helpers run in
    ingest so the in-tick query memos see the cleared
    `fs.dir_contents` and emit fresh ListDir cmds in the same
    tick (otherwise FsListDir/GitScan would order-flip on
    workspace tree changes).
  - `--no-workspace` mode: file-watch driver constructs
    lazily (the `notify::Watcher` itself only spawns on the
    first `Watch` cmd), and the runtime gates dispatch on
    `!no_workspace`. Standalone runs pay zero file-watch
    overhead.
  - New trace line: `WorkspaceCheckSync\tpath=<p>`.
  - `led_core::CanonPath::path_hash()` — 16-char lowercase
    hex of the canonical path. Mirrors legacy
    `led/crates/workspace/src/lib.rs:512-517`.

- **M25 — grapheme-aware column math** (2026-04-26):
  `Cursor::col` now indexes grapheme clusters (was: chars);
  `Cursor::preferred_col` is in display cells (was: chars). The
  rewrite handles wide CJK / emoji / combining-mark / ZWJ
  content end-to-end:
  - New `crates/core/src/grapheme.rs`: rope-walk helpers
    (`line_grapheme_len`, `grapheme_col_to_char`,
    `char_to_grapheme_col`, `prefix_display_width`,
    `display_col_to_grapheme`, `grapheme_display_width`).
  - `crates/core/src/wrap.rs`: refactored to a rope-aware API.
    `sub_line_count(line, content_cols)`, `sub_line_range`
    returns `SubLineRange { char_start, char_end, cells }`,
    `col_to_sub_line(gcol, line, content_cols)` returns
    grapheme col + display cells, plus
    `sub_line_cells_to_grapheme_col` for vertical-move landing.
  - `apply_move`: every variant rewritten to step graphemes
    horizontally and preserve `preferred_col` in cells across
    Up/Down/PageUp/PageDown. Word-boundary helpers walk
    grapheme clusters via `is_word_grapheme`.
  - Edit primitives: `delete_back` and `delete_forward`
    delete a full grapheme cluster (e.g. one Backspace on
    `é` written as `e + combining acute` removes both chars).
    `insert_char` re-derives `cursor.col` post-edit so a
    combining mark naturally extends the prior cluster.
    `insert_newline`/`insert_tab` use grapheme counts for
    indent length.
  - `body_model` cursor placement reroutes through display
    cells; popover/completion-popup anchors land at the
    correct visible column on wide-char lines.
  - **Goldens moved to green:** `edge/unicode_emoji`,
    `edge/unicode_rtl` (were failing pre-M25). 
    `edge/unicode_combining` snap refreshed to the new
    grapheme col semantics (cursor "C9" on `family 👨‍👩‍👧`
    instead of legacy's "C13" — the cluster is one position,
    not five). Per `ROADMAP.md` § "Golden-review discipline"
    this is an *intentional behavior improvement* (M25's whole
    point) and the refreshed snap reflects what users see:
    one Backspace removes the whole emoji family, cursor
    advances one position past the cluster, etc.

- **M23 — auto-indent / reflow / sort-imports** (2026-04-26):
  Three new commands wired through dispatch:
  - `Command::InsertTab` — replaces the active line's leading
    whitespace with the tree-sitter indent suggestion when one
    exists; falls back to inserting spaces up to the next
    4-col tab stop. Mid-content Tab is a no-op when the line
    is already correctly indented (legacy parity).
  - `Command::ReflowParagraph` (`Ctrl-q`) — dprint-driven
    paragraph / line-comment / block-comment reflow at
    cursor row, via the new portable `text-reflow` crate.
  - `Command::SortImports` (`Ctrl-x i`) — tree-sitter import
    extraction + alphabetical sort, with the matching
    "Imports sorted" / "Imports already sorted" alert.
  - Also extended: `Command::InsertNewline` consults
    `state-syntax::indent::suggest_indent` for the new line
    when a parse tree is available; falls back to the M3
    "match previous line's leading whitespace" rule.
  - Completion popup overlay now treats `Tab` (alongside
    `Enter`) as the commit key, matching legacy LSP
    convention. Without this, `Tab` inside the popup would
    fall through to `insert_tab` and re-indent the line.
  - Per-language indent + imports queries pre-compile on the
    main thread before the syntax-driver worker spawns
    (`runtime::spawn_drivers`) — avoids a tree-sitter FFI
    stall when dispatch's `Query::new` would otherwise race
    the worker's compilation. Swift and C are excluded from
    pre-warm because their grammars' init sequences fight
    with crossterm's TTY mode setup; their queries compile
    lazily on first Tab inside that language instead.
  - Goldens refreshed for capture variance: `actions/insert_tab`,
    `actions/sort_imports`, `actions/reflow_paragraph` legacy
    captures missed the post-Tab `didChange` and the
    sort/reflow buffer changes (legacy snapshot was taken
    before the dispatch effect propagated). The rewrite's
    captures are correct.
  - **`lsp_completion/enter`** also fixed by the same
    InsertNewline auto-indent path. Tricky bug: my first cut
    of `insert_newline` asked `suggest_indent` about
    `line_idx + 1` (the line *below* the split) — which in
    a fixture like `fn main() {\n    x.\n}` is the `}` line,
    triggering the closing-bracket short-circuit and returning
    the *opener's* indent (empty). Switched to asking about
    `line_idx` (the line being split); the structural indent
    of that line is exactly what the new line wants when Enter
    fires at EOL, and the popup-commit case (where Enter
    inserts a newline mid-buffer) gets the same correct
    answer.

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

### A. M23 — Auto-indent / reflow / sort-imports (SHIPPED, 2026-04-26)

All six core M23 goldens are now green:

- `actions/insert_tab` — green.
- `actions/reflow_paragraph` — green.
- `actions/sort_imports` — green.
- `keybindings/ctrl_x_i` — green.
- `keybindings/main_ctrl_q` — green.
- `keybindings/main_tab` — green.

`features/editing/type_delete_reflow` was originally listed
under M23 but it's actually gated on M26 — its expected trace
includes `WorkspaceFlushUndo` + `WorkspaceCheckSync` lines
that come from the cross-instance sync feature. The reflow
itself works on the rewrite (the script's
`type X press Ctrl-q` produces the expected frame); only the
M26 trace lines are missing. Folded into Cluster B below.

### B. M26 — External file change + cross-instance sync (SHIPPED, 2026-04-27)

5 of 6 M26-gated goldens are green. Design:
[`MILESTONE-26.md`](MILESTONE-26.md). See "What's solid" above
for the full landing summary.

- `smoke/external_change` — green.
- `edge/external_delete_open_file` — green (snap refreshed:
  legacy emitted two FsListDir from its multi-watcher setup;
  the rewrite's single source of truth produces one. Per
  `ROADMAP.md` § "Golden-review discipline" this is an
  intentional behavioural improvement).
- `driver_events/docstore/external_change` — green.
- `driver_events/workspace/workspace_changed` — green.
- `features/editing/type_delete_reflow` — green (script
  gained a trailing `wait 500ms` so settle covers the
  FlushUndo 200 ms debounce + CheckSync 100 ms debounce + the
  notify-touch round-trip).

The remaining failure:

- `edge/external_change_while_dirty` — frame mismatch (NOT
  trace mismatch). Pre-existing harness race: the script
  runs `press End` immediately after spawn, before the
  buffer has materialised, so the keystroke lands on an
  empty rope and the typed text ends up at column 0.
  Confirmed reproducible on rewrite HEAD without M26 by
  stashing changes; M26's *trace* side (FlushUndo +
  CheckSync after fs_write) passes correctly. The fix is
  the goldens-harness `wait_ready` improvement (poll for
  first painted body row rather than first PTY byte) —
  same fix that unblocks the three other harness-flake
  tests below.

LSP `workspace/didChangeWatchedFiles` shipped alongside the
M26 core. The runtime's `compute_lsp_watched_file_notifications`
helper fans matching `FileWatchEvent`s out as per-server
`LspCmd::DidChangeWatchedFiles`. Coverage:
`features/lsp/did_change_watched_files` (server registers
`**/*.toml`, harness writes a Cargo.toml, trace asserts one
`LspDidChangeWatchedFiles`).

The goldens harness already supports `fs_write` and
`fs_delete` script commands
(`goldens/src/scenario.rs:130, 141`); no harness change was
needed for these six scenarios.

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

- ~~`edge/lsp_rebase_after_insert`~~ — retired. Captured legacy's
  rebase-stale-diag-onto-new-line behaviour, which the rewrite
  deliberately doesn't do (memory `feedback_lsp_no_smear.md`).
  Replaced by **`edge/lsp_diagnostic_hides_after_insert`**, which
  asserts the rewrite's spec: stale diagnostic = invisible
  marker until the next pull lands. Passes in isolation; can
  flake under full-suite parallel load (LSP round-trip timing).
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

1. **Goldens-harness `wait_ready` fix** — poll for first
   painted body row rather than first PTY byte. Unblocks
   `edge/external_change_while_dirty` (M26's last failure),
   `driver_events/docstore_opened`,
   `driver_events/syntax_buffer_parsed`, and
   `features/git_workspace_open_file`. Same root cause across
   all four.
2. **M27 (GitHub PR)** — last remaining feature milestone.
   `GhPrState` + `driver-gh-pr/`, ETag-driven polling, PR
   comments alongside git gutter, fourth tier on the M20a
   `IssueCategory::NAV_LEVELS` issue-nav cycle.
3. ~~**LSP `workspace/didChangeWatchedFiles`** — M26-followup.~~
   Shipped alongside M26. `client/registerCapability` payloads
   are parsed via `globset`, the runtime memo
   `compute_lsp_watched_file_notifications` fans matching
   `FileWatchEvent`s out as `LspCmd::DidChangeWatchedFiles`,
   and `features/lsp/did_change_watched_files` covers the
   end-to-end path.

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
