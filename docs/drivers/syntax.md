# Driver: syntax

## Purpose

The syntax driver runs tree-sitter incrementally over every open buffer
for which a language can be identified, producing per-viewport highlight
spans, bracket pairs, and auto-indent suggestions. It is a resource
driver — every `SyntaxIn` is the response to a `SyntaxOut::BufferChanged`
— with a built-in coalescer so that a burst of edits collapses to a
single reparse. Language resolution happens in two stages: the
model-side buffer constructor resolves a `LanguageId` from the path
chain (symlink-aware); the driver additionally honours in-file
modelines (`vim:` / `emacs:` hints) which can override the filename-
based guess.

**Version stamping is a first-class feature.** Every `SyntaxIn` carries
a `doc_version: DocVersion` copied through from the request. The model
compares it against `buf.version()` at apply time (`syntax_can_apply_indent`
and rebase checks); stale responses produce highlights but not indent
application.

## Lifecycle

Started once at startup; single tokio task runs forever on
`spawn_local`. The driver keeps a persistent `HashMap<CanonPath,
BufSyntax>` of per-buffer parse trees and incremental state. Buffers
are auto-initialised on first `BufferChanged` (not on a separate "open"
message) and torn down on `SyntaxOut::BufferClosed`. Orphan buffer
lifecycle is derived-side: `syntax_lifecycle` watches the set of
materialized buffer paths and emits `BufferClosed` for removed paths.

No shutdown handshake — there is no `SyntaxIn::Closed` ack.
`BufferClosed` is fire-and-forget.

## Inputs (external → led)

- None directly. Every input is a `SyntaxOut` command from the model.
- Indirectly: doc content is passed by reference through
  `Arc<dyn Doc>` — the driver doesn't reach out to the filesystem or
  docstore.
- Tree-sitter parsers and queries are compile-time embedded (see
  `crates/syntax/src/language.rs`).

## Outputs from led (model → driver)

| Variant                                 | What it causes                                                       | Async? | Returns via                       |
|-----------------------------------------|----------------------------------------------------------------------|--------|-----------------------------------|
| `SyntaxOut::BufferChanged { path, language, doc, version, edit_ops, scroll_row, buffer_height, cursor_row, cursor_col, indent_row }` | Coalesce with any pending messages for the same path; update parse tree if `version != last_ver`; recompute highlights/brackets if doc or viewport changed; compute auto-indent if `indent_row` set | yes, via `spawn_local` | `SyntaxIn { ... }` (always one per command after coalesce) |
| `SyntaxOut::BufferClosed { path }`      | Drop the `BufSyntax` entry; also cancels any pending `BufferChanged` for that path during the same coalesce window | yes    | (none — fire-and-forget)          |

See `crates/syntax/src/lib.rs:32-53` for the exact shape.

## Inputs to led (driver → model)

| Variant                                                                                                               | Cause                                                    | Frequency                                    |
|-----------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------|----------------------------------------------|
| `SyntaxIn { path, doc_version, highlights, bracket_pairs, matching_bracket, indent, indent_row, reindent_chars }`    | One per coalesced batch per path; carries latest version | Per edit burst + per viewport change (coalesced — typing collapses to ~1) |

`SyntaxIn` is a struct, not an enum. Every path carries a full snapshot;
downstream code routes via `doc_version` compared to current buffer
version. The model splits `SyntaxIn` into three child streams keyed on
two predicates (`syntax_will_indent`, `syntax_can_apply_indent`):

1. `syntax_highlights_s` — applies when indent won't land
   (`Mut::SyntaxHighlights`).
2. `syntax_indent_s` — applies indent when `indent_row` matches and
   version is current (`Mut::ApplyIndent`).
3. `syntax_reindent_s` — sets `reindent_chars` only, when the indent
   will apply but a newer edit has already landed
   (`Mut::SetReindentChars`).

## State owned by this driver

Per open buffer with a language (`crates/syntax/src/lib.rs:85-94`):

- `state: SyntaxState` — the tree-sitter `Tree`, compiled queries
  (`highlights`, `indents`, `brackets`, `outline`, `injections`,
  `imports`), injection layers, and language-level config (reindent
  chars, increase/decrease regex).
- `last_ver: DocVersion` — version the tree is currently in sync with.
- `last_doc: Arc<dyn Doc>` — the doc that was last parsed against.
  Used as the pre-edit reference when replaying multi-op edit
  sequences.
- `last_scroll: Row`, `last_end_line: Row` — the viewport that was
  last highlighted, used to skip recompute when nothing moved.
- `cached_highlights: Rc<Vec<(Row, HighlightSpan)>>` — the last
  emitted highlight set, reused until doc or viewport changes.
- `cached_brackets: Vec<BracketPair>` — same caching for brackets.
- `reindent_chars: Arc<[char]>` — cheap clone of the language's
  trigger-reindent character set.

Driver-global: a single `HashMap<CanonPath, BufSyntax>` owned by the
async task. No locks — it's a single-consumer loop.

## External side effects

None. Tree-sitter parsing is in-process; no filesystem, no network, no
processes spawned. Memory use scales with open buffers × their size.

## Known async characteristics

- **Latency**: tree-sitter parse is fast on incremental edits
  (microseconds to low ms). Full reparse on a large file (megabyte-
  scale source) can hit 10–50 ms `[unclear — no recorded benchmarks]`.
  Highlights query execution dominates on viewport change for huge
  files; the driver limits work to `scroll_row..scroll_row+buffer_height+5`.
- **Ordering**: single-consumer mpsc drain → commands serialize per
  driver instance. Within a single `cmd_rx.recv()` wake, the coalescer
  drains *all* queued messages and merges per-path (extending
  `edit_ops`, keeping the latest version). This is the key
  performance lever — rapid typing produces one reparse per batch,
  not per keystroke.
- **Cancellation**: no explicit mechanism, but the coalescer
  effectively cancels: if two `BufferChanged` events for the same path
  arrive in the same batch, only the second version's reparse happens.
  Plus, if `BufferClosed` arrives in the same batch, the `BufferChanged`
  is dropped entirely (`pending.remove(&path)` at `lib.rs:159`).
- **Backpressure**: mpsc bounded at 64 on both command and result
  channels. Overflow drops via `try_send` in the `out.on` bridge;
  harmful only for the `BufferClosed` path (a dropped close leaks a
  `BufSyntax` entry until next close for that path).

## Translation to query arch

| Current behavior                                                    | New classification                                                    |
|---------------------------------------------------------------------|-----------------------------------------------------------------------|
| Reacts to `BufferChanged` with full snapshot                         | Resource driver for `Request::ParseSyntax { path, version, doc, scroll, buffer_height, indent_row, language }` |
| Coalesces bursts per path                                           | Dispatcher-side dedupe: if a `ParseSyntax` for the same path is already pending, replace with the newer request rather than queue |
| Reacts to `BufferClosed`                                            | `Request::DropSyntax { path }` (fire-and-forget), **or** driver observes `Event::BufferDematerialized` via input-driver channel |
| Emits `SyntaxIn` with viewport highlights + indent + reindent_chars | `Event::SyntaxParsed { path, doc_version, highlights, bracket_pairs, indent, indent_row, reindent_chars }` |
| Per-path incremental state in the driver                             | Stays in the driver — this is intrinsically stateful (the tree-sitter `Tree` wants to be reused); not a pure request/response |

The driver's stateful nature (the persistent per-path parse tree) means
it is *not* a pure function of `(doc, version)`. It benefits from
continuity across calls for a given path. The rewrite should preserve
that: the dispatcher can still treat it as request/response from
outside, but inside the driver the same per-path `BufSyntax` table
remains.

## State domain in new arch

- `SyntaxState` (per-buffer, inside `BufferState` or a sibling atom):
  `highlights: Loaded<Vec<(Row, HighlightSpan)>>`,
  `bracket_pairs: Vec<BracketPair>`,
  `reindent_chars: Arc<[char]>`,
  `last_parsed_version: DocVersion`.
- Auto-indent is transient, not state — it's consumed once by the
  `ApplyIndent` reducer and the `pending_indent_row` on the buffer is
  cleared. No atom slot needed.

## Versioned / position-sensitive data

**Entirely version-sensitive.** This is the archetypal driver that
drove the introduction of `DocVersion` stamping in the codebase.

Current behavior (`crates/syntax/src/lib.rs:220-255`):

1. The driver receives `version: DocVersion` and `edit_ops: Vec<EditOp>`.
2. If `version != bs.last_ver`, it computes `new_op_count = *version -
   *last_ver`. If the incoming `edit_ops` contains at least that many
   trailing ops (`edit_ops.len() >= new_op_count`), it replays just the
   new ones into the tree-sitter tree via `mark_edit` / `finish_edits`
   (or `apply_edit_op` for the single-op case).
3. Otherwise (insufficient ops — happens on session restore, after a
   coalesce that dropped ops, or on viewport-only requests that carry
   `edit_ops: vec![]`) the driver falls back to a full `reparse(&*doc)`.

Model-side rebase is done differently: `syntax_can_apply_indent`
(`model/mod.rs:1451-1459`) checks `buf.version() == syn.doc_version`
before applying indent. If the user typed while the driver was parsing,
`indent` lands on a stale row — the model drops it and only keeps
`reindent_chars`. Highlights, by contrast, are applied even when stale;
the next parse (triggered by the newer edit) will correct them within
a frame.

**Implications for rebase query**:

- The version stamp is `DocVersion`, already monotone. The dispatcher
  should store `last_parsed_version` per path so the UI can show
  "parsing..." when `buf.version() > last_parsed_version`.
- Highlights don't need to be rebased row-by-row — they are
  wholesale-replaced by the next parse. Consequence: the UI briefly
  shows stale colours during fast typing; acceptable and matches
  current behavior.
- Indent needs version-match rebase: if `doc_version` doesn't match
  the buffer's current version, don't apply. This is the *only*
  field in `SyntaxIn` that has a true rebase semantic (drop on
  mismatch).
- Bracket pairs and reindent_chars follow the same wholesale-replace
  semantics as highlights.

**The rewrite must preserve `doc_version` in `Event::SyntaxParsed`.**
Without it, stale indents land on moved rows and wreck the buffer.

## Edge cases and gotchas

- **Modeline detection runs inside the driver, not the model.** Even
  though the model pre-resolves `LanguageId::from_chain(&chain)` and
  passes it as `language`, `from_language_and_doc` still calls
  `detect_language_from_modeline` first and only falls back to the
  pre-resolved id. A `# vim: set ft=ruby :` line in a `.profile` file
  therefore overrides `profile` (no lang) with `ruby`.
- **Buffers with no language produce a degenerate `SyntaxIn` only
  when `indent_row.is_some()`.** `lib.rs:200-216`: if
  `SyntaxState::from_language_and_doc` returns `None`, the driver
  normally skips — but if `indent_row` was requested, it emits an
  empty `SyntaxIn` with `indent: None, reindent_chars: []` so the
  model's indent chain has a response to consume. Without this, the
  `pending_indent_row` on the buffer would never clear.
- **Coalescing merges `edit_ops` by *extension*.** Two
  `BufferChanged`s in the same batch for one path accumulate all
  their `edit_ops` into the second one, with `version` set to the
  second's version. The `new_op_count = ver_b - ver_a` then
  correctly covers both batches.
- **`indent_row` merge prefers the newer non-None.** `existing.indent_row
  = indent_row.or(existing.indent_row)` — if the newer batch has
  `None`, the older one's `Some(row)` is retained. This is important
  because viewport-only follow-up events (`scroll_row` changed but
  `indent_row: None`) shouldn't cancel a pending indent request from
  the earlier edit.
- **`last_scroll` is initialised to `Row(usize::MAX)`.** Guarantees
  the first `BufferChanged` for a newly-opened buffer triggers a
  viewport recompute (any real scroll row compares unequal).
- **`cached_highlights` is an `Rc`, cloned into every `SyntaxIn`.**
  The downstream chain stores it in `AppState`; structural sharing
  means the vector itself isn't copied on common paths. Preserve
  this in the rewrite — cloning highlights on every frame would be
  expensive.
- **`BracketPair`s are computed from `state.bracket_ranges` for the
  viewport byte range.** Brackets outside the viewport aren't
  emitted, so a pair that straddles the viewport boundary (open inside,
  close outside) is absent. `matching_bracket` is left `None` by the
  driver; the model computes it from the emitted pairs and the cursor
  position. `[unclear — confirm the model-side match logic lives in
  the highlights consumer, not a separate stream]`.
- **The `.stream()` at end of combinator chains is the FRP idiom for
  "materialize into a concrete Stream."** Preserve rewiring but not
  the specific method if the new arch is post-FRP.
- **`BufferClosed` is the only fire-and-forget output.** The driver
  has no way to report a close failure. Consequence: if a
  `BufferClosed` and a `BufferChanged` for the same path race (model
  sends `Closed`, then a late `Changed` from before the close
  re-queues), the driver re-auto-initialises. Harmless in practice
  (the new `BufSyntax` entry lingers until the next real close).

## Goldens checklist

Scenarios under `tests/golden/drivers/syntax/`:

1. `buffer_parsed/` — exists. Open a `.rs` file; assert `SyntaxIn`
   fires with non-empty highlights for the viewport rows.
2. `language_unknown/` — open a file with no recognisable extension
   and no modeline; assert no `SyntaxIn` fires.
3. `modeline_override/` — open a `.profile` with `# vim: set ft=bash :`;
   assert highlights use bash, not "no language."
4. `incremental_edit/` — open a `.rs`, type a character, assert a
   `SyntaxIn` arrives with the updated highlights and the new
   `doc_version`.
5. `edit_burst_coalesce/` — type rapidly (N keystrokes in <1 frame);
   assert fewer than N `SyntaxIn` events via the trace (validates
   coalesce).
6. `viewport_scroll/` — open a long file, scroll past `buffer_height +
   5`; assert a new `SyntaxIn` with a different `scroll_row` range.
7. `indent_applied/` — type a newline after `fn foo() {`; assert
   `ApplyIndent` lands with the language's indent string.
8. `indent_stale_version/` — force the ordering where a new edit lands
   between dispatch and response; assert indent is *not* applied but
   `reindent_chars` is still set. Needs a harness hook.
9. `buffer_closed/` — close a buffer, assert the driver's internal
   `BufSyntax` entry is dropped (observable via a follow-up
   `BufferChanged` triggering a full reparse rather than incremental).
10. `matching_bracket/` — cursor on a `(`, assert `matching_bracket`
    resolves in the model from emitted `bracket_pairs`.
