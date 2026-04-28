//! Browser tree and file-category derived memos.

use led_core::CanonPath;
use led_driver_fs_list_core::ListCmd;
use led_state_browser::TreeEntry;
use std::sync::Arc;

use super::inputs::*;

/// Per-file category set for the whole workspace. Feeds the
/// browser painter + the Alt-./ nav cycle.
///
/// LSP Error / Warning, plus git file-level categories (Unstaged,
/// StagedModified, StagedNew, Untracked). Info / Hint are filtered
/// out — they never colour the browser.
#[drv::memo(single)]
pub fn file_categories_map<'d>(
    diagnostics: DiagnosticsStatesInput<'d>,
    git: GitStateInput<'d>,
) -> Arc<imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>>> {
    let mut map: imbl::HashMap<CanonPath, imbl::HashSet<led_core::IssueCategory>> =
        imbl::HashMap::default();

    // LSP diagnostics — Error/Warning only, Info/Hint silent.
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

    // Git file-level statuses. `IssueCategory::resolve_display`
    // picks the winning letter / colour when a path carries both
    // a diagnostic and a git category (LSP precedes git by
    // `IssueCategory::precedence`).
    for (path, cats) in git.file_statuses.iter() {
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
    Arc::new(led_state_browser::ancestors_of(
        &led_state_browser::FsTree {
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
    let fs_copy = led_state_browser::FsTree {
        root: fs.root.clone(),
        dir_contents: fs.dir_contents.clone(),
        failed_dirs: fs.failed_dirs.clone(),
    };
    let entries = led_state_browser::walk_tree(&fs_copy, ui.expanded_dirs);
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
/// `dir_contents` yet. Used to drive `FsListDriver::execute`.
#[drv::memo(single)]
pub fn file_list_action<'a>(
    inputs: BrowserDerivedInputs<'a>,
) -> Vec<ListCmd> {
    let BrowserDerivedInputs { fs, ui, tabs: _, edits: _ } = inputs;
    let mut out: Vec<ListCmd> = Vec::new();
    // `failed_dirs` is the "we tried, it didn't work, don't ask
    // again until something changes" set. Without it, a stale
    // `expanded_dirs` entry pointing at a deleted directory would
    // re-fire `ListCmd::List` every tick — the runtime drops the
    // `Err` result silently, so the path never enters `dir_contents`,
    // so the next tick re-emits, so the worker re-fails, so the
    // wake notifier fires, and the main loop sits at 100 % CPU.
    if let Some(root) = fs.root.as_ref()
        && !fs.dir_contents.contains_key(root)
        && !fs.failed_dirs.contains(root)
    {
        out.push(ListCmd::List(root.clone()));
    }
    for dir in ui.expanded_dirs.iter() {
        if !fs.dir_contents.contains_key(dir) && !fs.failed_dirs.contains(dir) {
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
