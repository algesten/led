# Driver: git

## Purpose

The git driver wraps `libgit2` (via the `git2` crate) to produce two kinds of
status for a workspace: repo-wide file-level status (modified / staged /
untracked / new, plus the current branch name) and per-file line-level
diff ranges used to paint the editor gutter. It is a pure resource driver —
every `GitIn` event is the response to a previously dispatched
`GitOut::ScanFiles`. The actual cadence (50 ms after any "dirty" signal,
recomputed on save, `GitChanged` notify, workspace reload, etc.) is driven
from the model side via the timers driver, not by the git driver itself.

## Lifecycle

Started once at startup from `main.rs` when the runtime wires the driver
task into the tokio local set; consumes `GitOut` forever from the model's
output stream. There is no explicit shutdown handshake — when the outer
`LocalSet` finishes, the tokio task running `cmd_rx.recv()` drops with
the sender and terminates. No flush-on-exit guarantee is required: every
emission is idempotent and the next startup re-scans.

State internal to the driver is limited to `tracked: HashSet<CanonPath>`
— the paths that produced a non-empty `LineStatuses` in the previous scan,
so the next scan can emit `statuses: Vec::new()` for files that have since
become clean (otherwise the gutter would stay stale).

## Inputs (external → led)

- The git repository on disk: working-tree files, the index, and the HEAD
  tree (via `git2::Repository::open`, `repo.statuses`, `repo.index`,
  `repo.head().peel_to_tree`). No `.git/` watches, no signals — the driver
  is purely reactive to `GitOut::ScanFiles`.
- `std::fs::read` for the worktree content of each dirty file (used to
  compute INDEX↔WORKTREE hunks for unstaged line status).

## Outputs from led (model → driver)

| Variant                      | What it causes                                          | Async? | Returns via                              |
|------------------------------|---------------------------------------------------------|--------|------------------------------------------|
| `GitOut::ScanFiles { root }` | Run `repo.statuses()`, compute line diffs for every dirty file, emit file-statuses then line-statuses then "clear" events for formerly-dirty paths | yes, via `spawn_blocking` | `GitIn::FileStatuses` then N × `GitIn::LineStatuses` |

There are no per-file / per-action dispatches. One `ScanFiles` drives a
whole batch — file status **and** line status for every dirty file — from
a single `spawn_blocking` call. See `crates/git/src/lib.rs:43-88`.

## Inputs to led (driver → model)

| Variant                                       | Cause                                                                                                                     | Frequency                                   |
|-----------------------------------------------|---------------------------------------------------------------------------------------------------------------------------|---------------------------------------------|
| `GitIn::FileStatuses { statuses, branch }`    | First message after each `ScanFiles`; carries the full repo-wide map plus the current branch shorthand (None when detached) | One per scan                                |
| `GitIn::LineStatuses { path, statuses }`      | One per file with a non-empty WT/INDEX diff; `statuses` holds coalesced `+` line ranges categorised as `StagedModified` or `Unstaged` | N per scan (N = # dirty files)              |
| `GitIn::LineStatuses { path, statuses: [] }`  | "Clear" event for each path that had line statuses in the previous scan but no longer does (unstaged edits reverted, file staged, etc.) | Up to M per scan (M = previously-dirty paths no longer dirty) |

## State owned by this driver

- `tracked: HashSet<CanonPath>` — paths that emitted a non-empty
  `LineStatuses` last scan. Used to synthesise empty-list clear events
  when a file transitions from dirty to clean. Without this the gutter
  would retain its +/~ marks after `git add` or a revert.

That's it. No connection pool, no open repo handle — `git2::Repository::open`
is called on every scan (cheap; libgit2 memory-maps `.git/`).

## External side effects

- Reads the working tree (`std::fs::read`) for every dirty file's worktree
  content.
- Reads `.git/` through libgit2.
- No writes, no network.

## Known async characteristics

- **Latency**: whole-repo `statuses()` scales with working-tree size; line
  diffs add one libgit2 `Patch::from_buffers` per dirty file. Typical small
  repo: <10 ms. Large monorepos: tens to hundreds of ms (observed; not
  measured formally — `[unclear — no recorded latency numbers]`).
- **Ordering**: `cmd_rx.recv()` is a single-consumer loop, so `ScanFiles`
  commands are processed serially. A new `ScanFiles` that arrives while
  one is in flight is queued, not merged. Coalescing happens upstream
  (the `git_file_scan` 50 ms `Replace` timer ensures at most one pending
  dispatch per burst).
- **Cancellation**: no. Once a scan starts it runs to completion.
- **Backpressure**: the mpsc channel is bounded at 64; overflow is dropped
  via `try_send` in the `out.on` bridge. In practice upstream coalescing
  keeps depth near 1 so overflow is not observed.

## Translation to query arch

| Current behavior                                 | New classification                                    |
|--------------------------------------------------|-------------------------------------------------------|
| Reacts to `GitOut::ScanFiles`                    | Resource driver for `Request::GitScan { root }`       |
| Emits `GitIn::FileStatuses` first                | Response variant / follow-up `Event::GitFileStatuses` |
| Emits `GitIn::LineStatuses` per dirty file       | Part of the same response, or follow-up events per path |
| Emits empty `LineStatuses` for now-clean files   | Diff-against-previous-scan logic lives in the driver; stays there, or move to a reducer that compares Loaded<GitScan> snapshots |
| `git_file_scan` 50 ms coalescing timer           | Replaced by the dispatcher's "is a `GitScan` already pending / queued" guard + a scheduled `Request::GitScan` after any dirtying `Event` |

Open choice: does `Request::GitScan` complete with a single
`Event::GitScanned { file_statuses, line_statuses_by_path }` carrying
everything, or does the driver keep streaming N follow-up events? The
streaming form is marginally kinder to first-paint latency (file status
lands before every line diff is done); the single-event form is easier to
reason about and makes "clear" events unnecessary — the reducer just
replaces the prior `Loaded<GitScan>` wholesale.

## State domain in new arch

- `GitState` (dedicated domain atom): `file_statuses: HashMap<CanonPath, HashSet<IssueCategory>>`,
  `branch: Option<String>`, `line_statuses: HashMap<CanonPath, Vec<LineStatus>>`,
  plus a `Loaded<GitScan>` latch with the last-completed scan's
  `scan_seq` or document-revision equivalent.
- Pending-scan tracking collapses into a single `is_scan_pending: bool`
  or `Loaded::Pending(RequestId)` — no more `pending_file_scan`
  `Versioned` field.

## Versioned / position-sensitive data

**Yes — line status is position-sensitive.** The line ranges are computed
against the *worktree bytes at the moment of the scan*. If the user
continues editing after the scan was dispatched, the returned row numbers
are stale: they reference lines in the old worktree snapshot, not the
current buffer.

Current led does not rebase these against buffer edits. The gutter
temporarily lies during rapid typing and self-corrects on the next scan
(50 ms after settle). File status is not position-sensitive, so it doesn't
need rebasing.

For the rewrite: treat `Request::GitScan` as producing results stamped
with a **worktree revision** (e.g. file mtime, content hash, or a buffer
`DocVersion` when the buffer is dirty). The reducer should:

1. Accept the scan result into `GitState`.
2. For each path with a dirty buffer, rebase the `Vec<LineStatus>` against
   any `EditOp`s applied to the buffer between the scan's reference
   revision and the buffer's current revision. The rebase function is
   identical in shape to LSP-diagnostic rebase: shift / widen / invalidate
   row ranges based on inserted or removed lines in the edit ops.
3. Drop line ranges that straddle an edit boundary (safest — they'll be
   recomputed on the next scan).

Alternative: freeze the worktree-level diff for clean files (no rebase
needed) and only rebase for buffers that were dirty at scan time. File
status itself is not worth rebasing — it's atomic per file, not per row.

This is **the** driver that makes line-status rebase a first-class
requirement in the rewrite alongside LSP diagnostics.

## Edge cases and gotchas

- **Empty `statuses: Vec<LineStatus>` are meaningful.** The driver emits
  them when a previously-dirty path is now clean. The model must treat
  an empty vec as "clear the gutter," not "no-op." See the `tracked`
  set dance in `crates/git/src/lib.rs:66-83`.
- **`scan_file_statuses` and `scan_line_statuses` both return `Option`
  and silently drop on error.** If `Repository::open` fails (e.g. the
  directory is not a git repo), the whole scan produces no events —
  neither file statuses nor line statuses nor branch update. Standalone
  mode (no workspace) avoids this entirely: the derived emitter for
  `ScanFiles` filters on `workspace.loaded()` so no command is ever sent.
- **`scan_line_statuses` reads the worktree via `std::fs::read`, not
  through the docstore.** A file with unsaved buffer edits produces line
  statuses against the *on-disk* version, not the buffer rope. This is
  intentional: git line status is "what's different from INDEX," and
  INDEX is a disk concept. The buffer view is the user's source of
  truth for what they're typing; the gutter shows what the repo will
  see at save time.
- **The "precedence" sort is load-bearing.** `scan_line_statuses`
  produces both `StagedModified` (from HEAD↔INDEX) and `Unstaged` (from
  INDEX↔WORKTREE) hunks, potentially overlapping on the same row. The
  sort at `lib.rs:212-217` ensures `Unstaged` wins at display time via
  the binary search in `line_category_at`. Don't break this in the
  rewrite — reorder and the gutter colour is wrong on half-staged hunks.
- **Branch detection silently falls through to `None` for detached
  HEAD.** `head().shorthand()` returns `None` when HEAD is not a symbolic
  ref. The model interprets `None` as "no branch line in the status bar."
- **Untracked files appear in `file_statuses` with `IssueCategory::Untracked`
  but get no line-status entry.** `scan_line_statuses` is only called on
  paths that `git_statuses` returned, and the HEAD/INDEX lookup for a
  brand-new untracked file returns empty blobs — every line of the file
  becomes an `Unstaged +`, which matches the "all new" intuition.
- **`WT_RENAMED` is folded into `Unstaged`, `INDEX_RENAMED` into
  `StagedModified`.** There is no separate "renamed" category; a renamed
  file that also has content changes shows as modified. This matches
  `git status` short-form behavior closely enough for the sidebar.
- **Ordering of emitted events matters.** File statuses are always sent
  *before* the per-file line statuses. The model's reducer for
  `GitFileStatuses` clears stale sidebar entries that the subsequent
  line-statuses then repopulate. Reversing the order would briefly
  show line decorations for files that aren't in the sidebar map yet.

## Goldens checklist

Scenarios under `tests/golden/drivers/git/`:

1. `file_statuses/` — exists. Startup of a workspace with a known-dirty
   file. Asserts `GitIn::FileStatuses` arrives once with the expected
   branch and status map.
2. `line_statuses_dirty/` — `[unclear — not yet authored]`. Edit a
   tracked file, save, expect both `FileStatuses` and a `LineStatuses`
   with a non-empty range.
3. `line_statuses_clear/` — edit, save, then revert: expect a second
   `LineStatuses` with `statuses: []` for the formerly-dirty path.
4. `branch_change/` — checkout a different branch externally, trigger
   `GitChanged`, expect `FileStatuses` with the new `branch: Some(...)`.
5. `detached_head/` — checkout a bare commit, expect `branch: None`.
6. `staged_and_unstaged/` — modify a file, stage partially, modify again;
   expect overlapping `StagedModified`/`Unstaged` ranges with the correct
   precedence resolution at display time.
7. `not_a_repo/` — point workspace at a non-git dir, verify no `GitIn`
   events fire (scan returns `None` internally).
8. `scan_coalesce/` — rapid save / `GitChanged` burst; expect a single
   `ScanFiles` dispatch 50 ms after the last event (validates the timer
   driver's `Replace` schedule).
