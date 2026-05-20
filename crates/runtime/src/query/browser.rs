//! Browser tree and file-category derived memos.

use led_core::CanonPath;
use led_driver_fs_list_core::{ListCmd, TreeEntry, TreeEntryKind};
use std::sync::Arc;

use super::inputs::*;

/// Stage 1 of [`file_categories_map`] — LSP diagnostics → per-file
/// category set. Error/Warning only; Info/Hint never colour the
/// browser.
///
/// Caches on `DiagnosticsStatesInput` only. A git status churn
/// (file-statuses HashMap mutation) does not invalidate this
/// stage, so the diagnostic→category walk runs only when
/// diagnostics actually change.
#[drv::memo(single)]
pub fn diag_categories_map<'d>(
    diagnostics: DiagnosticsStatesInput<'d>,
) -> Arc<imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>>> {
    let mut map: imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>> =
        imbl::HashMap::default();
    for (path, bd) in diagnostics.by_path.iter() {
        for d in bd.diagnostics.iter() {
            let cat = match d.severity {
                led_state_diagnostics::DiagnosticSeverity::Error => {
                    led_core::IssueCategory::LspError
                }
                led_state_diagnostics::DiagnosticSeverity::Warning => {
                    led_core::IssueCategory::LspWarning
                }
                _ => continue,
            };
            map.entry(path.clone())
                .or_default()
                .insert(cat);
        }
    }
    Arc::new(map)
}

/// Stage 2 of [`file_categories_map`] — git file statuses → per-file
/// category set. `git.file_statuses` already arrives keyed by path
/// with category sets per file; this stage clones it into the
/// shared `imbl::HashMap<CanonPath, HashSet<IssueCategory>>` shape
/// so the merge can union it with the diagnostic side.
///
/// Caches on `GitStateInput` only. A diagnostic churn (the
/// other side of the merge) does not invalidate this stage.
#[drv::memo(single)]
pub fn git_categories_map<'g>(
    git: GitStateInput<'g>,
) -> Arc<imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>>> {
    let mut map: imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>> =
        imbl::HashMap::default();
    for (path, cats) in git.file_statuses.iter() {
        map.insert(path.clone(), cats.clone());
    }
    Arc::new(map)
}

/// Per-file category set for the whole workspace. Feeds the
/// browser painter + the Alt-./ nav cycle.
///
/// LSP Error / Warning, plus git file-level categories (Unstaged,
/// StagedModified, StagedNew, Untracked). Info / Hint are filtered
/// out — they never colour the browser.
///
/// Composed from [`diag_categories_map`] + [`git_categories_map`].
/// Top-level memo continues to take the original `DiagnosticsStatesInput +
/// GitStateInput` inputs because drv's `#[drv::memo]` requires `drv::Input`
/// projection types — it cannot take `Arc<imbl::HashMap<...>>` from the
/// intermediates as inputs directly. The cache-narrowing benefit accrues
/// to the two intermediates: a diagnostic-only churn re-runs only the
/// diag stage, a git-only churn re-runs only the git stage, and the
/// merge re-runs only when one side's `Arc` identity actually changed
/// (cheap union over two already-built maps).
#[drv::memo(single)]
pub fn file_categories_map<'d>(
    diagnostics: DiagnosticsStatesInput<'d>,
    git: GitStateInput<'d>,
) -> Arc<imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>>> {
    let diag_map = diag_categories_map(diagnostics);
    let git_map = git_categories_map(git);

    // Short-circuit when one side is empty — Arc-clone the other,
    // avoiding both the unioning walk and the allocation. Idle
    // ticks hit this path (no diagnostics, no git changes).
    if diag_map.is_empty() {
        return git_map;
    }
    if git_map.is_empty() {
        return diag_map;
    }

    // Union the two maps. `IssueCategory::resolve_display` picks the
    // winning letter / colour when a path carries both a diagnostic
    // and a git category (LSP precedes git by
    // `IssueCategory::precedence`).
    let mut map: imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>> =
        (*diag_map).clone();
    for (path, cats) in git_map.iter() {
        let entry = map.entry(path.clone()).or_default();
        for c in cats.iter() {
            entry.insert(*c);
        }
    }

    // PR membership arrives at M27 via the same merge pattern.

    Arc::new(map)
}

/// Shared input for the three browser-derived memos
/// (`browser_auto_expanded`, `browser_entries`, `file_list_action`).
/// All three read the same triple — drv 0.4 nested-inputs shape
/// lets them share the bundle instead of each taking three
/// positional args.
#[derive(Copy, Clone, drv::Input)]
pub struct BrowserDerivedInputs<'a> {
    pub fs: FsTreeInput<'a>,
    pub ui: BrowserUiInput<'a>,
    pub tabs: TabsActiveInput<'a>,
    pub edits: EditedBuffersInput<'a>,
}

/// Auto-expanded ancestor chain for the active tab, excluding
/// user-pinned dirs. Pure derivation — no state written anywhere.
/// Memoized so downstream consumers (entries walk, list-action
/// emitter, painter) share the computation.
///
/// Persistent ancestor reveal is handled separately: the runtime
/// writes ancestors of any newly-activated tab into
/// `browser.expanded_dirs` once, mirroring legacy's
/// `reveal_active_buffer` (`led/src/model/action/helpers.rs:36`).
/// Once persisted there, the user can collapse them at will and
/// the collapse sticks.
#[drv::memo(single)]
pub fn browser_auto_expanded<'a>(
    inputs: BrowserDerivedInputs<'a>,
) -> Arc<imbl::HashSet<CanonPath>> {
    let BrowserDerivedInputs { fs, ui, tabs, edits: _ } = inputs;
    let active_path = (*tabs.active)
        .and_then(|id| tabs.open.iter().find(|t| t.id == id))
        .map(|t| t.path.clone());
    Arc::new(led_driver_fs_list_core::ancestors_of(
        &led_driver_fs_list_core::FsTree {
            root: fs.root.clone(),
            dir_contents: fs.dir_contents.clone(),
            failed_dirs: fs.failed_dirs.clone(),
        },
        ui.expanded_dirs,
        active_path.as_ref(),
    ))
}

/// Flattened browser tree — the single visible-row list every
/// consumer walks. Pure derivation of
/// `(fs, expanded_dirs ∪ auto_expanded_dirs)`. `Arc`-wrapped so
/// the memo cache holds the same allocation across cache hits.
#[drv::memo(single)]
pub fn browser_entries<'a>(
    inputs: BrowserDerivedInputs<'a>,
) -> Arc<Vec<TreeEntry>> {
    let BrowserDerivedInputs { fs, ui, tabs: _, edits: _ } = inputs;
    // Ancestor reveal lives in `expanded_dirs` itself — the runtime
    // persists ancestors of any newly-activated tab on the
    // file_load completion path (legacy `reveal_active_buffer`).
    // No transient overlay; collapse_dir / collapse_all stick.
    let fs_copy = led_driver_fs_list_core::FsTree {
        root: fs.root.clone(),
        dir_contents: fs.dir_contents.clone(),
        failed_dirs: fs.failed_dirs.clone(),
    };
    let entries = led_driver_fs_list_core::walk_tree(&fs_copy, ui.expanded_dirs);
    Arc::new(entries)
}

/// Resolve `selected_path` to a row index in the current
/// entries. Used by dispatch (arrow nav, expand/collapse) and
/// the painter (which row to highlight). Returns 0 when the
/// selected path is absent, falls outside the current tree, or
/// the entries list is empty — matching the historical
/// `selected: usize = 0` default.
pub fn browser_selected_idx(
    entries: &[TreeEntry],
    selected_path: Option<&CanonPath>,
) -> usize {
    let Some(target) = selected_path else {
        return 0;
    };
    entries
        .iter()
        .position(|e| &e.path == target)
        .unwrap_or(0)
}

/// "What directory listings do we still need?"
///
/// Emits one `ListCmd::List` per path that's expected to have a
/// listing (workspace root, every user-expanded dir, every
/// auto-revealed ancestor of the active tab) but isn't in
/// `dir_contents` yet AND isn't currently in-flight. Used to
/// drive `FsListDriver::execute`.
#[drv::memo(single)]
pub fn file_list_action<'a, 'b>(
    inputs: BrowserDerivedInputs<'a>,
    driver: FsListDriverInput<'b>,
) -> Vec<ListCmd> {
    let BrowserDerivedInputs { fs, ui, tabs: _, edits: _ } = inputs;
    let mut out: Vec<ListCmd> = Vec::new();
    // Three gates:
    // - `dir_contents` covers "already listed successfully".
    // - `failed_dirs` covers "tried, failed — don't loop". Without
    //   it a stale `expanded_dirs` entry pointing at a deleted
    //   directory would re-fire `ListCmd::List` every tick and the
    //   wake notifier would peg the main loop at 100 % CPU.
    // - `driver.in_flight` covers "asked, waiting for the worker
    //   to answer" — without it the memo would queue duplicate
    //   List(p)s between an `execute` and the matching `Done`.
    let wanted = |p: &CanonPath| -> bool {
        !fs.dir_contents.contains_key(p)
            && !fs.failed_dirs.contains(p)
            && !driver.in_flight.contains(p)
    };
    if let Some(root) = fs.root.as_ref()
        && wanted(root)
    {
        out.push(ListCmd::List(root.clone()));
    }
    for dir in ui.expanded_dirs.iter() {
        if wanted(dir) {
            out.push(ListCmd::List(dir.clone()));
        }
    }
    // Auto-reveal listings come for free here: the runtime
    // persists ancestor expansions into `expanded_dirs` on the
    // file_load completion path (mirrors legacy
    // `reveal_active_buffer`), so the loop above already covers
    // them. We don't need a separate auto-reveal pass.
    out
}

/// "What should happen to the file-browser's preview tab right
/// now, given the current selection?"
///
/// Pure derivation — the syscall-bearing parts (path-chain
/// resolution, `open_or_focus_tab` itself) stay in dispatch.
/// The memo only decides which intent applies; dispatch reads
/// the intent and applies it on the next selection-move.
///
/// - **File row** → `Open(path)`: open or replace the single
///   preview slot with this file.
/// - **Directory row** → `Close`: close any active preview.
/// - **No selection** → `Keep`: leave whatever's open alone
///   (mirrors `preview_current_selection`'s `return` when
///   `entries.get(idx)` is `None`).
#[drv::memo(single)]
pub fn desired_preview_intent<'a>(
    inputs: BrowserDerivedInputs<'a>,
) -> PreviewIntent {
    let entries = browser_entries(inputs);
    let idx = browser_selected_idx(&entries, inputs.ui.selected_path.as_ref());
    let Some(entry) = entries.get(idx) else {
        return PreviewIntent::Keep;
    };
    match entry.kind {
        TreeEntryKind::File => PreviewIntent::Open(entry.path.clone()),
        TreeEntryKind::Directory { .. } => PreviewIntent::Close,
    }
}

/// Outcome of [`desired_preview_intent`]. Dispatch reads this
/// and applies it via `open_or_focus_tab` (Open) or
/// `close_preview` (Close); `Keep` is a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewIntent {
    /// Open or replace the preview tab pointing at this path.
    Open(CanonPath),
    /// Close the active preview tab (if any).
    Close,
    /// No change — selection is empty / out of bounds.
    Keep,
}
