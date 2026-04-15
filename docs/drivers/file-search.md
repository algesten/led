# Driver: file-search

## Purpose

The `file-search` driver is a resource driver for ripgrep‑style
workspace‑wide text search and replace. A `FileSearchOut::Search` runs the
query over every file reachable from `root` via `ignore::WalkBuilder`
(honouring `.gitignore`, `.ignore`, `.git/info/exclude`, and global gitignore),
collects up to 1000 hits, and returns a sorted `Vec<FileGroup>`. A
`FileSearchOut::Replace` performs the same search plus an in‑place atomic
rewrite of matched files, then re‑runs the search to return fresh results.
The driver coalesces rapid commands to the latest only — typing a long query
collapses to one actual search run.

See `crates/file-search/src/lib.rs` (118 lines) for the driver shell and
`crates/file-search/src/search.rs` (210 lines) for the grep and replace
implementations.

## Lifecycle

- **Start**: `driver(out: Stream<FileSearchOut>) -> Stream<FileSearchIn>`
  spawns one `tokio::spawn` task that drains the command mpsc, discards all
  but the latest command per blocking cycle, and dispatches to
  `tokio::task::spawn_blocking` for the CPU‑bound grep
  (`crates/file-search/src/lib.rs:51-107`).
- **Stop**: mpsc drop exits the worker naturally.
- **`--no-workspace`**: the driver runs identically, but the *model side*
  gates on workspace root: `model/file_search.rs:110-114` falls back to
  `state.startup.start_dir` when no workspace is loaded. So search still
  works but the root is the cwd, not a git root.

## Inputs (external → led)

1. **Files on disk**, traversed via `ignore::WalkBuilder`
   (`crates/file-search/src/search.rs:30-35` and `:168-173`) with defaults:
   - `hidden(true)` — skip dot‑files
   - `git_ignore(true)`, `git_global(true)`, `git_exclude(true)` — respect
     gitignore layers
   - No `file_type` restrictions beyond `is_file()`
2. **Regex engines**:
   - `grep_regex::RegexMatcherBuilder` for the search pass
     (`search.rs:21-24`). Case‑insensitive flag is inverted
     (`case_insensitive(!case_sensitive)`).
   - `regex::RegexBuilder` for the replace pass (`search.rs:122-125`).
     When `use_regex=false`, both passes use `regex_syntax::escape(query)`
     so the query is treated as a literal.
3. **Binary detection**: the searcher is built with
   `BinaryDetection::quit(0x00)` — any file containing a NUL byte is
   skipped.

## Outputs from led (model → driver)

Values of `FileSearchOut` (`crates/file-search/src/lib.rs:8-25`):

| Variant                                                                        | What it causes                                                                                                                                | Async? | Returns via                                  |
|--------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------|--------|----------------------------------------------|
| `Search { query, root, case_sensitive, use_regex }`                            | Walk `root`, grep every file, collect hits, sort by `relative` path, cap at 1000 total hits                                                    | yes (spawn_blocking) | `FileSearchIn::Results { results }`           |
| `Replace { query, replacement, root, case_sensitive, use_regex, scope, skip_paths }` | Either replace one match (`ReplaceScope::Single`) or every match (`ReplaceScope::All`, skipping `skip_paths`), atomically via tmpfile‑rename, then re‑search | yes                  | `FileSearchIn::ReplaceComplete { results, replaced_count }` |

Dispatchers in `led/src/derived.rs:572-585` (search) and `:587-600`
(replace) — both triggered by versioned `AppState` fields
(`pending_file_search`, `pending_file_replace`) that are populated by
model/file_search.rs:117-123 (search) and :612-628 / :680-697 (replace).

## Inputs to led (driver → model)

| Variant                                               | Cause                                                           | Frequency                                                                                                       | Consumed in                                                                                                                        |
|-------------------------------------------------------|-----------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|
| `Results { results: Vec<FileGroup> }`                 | Response to `Search`                                             | Per search query (coalesced: only the latest command between two blocking cycles runs)                          | `led/src/model/mod.rs:541-555` → `Mut::FileSearchResults` — stores in `state.file_search.results`, rebuilds `flat_hits`.          |
| `ReplaceComplete { results, replaced_count }`         | Response to `Replace`                                           | Per replace‑all confirmation; also per single‑replace of a non‑open file                                         | `led/src/model/mod.rs:557-574` → `Mut::FileSearchReplaceComplete` — updates results, emits "Replaced N" alert.                     |

Types (defined in `crates/state/src/file_search.rs`):

- `FileGroup { path: CanonPath, relative: String, hits: Vec<SearchHit> }`
- `SearchHit { row: Row, col: Col, line_text: String, match_start: usize, match_end: usize }`
  — `row`/`col` are zero‑based character positions; `match_start`/`match_end`
  are byte offsets within `line_text`.
- `ReplaceScope::Single { path, row, match_start, match_end }` — for
  unreplace / out‑of‑buffer single replace
- `ReplaceScope::All` — replace every match below `root`, skipping
  `skip_paths`

## State owned by this driver

**None.** file‑search is stateless between commands. Coalescing is done by
draining `cmd_rx.try_recv()` in a tight loop before kicking off the next
blocking task (`crates/file-search/src/lib.rs:52-57`). There is no cache,
no partial‑results store, no cancellation token.

## External side effects

- **Filesystem reads** (search): `ignore::WalkBuilder` traversal,
  `grep_searcher::Searcher::search_path` per candidate file,
  `UserPath::canonicalize` per hit file.
- **Filesystem writes** (replace): for `ReplaceScope::Single`,
  `std::fs::read_to_string(path)` + build new content in memory + atomic
  tmpfile/rename write (`search.rs:203-209`). For `ReplaceScope::All`,
  same per file where `regex::replace_all` changes content.
- **No directory creation.** Replace targets must already exist (they
  were discovered by the preceding search).

## Known async characteristics

- **Latency**: dominated by disk walk + regex. A cold cache on a
  medium‑sized repo (10k files) is ~100 ms; warm is ~10 ms. The CPU‑bound
  parts run on `spawn_blocking` so they don't starve the runtime.
- **Ordering**: **not** strictly FIFO. The coalescing drain
  (`search.rs:52-57`) discards all but the latest command in the mpsc at
  the moment the previous blocking task completes. A rapid sequence `A,B,C`
  where B and C arrive during A's execution will run as `A, then C` (B
  dropped). Acceptable for a typeahead search box; would be wrong for a
  general‑purpose resource driver.
- **Cancellation**: effectively yes via coalescing — the latest request
  supersedes in‑flight. But there is no explicit cancellation of the
  currently‑running blocking task; it runs to completion before the next
  starts. A user typing fast can see a stale result flash if the new
  query's run hasn't finished by the time the old one does.
- **Backpressure**: mpsc size 64. Overflow on `try_send` drops the command
  silently. Would manifest as "the new query never triggers a search" —
  not observed in practice.
- **Hit cap**: `total_hits > 1000` breaks the walk
  (`search.rs:97-100`). The UI does not currently show "...truncated".

## Translation to query arch

| Current behavior                                                                 | New classification                                                                           |
|----------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------|
| `FileSearchOut::Search`                                                          | Resource driver for `Request::SearchFiles { query, root, flags }`, result `Event::SearchResults` |
| `FileSearchOut::Replace { scope: Single }`                                       | Resource driver for `Request::ReplaceOne { path, row, range, replacement }`, result `Event::ReplaceComplete` |
| `FileSearchOut::Replace { scope: All }`                                          | Resource driver for `Request::ReplaceAll { query, replacement, root, flags, skip }`, result `Event::ReplaceComplete` |
| Coalescing drain                                                                 | Keep — same pattern (dispatch checks `is_pending(Request::SearchFiles)` and if the query differs from the pending one, supersede)   |
| 1000‑hit cap                                                                     | Keep as a config knob                                                                        |

The replace flow in the new arch should split `Single` vs `All` into two
distinct `Request` variants because the models are meaningfully different:
`Single` is a point edit (and falls back to in‑buffer when the file is
open), `All` is a workspace‑wide transform.

## State domain in new arch

- `Request::SearchFiles` result lands as `Loaded<Vec<FileGroup>>` in
  `SearchState.results`. `flat_hits` stays a derived cache.
- `Request::ReplaceAll` / `ReplaceOne` results update
  `SearchState.results` (fresh post‑replace results) and bump an
  `SearchState.replaced_count` counter for the alert.
- `SearchState` itself (`query`, `replacement`, `case_sensitive`,
  `use_regex`, `replace_mode`, `selection`, `scroll_offset`,
  `replace_stack`) stays as UI state.

## Versioned / position‑sensitive data

Search results are **snapshots of disk content at the time the grep ran**.
They are not version‑stamped against buffer edits — which is a real gotcha.
The model's replace flow works around this: `model/file_search.rs:569-575`
checks if a matching file is currently open in a buffer
(`find_buf_for_path`), and if so applies the replacement via
`replace_in_buffer(...)` on the in‑memory `BufferState` instead of
dispatching to the file‑search driver. Only when the file is not open does
the driver write to disk.

For `ReplaceAll` with mixed open/closed files
(`model/file_search.rs:706-802`): open files get in‑buffer edits with one
undo group per file; closed files are listed in `pending_replace_all` and
**opened as buffers**, then `apply_pending_replace` (`:806-844`) runs when
each `BufferOpen` arrives. This means the driver's `ReplaceScope::All` path
is never actually triggered when ALL files are closed — the model prefers
to open them and do in‑buffer edits so undo works.

`ReplaceScope::Single` is similarly gated: only dispatched to the driver
when `find_buf_for_path` returns `None`. [unclear — whether that code path
is tested today; smoke scenarios probably exercise only open‑buffer
replace.]

## Edge cases and gotchas

- **Coalescing drops intermediate commands.** A user typing "hello" fast
  can see the results jump from empty to the full "hello" results with
  nothing for "h", "he", "hel", "hell" — depending on how fast the grep
  for the previous query finishes.
- **Regex build error returns empty results silently.** `search.rs:25-28`:
  if `RegexMatcherBuilder::build` fails (invalid regex), the search
  returns `Vec::new()`. The UI shows "no results" with no indication of
  a syntax error. `run_replace` falls back to returning the plain search
  (`search.rs:127`).
- **In‑place writes are not fsync'd.** `write_atomic` does tmpfile + rename
  with no `sync_all` (`search.rs:203-209`). Same as docstore.
- **`match_start` / `match_end` are byte offsets, `col` is char count.**
  Non‑ASCII hits have different `col` vs `match_start` — the model uses
  byte offsets for in‑buffer `replace_in_buffer` and converts via
  `char_indices` (`model/file_search.rs:974-977`).
- **1000 hit cap is silent.** If a query matches more than 1000 times,
  the walker terminates early. The UI doesn't surface this.
- **Gitignore layers off when no git root.** `WalkBuilder` uses gitignore
  layers regardless of whether `root` is a git root. In standalone mode
  running from cwd, this picks up any `.gitignore` found in ancestors —
  behaviour inherited from `ignore` crate. Acceptable but worth noting.
- **`FileSearchIn::ReplaceComplete.results` is the post‑replace result
  set.** So a successful replace of the last hit on a line produces a
  result where that file is either gone (if no other hits) or has fewer
  hits. The model reducer
  (`model/mod.rs:557-574`) uses this to refresh `state.file_search.results`.
- **Skip list mechanism.** `ReplaceScope::All` honours
  `skip_paths: Vec<CanonPath>` to avoid rewriting files that the model
  already rewrote in‑buffer. Currently always passed as `Vec::new()` from
  `model/file_search.rs:627` and `:696` — the open‑file replace path
  avoids the driver entirely, so skip is moot. [unclear — dead parameter
  or future‑use?]
- **`replace_mode` vs `Replace` command**: `ReplaceAll` from the UI
  actually dispatches a series of per‑buffer in‑memory rewrites, plus
  optionally opens closed files to rewrite them in‑buffer too. The
  `FileSearchOut::Replace { scope: All }` command is only used when
  unreplace writes back to a closed file (`model/file_search.rs:682-697`).
  In the rewrite this should be simplified: decide whether replace is
  buffer‑mediated or driver‑mediated and pick one consistently.

## Goldens checklist

Under `goldens/scenarios/driver_events/file_search/`:

- `results/` — natural via typing a query in the search overlay.
- `replace_complete/` — natural via replace‑all confirmation in the search
  overlay.

Missing / to add:

- `search_regex_invalid/` — verifies silent empty results behaviour.
- `search_capped_at_1000/` — large repo exceeds hit cap; asserts result
  truncation (and, if the rewrite adds a "truncated" indicator, asserts
  that too).
- `search_binary_file_skipped/` — verifies NUL byte quits the searcher.
- `replace_single_closed_file/` — verifies the driver‑mediated single
  replace path (current tests likely only exercise open‑buffer replace).
- `search_coalesced/` — types three queries rapidly; asserts only the
  last one produces `FileSearchIn::Results`.
- `search_case_toggle/` and `search_regex_toggle/` — existing overlay
  actions that call `trigger_search`.

[unclear — whether the current golden runner can assert "only N events
fired over the scenario" (needed for coalescing test) or only "eventually
the frame converged".]
