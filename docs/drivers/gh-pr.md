# Driver: gh-pr

## Purpose

The gh-pr driver surfaces GitHub pull-request context for the current
branch so the editor can paint PR diff decorations, review-thread
comments, and a status-bar chip with the PR number/state. It is a
resource driver — every `GhPrIn` event is the response to a dispatched
`GhPrOut` — that shells out to the `gh` CLI for all network work.
Conditional HTTP polling (via `If-None-Match` / 304) keeps the steady-
state cost low: a single `gh api ... --include` call every 15 s while a
PR is loaded.

## Lifecycle

Started once at startup; the tokio task loops on `cmd_rx.recv()` forever.
`gh_binary` is captured at construction time — the runner passes a path
to the `fake-gh` helper for tests; production leaves it `None` and the
driver uses `"gh"` from `PATH`. No explicit shutdown; task drops with
the local set.

## Inputs (external → led)

- The `gh` CLI binary (or the `fake-gh` stand-in under tests). Every
  command is spawned synchronously via `tokio::task::spawn_blocking` +
  `std::process::Command`. No stdio streaming; the driver collects
  stdout into a `String` and exits.
- No filesystem watches; the driver is purely command-reactive.

## Outputs from led (model → driver)

| Variant                                              | What it causes                                                                                                                     | Async? | Returns via     |
|------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|--------|-----------------|
| `GhPrOut::LoadPr { branch, root }`                   | Full initial load: `gh pr view --json ...`, then `gh api <endpoint> --include` for ETag, then `gh api graphql` for review threads, then `gh pr diff` for hunks | yes    | `GhPrIn::PrLoaded` / `NoPr` / `GhUnavailable` |
| `GhPrOut::PollPr { api_endpoint, etag, root }`       | Conditional GET: `gh api <endpoint> --include -H 'If-None-Match: <etag>'`; on 200 re-runs `pr diff` + `api graphql` for fresh data | yes    | `GhPrIn::PrLoaded` / `PrUnchanged` |

**In-contract for the rewrite: the set of `gh` sub-commands.** Anything
else (which specific JSON fields are read, the GraphQL query shape, the
`If-None-Match` handshake, ETag extraction from `--include` headers) is
also in-contract because tests and fake-gh depend on them being invoked
that way. **Out-of-contract: the *timing* of when each dispatch fires.**
That is driven entirely from the model side via timers and state
transitions (see below) — the rewrite is free to reschedule.

### The exact `gh` commands invoked

1. `gh pr view --json number,state,url,reviews,headRefOid` — run in
   `root` as cwd. Produces PR metadata. Non-zero exit → `NoPr`. Binary
   missing → `GhUnavailable`.
2. `gh api <endpoint> --include` — no body required, only headers. Used
   twice: once on `LoadPr` to fetch the initial ETag, then on every
   `PollPr` as the conditional GET. `<endpoint>` is
   `repos/{owner}/{repo}/pulls/{number}` parsed from the `gh pr view`
   URL. On `PollPr` the driver also sends `-H "If-None-Match: <etag>"`.
3. `gh api graphql -f query=...` — a single GraphQL query that fetches
   `reviewThreads(first:100) { nodes { path line isOutdated
   comments(first:5) { nodes { body url author { login } } } } }`.
   Outdated threads are filtered client-side; the rewrite should do the
   same.
4. `gh pr diff` — run in `root`. Produces unified diff; the driver
   parses `+++` / `@@` / `+` / `-` into `HashMap<CanonPath,
   Vec<LineStatus>>` with `category: IssueCategory::PrDiff`. The parser
   is inline in `crates/gh-pr/src/lib.rs:521-626`.

`headRefOid` (from `pr view`) or `head.sha` (from the REST poll) is
used to fetch the PR's "base content" via `git2::Repository` and hash
each referenced file with `DefaultHasher` — this is the
`file_hashes: HashMap<CanonPath, PersistedContentHash>` map used later
to check whether a locally-open buffer still matches the PR's view
before painting decorations. The hash must match `TextDoc::content_hash()`
byte-for-byte (asserted by the `blob_hash_matches_textdoc_content_hash`
test).

## Inputs to led (driver → model)

| Variant                                                                                                                                                        | Cause                                                                                         | Frequency                                    |
|---------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------|----------------------------------------------|
| `GhPrIn::PrLoaded { number, state, url, api_endpoint, etag, diff_lines, comments, file_hashes }`                                                             | `LoadPr` succeeded, or `PollPr` returned 200 with a fresh body                                 | Once per distinct PR state change            |
| `GhPrIn::PrUnchanged`                                                                                                                                          | `PollPr` saw a `304` in the `--include` headers                                                | Every 15 s while nothing's changed — filtered out in `gh_pr_of.rs:68` |
| `GhPrIn::NoPr`                                                                                                                                                 | `gh pr view` exited non-zero, or its JSON couldn't be parsed                                   | Once on `LoadPr` for a branch with no PR     |
| `GhPrIn::GhUnavailable`                                                                                                                                        | `Command::new(gh_bin)` returned `NotFound` (binary missing)                                    | Once, at most                                |

All four variants flow through the same `gh_pr_in` stream into
`gh_pr_of.rs`; three produce `Mut::SetPrInfo(Some|None)` after conversion
via `to_pr_info`, and `PrUnchanged` is filtered out explicitly.

## State owned by this driver

None. Truly stateless — each command invocation is independent. The
only carried state is the `gh_binary: String` captured when the driver
is constructed.

Note: the driver does *not* cache the PR body, the ETag, or the last
commit hash. The model holds all of that in `AppState::git.pr` and
passes `etag` back on every `PollPr`.

## External side effects

- Spawns `gh` subprocesses. Each call is synchronous from the
  `spawn_blocking` thread's perspective (child process run to completion).
- Reads `.git/` via `git2` for `compute_file_hashes` (HEAD tree of
  `headRefOid`).
- No local writes, no network calls not mediated by `gh`.

## Known async characteristics

- **Latency**: a full `LoadPr` runs 4 subprocesses sequentially. Each
  takes 200 ms–1 s depending on API roundtrip; worst-case observed is
  ~4 s on a cold PR load. `PollPr` with a valid ETag is one subprocess,
  typically <500 ms. `[unclear — no formal benchmarks recorded]`.
- **Ordering**: single consumer loop over `cmd_rx` → commands serialise.
  A new `LoadPr` that arrives during a `PollPr` waits; there is no
  cancellation.
- **Cancellation**: no. A `LoadPr` following a branch switch must wait
  for any in-flight poll to finish first.
- **Backpressure**: mpsc bounded at 16; overflow dropped on `try_send`.
  Upstream dedupe on `branch` + `pr_settle_seq` + `pr_poll_seq` keeps
  the queue near empty.

## Translation to query arch

The four `gh` sub-commands stay exactly as they are. The scheduling
(what triggers each dispatch) is what moves out of FRP and into the
dispatcher:

| Current behavior                                                      | New classification                                               |
|-----------------------------------------------------------------------|------------------------------------------------------------------|
| `LoadPr` fires on branch change (`dedupe_by(s.git.branch)` in derived) | `Request::LoadPr { branch, root }` dispatched from a `Event::BranchChanged` handler |
| `LoadPr` fires after `pr_settle_seq` bumps (post-git-activity 2 s quiesce) | Dispatched from a "git quiesced" event, or scheduled via `Request::SetTimer("pr_settle", 2s, Replace)` → fires `Request::LoadPr` |
| `PollPr` fires every 15 s via `Schedule::Repeated` timer              | `Request::SetTimer("pr_poll", 15s, Repeated)` → each firing dispatches `Request::PollPr { api_endpoint, etag, root }` |
| `pr_poll` timer cancelled when `s.git.pr.is_none()`                   | Cancel in the `Event::PrCleared` handler                         |
| `PrLoaded { ... }` wholesale replaces `git.pr`                        | Same; one `Event::PrLoaded` carrying the full struct            |
| `PrUnchanged` filtered out silently                                   | Same — driver returns `Event::PrUnchanged` which the reducer ignores, **or** driver suppresses and returns no event |
| `NoPr` / `GhUnavailable` both clear `git.pr`                          | Both map to `Event::PrCleared` with a reason enum                |

Two candidates for the rewrite design:

1. **One request per `gh` subprocess.** Split `LoadPr` into
   `Request::GhPrView`, `Request::GhApiPrEndpoint`, `Request::GhApiGraphql`,
   `Request::GhPrDiff`. Each is an independent resource call;
   composition lives in a saga / coordinator at the domain level. Gives
   finer cancellation and caching. Drawback: 4× the dispatcher plumbing.
2. **Keep `LoadPr` / `PollPr` as composite requests** matching the
   current shape. Simpler, one-to-one with today's code. Drawback: no
   partial-cancellation of a slow `gh pr diff`.

Recommend #2 for parity, #1 as a later refactor.

## State domain in new arch

- `GitState::pr: Option<Loaded<PrInfo>>` — the whole `PrInfo` struct
  lives in `GitState` (current led puts it at `s.git.pr`, a plain
  `Option<PrInfo>`). `Loaded` adds Pending/Loaded/Error variants so the
  UI can show "Loading PR..." during the ~1 s load.
- No in-flight dedupe atom is needed — the dispatcher tracks
  `is_pending(Request::LoadPr { branch, root })` itself.

## Versioned / position-sensitive data

**Partially position-sensitive.** The `diff_lines` and `comments`
hashmaps carry row numbers (`Row`) that reference the PR's view of
each file, not the current buffer. The current led code uses
`file_hashes` to guard: if a buffer's `content_hash()` matches the
hash recorded for its path in `file_hashes`, painting is safe;
otherwise decorations are suppressed. This is a hash-match gate, not a
rebase, because the PR diff is defined against a specific commit's
tree — editing the buffer doesn't produce a meaningful rebased PR
diff, it just invalidates it.

For the rewrite: preserve the hash-gate semantics. `PrInfo.file_hashes`
stays; the rebase function for PR annotations is the degenerate
`invalidate-if-modified`. No edit-op replay needed.

## Edge cases and gotchas

- **`GhUnavailable` is treated the same as `NoPr` downstream**
  (`gh_pr_of.rs:67-70` maps both to `SetPrInfo(None)`). The distinction
  matters only for logging and potential error-banner UI in the
  rewrite; reducer logic shouldn't branch on them differently.
- **`PrUnchanged` still bumps the poll timer.** The 15 s timer is
  `Repeated`; 304s don't extend it. Consequence: a PR with no activity
  still hits `gh api` once per 15 s indefinitely. Rewrite may want an
  exponential backoff.
- **`If-None-Match` depends on the exact ETag format returned by
  GitHub.** `fetch_etag` scans for a line starting with `etag:`
  (case-insensitive) and splits on `:`. If GitHub changes to weak ETags
  (`W/"..."`), the match still works — the string is passed through
  verbatim.
- **The `--include` header parsing is whitespace-sensitive.** The
  driver looks for `\r\n\r\n` or `\n\n` to split headers from body, and
  scans for the literal substring `"304"` in the headers section to
  detect `304 Not Modified`. This is brittle — if any other header
  happens to contain `"304"` it would false-positive. (It's fine today
  because `gh`'s output has a narrow shape.)
- **`parse_unified_diff` has a first-pass `break` loop that does
  nothing.** `lib.rs:521-552` is effectively dead code; the real parse
  is the second pass starting at 556. Preserve behavior, not that dead
  code, in the rewrite.
- **All PR diff lines are categorised as `IssueCategory::PrDiff`.** No
  distinction between Added / Modified. The comment at `lib.rs:578-582`
  explicitly notes this simplification.
- **`file_hashes` only covers files that appear in `diff_lines.keys()`
  or `comments.keys()`.** A buffer for an unaffected file has no entry
  and the hash-gate trivially skips annotation (because there's
  nothing to annotate anyway).
- **`load_review_threads` filters `isOutdated: true` threads.**
  Outdated comments (those pinned to lines that no longer exist in the
  PR head) never appear in `comments`. This means the rewrite does not
  need to handle "ghost" comments at all — the gh side has already
  dropped them.
- **The driver clones `gh_bin: String` on every command.** Cheap, but
  a detail to preserve if tests depend on the exact spawn invocation.

## Goldens checklist

Scenarios under `tests/golden/drivers/gh-pr/`:

1. `pr_loaded/` — exists. Fake-gh `pr_view` + `pr_diff` configured.
   Assert `PrLoaded` fires with expected number/state and the status
   chip renders.
2. `no_pr/` — exists. Omit `pr_view` from fake-gh config; assert `NoPr`
   fires and no chip renders.
3. `gh_unavailable/` — needs a mechanism to launch led with an invalid
   `--test-gh-binary` path (or no override). Assert `GhUnavailable`
   handling.
4. `pr_304/` — needs fake-gh extension to honour `If-None-Match`; assert
   `PrUnchanged` is received and no state mutation occurs.
5. `pr_branch_change/` — checkout a different branch mid-session; assert
   old PR is cleared (`branch_clear_s` in `gh_pr_of.rs`) and new
   `LoadPr` fires for the new branch.
6. `pr_poll_cadence/` — virtual clock needed. Advance 15 s repeatedly,
   assert the poll dispatch appears on each tick while a PR is loaded
   and stops when the PR is cleared.
7. `pr_comments_on_cursor/` — `OpenPrUrl` action with cursor on a
   commented line → opens the comment URL; cursor elsewhere → opens
   the PR URL. Covers `gh_pr_of::open_pr_target_url`.
8. `pr_outdated_threads/` — fake-gh GraphQL response with `isOutdated:
   true`; assert those threads are filtered out of `comments`.
9. `pr_hash_gate/` — load PR, then edit the buffer; assert decorations
   are suppressed until the buffer is saved (or restored to matching
   content).
