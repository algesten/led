# gh-pr

## Summary

When the current workspace is a git repo on a branch that has an open (or
merged / closed) pull request on GitHub, led surfaces PR metadata: the PR
number and status in the status bar, the PR diff as gutter marks
(`IssueCategory::PrDiff`), review-thread comments anchored to specific
lines (`IssueCategory::PrComment`), and a keybinding (`Ctrl-x Ctrl-p`,
action `OpenPrUrl`) that opens either the PR web page or — when the cursor
is on a commented line — the direct URL of that review comment. All PR
data flows through the `gh-pr` driver, which shells out to the `gh` CLI
(`gh pr view`, `gh pr diff`, `gh api graphql`, `gh api repos/.../pulls/N`).
When `gh` is not installed, no PR data appears and led behaves as if no PR
were loaded.

## Behavior

### Loading a PR

A PR load fires whenever **the branch becomes known or changes**. The
derived layer observes `state.git.branch` with `dedupe_by`, and on a new
non-None value emits `GhPrOut::LoadPr { branch, root }`. The driver
performs these `gh` subprocess calls, in order:

1. `gh pr view --json number,state,url,reviews,headRefOid` — core metadata.
2. `gh api repos/{owner}/{repo}/pulls/{N} --include` — fetch the initial
   ETag (for subsequent conditional polling).
3. `gh api graphql -f query=...` — fetch `reviewThreads` (line-anchored
   comments), filtering out `isOutdated` threads and keeping the first
   comment of each thread (line, body, author, url).
4. `gh pr diff` — unified diff; parsed into per-file `LineStatus` ranges
   with `IssueCategory::PrDiff`. Only `+` lines are marked.
5. For every path referenced by either diff lines or comments, read the
   blob at `headRefOid` from the local git repo and compute a content hash
   (the same `DefaultHasher` algorithm as `TextDoc::content_hash`). These
   `file_hashes` let downstream rendering suppress PR annotations when the
   local buffer has diverged from the PR's head commit.

The driver sends `GhPrIn::PrLoaded { number, state, url, api_endpoint,
etag, diff_lines, comments, file_hashes }`. The model converts this into
`PrInfo` on `state.git.pr` via `Mut::SetPrInfo`. The `state` field maps to
`PrStatus`: `"MERGED"` → `Merged`, `"CLOSED"` → `Closed`, anything else →
`Open`.

### Branch change / clearing

A branch change clears PR state immediately: `gh_pr_of.rs` dedupes on
`s.git.branch` while `phase == Running` and emits `Mut::SetPrInfo(None)`
on each transition. This runs in parallel with the re-load; the UI flips
to "no PR" instantly and re-populates if `LoadPr` succeeds.

### Reloading after git activity

External git activity (commits, pushes, rebases) arrives as
`WorkspaceIn::GitChanged`. This signal bypasses `AppState` and drives a
`pr_settle` timer (duration out-of-contract). When the timer fires the
model bumps `pr_settle_seq`, and derived observes that bump and emits a
fresh `GhPrOut::LoadPr`. This ensures that after a `git push`, the PR
metadata is refreshed once the dust has settled rather than in the middle
of a multi-step git operation.

### Polling

While a PR is loaded, led conditionally polls the GitHub REST API to
notice remote changes (new reviews, state transitions). The contract for
the rewrite:

- **In scope:** when polling fires, the driver executes `gh api
  {api_endpoint} --include [ -H "If-None-Match: {etag}" ]` (a conditional
  GET). On `304 Not Modified` the driver returns `GhPrIn::PrUnchanged`,
  which the model filters out (no state update). On a non-304 response it
  re-parses the body, re-fetches review threads and diff, recomputes file
  hashes, and emits a full `GhPrIn::PrLoaded`.
- **Out of scope (per `project_rewrite_scope.md`):** the 15s poll interval
  and the fact that polling is driven by a repeating `pr_poll` timer. The
  rewrite may choose any polling strategy (different interval, long-poll,
  webhook, manual refresh) as long as (a) polling uses the conditional
  REST API with `If-None-Match`, (b) stale ETags are refreshed on each
  successful non-304, and (c) `PrUnchanged` produces no state change.

The `pr_poll` timer is set when `state.git.pr.is_some()` transitions from
false to true and cancelled on the reverse transition. A model handler
bumps `pr_poll_seq` on fire; derived observes that bump, checks that a PR
is still loaded and that the workspace is loaded, and emits
`GhPrOut::PollPr { api_endpoint, etag, root }`.

### Opening URLs

`Action::OpenPrUrl` (default keybinding `Ctrl-x Ctrl-p`) resolves as
follows, in `gh_pr_of::open_pr_target_url`:

1. If `state.git.pr` is `None` → action is a no-op.
2. If the active buffer's cursor is on a line that has a PR review comment
   with a non-empty URL → use that comment's URL.
3. Otherwise → use `pr.url` (the PR web page).

The chosen URL is written to `pending_open_url: Versioned<Option<String>>`
via `Mut::SetPendingOpenUrl`. Derived then fires it through the `OpenUrl`
driver, which on macOS invokes `open <url>` and on Linux `xdg-open <url>`.
This is fire-and-forget — no response is expected.

## User flow

User opens a workspace on a feature branch with a PR: branch appears in
status bar; a moment later, PR indicator appears next to it. User opens a
file with review comments; gutter shows both PR-diff bars and comment
markers. User navigates the cursor to a commented line and presses
`Ctrl-x Ctrl-p`: the browser opens the direct comment thread URL. User
checks out a different branch; PR indicator clears immediately; if the new
branch also has a PR, its data loads and the indicator returns. User runs
`git push` in a terminal; after the pr-settle window, led reloads PR data
and picks up any updated reviews.

## State touched

- `GitState.pr: Option<PrInfo>` — PR metadata, diff, comments, file hashes.
- `GitState.pr_settle_seq: Versioned<()>` — bumped when `pr_settle` timer
  fires; triggers reload.
- `GitState.pr_poll_seq: Versioned<()>` — bumped when `pr_poll` timer
  fires; triggers poll.
- `PrInfo.number` / `.status` / `.url` / `.api_endpoint` / `.etag` /
  `.diff_files` / `.comments` / `.file_hashes`.
- `PrStatus` — `Open` | `Merged` | `Closed`.
- `PrComment { line, body, author, url }`.
- `AppState.pending_open_url: Versioned<Option<String>>` — fire-and-forget
  URL open (shared with other actions that open URLs).

## Extract index

- Actions: `Action::OpenPrUrl` → `docs/extract/actions.md`.
- Keybindings: `Ctrl-x Ctrl-p` → `open_pr_url` → `docs/extract/keybindings.md`.
- Driver events:
  - `GhPrOut::LoadPr { branch, root }`
  - `GhPrOut::PollPr { api_endpoint, etag, root }`
  - `GhPrIn::PrLoaded { ... }`
  - `GhPrIn::PrUnchanged`
  - `GhPrIn::NoPr`
  - `GhPrIn::GhUnavailable`
  - `UiOut::OpenUrl` → system `open` / `xdg-open`
  → `docs/extract/driver-events.md`
- Timers: `pr_settle`, `pr_poll` (both out-of-contract for timing).
- Config keys: none. [unclear — gh binary path is currently overridable in
  tests via `Startup::test_gh_binary` but has no user-facing config key.]

## Edge cases

- **`gh` not installed.** `run_gh` detects `ErrorKind::NotFound` from the
  `Command::spawn` failure and returns `GhUnavailable`; the model maps
  this to `SetPrInfo(None)`. No alert. Status bar shows no PR indicator.
- **On a branch with no PR.** `gh pr view` exits non-zero; driver returns
  `NoPr`; mapped to `SetPrInfo(None)`. Status bar shows no PR indicator.
- **Detached HEAD / no branch.** `state.git.branch` is `None`; derived's
  `load_pr_command` returns `None`; `LoadPr` is never emitted.
- **Standalone / no workspace.** `workspace.loaded()` is `None`; `LoadPr`
  and `PollPr` both skip emission. Keybinding is effectively inert.
- **Poll with stale ETag** (ETag rotated server-side but the 304 path
  matches anyway): server returns a 200 with a new body and ETag; driver
  re-parses and updates state.
- **Poll body is malformed JSON.** Driver returns `PrUnchanged` (treat as
  no-op rather than clobber state).
- **Review thread with `isOutdated: true`.** Skipped — not shown.
- **Review thread with zero comments.** The `comments/nodes/0` pointer
  misses; the thread contributes a `PrComment { body: "", author: "",
  url: "" }` at the recorded line. The `OpenPrUrl` resolver filters empty
  URLs out and falls back to the PR URL.
- **Cursor on a commented line but no active buffer/tab.** Resolver falls
  back to `pr.url`.
- **File hash mismatch.** When the local buffer's `content_hash` differs
  from `pr.file_hashes[path]`, downstream rendering suppresses per-line PR
  annotations for that file (the comments are still known; they just
  aren't anchored to possibly-moved line numbers). [unclear — the exact
  suppression rule lives in `crates/state/src/annotations.rs`; this spec
  section describes intent only.]
- **PR URL is not `https://github.com/...`** (GitHub Enterprise). The
  `parse_github_url` helper only matches the public host; `api_endpoint`
  ends up empty and polling is effectively disabled. GraphQL calls also
  fail to derive `(owner, repo)` and return no comments.

## Error paths

- **`gh` binary missing.** `GhUnavailable` → `SetPrInfo(None)`. Silent.
- **`gh pr view` non-zero exit** (not on a PR branch, auth missing,
  rate-limited). `NoPr` → `SetPrInfo(None)`. Silent.
- **`gh api graphql` failure / invalid JSON.** `load_review_threads`
  returns empty map — PR loads without comments.
- **`gh pr diff` failure.** `load_pr` proceeds with `diff_lines = {}` — PR
  loads without gutter bars.
- **`gh api` poll failure** (network, rate-limit, unparseable headers).
  Driver returns `PrUnchanged` — treat as no-op. Next poll tick retries.
- **head-commit blob missing locally** (branch out of sync). File omitted
  from `file_hashes`; per-line annotations for that path are suppressed
  until the commit is fetched.

[unclear — there is no visible alert or status message for any of the `gh`
subprocess failures. The contract is "silent, stay with last known state".]
