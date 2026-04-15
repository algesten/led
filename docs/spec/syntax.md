# syntax

## Summary

led parses open buffers with tree-sitter to produce **syntax highlighting**,
**bracket-pair matching** (including rainbow nesting), **auto-indent
suggestions**, and the **declared re-indent trigger character set** used by
the editing layer. Parsing is performed by the `syntax` driver, which keeps
one `SyntaxState` per buffer, applies incremental edits when possible, and
re-runs queries only when the doc or viewport changes. Every response is
stamped with the `DocVersion` it was computed against, so the model can
reject stale results and keep the displayed highlights aligned with the
buffer's current contents — even after the buffer has moved on.

## Behavior

### Language detection

Language detection runs in two layers with modeline priority:

1. **Buffer constructor pre-resolution.** When a buffer is constructed,
   `LanguageId::from_chain(&path_chain)` walks every link in the path chain
   (user-typed path, intermediate symlinks, resolved target) and returns
   the first `(extension | well-known filename)` match. Extensions
   recognized today are `rs`, `ts`, `tsx`, `js`, `jsx`, `mjs`, `py`, `c`,
   `h`, `cpp`, `hpp`, `cc`, `cxx`, `hxx`, `swift`, `toml`, `json`, `sh`,
   `bash`, `rb`, `md`, `markdown`, `mk`. Well-known filenames include
   `Makefile`, `Gemfile`, `Rakefile`, `.profile`, `.bashrc`, `Pipfile`,
   `.babelrc`, `SConstruct`, `Snakefile`, etc. (the full table is in
   `crates/core/src/language.rs::filename_to_extension`). The chain
   traversal means user-typed dotfile names win over canonical paths: a
   `.profile` symlink to `/home/user/dotfiles/profile` is detected as
   Bash because `.profile` is a well-known filename even though `profile`
   is not.
2. **Modeline override inside the driver.** `SyntaxState::from_language_and_doc`
   scans the first few lines for Emacs (`-*- mode: ruby -*-`) or Vim
   (`vim: set ft=ruby`) modelines; a match **wins over** the
   extension-based `LanguageId`. This lets shell scripts with `#!` shebangs
   and extensionless files declare their language in content.

If no language is resolved, the driver produces no highlights, no bracket
pairs, and no `SyntaxState` — the buffer still edits, just without
colorization. The driver does forward `indent` responses (as `None`) when
`indent_row` is set, so the editing layer's "ask for indent" flow still
resolves even on unknown languages.

### Parse lifecycle

The driver keeps per-buffer state keyed by `CanonPath`:

- `SyntaxState` — tree-sitter parser + tree + compiled queries for
  highlights, indents, brackets, outline, injections, imports, and errors,
  plus regex patterns for `increase_indent_pattern` / `decrease_indent_pattern`.
- `last_ver: DocVersion` — last parsed version.
- `last_doc: Arc<dyn Doc>` — last parsed doc (used to replay multi-op
  edits using pre-edit byte positions).
- `last_scroll`, `last_end_line` — viewport cached for highlight range.
- `cached_highlights`, `cached_brackets` — reused when only viewport
  changes.
- `reindent_chars: Arc<[char]>` — characters that, when typed, cause the
  editing layer to ask for a fresh indent (forwarded in every response).

On each batch of `SyntaxOut::BufferChanged` commands the driver **coalesces**
per path (merging `edit_ops`, keeping the latest `doc`/`version`/
`scroll_row`/`buffer_height`/`indent_row`). It then:

1. If the buffer is newly seen, run language detection and create
   `SyntaxState`; if detection fails and `indent_row` is set, emit an empty
   `SyntaxIn` so the editing layer doesn't stall; otherwise skip the path.
2. If `version != last_ver`, apply the new edit ops. For a single op, call
   `apply_edit_op(op, doc)`. For multiple ops, replay each against a
   shadow `Doc` walked forward with the pre-edit state so tree-sitter's
   byte offsets are correct, then call `finish_edits(doc)` for one
   reparse. If `edit_ops` is missing or shorter than the version gap, fall
   back to a full `reparse(doc)`.
3. Compute the visible range `[scroll_row, min(scroll_row + buffer_height
   + 5, line_count))` and, if the doc changed or the viewport moved,
   recompute `highlights` and `bracket_pairs` for that range. Highlights
   are stored as `Vec<(Row, HighlightSpan)>`; brackets are mapped from
   tree-sitter byte ranges into `BracketPair { open_line, open_col,
   close_line, close_col, color_index }`.
4. If `indent_row` is set, compute `compute_auto_indent(doc, row)` — the
   indentation string the editor should use when (re)indenting that row.
5. Emit a `SyntaxIn { path, doc_version, highlights, bracket_pairs,
   matching_bracket: None, indent, indent_row, reindent_chars }`.

`matching_bracket` is intentionally always `None` from the driver; the
cursor-aware match is computed downstream by
`BufferState::update_matching_bracket` from the cached bracket pairs when
highlights land and after every cursor move. Rainbow depth is assigned by
`assign_rainbow_depth` in the syntax crate and travels on each
`BracketPair.color_index` (theme keys `bracket.rainbow.{0..N}`).

### Version stamping and rebase

**This is the load-bearing contract for the rewrite.** Every `SyntaxIn`
carries the `doc_version` it was computed against. Because parsing and
query evaluation can take time, by the time the result lands the buffer
may already have moved on. The acceptance rule lives on `BufferState`:

```rust
// BufferState::offer_syntax — pseudocode
if self.version == version {
    self.syntax_highlights = highlights;
    self.bracket_pairs = bracket_pairs;
    self.update_matching_bracket();
    true
} else {
    false  // stale — drop
}
```

For indent suggestions, `Mut::ApplyIndent` is gated by two checks
(`syntax_will_indent` and `syntax_can_apply_indent` in `model/mod.rs`):
the response must reference the current `pending_indent_row`, and the
version must match the current buffer. When the row matches but the
version doesn't, the editing layer still accepts the `reindent_chars`
update (via `Mut::SetReindentChars`) so the next keystroke can trigger a
fresh indent request.

**What the rewrite must preserve:** highlights/brackets/indents are
*opportunistic* — if a later edit raced the parse, the response is dropped
and the next `SyntaxOut::BufferChanged` (emitted automatically by the
viewport/sequence observers in derived) produces a fresh attempt at the
new version. Rebase/replay of stale highlight offsets against newer edits
is **not** performed and is **not** required; drop-and-reparse is the
contract. The rewrite may, however, keep pre-existing highlights on
display while a newer version's parse is in flight (the current
implementation does exactly this — `cached_highlights` on `BufferState` is
only overwritten on a version-matched accept).

### Viewport-only updates

When only the active buffer's cursor or scroll changes (no doc edit), a
separate derived stream emits a `SyntaxOut::BufferChanged` with empty
`edit_ops`. The driver recomputes highlights/brackets for the new viewport
without reparsing. This is what keeps coloring correct when scrolling past
the previously-highlighted range.

### Buffer close

`SyntaxOut::BufferClosed { path }` removes the per-buffer state entry.
Fire-and-forget; no response.

### Auto-indent and reindent triggers

The editing layer sets `pending_indent_row` on the buffer when it wants
the syntax driver to compute an indent (e.g. after Enter, after typing a
character in `reindent_chars`). The syntax driver reads `indent_row` off
the next `BufferChanged`, runs `compute_auto_indent`, and returns the
suggested indent string (or `None` if the language's indent config has no
opinion). The model's `ApplyIndent` path then either applies the
computed indent or, if the buffer had a tab fallback pending, inserts a
soft tab.

`reindent_chars` is an `Arc<[char]>` carried on every `SyntaxIn` and
cached on the buffer. The editing layer consults it to decide when typing
a character should raise an indent request.

## User flow

User opens `main.rs`: pre-resolved `LanguageId::Rust` reaches the driver,
which builds a `SyntaxState`, parses, and returns highlights. The golden
frame shows the Rust source colored. User starts typing `fn foo(` — each
keystroke bumps the buffer version, the edit ops ride the next
`BufferChanged`, the driver applies them incrementally, re-queries the
viewport, and returns new highlights stamped with the new version. User
presses `Enter`: the editing layer sets `pending_indent_row`, the driver
computes the indent, and the response carries both highlights and a
suggested indent string. User pastes a large block: the multi-op edit
replay ensures tree-sitter sees correct byte positions, then one reparse
settles it. User moves the cursor onto an open paren: the cached bracket
pairs yield a `matching_bracket` for the close-paren, which the renderer
highlights. User opens a `.profile` file: `LanguageId::from_chain` finds
it via the well-known filename table and it parses as Bash. User opens a
file whose first line is `# vim: set ft=ruby :`: the driver's modeline
override re-routes parsing to Ruby regardless of the pre-resolved
language.

## State touched

- `BufferState.syntax_highlights: Rc<Vec<(Row, HighlightSpan)>>`
- `BufferState.bracket_pairs: Rc<Vec<BracketPair>>`
- `BufferState.matching_bracket: Option<(Row, Col)>` — computed from
  cursor + bracket pairs.
- `BufferState.reindent_chars: Arc<[char]>`
- `BufferState.pending_indent_row: Option<Row>` / `pending_tab_fallback`
- `BufferState.pending_syntax_request: Option<SyntaxRequest>` — `Full`
  (new buffer, viewport change) or `Partial { edit_ops }` (post-edit).
- `BufferState.pending_syntax_seq: SyntaxSeq` — bumped on every request.
- `BufferState.version: DocVersion` — the acceptance gate.
- `BufferState.language: Option<LanguageId>` — pre-resolved at construction.

## Extract index

- Actions: none directly user-named. The reflow / import-sort actions
  consume `SyntaxState` via `from_chain_and_doc`. → `docs/extract/actions.md`
- Keybindings: none specific to syntax.
- Driver events:
  - `SyntaxOut::BufferChanged { path, language, doc, version, edit_ops,
    scroll_row, buffer_height, cursor_row, cursor_col, indent_row }`
  - `SyntaxOut::BufferClosed { path }`
  - `SyntaxIn { path, doc_version, highlights, bracket_pairs,
    matching_bracket, indent, indent_row, reindent_chars }`
  → `docs/extract/driver-events.md`
- Timers: none. (Coalescing is intra-driver and `Schedule::Replace`-free.)
- Config keys: theme colors via `HighlightSpan` theme keys
  (`bracket.rainbow.*`, language-family keys). → `docs/extract/config-keys.md`

## Edge cases

- **Unknown extension, no modeline.** No `SyntaxState`; buffer displays
  uncolored. Indent responses, if requested, come back as `None`. Editing
  still works.
- **Modeline conflicts with extension.** Modeline wins (e.g. `.py` file
  with `# vim: set ft=ruby :` parses as Ruby).
- **Binary / non-UTF8 file.** [unclear — the `Doc` layer controls whether
  the buffer ever opens; if it does, tree-sitter runs against the byte
  content. Malformed parses degrade to `(ERROR)` nodes which are captured
  by the `error_query` for rendering.]
- **Very long line / very large file.** Highlights are computed only for
  `[scroll_row, scroll_row + buffer_height + 5]`. Full reparse happens on
  every version jump that can't be replayed; performance scales with
  tree-sitter's incremental edit path.
- **Viewport beyond EOF.** `end_line` is clamped to `doc.line_count()`;
  bracket matches outside `[0, doc.len_bytes())` are filtered.
- **Nested language injection** (e.g. a regex inside Rust). Handled by
  `injection_layers`, built on each reparse from the `injections` query.
- **Rainbow depth across multiple bracket kinds.** `assign_rainbow_depth`
  computes nesting over the visible bracket set; color indices are stable
  for the duration of a render.
- **Cursor-driven `matching_bracket`.** Recomputed by `BufferState` on
  every cursor move and after every accepted syntax response; clears when
  the cursor leaves any bracket.
- **Stale response arrives after a buffer is closed and another opened at
  the same path.** The driver keyed by `CanonPath` — closes drop state,
  so the new buffer starts fresh; the stale response (if any) is version-
  rejected on the buffer side.
- **Shadow-doc replay bounds.** The multi-op replay clamps each op's
  offset to the current shadow length to avoid panics on inconsistent
  edit_ops; if the shadow falls out of sync, the fallback `reparse(doc)`
  still produces a correct tree.

## Error paths

- **Parser / query compile failure** (missing grammar, malformed query).
  `SyntaxState::from_entry` returns `None` — the buffer is treated as
  "no language" for the remainder of its lifetime in that driver. No
  alert; editing continues.
- **Edit op replay vs. doc mismatch.** If `new_op_count > edit_ops.len()`,
  the driver falls back to a full reparse. Highlights may be momentarily
  empty but will be correct on the next response.
- **Indent query failure / language with no indent config.** `indent`
  field is `None`; the editing layer's tab-fallback path may still insert
  a soft tab.
- **Tree-sitter `InputEdit` inconsistency.** Guarded by the shadow-doc
  replay; if it still produces garbage, the next full parse corrects it.

[unclear — there is no explicit user-visible error path for "syntax driver
crashed." The driver runs on a `LocalSet` task; panics would be observable
as an absence of further `SyntaxIn` events. The rewrite should decide
whether a restart/alert story is required or the current silent-continue
behavior is the spec.]
