# git

## Summary

led surfaces git awareness at three levels: a **branch name** in the status
bar, **file-level status badges** in the sidebar and status bar (untracked,
staged, modified), and **line-level change bars** in the gutter (staged vs.
unstaged added/modified lines). All three come from a single repo-wide scan
performed by the `git` driver via libgit2, debounced behind a short timer so
that bursts of activity (saves, workspace events) produce one scan, not many.
When the current workspace is not a git repository, all three indicators
remain empty and the driver simply produces nothing — there is no error
surface for "not a repo".

## Behavior

**Detection is workspace-scoped.** The `git` driver receives a single
`GitOut::ScanFiles { root }` command. The root is taken from the loaded
workspace; in standalone / no-workspace mode the command is never emitted.
Opening the repo is attempted fresh on each scan — if `git2::Repository::open`
fails, the scan silently returns nothing (no alert, no partial state) and
branch/file/line status stay at their previous values. The driver itself is
stateless about repo identity; each scan resolves the workdir, walks
statuses, and diffs blobs.

**File-level statuses** come from `repo.statuses(...)` with untracked files
and untracked directories included and submodules excluded. Each entry is
mapped onto the `IssueCategory` enum (the single source of truth for
status-like categories across git, LSP, and PR):

- `WT_MODIFIED` / `WT_RENAMED` → `Unstaged`
- `INDEX_MODIFIED` / `INDEX_RENAMED` → `StagedModified`
- `INDEX_NEW` → `StagedNew`
- `WT_NEW` → `Untracked`

A file can carry multiple categories simultaneously. When the browser or
status bar needs to pick one display glyph, `IssueCategory::resolve_display`
(and its `precedence()` ordering Unstaged > StagedNew > StagedModified >
Untracked) chooses the highest-precedence category. `CategoryInfo` provides
the letter (`M`, `A`, `U`, or a bullet for categories without a letter) and
the theme key (`git.modified`, `git.added`, `git.untracked`) used by the
gutter, sidebar, and directory rollups. Directory totals are computed by
`directory_categories` walking the file map with the directory prefix.

**The branch name** is the shorthand of `repo.head()` (e.g. `main`,
`feature/foo`) and is emitted alongside each `FileStatuses` payload. Status
bar rendering uses it as the git indicator; an empty/detached HEAD produces
`None`. The branch stream is also the trigger for PR metadata — see
`gh-pr.md`.

**Line-level statuses** are computed for every file that `FileStatuses`
reports as non-clean. For each such path, the driver reads three byte
buffers — the HEAD blob (may be empty for new files), the INDEX blob (may
be empty for unstaged files), and the worktree file on disk — and runs two
diffs: HEAD↔INDEX attributes added lines as `StagedModified`, INDEX↔WORKTREE
attributes them as `Unstaged`. Ranges are coalesced when adjacent and sorted
by start row; on overlap, unstaged wins (it sorts first by precedence tie-
break, and the `line_category_at` helper uses a binary search that hits the
earlier entry). Only `+` lines from the diff are recorded — deletions and
context lines are intentionally dropped, so the gutter shows a bar on each
line that was *added or modified* in the new side of the diff.

**Clearing dirty files.** The driver tracks paths that produced non-empty
line statuses in the previous scan. If a path no longer has any dirty lines
(e.g. user saved a clean revert), the next scan emits an empty
`LineStatuses` for it. This is how gutter bars disappear when an edit is
reverted.

**Debounced rescan on activity.** `GitState::pending_file_scan` is a
`Versioned<()>` bumped by every signal that could have changed git state:

- workspace load and `WorkspaceChanged` events (external fs changes below
  the root);
- `BufferSaved` and `BufferSavedAs` (user Ctrl-S, SaveAs, format-on-save);
- `Resumed` (leaving SIGTSTP-suspend);
- `GitChanged` events from the workspace driver (external `git` command
  detection).

Each bump triggers the `git_file_scan` timer (50ms, `Schedule::Replace`),
which on fire bumps `scan_seq`. The `derived.rs` chain then emits a
`GitOut::ScanFiles`. The 50ms timer coalesces bursts (e.g. saving many files,
`git checkout` rewriting a tree) into one scan. (Timer duration itself is
out-of-contract for the rewrite.)

**Gutter rendering.** Line statuses live on `BufferStatus::git_line_statuses`.
The gutter pass queries `best_category_at(statuses, row)` for each visible
row to pick the precedence-winning category and its `theme_key`. In
conjunction with LSP diagnostics, gutter colors follow a fixed priority
ladder (LspError > LspWarning > Unstaged > StagedModified/New >
PrComment/PrDiff).

**Sidebar rendering.** The browser row for a file shows the resolved
letter/bullet and theme color from `resolve_display` on the file's
`IssueCategory` set. Directories display the aggregated category set for
everything underneath.

**Status bar.** The branch name appears in the status bar when
`state.git.branch` is `Some`. The active buffer's own modification indicator
(e.g. the leading `●` seen in the golden frame) is derived from buffer
dirty-state rather than from git itself, but the two read as a unified "what
has changed" story.

## User flow

User opens a workspace that is a git repo. After startup the 50ms debounce
fires; `GitScan` dispatches, the driver returns `FileStatuses` and a cascade
of `LineStatuses`. Branch appears in status bar; untracked files show `U`
and modified files show `M` in the sidebar; the gutter of any open dirty
file renders bars. User edits a file; the save (Ctrl-S) triggers another
scan after the debounce; the new worktree diff produces fresh line statuses
and the gutter updates. User runs `git add -p` in a terminal; the
workspace driver's `GitChanged` event triggers a rescan; lines that moved
from `Unstaged` to `StagedModified` change color. User reverts a file; the
rescan produces no dirty lines for that path; the driver emits an empty
`LineStatuses` and the gutter bars vanish.

## State touched

- `GitState.branch: Option<String>` — current branch shorthand.
- `GitState.file_statuses: HashMap<CanonPath, HashSet<IssueCategory>>` —
  per-file category set.
- `GitState.pending_file_scan: Versioned<()>` — request to schedule a scan.
- `GitState.scan_seq: Versioned<()>` — bumped when the debounce timer fires;
  observed by derived to emit `GitOut::ScanFiles`.
- `BufferStatus.git_line_statuses: Vec<LineStatus>` — per-buffer gutter data.
  Stored on the buffer so the gutter renderer only touches the active buffer.
- `BufferState.git_line_statuses()` — reader used by the gutter pass.

## Extract index

- Actions: none directly; `GitChanged` is a `Mut` produced by the workspace
  driver, not an `Action`. → `docs/extract/actions.md`
- Keybindings: none specific to git status.
- Driver events:
  - `GitOut::ScanFiles { root }` (outbound)
  - `GitIn::FileStatuses { statuses, branch }`
  - `GitIn::LineStatuses { path, statuses }`
  - `WorkspaceIn::GitChanged` (indirect trigger) → `docs/extract/driver-events.md`
- Timers: `git_file_scan` (50ms, Replace) → `docs/extract/driver-events.md` §Timers.
  [note: duration is out-of-contract per `project_rewrite_scope.md`.]
- Config keys: none.

## Edge cases

- **Not a repo.** `git2::Repository::open` fails; scan silently returns
  `None`. Branch stays `None`, file/line statuses stay empty. No alert.
- **Detached HEAD.** `repo.head().shorthand()` is `None`; branch rendered as
  empty; PR subsystem does not attempt to load.
- **File in repo but not yet on disk** (newly staged, worktree deleted): no
  `fs::read` → path skipped in line-status pass; file-level `Untracked`/`New`
  still reported.
- **Large repos.** The scan is synchronous inside `spawn_blocking`; per-file
  diffs are blob-vs-blob, not worktree-walking. Scaling is the
  responsibility of libgit2. [unclear — no maximum workspace size test.]
- **Bitmap of dirty files shrinks between scans.** Previously tracked paths
  that are no longer dirty receive an explicit empty `LineStatuses` so the
  gutter clears.
- **Submodules.** Explicitly excluded via `StatusOptions::exclude_submodules`.
- **Symlinks.** Scanning operates on repo paths reported by libgit2, then
  canonicalized via `UserPath::canonicalize`. Out-of-repo symlink targets are
  not reachable from the git scan (they'd be outside the repo).
- **Multiple categories on one line.** `best_category_at` resolves by
  precedence; the sort order ensures unstaged beats staged on overlap.
- **Concurrent scan.** The driver channel is 64-deep and processes commands
  sequentially; bursts queue rather than overlap. The 50ms debounce in
  derived is the primary rate-limiter.

## Error paths

- **Repo open failure (not a repo / permissions / corrupted).** Silent: the
  scan produces no `GitIn::FileStatuses`, state stays untouched. No alert.
- **libgit2 status query failure.** Same as above — the scan falls through
  its `?` operators and emits nothing.
- **Worktree `fs::read` error.** Path skipped for line statuses; file-level
  status still reported. Gutter for that path will not update.
- **`Patch::from_buffers` failure** (binary file, invalid utf-8 in diff
  machinery). The hunk loop is skipped for that diff — that path may still
  have a file-level badge but no line bars.
- **Externally-modified `.git` directory mid-scan.** libgit2 may return a
  partial error; the scan falls through and retains the previous values.
  The next `WorkspaceChanged` / save will re-trigger.

[unclear — there is no user-facing alert for "git scan failed"; a silent
retain-last-values policy is the current contract. Should the rewrite add a
warn/error path, that becomes a new spec line.]
