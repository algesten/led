# Milestone 23 — Auto-indent, reflow, sort-imports

After M23, three syntax-tree-aware editing operations come online:
language-aware **auto-indent** on `Enter` and on `Tab`, **paragraph
reflow** under `Ctrl-q` (dprint-driven for prose / line-comment
blocks / `/** */` blocks), and **sort imports** under `Ctrl-x i`
(tree-sitter-driven extraction + line sort with the
"already-sorted" alert path). The keybindings `Tab`, `Ctrl-q`,
and `Ctrl-x i` are reserved in the M5/M6 keymap; M23 wires them
through to real handlers.

This is the first milestone where dispatch reaches into the
`Atoms.syntax` source for **on-demand tree queries** (sync,
single-tick, no driver round-trip). Every prior dispatch path has
been "mutate atoms only"; M23 introduces the read-cached-tree-
into-the-dispatch-tick pattern that later milestones will reuse
(outline picker, structural selection, refactor previews). The
discipline is captured in D1.

Prerequisite reading:

1. `docs/spec/editing.md` § "Tab / indent", § "Newline with
   auto-indent", § "Paragraph reflow", § "Sort imports".
   Authoritative behaviour to replicate. Pay particular
   attention to:
   - `pending_indent_row` + `pending_tab_fallback` gating —
     legacy serialises the indent through the syntax driver.
     The rewrite collapses this to a sync compute (D1).
   - `request_indent(row, tab_fallback=true)` — the InsertTab
     path. `tab_fallback=true` means "if the language has no
     indent opinion, insert a soft tab" (D5).
   - `reflow::reflow_buffer` — selects line-comment / block-
     comment / paragraph mode by file extension + cursor row.
   - `sort_imports_text` — preserves `\n`-separated groups,
     returns `None` when already sorted (drives the alert
     dichotomy "Imports sorted" vs "Imports already sorted").
2. `docs/spec/syntax.md` § "Auto-indent and reindent triggers"
   — the tree-sitter contract. Confirms that the rewrite owns
   the tree in `Atoms.syntax` and consumers pull from it on
   demand.
3. Legacy `led/src/model/editing_of.rs:55-127` (newline,
   insert_tab streams), `editing_of.rs:293-353` (sort_imports
   streams). Reference port for the dispatch wiring.
4. Legacy `led/src/model/reflow_of.rs` + `led/src/model/reflow.rs`
   — the dprint wrapper, the prefix detection, the bounds
   walking. Whole reflow.rs (1006 lines incl. tests) ports
   verbatim modulo `BufferState`/`Doc` → `EditedBuffer`/`Rope`
   API translation.
5. Legacy `led/crates/syntax/src/import.rs` (96 lines) +
   `led/crates/syntax/src/indent.rs` (350 lines) + the per-
   language `queries/<lang>/{imports,indents}.scm` files —
   the tree-sitter helpers and the queries we ship.
6. `goldens/scenarios/actions/{insert_tab,reflow_paragraph,
   sort_imports}/`, `goldens/scenarios/keybindings/
   {ctrl_x/i,main/ctrl_q,main/tab}/`, and
   `goldens/scenarios/features/editing_type_delete_reflow/`
   (if it exists) — the seven failures listed in
   `GOLDEN-TODO.md` § Cluster A. Read every `script.txt` /
   `dispatched.snap` / `frame.snap` so the dispatch wiring
   matches what the goldens expect.

---

## Goal

```
$ cargo run -p led -- src/lib.rs
# Cursor on `let x = 1;` (no indent), Tab:
#   Indents to col 5 (4 spaces) inside `fn main() { … }`.
# Cursor on the closing `}`, Tab:
#   Outdents to col 1 (matching the `fn` line's indent).
# Inside that block, Enter at end of `let x = 1;`:
#   New line lands at col 5 (matches the surrounding indent).

$ cargo run -p led -- README.md
# Cursor on a long paragraph, Ctrl-q:
#   Paragraph re-wraps to 100 columns; cursor stays on the
#   row it started on (clamped to new EOL if needed).
# Cursor on a fenced ```rust block, Ctrl-q:
#   Alert "Nothing to reflow"; buffer untouched.

$ cargo run -p led -- main.rs
# (uses imports `std::path::Path`, `std::collections::HashMap`,
#  `std::fs` in arbitrary order)
# Ctrl-x i:
#   Imports rewritten in alphabetical order; cursor stays put;
#   alert "Imports sorted".
# Ctrl-x i again:
#   Buffer unchanged; alert "Imports already sorted".
```

## Scope

### In

- **`crates/state-syntax/src/indent.rs`** — port of legacy
  `led/crates/syntax/src/indent.rs` (350 LOC). Public API:

  ```rust
  use ropey::Rope;
  use tree_sitter::Tree;
  use crate::Language;

  /// Compute the indent string a line should start with given
  /// the current parse tree. `line` is 0-indexed. Returns
  /// `None` when:
  ///   * the language has no `indents.scm`,
  ///   * `line == 0` (no basis row to reference),
  ///   * the tree is in a deep error state and the regex
  ///     fallback also abstains.
  /// Callers fall back to "match previous line's indent" or
  /// "insert one tab unit" depending on context.
  pub fn suggest_indent(
      lang: Language,
      tree: &Tree,
      rope: &Rope,
      line: usize,
  ) -> Option<String>;
  ```

  Behind this, the module ports `find_basis_row`,
  `closing_bracket_indent`, `apply_indent_delta`,
  `detect_indent_unit`, `regex_indent` from legacy. The query
  `Query::new` calls go through a `OnceLock<Query>` per
  language, same shape as `driver-syntax/native`'s
  `grammars_for`. The `indents.scm` source files ship as
  `include_str!("queries/<lang>/indents.scm")` mirroring the
  legacy layout — 9 languages have indent queries (rust,
  typescript, javascript, python, c, swift, toml, json, bash);
  Markdown / Make / Cpp / Ruby return `None` from
  `indents_for(lang)` and `suggest_indent` shorts to `None`
  for them.

  Why this lives in `state-syntax` (not a new crate): the
  helper takes a `tree_sitter::Tree` + `Rope` + `Language` and
  returns a `String`. `state-syntax` already depends on
  `tree-sitter` (it owns `Tree`) and on `ropey`; making this
  a sibling module keeps the helper one `use` away from any
  crate that already reads `SyntaxStates`. No new dep edges.

- **`crates/state-syntax/src/import.rs`** — port of legacy
  `led/crates/syntax/src/import.rs` (96 LOC). Public API:

  ```rust
  /// Find the import block in `tree` and return a sort plan,
  /// or `None` when:
  ///   * the language has no `imports.scm`,
  ///   * no imports were captured,
  ///   * imports are already in sorted order.
  /// The replacement preserves blank-line-separated groups —
  /// each group sorts independently.
  pub fn sort_imports(
      lang: Language,
      tree: &Tree,
      rope: &Rope,
  ) -> Option<SortImportsPlan>;

  pub struct SortImportsPlan {
      pub start_char: usize,    // inclusive
      pub end_char: usize,      // exclusive
      pub replacement: String,  // joined imports, with `\n`
                                // between groups
  }
  ```

  Five languages have `imports.scm` in legacy: rust,
  typescript, javascript, python, swift. The other languages
  in our `Language` enum (markdown, json, toml, c, cpp, ruby,
  bash, make) return `None`. Languages without an imports
  query take the "Imports already sorted" alert path so the
  user gets the same feedback channel.

- **`crates/state-syntax/queries/`** — new directory holding
  one `indents.scm` and (where applicable) one `imports.scm`
  per language. Files copied verbatim from legacy
  `led/crates/syntax/queries/<lang>/`. Total: ~14 small `.scm`
  files. The crate's `build.rs` is unchanged (these queries
  load via `include_str!`, not via build-time compilation).

- **`crates/text-reflow/`** — new portable workspace member,
  no driver, no async, no state. Pure text-in / text-out
  helpers. Public API:

  ```rust
  pub struct ReflowPlan {
      pub start_char: usize,   // inclusive
      pub end_char: usize,     // exclusive
      pub replacement: String,
  }

  /// Run reflow at the cursor row. Returns `None` when the
  /// cursor doesn't sit in a reflowable region (raw code,
  /// inside a fenced code block, on a blank line in a non-
  /// markdown/txt file).
  pub fn reflow_at(
      rope: &Rope,
      cursor_row: usize,
      file_extension: Option<&str>,
  ) -> Option<ReflowPlan>;

  /// Run reflow over a region (mark..cursor). Walks the rows
  /// collecting a plan per comment block / paragraph; sorts
  /// plans descending by `start_char` so applying them in
  /// sequence keeps offsets valid.
  pub fn reflow_region(
      rope: &Rope,
      start_row: usize,
      end_row: usize,
      file_extension: Option<&str>,
  ) -> Option<Vec<ReflowPlan>>;
  ```

  Inside the crate: `dprint_plugin_markdown` (already a
  workspace dep — legacy uses it). The `LINE_WIDTH = 100`
  constant ports verbatim. Prefix detection (`detect_line_comment`
  for `//` / `///` / `//!`, block-middle for ` * `, paragraph
  bounds with fenced-block awareness) ports verbatim;
  `BufferState`/`Doc` calls translate to `Rope::line()` /
  `Rope::char_to_line` / `Rope::line_to_char`.

  Why a separate crate (not in `runtime/src/dispatch/`):
  - Portable (no `state-tabs`, no driver dep) — testable in
    isolation, runs on any `Rope`.
  - Bundles dprint, which is heavy enough that confining the
    dep to one crate keeps `runtime`'s compile time honest.
  - Mirrors the legacy split: `led/src/model/reflow.rs` is
    600 LOC of pure text manipulation that wanted its own
    home; `led/src/model/reflow_of.rs` was the FRP wiring.
    The rewrite splits the same way: pure logic in
    `text-reflow/`, dispatch wiring in `runtime/src/dispatch/
    reflow.rs`.

- **`Command` extensions** in `crates/core/src/command.rs`:

  ```rust
  pub enum Command {
      // … existing variants …

      // M23 — Editing extensions.
      InsertTab,
      ReflowParagraph,
      SortImports,
  }
  ```

  Plus the matching `parse_command` arms:
  - `"insert_tab"` → `Command::InsertTab`
  - `"reflow_paragraph"` → `Command::ReflowParagraph`
  - `"sort_imports"` → `Command::SortImports`

  `Wait(_)` and `KbdMacro*` already shipped with M22.

- **Default keymap additions** in `runtime/src/keymap.rs`:

  ```rust
  m.bind("tab", Command::InsertTab);
  m.bind("ctrl+q", Command::ReflowParagraph);
  m.bind_chord("ctrl+x", "i", Command::SortImports);
  ```

  These three keys are already reserved in the current keymap
  comments (`runtime/src/keymap.rs:273-275, :324-326`) and
  the goldens encode them — no contention. The browser
  context already binds `ctrl+q` to `CollapseAll`; that takes
  precedence inside the side panel via `bind_browser`, which
  is consulted before `direct` (same precedence rule that
  `alt+enter` follows for `OpenSelectedBg`).

- **Reflow dispatch arm** in `runtime/src/dispatch/reflow.rs`
  (new submodule). Shape mirrors `dispatch/save.rs` /
  `dispatch/nav.rs`:

  ```rust
  pub(super) fn reflow_paragraph(
      tabs: &mut Tabs,
      edits: &mut BufferEdits,
      alerts: &mut AlertState,
  ) {
      // 1. Resolve the active tab + buffer; preview tabs
      //    are read-only, no-op.
      // 2. Look at the buffer's PathChain → file extension.
      // 3. If mark is set, call reflow_region(rope, mark_row,
      //    cursor_row, ext); else reflow_at(rope, cursor_row,
      //    ext).
      // 4. None → set_info("Nothing to reflow", …) and bail.
      // 5. Some plans → walk plans descending start_char,
      //    apply each as one Edit::Replace on the rope.
      //    Bump version. Record one undo group via
      //    history.record_replace per plan; close_group
      //    around the whole reflow.
      // 6. Cursor stays on the same row index, clamped to
      //    the new line length. preferred_col = cursor.col.
      // 7. No alert on success — the visible reflow is
      //    feedback enough (legacy parity).
  }
  ```

- **Sort-imports dispatch arm** in `runtime/src/dispatch/sort_imports.rs`
  (new submodule):

  ```rust
  pub(super) fn sort_imports(
      tabs: &mut Tabs,
      edits: &mut BufferEdits,
      syntax: &SyntaxStates,
      alerts: &mut AlertState,
  ) {
      // 1. Resolve active tab + buffer (preview no-op).
      // 2. Pull SyntaxState from atoms.syntax.by_path[path].
      //    No tree yet → set_info("Imports already sorted",…)
      //    and bail. Same alert shape; languages without
      //    syntax support get the legacy-parity "no-op
      //    feedback" path. (Optional: add a separate
      //    "Syntax not ready" alert later — not in M22.)
      // 3. Call state_syntax::import::sort_imports(
      //        lang, &*syn.tree, &eb.rope).
      //    None  → "Imports already sorted".
      //    Some  → apply replacement at [start_char,
      //              end_char), record one undo group,
      //              "Imports sorted", bump version.
      // 4. Cursor: if cursor was inside the import block,
      //    clamp to start of (start_char's row).
      //    Otherwise leave it where it was.
      //    Matches legacy `edit_at` behaviour which preserves
      //    cursor through the rope edit.
  }
  ```

  The `&SyntaxStates` reference goes onto `Dispatcher`. Since
  this is the first dispatch path that reads syntax state,
  see D1 for the access discipline.

- **InsertTab dispatch arm** in `runtime/src/dispatch/edit.rs`
  (extending the existing edit submodule):

  ```rust
  pub(super) fn insert_tab(
      tabs: &mut Tabs,
      edits: &mut BufferEdits,
      syntax: &SyntaxStates,
  ) {
      // Preview tabs no-op.
      // 1. Resolve active EditedBuffer + cursor row.
      // 2. Try state_syntax::indent::suggest_indent(lang,
      //    tree, rope, cursor_row) when atoms.syntax has a
      //    parsed tree. None → fall through.
      // 3. Apply: replace the line's leading whitespace with
      //    the suggested indent string. The cursor lands at
      //    indent_string.chars().count() on the same line.
      //    One undo group; close_group around the rewrite.
      // 4. Fallback (no language, no tree, suggestion
      //    declined): plain tab-stop. Insert spaces from
      //    the cursor's current column up to the next
      //    multiple of 4. This is the `tab_fallback=true`
      //    branch from legacy `request_indent`. Cursor
      //    advances by the inserted span.
      // 5. After the edit, also clear any active mark
      //    (mirrors the kill_mark in legacy edit handlers)
      //    and adjust scroll if the cursor moved past the
      //    visible region.
  }
  ```

  Tab fallback width: hard-coded `TAB_STOP = 4`. Legacy
  reads `Dimensions.tab_stop` which is also hard-coded to 4
  (`POST-REWRITE-REVIEW.md` § "Hardcoded settings" flags it
  for a future config knob; not in M23).

- **InsertNewline auto-indent upgrade** in
  `runtime/src/dispatch/edit.rs::insert_newline`. Today the
  function copies the previous line's leading whitespace
  unconditionally; M23 layers on the tree-driven path:

  ```rust
  // Replace the current literal-copy block with:
  //   1. If atoms.syntax has a tree for this path AND the
  //      language has indents.scm: call suggest_indent for
  //      the about-to-be-created row. Use that as the new
  //      line's prefix.
  //   2. Else: keep the existing "match previous line"
  //      behaviour as the fallback.
  // (See D2 for why we don't need a post-edit re-parse.)
  ```

  The rest of `insert_newline` (cursor placement, undo
  recording, mark clearing) is unchanged.

- **`Dispatcher` field additions** in `runtime/src/dispatch/
  mod.rs`:

  ```rust
  pub struct Dispatcher<'a> {
      // … existing …

      /// Per-buffer parse trees + tokens. M23 dispatch arms
      /// (insert_tab, insert_newline auto-indent,
      /// sort_imports) read trees on demand to derive
      /// indent strings / import-sort plans.
      pub syntax: &'a SyntaxStates,
  }
  ```

  The `syntax` ref is `&` (read-only) — dispatch never
  mutates the tree. Mutation only happens in the ingest
  phase when a `SyntaxOut` arrives.

- **Dispatch wiring** in `Dispatcher::run_command`. Three new
  match arms:

  ```rust
  Command::InsertTab => {
      edit::insert_tab(self.tabs, self.edits, self.syntax);
      DispatchOutcome::Continue
  }
  Command::ReflowParagraph => {
      reflow::reflow_paragraph(
          self.tabs, self.edits, self.alerts);
      DispatchOutcome::Continue
  }
  Command::SortImports => {
      sort_imports::sort_imports(
          self.tabs, self.edits, self.syntax, self.alerts);
      DispatchOutcome::Continue
  }
  ```

  Slot order: between `Command::Wait(_)` and the closing
  `}` of the match. M22 left `Command::Wait` as the trailing
  arm; M23 inserts before it to keep `Wait` visually near
  `KbdMacro*`.

- **Trace lines** — none. None of the three commands cross
  the driver boundary in the rewrite (D1). `dispatched.snap`
  goldens for these scenarios contain only the standard
  startup lines (`FsListDir`, `FileOpen`, `GitScan`, the
  LSP `initialize` exchange) and the absence of any
  dispatch-emitted line is the correct behaviour.

### Out

Per the roadmap and the legacy spec, deliberately **not** in
M23:

- **`reindent_chars`** — the per-language set of characters
  that, when typed, ask for a fresh indent (e.g. `}` in Rust).
  Legacy carries this on every `SyntaxIn` reply and the
  editing layer consults it after every `InsertChar`. The
  rewrite would need either:
    1. A new field on `SyntaxOut` carrying the chars, or
    2. A static `reindent_chars_for(lang)` table.
  Either way it's plumbing that doesn't move a golden.
  `goldens/scenarios/features/auto_indent/*` (which would
  exercise this) is empty on `main`. Defer to a follow-up
  ("M23a — reindent triggers") if a real golden surfaces.
- **Two-stage indent-via-driver** (legacy
  `request_indent` → `Mut::ApplyIndent`). The rewrite
  computes sync against the cached tree; D1 + D2 explain
  why the tree-staleness window doesn't break the goldens.
- **`pending_indent_row` / `pending_tab_fallback`
  serialisation** — a consequence of the above. We don't
  set those flags, we don't gate edits on them, no
  `is_indent_in_flight` predicate.
- **Region reflow via mark + cursor** — `text-reflow`
  exposes `reflow_region` for symmetry with legacy, but
  the M23 dispatch path only calls `reflow_at`. The mark-
  bounded region case has no golden in
  `goldens/scenarios/`; ship the function, wire it later
  when a scenario asks. Same shape as M22's `Wait` arm.
- **`Action::Outline`** — listed as "M18" in the roadmap;
  the alt+o handler is still a stub alert. M23 does not
  touch it.
- **`reflow_buffer` panic safety wrapper** —
  `POST-REWRITE-REVIEW.md` notes legacy assumes dprint
  doesn't panic. We make the same assumption; if it
  panics, the dispatch tick crashes. (`text-reflow`
  already wraps `format_text` in `Result<Option<...>>`;
  we only need to handle the `Err` arm explicitly to
  print "Nothing to reflow".)
- **`Doc` trait abstraction** — legacy reads buffers via a
  `dyn Doc` to keep reflow / sort_imports / indent
  generic. The rewrite is `Rope`-only end-to-end (the
  `Doc` trait was an FRP-era polymorphism point); the
  ports translate `doc.line(Row(r), &mut s)` → `rope.line(r)`
  directly. No new trait, no shim.
- **Modeline-driven language override** — `docs/spec/syntax.md`
  § "Language detection" describes the `# vim: set ft=…`
  override. Not in M23 (independent of indent / reflow /
  sort).

## Architecture conformance

This milestone touches no source / driver / memo
boundaries; it's a dispatch-only feature (three new
`Command` arms, three pure helper modules / crates). Mapped
against the `EXAMPLE-ARCH.md` axes:

- **Sources (§ "Sources: two kinds of ground truth")** —
  no new source. `SyntaxStates` (external-fact, driver-
  populated) and `BufferEdits` (user-decision, dispatch-
  populated) are the two existing sources M23 reads /
  writes; their separation is preserved.
- **Drivers (§ "Drivers: the sync/async split")** — no
  new driver. The three operations are pure CPU work
  (tree query + string ops + dprint), fast enough to run
  on the dispatch tick. There is no peer to communicate
  with, so the sync/async split doesn't apply.
- **Queries (§ "Queries: desired state, not transitions")**
  — `suggest_indent`, `sort_imports`, `reflow_at` are the
  pure-function "given current state, what should be
  true" shape. They aren't `#[drv::memo]`d (D1) because
  every call is a one-shot user-triggered compute with no
  cache to hit.
- **Main loop phases (§ "The main loop")** — dispatch is
  in the ingest phase. M23 reads
  `Atoms.syntax`/`Atoms.edits` and mutates
  `Atoms.edits`/`Atoms.alerts` — a normal ingest-phase
  source mutation, no new wiring.
- **Crate layout (§ "Organizing the code")** — `state-
  syntax/` gains modules (no new crate, no new dep
  edges); `text-reflow/` is a stand-alone utility crate
  (D4). Cross-source composition (the dispatch wiring)
  stays in `runtime/`.
- **Guideline 8 (driver ignorance)** — the helpers don't
  import any driver. `text-reflow` is dep-pure
  (`ropey + dprint-plugin-markdown`). `state-syntax`
  modules add no driver edges.
- **Guideline 9 (crate boundaries)** — every new helper
  has a crate-level boundary (`text-reflow` standalone;
  `state-syntax::indent`/`import` modules in the existing
  state crate); the compiler — not discipline — keeps
  drivers out.
- **Guideline 11 (consumer declares inputs)** — N/A; no
  new memos cross crates.
- **Guideline 14 (zero alloc on idle)** — these dispatch
  arms only fire on the user keystroke. The idle path is
  unaffected. The arms themselves allocate freely while
  building replacement strings — that's the user-active
  path, not the idle path, and is consistent with the
  guideline as worded.

## Key design decisions

### D1 — `Atoms.syntax` is read on-demand by dispatch; no driver round-trip

This is the new pattern M23 introduces. Every prior dispatch
path is "mutate atoms only"; M23's three commands (InsertTab,
InsertNewline auto-indent, SortImports) need to walk a
`tree_sitter::Tree` to compute their effect.

The cleanest place for the read is **inside the dispatch
function**, taking `&SyntaxStates` on the function
signature. Rationale:

- The tree is already in `Atoms.syntax` (M15 put it there);
  no driver round-trip is needed. The
  `EXAMPLE-ARCH.md` § "The execute pattern" applies to
  driver-mediated I/O; M23's operations have no I/O peer.
- The three pure helpers — `suggest_indent`, `sort_imports`,
  `reflow_at` — are precisely the "given the current state
  of everything, what should be true?" pattern from
  EXAMPLE-ARCH § "Query-driven vs reactive". They take
  immutable refs to source data and return a
  `Option<Plan>`. They aren't `#[drv::memo]`d because the
  cache hit rate is zero (each keystroke triggers exactly
  one call). Memoising would add per-call overhead with
  no payback.
- Keeping the read in dispatch — not in `query.rs` — keeps
  paint-side memos free of any indent / import work and
  makes the data-flow obvious (`run_command` is the one
  place that decides what each `Command` does).
- Dispatch is part of the **ingest phase** per
  EXAMPLE-ARCH § "The main loop": "Drain UI events into
  user-decision sources." Reading an external-fact source
  inside ingest to derive what to write is the same shape
  used by the existing `dispatch::nav::next_issue` (reads
  diagnostics + git, mutates tabs) and
  `dispatch::save::request_save_active` (reads tabs +
  edits, mutates store). M23 just extends that precedent
  to `Atoms.syntax`.

The discipline going forward: **dispatch can read external-
fact sources synchronously to compute its mutations.** When
the computation is single-shot (one call per keystroke),
write a plain function. When the computation is per-tick
(every render), write a memo.

### D2 — Sync compute against the pre-edit tree is good enough

Legacy serialises every indent through the syntax driver
because its FRP graph can't reach into the cached tree from
the action stream. The rewrite has no such constraint —
`Atoms.syntax` is a regular field — so we can just read
the tree and compute synchronously.

The trade-off is that the tree may be one or two edits
stale. For our three commands:

- **InsertNewline auto-indent**: the basis row (the row
  ABOVE the new line) existed in the old tree and won't
  have moved. The indent suggestion stays correct; the
  driver re-parses on the next tick anyway and any
  follow-up indent (typing `}` to outdent — out of M23)
  would consult the fresh tree.
- **InsertTab on the cursor row**: the row's structural
  context (basis + nesting) hasn't changed since the last
  parse. If the user typed before pressing Tab, the
  per-char inserts since the last parse are within the
  same line; the basis row's structural classification is
  unchanged.
- **SortImports**: the import block is structural; the
  user typing inside an import won't change which lines
  ARE imports (until the typing produces a syntactically
  invalid block, in which case the tree is in error and
  the imports query returns an empty set → "already
  sorted" alert. Acceptable.).

The risk is a deeply-malformed tree that mis-categorises a
line — e.g. a half-typed `use std::io::` flagged as not-an-
import. Acceptable: the user re-presses `Ctrl-x i` once
the parse settles. No alert spam, no incorrect edits.

### D3 — Tree-not-yet-parsed → fall back, never block

Three first-tick paths can hit dispatch before the syntax
driver has produced a `SyntaxOut`:

- A fresh open: M15 dispatches `SyntaxCmd::Parse` on the
  load completion; the worker takes 5–50ms. A user who
  presses `Tab` immediately races the parse.
- A buffer with no language: `from_chain(path) == None`,
  `Atoms.syntax.by_path` has no entry.
- A buffer in a language we don't have indent / imports
  for (e.g. `Make` for indent, `Markdown` for imports).

In all three cases the dispatch arms degrade gracefully:

- **InsertTab** → tab-stop fallback (insert spaces to next
  multiple of 4). Already what the `keybindings/main/tab`
  golden expects (cursor lands at L1:C5 on a plain
  `buffer.txt` with no language).
- **InsertNewline** → match-previous-line indent (current
  rewrite behaviour, already covered by goldens).
- **SortImports** → "Imports already sorted" alert +
  no-op. Slightly counter-intuitive on a no-language
  buffer ("there ARE no imports"), but it's the legacy
  parity wording (`editing_of.rs:329-353`) and matches
  the `keybindings/ctrl_x/i` golden. A future polish
  could emit "No imports query for this language"; not
  in M23.

### D4 — Reflow lives in its own crate; sort/indent live in `state-syntax`

`text-reflow` is pure text manipulation (no tree-sitter, no
`Language` enum) — it consumes a `Rope` + a file extension
string and returns plans. Putting it in its own crate keeps
the dprint dep contained and makes its 600 LOC of legacy port
testable in isolation. This is EXAMPLE-ARCH § "Guidelines"
guideline 9 ("Enforce driver ignorance with crate
boundaries") generalised: a heavy pure-logic helper earns
its own crate because the alternative is bloating
`runtime`'s compile time and hiding the dep edge inside
the integration crate.

The crate doesn't slot into a named EXAMPLE-ARCH tier
(`state-*` / `driver-*` / `platform-*` / `core/`) because
it's neither a source nor an async wrapper nor a shared
primitive — it's pure-logic CPU work. Existing rewrite
precedent for "stand-alone utility crate that doesn't fit
a tier": `fake-gh/`, `fake-lsp/`, `goldens/`. The named
tiers are organising principles, not exhaustive
classifications.

`indent.rs` and `import.rs` need a `tree_sitter::Tree` plus a
language-keyed query, so the natural home is alongside
`state-syntax`'s existing `Tree`-using code. Moving them to
a separate `syntax-helpers/` crate would be cleaner in
principle (crate-per-helper) but ships zero ergonomic value:
both helpers are read-only on `state-syntax`'s types, and
`state-syntax` already pulls tree-sitter + ropey. Single
crate keeps the dep graph flat.

(Both choices keep the EXAMPLE-ARCH guideline-8 invariant:
no domain driver imports either of these crates. Only
`runtime` does, and `runtime` is the integration crate
where cross-source composition lives.)

### D5 — InsertTab semantics: "indent the line", not "insert one char"

Legacy `Action::InsertTab` is a misnomer: it doesn't insert
a `\t`. It calls `request_indent(cursor_row, tab_fallback=true)`,
which computes the line's correct indent (replacing existing
leading whitespace) and applies it. The fallback (no syntax)
inserts spaces up to the next 4-col tab stop **at the cursor
position**, not at the line start.

The rewrite preserves both semantics:

- **Tree path**: replace the line's leading whitespace with
  `suggest_indent`'s output. Cursor lands at the new
  whitespace boundary. The `actions/insert_tab` golden's
  expected end state (cursor at L2:C5 inside `fn main() {
  let x = 1; }` with the line indented to 4 spaces) is the
  output of this path.
- **Fallback path**: insert spaces from the cursor's
  current column up to the next multiple of 4. Cursor
  advances by the inserted amount. The `keybindings/main/
  tab` golden (cursor at L1:C0 on plain buffer.txt → L1:C5
  with 4 leading spaces) is this path: 4 spaces inserted,
  cursor advances 4 columns.

The two paths diverge in **WHERE the inserted whitespace
goes**: tree path replaces leading whitespace, fallback
inserts at cursor. They share the "ends with cursor at the
end of the inserted/replaced region" outcome, which is what
the goldens key on.

### D6 — Reflow is sync; dprint is bundled; no panic guards

Legacy assumes `dprint_plugin_markdown::format_text` returns
`Ok` or `Err` and never panics. We make the same assumption.
`format_text`'s contract is "syntactically pure, doesn't
read filesystem" — panics would be a dprint bug, not a led
bug. If one ever surfaces, the dispatch tick crashes; the
goldens harness catches the abort and the test fails loudly.

The 100-column constant carries over from legacy (`reflow.rs:6`).
A future `theme.toml` / `settings.toml` could expose it as
`reflow_width`; not in M23. The hardcoded value matches the
ruler at column 110 and the `keybindings/main/ctrl_q` golden's
expected wrap width.

### D7 — Sort-imports overwrites without preserving cursor inside the block

If the cursor sits inside the import block when `Ctrl-x i`
fires, the simplest correct behaviour is to land the cursor
at the start of the rewritten block. Legacy preserves the
cursor row by char offset; that means cursor can land in
the middle of a totally different import line after the
sort. Our cleaner rule:

```
if cursor_char in [start_char, end_char):
    cursor → row of start_char, col 0
else:
    cursor unchanged
```

Both the legacy and the rewrite golden snapshots have the
cursor outside the rewritten block (`actions/sort_imports`
ends at L1:C1 — start of file), so this rule lines up
without a refresh. If a future scenario lands the cursor
inside the block, we may need to revisit; flagged in
`POST-REWRITE-REVIEW.md`.

### D8 — `text-reflow` knows about extensions, not languages

Reflow's mode selection is "is this a `.md` / `.markdown` /
`.txt` file (paragraph mode), or a Rust-style source file
(line / block comment mode)". That's an extension question,
not a `Language` enum question — Markdown WITHOUT a syntax
tree (e.g. on a bare `.markdown` file in a no-language
project) still wants paragraph reflow. Same on `.txt`.

`text-reflow` accepts an `Option<&str>` extension string
and routes on it; the `Language` enum stays inside
`state-syntax`. This keeps the reflow crate's dep graph
pure (`ropey + dprint-plugin-markdown` only).

### D9 — Auto-indent applies on Enter only when the language has indents

Languages we ship indent queries for in M23: rust,
typescript, javascript, python, c, swift, toml, json, bash.
Languages without: markdown, make, cpp, ruby. (We have
syntax queries for all of these but not indent queries.
The `cpp` / `ruby` queries can land in a follow-up.)

When `suggest_indent` returns `None` for any reason, the
existing "match previous line's leading whitespace"
fallback fires. This guarantees Enter still produces a
sensible indent on every file type, including ones with
no syntax support at all.

### D10 — The `text-reflow` crate has no `state-tabs` / `state-buffer-edits` dep

`reflow_at` takes a `Rope`. `reflow_region` takes a `Rope`
and two row indices. Neither knows about `Tab`, `Cursor`,
`EditedBuffer`, or marks — translation between dispatch's
state and reflow's inputs happens in `runtime/src/dispatch/
reflow.rs`. This is the standard "pure helper, dispatch
glues" split that already structures `state-syntax`'s
`rebase_tokens` / `RopeDiff::between` (consumed by
`runtime/src/diag_offer.rs`). M23 just keeps the pattern.

## Types

### `core` additions

`Command` gains three variants. The enum's flat-dep-free
shape (set up in M22 D9) means the new variants ship without
new dependencies:

```rust
// crates/core/src/command.rs
pub enum Command {
    // … existing variants …
    InsertTab,
    ReflowParagraph,
    SortImports,
}
```

`parse_command` adds three arms; the `default_keymap` test
adds three pairs.

### `state-syntax` additions

```rust
// crates/state-syntax/src/lib.rs
pub mod indent;     // new module
pub mod import;     // new module

// indent.rs
pub fn suggest_indent(
    lang: Language,
    tree: &tree_sitter::Tree,
    rope: &ropey::Rope,
    line: usize,
) -> Option<String>;

// import.rs
pub struct SortImportsPlan {
    pub start_char: usize,
    pub end_char: usize,
    pub replacement: String,
}

pub fn sort_imports(
    lang: Language,
    tree: &tree_sitter::Tree,
    rope: &ropey::Rope,
) -> Option<SortImportsPlan>;
```

The crate's `Cargo.toml` already has `tree-sitter` + `ropey`
+ all the per-grammar deps. M23 adds nothing new to the
manifest beyond declaring the two new modules in `lib.rs`.
The query files ship as `include_str!("../queries/<lang>/
indents.scm")`.

### `text-reflow` (new crate)

```rust
// crates/text-reflow/src/lib.rs
use ropey::Rope;

pub struct ReflowPlan {
    pub start_char: usize,
    pub end_char: usize,
    pub replacement: String,
}

pub fn reflow_at(
    rope: &Rope,
    cursor_row: usize,
    file_extension: Option<&str>,
) -> Option<ReflowPlan>;

pub fn reflow_region(
    rope: &Rope,
    start_row: usize,
    end_row: usize,
    file_extension: Option<&str>,
) -> Option<Vec<ReflowPlan>>;
```

Dep graph: `ropey + dprint-plugin-markdown` only. No
`led-core`, no `state-*`, no `driver-*`. Fits the same
"pure portable helper" niche as the proposed-but-not-yet-
created `core-text` would.

### `Dispatcher` additions

```rust
// crates/runtime/src/dispatch/mod.rs
pub struct Dispatcher<'a> {
    // … existing …
    pub syntax: &'a SyntaxStates,
}
```

One new field: read-only borrow of the syntax atom.
`Atoms.syntax` is already in scope at the `let Atoms { … }`
destructure in `runtime/src/lib.rs::run`, so wiring is a
one-line addition there.

## Crate changes

```
crates/
  core/
    src/command.rs        + Command::InsertTab,
                          + Command::ReflowParagraph,
                          + Command::SortImports,
                          + parse_command arms (×3),
                          + default_keymap test rows (×3).
  state-syntax/
    src/lib.rs            + pub mod indent;
                          + pub mod import;
    src/indent.rs         NEW — port of legacy indent.rs.
    src/import.rs         NEW — port of legacy import.rs +
                                SortImportsPlan struct.
    queries/
      rust/
        indents.scm       NEW (copy from legacy).
        imports.scm       NEW (copy from legacy).
      typescript/
        indents.scm       NEW.
        imports.scm       NEW.
      javascript/
        indents.scm       NEW.
        imports.scm       NEW.
      python/
        indents.scm       NEW.
        imports.scm       NEW.
      swift/
        indents.scm       NEW.
        imports.scm       NEW.
      c/
        indents.scm       NEW.
      bash/
        indents.scm       NEW.
      json/
        indents.scm       NEW.
      toml/
        indents.scm       NEW.
  text-reflow/             NEW workspace member.
    Cargo.toml             ropey + dprint-plugin-markdown.
    src/lib.rs             reflow_at + reflow_region +
                           internal port of reflow.rs.
  runtime/
    Cargo.toml             + led-text-reflow dep.
    src/keymap.rs          + tab → InsertTab,
                           + ctrl+q → ReflowParagraph,
                           + ctrl+x i → SortImports.
    src/dispatch/
      mod.rs               + Dispatcher.syntax field,
                           + Command::InsertTab arm,
                           + Command::ReflowParagraph arm,
                           + Command::SortImports arm,
                           + reflow / sort_imports submodule
                             registrations.
      edit.rs              + insert_tab function,
                           + insert_newline rewrites the
                             indent block to call
                             state_syntax::indent::
                             suggest_indent first.
      reflow.rs            NEW submodule — dispatch glue
                           between text-reflow + Dispatcher.
      sort_imports.rs      NEW submodule — dispatch glue
                           between state_syntax::import +
                           Dispatcher.
    src/lib.rs             + Dispatcher{ … syntax: &atoms.
                             syntax, … } at the construction
                             site (already passes &atoms.
                             diagnostics so the pattern is
                             established).
```

New workspace members: `led-text-reflow`. (`state-syntax`
gains modules but is unchanged at the crate level.)

## Testing

### `state-syntax::indent` (unit)
- `suggest_indent` on a bare `fn main() {\n}` Rust source
  → row 1 returns `Some("    ")` (four spaces, the indent
  unit detected from the rust default).
- `suggest_indent` after pressing `}` on a row → returns
  `Some("")` (outdent to enclosing `fn` line's indent).
- `suggest_indent` on row 0 → `None` (no basis row).
- `suggest_indent` on a Markdown file (no indents.scm)
  → `None`.
- `suggest_indent` on a malformed Rust file with a top-
  level `(ERROR)` node → falls back to regex (still
  returns `Some` matching the previous line, when the
  error is shallow).
- `apply_indent_delta` round-trip: `Greater` adds one
  unit, `Less` removes one unit, `Equal` unchanged.
- `detect_indent_unit` on a tab-indented file returns
  `"\t"`; on a 4-space file returns `"    "`; on an
  empty file returns `"    "` (default).

### `state-syntax::import` (unit)
- `sort_imports` on three already-sorted Rust uses → `None`.
- `sort_imports` on three out-of-order uses → `Some(plan)`
  whose `replacement` is the sorted concatenation.
- `sort_imports` on imports separated by a blank line:
  each group sorts independently.
- `sort_imports` on a Python `import a / import b /
  from c import d` block produces sorted output.
- `sort_imports` on a Markdown file → `None` (no query).

### `text-reflow` (unit)
- `reflow_at` on a long markdown paragraph → `Some(plan)`
  wrapping to ≤100 chars.
- `reflow_at` on a `.rs` file with `///` doc-comment block
  → `Some(plan)` keeping the `///` prefix on every
  reflowed line.
- `reflow_at` on a `/** … */` block → `Some(plan)` with
  ` * ` middles preserved.
- `reflow_at` on a fenced code block in a `.md` file →
  `None` (cursor inside fence, untouched).
- `reflow_at` on a blank line in a `.rs` file → `None`.
- `reflow_at` on plain code in a `.rs` file → `None`.
- `reflow_region` over a paragraph + a code-fenced block
  → reflows only the paragraph; fence content preserved
  byte-for-byte.

### `dispatch::edit::insert_tab` (unit)
- No tree, no language: cursor at L1:C0 → cursor at L1:C5
  after Tab; rope gains 4 leading spaces.
- No tree, no language, cursor at L1:C2: cursor at L1:C5
  (next tab stop above current col).
- Rust file with parsed tree: cursor on the body line of
  `fn main() { let x = 1; }` → indent replaces the line's
  leading whitespace with the suggested indent (4 spaces).
- Markdown file (no indents.scm): tab-stop fallback fires.

### `dispatch::edit::insert_newline` (existing)
- Existing tests remain green.
- New: cursor inside a Rust `fn main() { … }` body, Enter
  → new line takes the 4-space indent from the indents.scm
  suggestion.
- New: cursor in a no-language buffer, Enter → falls back
  to copying previous line's leading whitespace (the
  current behaviour).

### `dispatch::reflow` (unit)
- Reflow with no active tab → no-op, no alert.
- Reflow on a code line in a `.rs` file → "Nothing to
  reflow" alert.
- Reflow on a long `///` block → BufferUpdate applied,
  no alert.
- Reflow on a long markdown paragraph → BufferUpdate
  applied, no alert; cursor row preserved.

### `dispatch::sort_imports` (unit)
- Sort imports on a Rust file with shuffled imports →
  BufferUpdate + "Imports sorted" alert.
- Sort imports on the same file twice → first sorts,
  second emits "Imports already sorted".
- Sort imports on a no-language buffer → "Imports
  already sorted".
- Sort imports before the syntax tree has parsed → same.

### `runtime` integration
- Headless run: open `main.rs`, wait 500ms for parse,
  press Tab on `let x = 1;` line — verify the line
  becomes `    let x = 1;` and cursor is at the
  correct column.
- Headless run: open `notes.md`, wait, press Ctrl-q on
  a long paragraph — verify the rope is reflowed.
- Headless run: open a Rust file with shuffled imports,
  wait, Ctrl-x i — verify imports are sorted and alert
  fires.
- Headless run on `buffer.txt` (no language): Tab adds
  4 spaces (`keybindings/main/tab` golden).

Expected: +30 unit tests + 4 integration.

## Done criteria

- All existing tests pass.
- New tests green.
- Clippy: net delta ≤ +2 from post-M22.
- Interactive smoke:
  - Open `cd led-rewrite && cargo run -p led -- src/main.rs`,
    cursor on a top-level `fn`, Tab indents correctly;
    Enter inside a block lands at the matching indent.
  - `Ctrl-q` on this milestone doc rewraps the current
    paragraph to ≤100 columns (the doc has a few of
    those).
  - `Ctrl-x i` on a Rust file with shuffled `use` lines
    sorts them and emits "Imports sorted".
- Goldens (`GOLDEN-TODO.md` § Cluster A, all seven):
  - `actions/insert_tab` — green.
  - `actions/reflow_paragraph` — green.
  - `actions/sort_imports` — green.
  - `keybindings/ctrl_x/i` — green.
  - `keybindings/main/ctrl_q` — green.
  - `keybindings/main/tab` — green.
  - `features/editing_type_delete_reflow` — green if it
    exists on `main`; otherwise authored as part of M23
    on `main` first per the branch rule.
- `GOLDEN-TODO.md` updated: Cluster A removed, totals
  refreshed.

## Growth-path hooks

- **`reindent_chars` triggers** — extend `state-syntax::
  indent` with `reindent_chars_for(lang) -> &'static [char]`;
  dispatch-side `insert_char` checks the table and re-runs
  `suggest_indent` after a triggering keystroke. Lands in
  M23a if real users (or new goldens) ask.
- **Configurable reflow width / tab stop** — both are
  hard-coded constants today (100 / 4). When a settings
  config crate exists, swap them for atoms-resolved
  values.
- **Cpp / Ruby indent queries** — port from upstream
  tree-sitter or write fresh; no architecture impact.
- **Region reflow via mark + cursor** — wire `reflow_
  region` (already in the crate) into the dispatch
  function once a golden exercises the path. Either
  `Action::ReflowParagraph` checks for an active mark
  and switches modes, or a separate `Action::ReflowRegion`
  binds to a different chord.
- **Format-on-save in non-LSP languages** — once
  `text-reflow` exists, the save flow could optionally
  reflow comment blocks before the disk write. Out of
  scope for M23 (legacy doesn't do it either; flagged in
  `POST-REWRITE-REVIEW.md` § Hardcoded settings as
  potential future work).
- **`reflow_buffer` panic capture** — wrap the dprint
  call in `std::panic::catch_unwind` to translate a
  panic into a "Reflow failed" alert. Defer until a
  panic actually shows up; speculative reliability work
  isn't M23's burden.
