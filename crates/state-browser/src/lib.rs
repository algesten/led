//! File-browser sidebar state — split into two sources per
//! EXAMPLE-ARCH § "Sources: two kinds of ground truth":
//!
//! - [`FsTree`] — **external fact**, written by the FS-list driver.
//!   Holds the workspace root and the per-directory listings cache.
//! - [`BrowserUi`] — **user decision**, mutated by dispatch. Holds
//!   which directories are expanded, the selection + scroll, the
//!   visible-panel toggle, and focus. The flattened `entries`
//!   vector the painter walks lives here as a cache; it is a pure
//!   derivation of `(FsTree, BrowserUi.expanded_dirs)` and is
//!   rebuilt whenever either side changes.
//!
//! Rebuild semantics match legacy `led/src/model/browser`: walk
//! `root` + `expanded_dirs` + `dir_contents`, sort children
//! dirs-first then files-alphabetically, and filter out leading-`.`
//! names.

use std::sync::Arc;

use imbl::{HashMap, HashSet, Vector};
use led_core::CanonPath;
pub use led_driver_fs_list_core::{DirEntry, DirEntryKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Main,
    Side,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeEntryKind {
    File,
    Directory { expanded: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: CanonPath,
    pub name: String,
    pub depth: usize,
    pub kind: TreeEntryKind,
}

/// **External-fact** source: the file-system view of the workspace.
/// Written exclusively by the FS-list driver's ingest path; never
/// mutated by dispatch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsTree {
    pub root: Option<CanonPath>,
    /// Per-directory listing cache, filled by the FS driver.
    pub dir_contents: HashMap<CanonPath, Vector<DirEntry>>,
}

/// **User-decision** source: the browser's UI state. Dispatch
/// mutates this; the runtime rebuilds `entries` after every
/// mutation (or after `FsTree` changes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserUi {
    pub expanded_dirs: HashSet<CanonPath>,
    /// Flattened tree view, derived from `(FsTree, expanded_dirs)`.
    /// Wrapped in `Arc` so cache-hit clones of [`BrowserUi`] are a
    /// pointer copy.
    pub entries: Arc<Vec<TreeEntry>>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub visible: bool,
    pub focus: Focus,
}

impl Default for BrowserUi {
    fn default() -> Self {
        Self {
            expanded_dirs: HashSet::default(),
            entries: Arc::new(Vec::new()),
            selected: 0,
            scroll_offset: 0,
            visible: true,
            focus: Focus::Main,
        }
    }
}

/// Walk the tree and refresh `ui.entries`. Pure derivation; call
/// whenever `fs.dir_contents` or `ui.expanded_dirs` changes. Also
/// clamps `ui.selected` if the new tree is shorter.
pub fn rebuild_entries(ui: &mut BrowserUi, fs: &FsTree) {
    let mut out: Vec<TreeEntry> = Vec::new();
    if let Some(root) = fs.root.as_ref() {
        emit_children_of(fs, &ui.expanded_dirs, root, 0, &mut out);
    }
    ui.entries = Arc::new(out);
    clamp_selection(ui);
}

fn emit_children_of(
    fs: &FsTree,
    expanded: &HashSet<CanonPath>,
    dir: &CanonPath,
    depth: usize,
    out: &mut Vec<TreeEntry>,
) {
    let Some(children) = fs.dir_contents.get(dir) else {
        return;
    };
    let mut dirs: Vec<&DirEntry> = Vec::new();
    let mut files: Vec<&DirEntry> = Vec::new();
    for entry in children.iter() {
        if entry.name.starts_with('.') {
            continue;
        }
        match entry.kind {
            DirEntryKind::Directory => dirs.push(entry),
            DirEntryKind::File => files.push(entry),
        }
    }
    dirs.sort_by_key(|e| e.name.to_lowercase());
    files.sort_by_key(|e| e.name.to_lowercase());

    for entry in dirs {
        let is_expanded = expanded.contains(&entry.path);
        out.push(TreeEntry {
            path: entry.path.clone(),
            name: entry.name.clone(),
            depth,
            kind: TreeEntryKind::Directory {
                expanded: is_expanded,
            },
        });
        if is_expanded {
            emit_children_of(fs, expanded, &entry.path, depth + 1, out);
        }
    }
    for entry in files {
        out.push(TreeEntry {
            path: entry.path.clone(),
            name: entry.name.clone(),
            depth,
            kind: TreeEntryKind::File,
        });
    }
}

fn clamp_selection(ui: &mut BrowserUi) {
    if ui.entries.is_empty() {
        ui.selected = 0;
        ui.scroll_offset = 0;
        return;
    }
    if ui.selected >= ui.entries.len() {
        ui.selected = ui.entries.len() - 1;
    }
}

impl BrowserUi {
    pub fn expand(&mut self, path: CanonPath, fs: &FsTree) {
        self.expanded_dirs.insert(path);
        rebuild_entries(self, fs);
    }

    pub fn collapse(&mut self, path: CanonPath, fs: &FsTree) {
        self.expanded_dirs.remove(&path);
        rebuild_entries(self, fs);
    }

    pub fn collapse_all(&mut self, fs: &FsTree) {
        self.expanded_dirs = HashSet::default();
        self.selected = 0;
        self.scroll_offset = 0;
        rebuild_entries(self, fs);
    }

    pub fn selected_entry(&self) -> Option<&TreeEntry> {
        self.entries.get(self.selected)
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let n = self.entries.len() as isize;
        let cur = self.selected as isize;
        let next = (cur + delta).clamp(0, n - 1) as usize;
        self.selected = next;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.entries.len() - 1;
        }
    }

    /// Clamp `scroll_offset` so `selected` stays within
    /// `visible_rows` of the viewport.
    pub fn clamp_scroll(&mut self, visible_rows: usize) {
        if visible_rows == 0 || self.entries.is_empty() {
            self.scroll_offset = 0;
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible_rows {
            self.scroll_offset = self.selected + 1 - visible_rows;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

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

    fn seeded() -> (FsTree, BrowserUi) {
        let mut fs = FsTree {
            root: Some(canon("/project")),
            ..Default::default()
        };
        let mut root_children = Vector::new();
        root_children.push_back(dir_entry("sub", "/project/sub", DirEntryKind::Directory));
        root_children.push_back(dir_entry("alpha.txt", "/project/alpha.txt", DirEntryKind::File));
        root_children.push_back(dir_entry("beta.txt", "/project/beta.txt", DirEntryKind::File));
        root_children.push_back(dir_entry(".hidden", "/project/.hidden", DirEntryKind::File));
        fs.dir_contents.insert(canon("/project"), root_children);

        let mut sub_children = Vector::new();
        sub_children.push_back(dir_entry(
            "inner.txt",
            "/project/sub/inner.txt",
            DirEntryKind::File,
        ));
        fs.dir_contents.insert(canon("/project/sub"), sub_children);
        (fs, BrowserUi::default())
    }

    #[test]
    fn default_is_empty_and_visible() {
        let ui = BrowserUi::default();
        assert!(ui.entries.is_empty());
        assert!(ui.visible);
        assert_eq!(ui.focus, Focus::Main);
    }

    #[test]
    fn rebuild_without_root_is_empty() {
        let fs = FsTree::default();
        let mut ui = BrowserUi::default();
        rebuild_entries(&mut ui, &fs);
        assert!(ui.entries.is_empty());
    }

    #[test]
    fn rebuild_sorts_dirs_first_then_files_alphabetically() {
        let (fs, mut ui) = seeded();
        rebuild_entries(&mut ui, &fs);
        assert_eq!(ui.entries.len(), 3); // sub + alpha + beta; .hidden filtered
        assert_eq!(ui.entries[0].name, "sub");
        assert_eq!(ui.entries[1].name, "alpha.txt");
        assert_eq!(ui.entries[2].name, "beta.txt");
    }

    #[test]
    fn rebuild_recurses_into_expanded_dirs() {
        let (fs, mut ui) = seeded();
        ui.expanded_dirs.insert(canon("/project/sub"));
        rebuild_entries(&mut ui, &fs);
        assert_eq!(ui.entries.len(), 4);
        assert_eq!(ui.entries[0].name, "sub");
        assert_eq!(ui.entries[1].name, "inner.txt");
        assert_eq!(ui.entries[1].depth, 1);
        assert_eq!(ui.entries[2].name, "alpha.txt");
    }

    #[test]
    fn rebuild_filters_hidden_entries() {
        let (fs, mut ui) = seeded();
        rebuild_entries(&mut ui, &fs);
        assert!(!ui.entries.iter().any(|e| e.name == ".hidden"));
    }

    #[test]
    fn expand_flips_chevron_and_reveals_children() {
        let (fs, mut ui) = seeded();
        ui.expand(canon("/project/sub"), &fs);
        assert!(matches!(
            ui.entries[0].kind,
            TreeEntryKind::Directory { expanded: true }
        ));
        assert_eq!(ui.entries.len(), 4);
    }

    #[test]
    fn collapse_removes_from_expanded_and_rebuilds() {
        let (fs, mut ui) = seeded();
        ui.expand(canon("/project/sub"), &fs);
        assert_eq!(ui.entries.len(), 4);
        ui.collapse(canon("/project/sub"), &fs);
        assert_eq!(ui.entries.len(), 3);
        assert!(matches!(
            ui.entries[0].kind,
            TreeEntryKind::Directory { expanded: false }
        ));
    }

    #[test]
    fn collapse_all_clears_expanded_and_resets_selection() {
        let (fs, mut ui) = seeded();
        ui.expand(canon("/project/sub"), &fs);
        ui.selected = 2;
        ui.scroll_offset = 1;
        ui.collapse_all(&fs);
        assert!(ui.expanded_dirs.is_empty());
        assert_eq!(ui.selected, 0);
        assert_eq!(ui.scroll_offset, 0);
    }

    #[test]
    fn move_selection_clamps_at_ends() {
        let (fs, mut ui) = seeded();
        rebuild_entries(&mut ui, &fs);
        ui.move_selection(-10);
        assert_eq!(ui.selected, 0);
        ui.move_selection(100);
        assert_eq!(ui.selected, ui.entries.len() - 1);
    }

    #[test]
    fn select_first_and_last() {
        let (fs, mut ui) = seeded();
        rebuild_entries(&mut ui, &fs);
        ui.select_last();
        assert_eq!(ui.selected, ui.entries.len() - 1);
        ui.select_first();
        assert_eq!(ui.selected, 0);
    }

    #[test]
    fn clamp_scroll_brings_selected_into_window() {
        let (fs, mut ui) = seeded();
        rebuild_entries(&mut ui, &fs);
        ui.selected = 2;
        ui.scroll_offset = 0;
        ui.clamp_scroll(2);
        assert_eq!(ui.scroll_offset, 1);
    }

    #[test]
    fn clamp_scroll_pulls_back_when_selected_above_window() {
        let (fs, mut ui) = seeded();
        rebuild_entries(&mut ui, &fs);
        ui.selected = 0;
        ui.scroll_offset = 2;
        ui.clamp_scroll(5);
        assert_eq!(ui.scroll_offset, 0);
    }

    #[test]
    fn rebuild_clamps_selected_past_new_end() {
        let (fs, mut ui) = seeded();
        ui.expand(canon("/project/sub"), &fs);
        ui.selected = 3; // valid at 4 entries
        ui.collapse(canon("/project/sub"), &fs);
        assert_eq!(ui.selected, 2);
    }
}
