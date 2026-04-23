//! File-browser primitives (M11).
//!
//! Exposed functions are called by `run_command` when a browser-level
//! command resolves. They mutate only the user-owned fields on
//! [`BrowserUi`] (`expanded_dirs`, `selected_path`, `scroll_offset`,
//! `visible`, `focus`) and [`Tabs`] for open/preview side-effects.
//!
//! The flattened tree, the auto-reveal ancestor set, and the
//! selection index all come from the query layer as memos — no
//! imperative rebuild calls on the state atom.
//!
//! All functions are silent no-ops when preconditions fail (no root,
//! no selection, closed tab, etc.) — matches legacy browser
//! behaviour.
//!
//! [`BrowserUi`]: led_state_browser::BrowserUi

use std::cmp::Ordering;
use std::collections::HashMap;

use led_core::{CanonPath, PathChain};
use led_state_browser::{BrowserUi, Focus, FsTree, TreeEntry, TreeEntryKind};
use led_state_tabs::Tabs;

use crate::query::{BrowserUiInput, FsTreeInput, TabsActiveInput, browser_entries, browser_selected_idx};

use super::shared::{close_preview, open_or_focus_tab};

/// Helper: call the `browser_entries` memo given raw state.
fn entries_of(browser: &BrowserUi, fs: &FsTree, tabs: &Tabs) -> std::sync::Arc<Vec<TreeEntry>> {
    browser_entries(
        FsTreeInput::new(fs),
        BrowserUiInput::new(browser),
        TabsActiveInput::new(tabs),
    )
}

/// Current selected entry (clone), or None on empty tree.
fn current_entry(browser: &BrowserUi, fs: &FsTree, tabs: &Tabs) -> Option<TreeEntry> {
    let entries = entries_of(browser, fs, tabs);
    let idx = browser_selected_idx(&entries, browser.selected_path.as_ref());
    entries.get(idx).cloned()
}

/// Find the TreeEntry that contains `idx` in the flat tree view —
/// walks backward for the first entry with depth `selected.depth -
/// 1`. Returns `None` for depth-0 rows (their parent is the
/// workspace root, handled by the caller).
fn parent_tree_entry<'a>(
    entries: &'a [TreeEntry],
    idx: usize,
) -> Option<&'a TreeEntry> {
    let selected = entries.get(idx)?;
    if selected.depth == 0 {
        return None;
    }
    entries[..idx]
        .iter()
        .rev()
        .find(|e| e.depth + 1 == selected.depth)
}

/// Build the user-typed [`PathChain`] for the selected browser
/// entry and stash it on `path_chains`. For a symlinked `.profile`
/// sitting under `~/`, this reconstructs `<canonical-~>/.profile` —
/// the path the user WOULD have typed — so the chain walker then
/// follows the symlink to the real target, but language detection
/// wins off the user-facing basename.
fn stash_browser_chain(
    entries: &[TreeEntry],
    fs: &FsTree,
    path_chains: &mut HashMap<CanonPath, PathChain>,
    idx: usize,
) {
    let Some(entry) = entries.get(idx) else {
        return;
    };
    let parent_dir = match parent_tree_entry(entries, idx) {
        Some(p) => p.path.as_path().to_path_buf(),
        None => {
            // depth-0 → parent is workspace root.
            let Some(root) = fs.root.as_ref() else { return };
            root.as_path().to_path_buf()
        }
    };
    let user_pathbuf = parent_dir.join(&entry.name);
    let chain = led_core::UserPath::new(user_pathbuf).resolve_chain();
    path_chains.insert(entry.path.clone(), chain);
}

/// Toggle `browser.visible`. When toggling off while focus is Side,
/// auto-swap focus back to Main so the next keystroke lands in the
/// editor.
pub(super) fn toggle_side_panel(browser: &mut BrowserUi) {
    browser.visible = !browser.visible;
    if !browser.visible && browser.focus == Focus::Side {
        browser.focus = Focus::Main;
    }
}

/// Flip focus between Main and Side. If the panel isn't visible and
/// the user asked to focus it, show it.
pub(super) fn toggle_focus(browser: &mut BrowserUi) {
    browser.focus = match browser.focus {
        Focus::Main => {
            browser.visible = true;
            Focus::Side
        }
        Focus::Side => Focus::Main,
    };
}

/// Expand the selected directory. No-op when the selection is a
/// file or there's no selection. The query-layer memo picks up
/// the new `expanded_dirs` entry on the next tick.
pub(super) fn expand_dir(browser: &mut BrowserUi, fs: &FsTree, tabs: &Tabs) {
    let Some(entry) = current_entry(browser, fs, tabs) else {
        return;
    };
    if matches!(entry.kind, TreeEntryKind::Directory { expanded: false }) {
        browser.expanded_dirs.insert(entry.path);
    }
}

/// Collapse the selected directory. When the selection is a file,
/// collapse the file's parent instead (so `Left` in a deep tree
/// "zooms out" one level). Selection moves to the collapsed dir's
/// row.
pub(super) fn collapse_dir(browser: &mut BrowserUi, fs: &FsTree, tabs: &Tabs) {
    let entries = entries_of(browser, fs, tabs);
    let idx = browser_selected_idx(&entries, browser.selected_path.as_ref());
    let Some(entry) = entries.get(idx).cloned() else {
        return;
    };
    let target_path = match entry.kind {
        TreeEntryKind::Directory { expanded: true } => entry.path.clone(),
        TreeEntryKind::Directory { expanded: false } => {
            return;
        }
        TreeEntryKind::File => {
            match find_expanded_ancestor(&entries, &entry.path) {
                Some(p) => p,
                None => return,
            }
        }
    };
    browser.expanded_dirs.remove(&target_path);
    // Selection jumps to the collapsed dir's row. Path-based so
    // the next memo rebuild finds it at the right index.
    browser.selected_path = Some(target_path);
}

fn find_expanded_ancestor(entries: &[TreeEntry], child: &CanonPath) -> Option<CanonPath> {
    let child_idx = entries.iter().position(|e| &e.path == child)?;
    let child_depth = entries[child_idx].depth;
    for i in (0..child_idx).rev() {
        if entries[i].depth < child_depth
            && matches!(
                entries[i].kind,
                TreeEntryKind::Directory { expanded: true }
            )
        {
            return Some(entries[i].path.clone());
        }
    }
    None
}

/// Collapse every expanded directory + reset selection/scroll.
pub(super) fn collapse_all(browser: &mut BrowserUi) {
    browser.expanded_dirs = imbl::HashSet::default();
    browser.selected_path = None;
    browser.scroll_offset = 0;
}

/// Open the selected entry.
///
/// - **Directory**: toggle expand/collapse (Enter as "drill in" / out).
/// - **File**: promote an existing preview at that path, replace the
///   preview with this file, or create a fresh preview. Focus → Main.
pub(super) fn open_selected(
    browser: &mut BrowserUi,
    fs: &FsTree,
    tabs: &mut Tabs,
    path_chains: &mut HashMap<CanonPath, PathChain>,
) {
    let entries = entries_of(browser, fs, tabs);
    let idx = browser_selected_idx(&entries, browser.selected_path.as_ref());
    let Some(entry) = entries.get(idx).cloned() else {
        return;
    };
    match entry.kind {
        TreeEntryKind::Directory { expanded } => {
            if expanded {
                browser.expanded_dirs.remove(&entry.path);
            } else {
                browser.expanded_dirs.insert(entry.path);
            }
        }
        TreeEntryKind::File => {
            stash_browser_chain(&entries, fs, path_chains, idx);
            open_or_focus_tab(tabs, &entry.path, /* promote= */ true);
            browser.selected_path = Some(entry.path);
            browser.focus = Focus::Main;
        }
    }
}

/// `Alt-Enter` — open without stealing focus from the browser.
/// Legacy declared this as "open in background"; for M11 we treat
/// it as an open that leaves focus on Side.
pub(super) fn open_selected_bg(
    browser: &mut BrowserUi,
    fs: &FsTree,
    tabs: &mut Tabs,
    path_chains: &mut HashMap<CanonPath, PathChain>,
) {
    let entries = entries_of(browser, fs, tabs);
    let idx = browser_selected_idx(&entries, browser.selected_path.as_ref());
    let Some(entry) = entries.get(idx).cloned() else {
        return;
    };
    if let TreeEntryKind::File = entry.kind {
        stash_browser_chain(&entries, fs, path_chains, idx);
        open_or_focus_tab(tabs, &entry.path, /* promote= */ false);
    }
}

/// React to a browser-selection move.
///
/// - File row → open (or replace) the preview tab for that path.
///   Arrow-nav through files reuses the single preview slot.
/// - Directory row → close the open preview (if any).
fn preview_current_selection(
    browser: &BrowserUi,
    entries: &[TreeEntry],
    fs: &FsTree,
    tabs: &mut Tabs,
    path_chains: &mut HashMap<CanonPath, PathChain>,
) {
    let idx = browser_selected_idx(entries, browser.selected_path.as_ref());
    let Some(entry) = entries.get(idx) else {
        return;
    };
    match entry.kind {
        TreeEntryKind::File => {
            let path = entry.path.clone();
            stash_browser_chain(entries, fs, path_chains, idx);
            open_or_focus_tab(tabs, &path, /* promote= */ false);
        }
        TreeEntryKind::Directory { .. } => {
            close_preview(tabs);
        }
    }
}

/// Browser-context selection move (Up/Down in focus=Side). Delta +1
/// = one row down. Path-based: clamps the new index into the
/// current entries, writes back the path of that row. Also opens a
/// preview tab for the file now under the cursor (closes preview on
/// directory rows).
pub(super) fn move_selection(
    browser: &mut BrowserUi,
    fs: &FsTree,
    tabs: &mut Tabs,
    path_chains: &mut HashMap<CanonPath, PathChain>,
    delta: isize,
) {
    let entries = entries_of(browser, fs, tabs);
    if entries.is_empty() {
        browser.selected_path = None;
        return;
    }
    let cur = browser_selected_idx(&entries, browser.selected_path.as_ref());
    let n = entries.len() as isize;
    let next = (cur as isize + delta).clamp(0, n - 1) as usize;
    browser.selected_path = Some(entries[next].path.clone());
    preview_current_selection(browser, &entries, fs, tabs, path_chains);
}

/// Browser-context page-move. `page_rows` is the visible window.
pub(super) fn page_selection(
    browser: &mut BrowserUi,
    fs: &FsTree,
    tabs: &mut Tabs,
    path_chains: &mut HashMap<CanonPath, PathChain>,
    page_rows: usize,
    down: bool,
) {
    let delta = if down {
        page_rows as isize
    } else {
        -(page_rows as isize)
    };
    move_selection(browser, fs, tabs, path_chains, delta);
}

pub(super) fn select_first(
    browser: &mut BrowserUi,
    fs: &FsTree,
    tabs: &mut Tabs,
    path_chains: &mut HashMap<CanonPath, PathChain>,
) {
    let entries = entries_of(browser, fs, tabs);
    browser.selected_path = entries.first().map(|e| e.path.clone());
    preview_current_selection(browser, &entries, fs, tabs, path_chains);
}

pub(super) fn select_last(
    browser: &mut BrowserUi,
    fs: &FsTree,
    tabs: &mut Tabs,
    path_chains: &mut HashMap<CanonPath, PathChain>,
) {
    let entries = entries_of(browser, fs, tabs);
    browser.selected_path = entries.last().map(|e| e.path.clone());
    preview_current_selection(browser, &entries, fs, tabs, path_chains);
}

/// Sort helper used by tests (tree-order comparator).
pub(super) fn _dummy_ordering() -> Ordering {
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use imbl::Vector;
    use led_core::{CanonPath, UserPath};
    use led_state_browser::{BrowserUi, DirEntry, DirEntryKind, Focus, FsTree};
    use led_state_tabs::{Tab, TabId, Tabs};

    use super::*;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn dir_entry(name: &str, path: &str, kind: DirEntryKind) -> DirEntry {
        DirEntry {
            name: name.into(),
            path: canon(path),
            kind,
        }
    }

    fn seeded() -> (BrowserUi, FsTree) {
        let mut fs = FsTree {
            root: Some(canon("/project")),
            ..Default::default()
        };
        let mut children = Vector::new();
        children.push_back(dir_entry("sub", "/project/sub", DirEntryKind::Directory));
        children.push_back(dir_entry("alpha.txt", "/project/alpha.txt", DirEntryKind::File));
        children.push_back(dir_entry("beta.txt", "/project/beta.txt", DirEntryKind::File));
        fs.dir_contents.insert(canon("/project"), children);

        let mut sub_children = Vector::new();
        sub_children.push_back(dir_entry(
            "inner.txt",
            "/project/sub/inner.txt",
            DirEntryKind::File,
        ));
        fs.dir_contents.insert(canon("/project/sub"), sub_children);
        (BrowserUi::default(), fs)
    }

    #[test]
    fn toggle_side_panel_flips_visible() {
        let mut b = BrowserUi::default();
        assert!(b.visible);
        toggle_side_panel(&mut b);
        assert!(!b.visible);
    }

    #[test]
    fn toggle_side_panel_off_auto_swaps_focus_to_main() {
        let mut b = BrowserUi {
            focus: Focus::Side,
            ..Default::default()
        };
        toggle_side_panel(&mut b);
        assert_eq!(b.focus, Focus::Main);
    }

    #[test]
    fn toggle_focus_shows_panel_and_flips_focus() {
        let mut b = BrowserUi {
            visible: false,
            ..Default::default()
        };
        toggle_focus(&mut b);
        assert!(b.visible);
        assert_eq!(b.focus, Focus::Side);
        toggle_focus(&mut b);
        assert_eq!(b.focus, Focus::Main);
    }

    #[test]
    fn expand_dir_on_selected_dir_adds_to_expanded() {
        let (mut b, fs) = seeded();
        let tabs = Tabs::default();
        b.selected_path = Some(canon("/project/sub"));
        expand_dir(&mut b, &fs, &tabs);
        assert!(b.expanded_dirs.contains(&canon("/project/sub")));
    }

    #[test]
    fn expand_dir_on_file_is_noop() {
        let (mut b, fs) = seeded();
        let tabs = Tabs::default();
        b.selected_path = Some(canon("/project/alpha.txt"));
        expand_dir(&mut b, &fs, &tabs);
        assert!(b.expanded_dirs.is_empty());
    }

    #[test]
    fn collapse_dir_on_expanded_collapses() {
        let (mut b, fs) = seeded();
        let tabs = Tabs::default();
        b.expanded_dirs.insert(canon("/project/sub"));
        b.selected_path = Some(canon("/project/sub"));
        collapse_dir(&mut b, &fs, &tabs);
        assert!(!b.expanded_dirs.contains(&canon("/project/sub")));
    }

    #[test]
    fn collapse_dir_on_leaf_collapses_parent() {
        let (mut b, fs) = seeded();
        let tabs = Tabs::default();
        b.expanded_dirs.insert(canon("/project/sub"));
        b.selected_path = Some(canon("/project/sub/inner.txt"));
        collapse_dir(&mut b, &fs, &tabs);
        assert!(!b.expanded_dirs.contains(&canon("/project/sub")));
        assert_eq!(b.selected_path, Some(canon("/project/sub")));
    }

    #[test]
    fn collapse_all_clears_expanded_and_resets_selection() {
        let (mut b, _fs) = seeded();
        b.expanded_dirs.insert(canon("/project/sub"));
        b.selected_path = Some(canon("/project/beta.txt"));
        b.scroll_offset = 1;
        collapse_all(&mut b);
        assert!(b.expanded_dirs.is_empty());
        assert_eq!(b.selected_path, None);
        assert_eq!(b.scroll_offset, 0);
    }

    #[test]
    fn open_selected_on_dir_toggles_expand() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        b.selected_path = Some(canon("/project/sub"));
        open_selected(&mut b, &fs, &mut tabs, &mut pc);
        assert!(b.expanded_dirs.contains(&canon("/project/sub")));
        open_selected(&mut b, &fs, &mut tabs, &mut pc);
        assert!(!b.expanded_dirs.contains(&canon("/project/sub")));
    }

    #[test]
    fn open_selected_on_file_creates_real_tab_and_focuses_main() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        b.selected_path = Some(canon("/project/alpha.txt"));
        b.focus = Focus::Side;
        open_selected(&mut b, &fs, &mut tabs, &mut pc);
        assert_eq!(tabs.open.len(), 1);
        assert!(!tabs.open[0].preview);
        assert_eq!(tabs.active, Some(tabs.open[0].id));
        assert_eq!(b.focus, Focus::Main);
    }

    #[test]
    fn open_selected_bg_creates_preview_and_keeps_side_focus() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        b.selected_path = Some(canon("/project/alpha.txt"));
        b.focus = Focus::Side;
        open_selected_bg(&mut b, &fs, &mut tabs, &mut pc);
        assert_eq!(tabs.open.len(), 1);
        assert!(tabs.open[0].preview);
        assert_eq!(b.focus, Focus::Side);
    }

    #[test]
    fn open_selected_on_preview_promotes_it() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("/project/alpha.txt"),
            preview: true,
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        b.selected_path = Some(canon("/project/alpha.txt"));
        open_selected(&mut b, &fs, &mut tabs, &mut pc);
        assert_eq!(tabs.open.len(), 1);
        assert!(!tabs.open[0].preview);
    }

    #[test]
    fn open_selected_on_different_file_replaces_preview_path() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("/project/alpha.txt"),
            preview: true,
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        b.selected_path = Some(canon("/project/beta.txt"));
        open_selected_bg(&mut b, &fs, &mut tabs, &mut pc);
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].path, canon("/project/beta.txt"));
        assert!(tabs.open[0].preview);
    }

    #[test]
    fn move_selection_moves_within_entries() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        // Starts with selected_path = None → idx 0 = sub/.
        move_selection(&mut b, &fs, &mut tabs, &mut pc, 2);
        assert_eq!(b.selected_path, Some(canon("/project/beta.txt")));
        move_selection(&mut b, &fs, &mut tabs, &mut pc, -1);
        assert_eq!(b.selected_path, Some(canon("/project/alpha.txt")));
    }

    #[test]
    fn move_selection_opens_preview_for_file_row() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        // Start at idx 0 (sub/, directory). Move to alpha.txt (file).
        move_selection(&mut b, &fs, &mut tabs, &mut pc, 1);
        assert_eq!(tabs.open.len(), 1);
        assert!(tabs.open[0].preview);
        assert_eq!(tabs.open[0].path, canon("/project/alpha.txt"));
    }

    #[test]
    fn move_selection_onto_directory_row_does_not_open_anything() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        b.selected_path = Some(canon("/project/alpha.txt"));
        move_selection(&mut b, &fs, &mut tabs, &mut pc, -1);
        assert_eq!(b.selected_path, Some(canon("/project/sub")));
        assert!(tabs.open.is_empty());
    }

    #[test]
    fn nav_onto_directory_closes_existing_preview() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        // sub/ → alpha.txt (preview).
        move_selection(&mut b, &fs, &mut tabs, &mut pc, 1);
        assert_eq!(tabs.open.len(), 1);
        assert!(tabs.open[0].preview);
        // Back to sub/ → preview closes.
        move_selection(&mut b, &fs, &mut tabs, &mut pc, -1);
        assert!(tabs.open.is_empty());
        assert!(tabs.active.is_none());
    }

    #[test]
    fn close_preview_restores_previous_tab() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        tabs.open.push_back(Tab {
            id: TabId(7),
            path: canon("/project/committed.rs"),
            ..Default::default()
        });
        tabs.active = Some(TabId(7));

        move_selection(&mut b, &fs, &mut tabs, &mut pc, 1); // alpha.txt (preview)
        assert_eq!(tabs.open.len(), 2, "real + preview");
        move_selection(&mut b, &fs, &mut tabs, &mut pc, -1); // back to sub/
        assert_eq!(tabs.open.len(), 1, "preview removed");
        assert_eq!(tabs.active, Some(TabId(7)), "real tab restored");
    }

    #[test]
    fn successive_file_nav_replaces_the_same_preview_slot() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        let mut pc = HashMap::new();
        move_selection(&mut b, &fs, &mut tabs, &mut pc, 1); // alpha.txt
        move_selection(&mut b, &fs, &mut tabs, &mut pc, 1); // beta.txt
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].path, canon("/project/beta.txt"));
        assert!(tabs.open[0].preview);
    }

    #[test]
    fn _dummy_ordering_is_equal() {
        assert_eq!(_dummy_ordering(), Ordering::Equal);
    }
}
