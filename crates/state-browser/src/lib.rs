//! The `BrowserState` source — file-browser sidebar state.
//!
//! Holds the workspace root, a per-directory listings cache populated
//! by the FS driver, the set of expanded directories, the flattened
//! `entries` vector the painter walks, and the user's selection +
//! scroll + focus state.
//!
//! Rebuild semantics match legacy `led/src/model/browser`: the
//! flattened tree walks `root` + `expanded_dirs` + `dir_contents`,
//! sorts children dirs-first then files-alphabetically, and filters
//! out leading-`.` names.

use std::sync::Arc;

use imbl::{HashMap, HashSet, Vector};
use led_core::CanonPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Main,
    Side,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: CanonPath,
    pub kind: DirEntryKind,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserState {
    pub root: Option<CanonPath>,
    /// Per-directory listing cache, filled in by the FS driver.
    pub dir_contents: HashMap<CanonPath, Vector<DirEntry>>,
    pub expanded_dirs: HashSet<CanonPath>,
    /// Flattened tree view. Rebuilt by [`rebuild_entries`]; wrapped in
    /// `Arc` so cache-hit clones of [`BrowserState`] are cheap.
    pub entries: Arc<Vec<TreeEntry>>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub visible: bool,
    pub focus: Focus,
}

impl Default for BrowserState {
    fn default() -> Self {
        Self {
            root: None,
            dir_contents: HashMap::default(),
            expanded_dirs: HashSet::default(),
            entries: Arc::new(Vec::new()),
            selected: 0,
            scroll_offset: 0,
            visible: true,
            focus: Focus::Main,
        }
    }
}

impl BrowserState {
    /// Walk the tree and produce `entries`. Directories come first
    /// within each parent, then files; both groups sorted by
    /// locale-insensitive name. Hidden entries (leading `.`) are
    /// filtered out. Expanded subdirectories recurse inline.
    pub fn rebuild_entries(&mut self) {
        let mut out: Vec<TreeEntry> = Vec::new();
        let Some(root) = self.root.clone() else {
            self.entries = Arc::new(out);
            self.clamp_selection();
            return;
        };
        self.emit_children_of(&root, 0, &mut out);
        self.entries = Arc::new(out);
        self.clamp_selection();
    }

    fn emit_children_of(&self, dir: &CanonPath, depth: usize, out: &mut Vec<TreeEntry>) {
        let Some(children) = self.dir_contents.get(dir) else {
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
            let expanded = self.expanded_dirs.contains(&entry.path);
            out.push(TreeEntry {
                path: entry.path.clone(),
                name: entry.name.clone(),
                depth,
                kind: TreeEntryKind::Directory { expanded },
            });
            if expanded {
                self.emit_children_of(&entry.path, depth + 1, out);
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

    fn clamp_selection(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
            return;
        }
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len() - 1;
        }
    }

    pub fn expand(&mut self, path: CanonPath) {
        self.expanded_dirs.insert(path);
        self.rebuild_entries();
    }

    pub fn collapse(&mut self, path: CanonPath) {
        self.expanded_dirs.remove(&path);
        self.rebuild_entries();
    }

    pub fn collapse_all(&mut self) {
        self.expanded_dirs = HashSet::default();
        self.selected = 0;
        self.scroll_offset = 0;
        self.rebuild_entries();
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
    /// `visible_rows` of the viewport. Called by dispatch after
    /// every selection change.
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

    fn seeded_state() -> BrowserState {
        let mut b = BrowserState {
            root: Some(canon("/project")),
            ..Default::default()
        };
        let mut root_children = Vector::new();
        root_children.push_back(dir_entry("sub", "/project/sub", DirEntryKind::Directory));
        root_children.push_back(dir_entry("alpha.txt", "/project/alpha.txt", DirEntryKind::File));
        root_children.push_back(dir_entry("beta.txt", "/project/beta.txt", DirEntryKind::File));
        root_children.push_back(dir_entry(".hidden", "/project/.hidden", DirEntryKind::File));
        b.dir_contents.insert(canon("/project"), root_children);

        let mut sub_children = Vector::new();
        sub_children.push_back(dir_entry(
            "inner.txt",
            "/project/sub/inner.txt",
            DirEntryKind::File,
        ));
        b.dir_contents.insert(canon("/project/sub"), sub_children);
        b
    }

    #[test]
    fn default_is_empty_and_visible() {
        let b = BrowserState::default();
        assert!(b.entries.is_empty());
        assert!(b.visible);
        assert_eq!(b.focus, Focus::Main);
    }

    #[test]
    fn rebuild_without_root_is_empty() {
        let mut b = BrowserState::default();
        b.rebuild_entries();
        assert!(b.entries.is_empty());
    }

    #[test]
    fn rebuild_sorts_dirs_first_then_files_alphabetically() {
        let mut b = seeded_state();
        b.rebuild_entries();
        assert_eq!(b.entries.len(), 3); // sub + alpha + beta; .hidden filtered
        assert_eq!(b.entries[0].name, "sub");
        assert_eq!(b.entries[1].name, "alpha.txt");
        assert_eq!(b.entries[2].name, "beta.txt");
    }

    #[test]
    fn rebuild_recurses_into_expanded_dirs() {
        let mut b = seeded_state();
        b.expanded_dirs.insert(canon("/project/sub"));
        b.rebuild_entries();
        assert_eq!(b.entries.len(), 4);
        assert_eq!(b.entries[0].name, "sub");
        assert_eq!(b.entries[1].name, "inner.txt");
        assert_eq!(b.entries[1].depth, 1);
        assert_eq!(b.entries[2].name, "alpha.txt");
    }

    #[test]
    fn rebuild_filters_hidden_entries() {
        let mut b = seeded_state();
        b.rebuild_entries();
        assert!(!b.entries.iter().any(|e| e.name == ".hidden"));
    }

    #[test]
    fn expand_flips_chevron_and_reveals_children() {
        let mut b = seeded_state();
        b.expand(canon("/project/sub"));
        assert!(matches!(
            b.entries[0].kind,
            TreeEntryKind::Directory { expanded: true }
        ));
        assert_eq!(b.entries.len(), 4);
    }

    #[test]
    fn collapse_removes_from_expanded_and_rebuilds() {
        let mut b = seeded_state();
        b.expand(canon("/project/sub"));
        assert_eq!(b.entries.len(), 4);
        b.collapse(canon("/project/sub"));
        assert_eq!(b.entries.len(), 3);
        assert!(matches!(
            b.entries[0].kind,
            TreeEntryKind::Directory { expanded: false }
        ));
    }

    #[test]
    fn collapse_all_clears_expanded_and_resets_selection() {
        let mut b = seeded_state();
        b.expand(canon("/project/sub"));
        b.selected = 2;
        b.scroll_offset = 1;
        b.collapse_all();
        assert!(b.expanded_dirs.is_empty());
        assert_eq!(b.selected, 0);
        assert_eq!(b.scroll_offset, 0);
    }

    #[test]
    fn move_selection_clamps_at_ends() {
        let mut b = seeded_state();
        b.rebuild_entries();
        b.move_selection(-10);
        assert_eq!(b.selected, 0);
        b.move_selection(100);
        assert_eq!(b.selected, b.entries.len() - 1);
    }

    #[test]
    fn select_first_and_last() {
        let mut b = seeded_state();
        b.rebuild_entries();
        b.select_last();
        assert_eq!(b.selected, b.entries.len() - 1);
        b.select_first();
        assert_eq!(b.selected, 0);
    }

    #[test]
    fn clamp_scroll_brings_selected_into_window() {
        let mut b = seeded_state();
        b.rebuild_entries();
        b.selected = 2;
        b.scroll_offset = 0;
        b.clamp_scroll(2); // window [0..2); selected 2 is off-screen.
        assert_eq!(b.scroll_offset, 1); // window moves to [1..3)
    }

    #[test]
    fn clamp_scroll_pulls_back_when_selected_above_window() {
        let mut b = seeded_state();
        b.rebuild_entries();
        b.selected = 0;
        b.scroll_offset = 2;
        b.clamp_scroll(5);
        assert_eq!(b.scroll_offset, 0);
    }

    #[test]
    fn rebuild_clamps_selected_past_new_end() {
        let mut b = seeded_state();
        b.expand(canon("/project/sub"));
        b.selected = 3; // valid at 4 entries
        b.collapse(canon("/project/sub"));
        // After collapse, entries = 3; selected should clamp.
        assert_eq!(b.selected, 2);
    }
}
