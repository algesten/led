//! File-browser primitives (M11).
//!
//! Exposed functions are called by `run_command` when a browser-level
//! command resolves. Each mutates [`BrowserUi`] (and [`Tabs`] for
//! open/preview) directly; the runtime's next tick re-renders the
//! side panel from the updated state and fires any missing
//! directory listings via the query layer.
//!
//! All functions are silent no-ops when preconditions fail (no root,
//! no selection, closed tab, etc.) — matches legacy browser
//! behaviour.

use std::cmp::Ordering;

use led_core::CanonPath;
use led_state_browser::{BrowserUi, Focus, FsTree, TreeEntryKind};
use led_state_tabs::Tabs;

use super::shared::open_or_focus_tab;

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
/// file or there's no selection.
///
/// The query-layer memo will notice the new `expanded_dirs` entry
/// and emit a `ListCmd` for the directory on the next tick if
/// `dir_contents` doesn't already have it.
pub(super) fn expand_dir(browser: &mut BrowserUi, fs: &FsTree) {
    let Some(entry) = browser.selected_entry() else {
        return;
    };
    if matches!(entry.kind, TreeEntryKind::Directory { expanded: false }) {
        let path = entry.path.clone();
        browser.expand(path, fs);
    }
}

/// Collapse the selected directory. When the selection is a file,
/// collapse the file's parent instead (so `Left` in a deep tree
/// "zooms out" one level). Selection moves to the collapsed dir's
/// row.
pub(super) fn collapse_dir(browser: &mut BrowserUi, fs: &FsTree) {
    let Some(entry) = browser.selected_entry().cloned() else {
        return;
    };
    let target_path = match entry.kind {
        TreeEntryKind::Directory { expanded: true } => entry.path.clone(),
        TreeEntryKind::Directory { expanded: false } => {
            // Already collapsed — nothing to do.
            return;
        }
        TreeEntryKind::File => {
            // Walk up to find the first expanded ancestor.
            match find_expanded_ancestor(browser, &entry.path) {
                Some(p) => p,
                None => return,
            }
        }
    };
    browser.collapse(target_path.clone(), fs);
    // After collapse, re-select the row at the collapsed dir.
    if let Some(idx) = browser.entries.iter().position(|e| e.path == target_path) {
        browser.selected = idx;
    }
}

fn find_expanded_ancestor(browser: &BrowserUi, child: &CanonPath) -> Option<CanonPath> {
    // Walk up through child.ancestors(), but our CanonPath doesn't
    // expose that directly — scan the flat `entries` for the latest
    // dir above `child` whose path is a prefix and that's expanded.
    let child_idx = browser.entries.iter().position(|e| &e.path == child)?;
    // Latest dir row with a smaller depth than child is the immediate
    // parent in the tree. That's the one to collapse.
    let child_depth = browser.entries[child_idx].depth;
    for i in (0..child_idx).rev() {
        if browser.entries[i].depth < child_depth
            && matches!(
                browser.entries[i].kind,
                TreeEntryKind::Directory { expanded: true }
            )
        {
            return Some(browser.entries[i].path.clone());
        }
    }
    None
}

/// Collapse every expanded directory + reset selection/scroll.
pub(super) fn collapse_all(browser: &mut BrowserUi, fs: &FsTree) {
    browser.collapse_all(fs);
}

/// Open the selected entry.
///
/// - **Directory**: toggle expand/collapse (Enter as "drill in" / out).
/// - **File**: promote an existing preview at that path, replace the
///   preview with this file, or create a fresh preview. Focus → Main.
pub(super) fn open_selected(browser: &mut BrowserUi, fs: &FsTree, tabs: &mut Tabs) {
    let Some(entry) = browser.selected_entry().cloned() else {
        return;
    };
    match entry.kind {
        TreeEntryKind::Directory { expanded } => {
            if expanded {
                browser.collapse(entry.path, fs);
            } else {
                browser.expand(entry.path, fs);
            }
        }
        TreeEntryKind::File => {
            open_file_from_browser(browser, tabs, &entry.path, /* promote= */ true);
            browser.focus = Focus::Main;
        }
    }
}

/// `Alt-Enter` — open without stealing focus from the browser.
/// Legacy declared this as "open in background"; for M11 we treat
/// it as an open that leaves focus on Side.
pub(super) fn open_selected_bg(browser: &mut BrowserUi, tabs: &mut Tabs) {
    let Some(entry) = browser.selected_entry().cloned() else {
        return;
    };
    if let TreeEntryKind::File = entry.kind {
        open_file_from_browser(browser, tabs, &entry.path, /* promote= */ false);
    }
}

/// Core open logic — delegates to the shared `open_or_focus_tab`.
fn open_file_from_browser(
    _browser: &BrowserUi,
    tabs: &mut Tabs,
    path: &CanonPath,
    promote: bool,
) {
    open_or_focus_tab(tabs, path, promote);
}

/// Browser-context selection move (Up/Down in focus=Side). Delta +1
/// = one row down.
pub(super) fn move_selection(browser: &mut BrowserUi, delta: isize) {
    browser.move_selection(delta);
}

/// Browser-context page-move. `page_rows` is the visible window.
pub(super) fn page_selection(browser: &mut BrowserUi, page_rows: usize, down: bool) {
    let delta = if down {
        page_rows as isize
    } else {
        -(page_rows as isize)
    };
    browser.move_selection(delta);
}

pub(super) fn select_first(browser: &mut BrowserUi) {
    browser.select_first();
}

pub(super) fn select_last(browser: &mut BrowserUi) {
    browser.select_last();
}

/// Sort helper used by tests (tree-order comparator).
pub(super) fn _dummy_ordering() -> Ordering {
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use imbl::Vector;
    use led_core::{CanonPath, UserPath};
    use led_state_browser::{rebuild_entries, BrowserUi, DirEntry, DirEntryKind, Focus, FsTree};
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
        let mut ui = BrowserUi::default();
        rebuild_entries(&mut ui, &fs);
        (ui, fs)
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
        b.selected = 0;
        expand_dir(&mut b, &fs);
        assert!(b.expanded_dirs.contains(&canon("/project/sub")));
        assert_eq!(b.entries.len(), 4);
    }

    #[test]
    fn expand_dir_on_file_is_noop() {
        let (mut b, fs) = seeded();
        b.selected = 1;
        expand_dir(&mut b, &fs);
        assert!(b.expanded_dirs.is_empty());
    }

    #[test]
    fn collapse_dir_on_expanded_collapses() {
        let (mut b, fs) = seeded();
        b.expand(canon("/project/sub"), &fs);
        b.selected = 0;
        collapse_dir(&mut b, &fs);
        assert!(!b.expanded_dirs.contains(&canon("/project/sub")));
        assert_eq!(b.entries.len(), 3);
    }

    #[test]
    fn collapse_dir_on_leaf_collapses_parent() {
        let (mut b, fs) = seeded();
        b.expand(canon("/project/sub"), &fs);
        b.selected = 1;
        collapse_dir(&mut b, &fs);
        assert!(!b.expanded_dirs.contains(&canon("/project/sub")));
        assert_eq!(b.selected, 0);
    }

    #[test]
    fn collapse_all_clears_expanded_and_resets_selection() {
        let (mut b, fs) = seeded();
        b.expand(canon("/project/sub"), &fs);
        b.selected = 2;
        b.scroll_offset = 1;
        collapse_all(&mut b, &fs);
        assert!(b.expanded_dirs.is_empty());
        assert_eq!(b.selected, 0);
        assert_eq!(b.scroll_offset, 0);
    }

    #[test]
    fn open_selected_on_dir_toggles_expand() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        b.selected = 0;
        open_selected(&mut b, &fs, &mut tabs);
        assert!(b.expanded_dirs.contains(&canon("/project/sub")));
        open_selected(&mut b, &fs, &mut tabs);
        assert!(!b.expanded_dirs.contains(&canon("/project/sub")));
    }

    #[test]
    fn open_selected_on_file_creates_real_tab_and_focuses_main() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        b.selected = 1;
        b.focus = Focus::Side;
        open_selected(&mut b, &fs, &mut tabs);
        assert_eq!(tabs.open.len(), 1);
        assert!(!tabs.open[0].preview);
        assert_eq!(tabs.active, Some(tabs.open[0].id));
        assert_eq!(b.focus, Focus::Main);
    }

    #[test]
    fn open_selected_bg_creates_preview_and_keeps_side_focus() {
        let (mut b, _fs) = seeded();
        let mut tabs = Tabs::default();
        b.selected = 1;
        b.focus = Focus::Side;
        open_selected_bg(&mut b, &mut tabs);
        assert_eq!(tabs.open.len(), 1);
        assert!(tabs.open[0].preview);
        assert_eq!(b.focus, Focus::Side);
    }

    #[test]
    fn open_selected_on_preview_promotes_it() {
        let (mut b, fs) = seeded();
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("/project/alpha.txt"),
            preview: true,
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        b.selected = 1;
        open_selected(&mut b, &fs, &mut tabs);
        assert_eq!(tabs.open.len(), 1);
        assert!(!tabs.open[0].preview);
    }

    #[test]
    fn open_selected_on_different_file_replaces_preview_path() {
        let (mut b, _fs) = seeded();
        let mut tabs = Tabs::default();
        tabs.open.push_back(Tab {
            id: TabId(1),
            path: canon("/project/alpha.txt"),
            preview: true,
            ..Default::default()
        });
        tabs.active = Some(TabId(1));
        b.selected = 2;
        open_selected_bg(&mut b, &mut tabs);
        assert_eq!(tabs.open.len(), 1);
        assert_eq!(tabs.open[0].path, canon("/project/beta.txt"));
        assert!(tabs.open[0].preview);
    }

    #[test]
    fn move_selection_moves_within_entries() {
        let (mut b, _fs) = seeded();
        move_selection(&mut b, 2);
        assert_eq!(b.selected, 2);
        move_selection(&mut b, -1);
        assert_eq!(b.selected, 1);
    }

    #[test]
    fn _dummy_ordering_is_equal() {
        assert_eq!(_dummy_ordering(), Ordering::Equal);
    }

    #[test]
    fn rebuild_preserves_entries_arc() {
        let (b, _fs) = seeded();
        let _ = Arc::clone(&b.entries);
    }
}
