# Milestone 19 — Git integration

Nineteenth vertical slice. After M19 the editor surfaces three
git signals that every other editor in the space takes for
granted: **branch name** in the status bar, **per-file status
letters** (`M`, `A`, `U`, `•`) in the browser, and **per-line
change bars** in the gutter. All three are wired to the existing
`IssueCategory` taxonomy — the same enum the file browser
painter + gutter ladder already consume — so the surface area
is mostly *data plumbing*. Painting already knows how to pick
the winning category per file / line.

Prerequisite reading:

1. `docs/spec/git.md` — whole file. Authoritative behaviour
   reference (detection, file / line mapping onto
   `IssueCategory`, branch, clearing-dirty discipline, error
   paths).
2. `docs/drivers/git.md` — the libgit2 ABI + internal state
   (`tracked: HashSet<CanonPath>`), ordering guarantees (file
   statuses before line statuses), and edge cases.
3. `crates/core/src/issue.rs` — the single source of truth for
   the category enum. The git variants (`Unstaged`,
   `StagedModified`, `StagedNew`, `Untracked`) already exist
   there, pre-plumbed for M19.
4. Legacy `led/crates/git/src/lib.rs` (269 LOC) — the exact
   behaviour we're porting: libgit2 status, HEAD↔INDEX and
   INDEX↔WORKTREE diffs, `+`-line collection, precedence sort,
   tracked-set clearing.
5. Legacy `led/crates/core/src/git.rs` — `LineStatus`,
   `line_category_at`, `best_category_at`. We port these into
   `led-core::git` on this branch.
6. `goldens/scenarios/driver_events/git/file_statuses/` and
   `goldens/scenarios/features/git/workspace_open_file/` — the
   two pre-authored scenarios this milestone moves to green.

---

## Goal

```
$ cargo run -p led -- tracked.txt untracked.txt
# After the workspace scan settles (~once at startup):
# * Status bar's left half reads ` main ● tracked.txt` —
#   branch first, then the dirty indicator.
# * The browser lists:
#       tracked.txt       (no letter — clean)
#     U untracked.txt     (Untracked, cyan)
# * Gutter for tracked.txt (after saving an edit) shows a
#   coloured bar at the modified row:
#       │  │ edited line             ← unstaged yellow
#       │  │ untouched line
```

When the workspace is not a git repo (`repo.open` fails), every
indicator stays empty — no alert, no spinner, silent no-op.
Matches legacy exactly.

## Scope

### In

- **`led-core::git` module** — port from legacy.

  ```rust
  pub struct LineStatus {
      pub category: IssueCategory,
      pub rows:     std::ops::Range<usize>,
  }

  pub fn line_category_at(statuses: &[LineStatus], row: usize)
      -> Option<IssueCategory>;
  pub fn best_category_at(statuses: &[LineStatus], row: usize)
      -> Option<IssueCategory>;
  ```

  `line_category_at` is a binary search on a non-overlapping
  sorted list; `best_category_at` is an O(n) scan that picks
  the winning precedence when ranges *can* overlap (git
  unstaged + PR diff, future). M19 uses `best_category_at`
  since git alone already produces overlapping ranges
  (unstaged vs staged).

- **`state-git` crate** — new workspace member.

  ```rust
  pub struct GitState {
      pub branch:         Option<String>,
      pub file_statuses:  imbl::HashMap<CanonPath, imbl::HashSet<IssueCategory>>,
      pub line_statuses:  imbl::HashMap<CanonPath, Arc<Vec<LineStatus>>>,
  }
  ```

  `imbl` everywhere so clones are pointer-cheap (drv memos
  over `BrowserDerivedInputs` project across this atom). The
  inner `Vec<LineStatus>` is `Arc`-wrapped for the same
  reason — gutter memos re-paint on identity change, not on
  structural equality.

- **`driver-git` crate pair (core + native)** — new workspace
  members.

  ```rust
  // core — ABI shared with runtime
  #[derive(Debug, Clone)]
  pub enum GitCmd {
      ScanFiles { root: CanonPath },
  }

  #[derive(Debug, Clone)]
  pub enum GitEvent {
      FileStatuses {
          statuses: HashMap<CanonPath, HashSet<IssueCategory>>,
          branch:   Option<String>,
      },
      LineStatuses {
          path:     CanonPath,
          statuses: Vec<LineStatus>,
      },
  }

  pub trait Trace: Send + Sync {
      fn git_scan_start(&self, root: &CanonPath);
      fn git_scan_done(&self, ok: bool, n_files: usize);
  }

  pub struct GitDriver { /* tx + rx + trace */ }
  impl GitDriver {
      pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a GitCmd>);
      pub fn process(&self) -> Vec<GitEvent>;
  }
  ```

  `native` spawns a single `std::thread` (per
  `feedback_no_tokio_for_drivers`) with a bounded mpsc inbox.
  The worker holds `tracked: HashSet<CanonPath>` across
  scans so a previously-dirty path transitioning to clean
  emits an explicit `LineStatuses { statuses: vec![] }` —
  that's the signal the runtime uses to clear gutter bars.

  Port the libgit2 logic from `led/crates/git/src/lib.rs`
  verbatim: `scan_file_statuses` (repo.statuses +
  repo.head().shorthand()) and `scan_line_statuses`
  (HEAD↔INDEX → `StagedModified`, INDEX↔WORKTREE →
  `Unstaged`, sort by start row with unstaged-wins precedence
  tie-break).

- **Runtime wiring** —

  - `Drivers` grows `git: GitDriver` + `_git_native: GitNative`.
  - `Atoms` grows `git: GitState` + `git_scan_pending: bool`
    (dispatcher-owned flag: next tick emits `GitCmd::ScanFiles`).
  - `spawn_drivers` wires the git driver alongside the others.
  - Main-loop ingest loop picks up `GitEvent::FileStatuses`
    (replaces the whole map + branch) and `GitEvent::LineStatuses`
    (per-path insert, or remove-on-empty for the "clear" case).
  - Scan is emitted:
    - **Startup** — once `fs.root` is `Some` and we haven't
      scanned yet (paralleling the `lsp_init_sent` pattern).
    - **On save** — after any successful `file_write` completion
      sets `git_scan_pending = true`. The main loop's execute
      phase drains the flag into a `GitCmd::ScanFiles`.
  - Emits `GitScan\troot=<p>` through `Trace::git_scan_start`
    on every dispatched command.

- **Browser integration** — `file_categories_map` memo
  (query.rs) extends to union `GitState.file_statuses` into the
  result alongside the LSP-derived categories. No painter
  change: the browser row painter already calls
  `resolve_display` on the merged set, so file-status letters
  appear for free.

- **Gutter integration** — `body_model` (query.rs) grows a
  `GitLineStatusesInput` nested input and, per rendered row,
  consults `best_category_at` across the merged diagnostic +
  git ranges for that buffer. The priority ladder (spec
  `git.md`):

  `LspError > LspWarning > Unstaged > StagedModified/New > PrComment/PrDiff`

  The painter already translates the winning category through
  `Theme::category_style` (via `git_modified`, `git_added`,
  `git_untracked` + the existing diagnostics palette).

- **Status bar** — `status_bar_model` grows a
  `GitStateInput<'a>` and, in the default left-string branch,
  prepends `" {branch}"` before `"{modified}{lsp}"`. Matches
  legacy's ` {branch}{modified}{pr}{lsp}` shape (PR lands at
  M27). No branch → no leading segment, just the current
  default.

- **Theme styles** — the three chrome slots
  (`git_modified`, `git_added`, `git_untracked`) already exist
  with legacy-matching defaults. `Theme::category_style` maps
  `IssueCategory::Unstaged → git_modified`,
  `StagedModified|StagedNew → git_added`,
  `Untracked → git_untracked` already. No new theme slots.

### Out

Per `ROADMAP.md` M19 and the explicit deferrals in
`docs/spec/git.md`:

- **Debounced scan via the timers driver.** Legacy coalesces
  bursts (`git_file_scan` 50ms `Replace`); the rewrite has no
  timers driver yet (out-of-contract until later). M19 scans
  once at startup + once per buffer save. Burst coalescing is
  trivial because saves are user-paced, not machine-paced — the
  gap between two `Ctrl-S`s is always > 50ms. When the timers
  driver lands, insert a `Replace(50ms)` between the flag bump
  and the dispatch without changing any other code.
- **Rebase line statuses through the edit log.** Legacy
  doesn't; neither does M19. Per `docs/drivers/git.md`: the
  gutter "temporarily lies during rapid typing and self-
  corrects on the next scan." A future milestone (likely
  coupled to M26 external-watcher work) can graft the rebase
  primitive on without rewriting the driver.
- **`WorkspaceChanged` signal** (external `git checkout`
  retriggering a rescan). No workspace driver yet. When M26
  lands (file watcher), wire its notifications into
  `git_scan_pending` and the scan self-refreshes on external
  activity.
- **`Resumed` trigger** (post-SIGTSTP rescan). Ships with
  M20 (lifecycle / suspend); pure one-line addition.
- **`NextIssue` / `PrevIssue` cycle extension.** Deferred to
  **M20a** per the roadmap. M19 only *populates*
  `GitState.file_statuses` / `.line_statuses` — the nav
  dispatch that walks them ships with the tiered nav
  implementation. `IssueCategory::NAV_LEVELS` already contains
  the git levels; M20a writes the `collect_positions` lens
  that joins diagnostics + git + (later) PR.
- **Directory rollups in the browser** (a parent dir showing
  the merged category of its descendants). `directory_categories`
  already exists in `led-core::issue`; the browser painter
  doesn't yet call it. Rollups land with M27 or a follow-up
  when the UX warrants.
- **`File staged for delete`** (`WT_DELETED` /
  `INDEX_DELETED`). Legacy silently ignores these — the path
  isn't present as an open tab, so there's nothing to decorate.
  M19 matches.
- **Submodules.** Excluded at `StatusOptions::exclude_submodules`
  per legacy.
- **Alerts on scan failure.** Silent: legacy doesn't alert on
  `repo.open` failure, and neither does M19. The gutter /
  sidebar simply stays blank. Spec `git.md` § "Error paths"
  explicitly calls this out as the contract.

## Key design decisions

### D1 — Driver is stateless about repo identity

Every `ScanFiles` re-opens the repo via
`git2::Repository::open(root)`. libgit2 memory-maps `.git/`
cheaply; holding a handle across calls would require
invalidation on external mutations (the `WorkspaceChanged`
case). Matches legacy exactly.

### D2 — `tracked` set lives in the driver, not the atom

The clear-gutter-on-revert signal needs a list of
previously-dirty paths to diff against. Option A: the atom
stores it; the driver emits only non-empty LineStatuses and
the runtime computes the diff. Option B: the driver stores
it; the driver emits explicit empty-LineStatuses for
formerly-dirty paths.

B wins. The *observation* "this path had lines last scan and
has none now" is a driver-internal fact, not a user-visible
state. Bundling the diff into the driver's emission keeps the
atom's shape dead-simple (per-path presence/absence = current
truth) and matches legacy `crates/git/src/lib.rs:66-83`.

### D3 — File + line statuses are one scan, two emissions

Legacy pushes `FileStatuses` first, then each non-empty
`LineStatuses`, then each empty clear-event. M19 preserves
the ordering because the ingest reducer in the runtime clears
stale sidebar entries via the file-statuses snapshot, and the
subsequent line-statuses populate gutter marks only for the
paths the snapshot confirmed are dirty. Reversing the order
would flash uncatalogued gutter bars for a frame.

`driver-git/native` serialises both emissions from one
`spawn_blocking`-equivalent thread turn, so the runtime sees
them in order every time.

### D4 — `GitState` lives in its own crate, not on a buffer

LSP diagnostics were folded into `DiagnosticsStates.by_path`
because they have no repo-level signal alongside the per-
buffer data. Git has **three different scales**:

- Repo-wide (`branch`).
- Per-file (`file_statuses`, including for files not open
  as buffers — for browser decoration).
- Per-line (`line_statuses`, for open buffers + their
  gutter).

Bundling into `GitState` keeps all three updated from the same
scan without needing a reducer that decomposes a payload over
three separate atoms. It also keeps memo invalidation tight —
only browser + gutter + status-bar cross the boundary.

### D5 — No debounce primitive for M19

Legacy's 50ms `Replace` debounce exists because rapid save +
`GitChanged` events could queue ten `ScanFiles` and the worker
thread would churn. Both triggers are absent in M19:

- Rapid saves are user-paced (even autosave on edit — deferred
  — would gate on a minimum interval).
- `GitChanged` is workspace-driver territory (M26 + a future
  workspace crate).

So for M19, the single "scan after save" rule plus the
startup one-shot produce at most one scan per second of human
activity — no debounce needed. When the timers driver lands
and `GitChanged` ships, insert a `Replace(50ms)` Timer between
the `git_scan_pending` flag bump and the dispatch. One-line
change.

### D6 — Line statuses don't rebase in M19

Legacy doesn't rebase line statuses through the edit log; the
gutter self-corrects on the next scan. The rewrite inherits
the imperfection deliberately because:

- The LSP no-smear rule shows staleness by *hiding*, not
  rebasing. Gutter bars are additive, not load-bearing for
  correctness, so "stale for 50ms" is acceptable UX.
- Rebase would require threading `EditedBuffer.history`
  through the ingest reducer, which couples this driver to
  the buffer-edits crate in a way that's hard to undo when
  we *do* want to rebase (future PR-diff work might need a
  fundamentally different shape).

When rebase finally lands, it lives in a dedicated
`rebase_line_statuses` helper in `led-runtime`, same as the
LSP replay path — not in the driver.

### D7 — Gutter priority is a single helper

`docs/spec/git.md` locks in the priority ladder:

```
LspError > LspWarning > Unstaged > StagedModified/StagedNew > PrComment/PrDiff
```

`IssueCategory::precedence()` already encodes this numeric
order. The body_model gutter pass computes:

```rust
let lsp_cat = diagnostics_category_for(row);        // existing
let git_cat = git_line_category_at(row);            // new
let winner = [lsp_cat, git_cat]
    .into_iter().flatten()
    .min_by_key(|c| c.precedence());
```

No ad-hoc priority table in the painter — every extension
(PR at M27) is one `.into_iter().flatten()` addition.

### D8 — Scan dispatch is an execute-phase flag drain

Same pattern as save + file-search + LSP:

```rust
// dispatch phase — mutate AppState
if save_happened || startup && !git_scanned_yet {
    atoms.git_scan_pending = true;
}

// execute phase — drain the flag
if std::mem::take(&mut atoms.git_scan_pending)
    && let Some(root) = fs.root.as_ref()
{
    drivers.git.execute(std::iter::once(&GitCmd::ScanFiles {
        root: root.clone(),
    }));
}
```

No memo, no lens — the flag is driver-outbound bookkeeping
(`feedback_no_driver_types_in_appstate` specifically excludes
driver protocol types; a plain `bool` is fine).

## Types

### `led-core::git` (new module)

```rust
// crates/core/src/git.rs
use std::ops::Range;
use crate::IssueCategory;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineStatus {
    pub category: IssueCategory,
    pub rows:     Range<usize>,
}

pub fn line_category_at(statuses: &[LineStatus], row: usize)
    -> Option<IssueCategory>;
pub fn best_category_at(statuses: &[LineStatus], row: usize)
    -> Option<IssueCategory>;
```

### `state-git` (new crate)

```rust
use std::sync::Arc;
use imbl::{HashMap, HashSet};
use led_core::{CanonPath, IssueCategory, git::LineStatus};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitState {
    pub branch:        Option<String>,
    pub file_statuses: HashMap<CanonPath, HashSet<IssueCategory>>,
    pub line_statuses: HashMap<CanonPath, Arc<Vec<LineStatus>>>,
}
```

### `driver-git/core` (new crate)

```rust
// types above, plus the driver handle
pub struct GitDriver {
    tx:    mpsc::Sender<GitCmd>,
    rx:    mpsc::Receiver<GitEvent>,
    trace: Arc<dyn Trace>,
}
```

### `driver-git/native` (new crate)

```rust
pub struct GitNative { /* thread join handle */ }

pub fn spawn(trace: Arc<dyn Trace>, notify: Notifier)
    -> (GitDriver, GitNative);
```

Worker thread body: loop over `cmd_rx.recv()`, on
`ScanFiles(root)` run the two scan helpers (ported from
legacy), emit `FileStatuses` + each `LineStatuses` +
clear-events for vanished paths, update `tracked`.

### Runtime additions

- `Atoms.git: GitState`
- `Atoms.git_scan_pending: bool`
- `Atoms.git_scanned_initial: bool`
- `Drivers.git: GitDriver` + `_git_native: GitNative`
- `query::GitStateInput<'a>` (`drv::Input`-derived)
- `query::StatusBarInputs` grows `git: GitStateInput<'a>`
- `query::BodyInputs` grows `git: GitStateInput<'a>`
- `query::file_categories_map` grows a `git: GitStateInput<'a>`
  second positional input
- `trace::Trace::git_scan_start(&CanonPath)` added (alongside
  the existing `lsp_request_diagnostics` pattern); `FileTrace`
  writes `GitScan\troot=<p>`.

## Crate changes

```
crates/
  core/src/
    git.rs                   NEW — LineStatus + line_category_at +
                              best_category_at
    lib.rs                   + pub mod git;
  state-git/                 NEW — GitState atom
  driver-git/core/           NEW — GitCmd / GitEvent / Trait / Handle
  driver-git/native/         NEW — libgit2 scan worker
  runtime/src/
    lib.rs                   + Drivers.git, Atoms.git +
                              git_scan_pending + ingest + execute +
                              startup scan trigger + post-save trigger
    trace.rs                 + git_scan_start; FileTrace emits GitScan
    query.rs                 + GitStateInput; file_categories_map
                              merges git; body_model picks winning
                              category; status_bar_model prepends
                              branch
    dispatch/save.rs         set git_scan_pending after save completes
    dispatch/mod.rs          Dispatcher.git_scan_pending: &mut bool
                              (no, use the flag on Atoms, dispatched
                              from the execute-phase save completion
                              path in lib.rs — no dispatcher touch)
```

New workspace members:

```toml
"crates/state-git",
"crates/driver-git/core",
"crates/driver-git/native",
```

With workspace-dep aliases `led-state-git`,
`led-driver-git-core`, `led-driver-git-native`.

## Testing

### `led-core::git`
- `line_category_at` finds a covering range via binary search.
- `line_category_at` returns `None` off the end of the list.
- `best_category_at` picks unstaged over staged on overlap.
- `best_category_at` picks LspError over Unstaged when a
  merged list crosses sources.

### `state-git`
- `GitState::default` is empty (no branch, empty maps).
- Inserting a path + categories round-trips.
- Clearing a path via `remove` reflects via `is_empty` on the
  specific path's entry.

### `driver-git` (core)
- `execute` forwards a batch into the channel.
- `process` returns the stashed events (unit-test with a mock
  `mpsc::Sender<GitEvent>`).

### `driver-git` (native) — integration-style
- Spawn on a temp repo with one modified file + one untracked
  file. Expect:
  - `FileStatuses` with both entries; branch = `main` or
    whatever `git init` produces.
  - `LineStatuses` for the modified file with one range.
- Second scan after staging the modified file — expect the
  category to move from `Unstaged` to `StagedModified` and an
  empty `LineStatuses` for the staged copy when the worktree
  matches INDEX (the clear-event case).
- Scan on a non-repo path — no events (silent no-op).
- Scan on a detached HEAD — `branch = None`.

### `runtime::query`
- `file_categories_map` merges a git-only file correctly.
- `file_categories_map` prefers LspError for a file that has
  both an error and unstaged changes (via `resolve_display`
  precedence).
- `status_bar_model` prepends ` main` when `git.branch` is
  `Some("main")`.
- `status_bar_model` omits the branch segment when
  `git.branch` is `None`.
- `body_model` picks the git unstaged category for a row with
  no diagnostic.
- `body_model` picks LspError over git unstaged for a row with
  both.

### `runtime::run`
- Startup with `fs.root = Some(_)` emits one
  `GitCmd::ScanFiles` in the first tick's execute.
- A successful write-completion sets
  `git_scan_pending = true` and the next execute phase fires
  exactly one further `ScanFiles`.
- Non-repo workspace — the scan is dispatched but no
  `FileStatuses` / `LineStatuses` events reach the runtime
  (verified via the mock driver).

Expected: +25 tests.

## Done criteria

- All existing tests pass.
- New tests green.
- Clippy unchanged from post-M18 baseline.
- Interactive smoke on the rewrite's own workspace:
  - `cd /Users/martin/dev/led-rewrite && cargo run -p led`.
    Status bar shows ` rewrite ●path` (current branch + dirty
    marker + path). Edit a line → gutter shows yellow bar at
    that row after the save scan. Check that unstaged changes
    decorate the browser row with `M`.
- Goldens:
  - `goldens/scenarios/driver_events/git/file_statuses/` —
    green. The `GitScan` trace fires; the `U` letter stays
    absent because `git_init = true` creates an empty `.git/`
    that libgit2 refuses to open, which is the documented
    silent-no-op contract.
  - `goldens/scenarios/features/git/workspace_open_file/` —
    **partial** green. M19 makes the `GitScan` trace line
    match; the remaining mismatch
    (`WorkspaceFlushUndo` + `WorkspaceCheckSync`) is
    workspace-driver territory — it lands with the session /
    external-watcher milestones (M21 + M26). The scenario
    is deliberately *left failing* post-M19 to signal the
    dependency chain clearly in the test report.

## Growth-path hooks

- **Real debouncing (`Replace(50ms)`)** — drop-in when the
  timers driver lands. The dispatch-side flag doesn't change;
  only the drain gets gated on a timer fire.
- **`WorkspaceChanged` rescan** — M26 external-watcher hook.
  Sets `git_scan_pending` from the watcher ingest.
- **`Resumed` rescan** — M20 lifecycle. Sets
  `git_scan_pending` on the `Running → Running` phase
  re-entry from `Suspended`.
- **Line-status rebase through the edit log** — future. The
  rebase helper lives in the runtime alongside
  `replay_diagnostics`; inputs are `(prev_rope_hash, edit_log,
  Vec<LineStatus>)`. No driver impact.
- **PR tier (`PrComment`, `PrDiff`)** — M27. The gutter's
  `best_category_at` merge naturally picks up the PR state's
  line ranges via an extra input.
- **Directory rollups** — `directory_categories` is already
  written; the browser painter gains a rollup call when a
  dir's descendants carry any category and the UX needs it.
- **Gh-pr branch ETag polling** — M27 reads `GitState.branch`
  to key its poll cycle; no data-shape change on this side.
