//! File-browser sidebar state — split into two sources per
//! EXAMPLE-ARCH § "Sources: two kinds of ground truth":
//!
//! - [`FsTree`] — **external fact**, written by the FS-list driver.
//!   Holds the workspace root and the per-directory listings cache.
//! - [`BrowserUi`] — **user decision**, mutated by dispatch. Holds
//!   which directories are user-pinned open, the current selection
//!   target (path, not index), scroll offset, the visible-panel
//!   toggle, and focus. **No derived fields:** the flattened
//!   entries list, the effective expansion set (user ∪
//!   ancestors-of-active-tab), and the resolved selected index
//!   all live in the query layer as memos over
//!   `(FsTree, BrowserUi, TabsActiveInput)`.
//!
//! Walk semantics match legacy `led/src/model/browser`: walk
//! `root` + effective expansion + `dir_contents`, sort children
//! dirs-first then files-alphabetically, and filter out leading-`.`
//! names.

use imbl::{HashMap, HashSet, Vector};
use led_core::CanonPath;
pub use led_driver_fs_list_core::{DirEntry, DirEntryKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, drv::Input)]
pub enum Focus {
    #[default]
    Main,
    Side,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, drv::Input)]
pub enum TreeEntryKind {
    File,
    Directory { expanded: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, drv::Input)]
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

/// **User-decision** source: the browser's UI state. Every field
/// here is either a user-driven decision or a scroll-position
/// cache that only the browser itself writes.
///
/// The flattened tree `entries`, the ephemeral ancestor expansion
/// for the active tab, and the resolved selection index are all
/// derived — they live in `runtime::query` as memos.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserUi {
    /// User-pinned expansions. Mutated ONLY by explicit user
    /// actions (`Expand` / `Collapse` / `CollapseAll`). Persists
    /// across tab switches. Ancestor-of-active-tab auto-reveal is
    /// NOT written here — it's derived per-tick by the query
    /// layer.
    pub expanded_dirs: HashSet<CanonPath>,
    /// The row the user (or the active-tab snap) is currently on.
    /// Path-based, not index-based, so the selection stays stable
    /// when the tree reshapes (auto-reveal, listing-arrival,
    /// expand/collapse). The painter resolves path → row via the
    /// `browser_entries` memo; `None` means "no tab active yet
    /// and user hasn't explicitly selected anything."
    pub selected_path: Option<CanonPath>,
    pub scroll_offset: usize,
    pub visible: bool,
    pub focus: Focus,
}

impl Default for BrowserUi {
    fn default() -> Self {
        Self {
            expanded_dirs: HashSet::default(),
            selected_path: None,
            scroll_offset: 0,
            visible: true,
            focus: Focus::Main,
        }
    }
}

/// Walk `fs.dir_contents` from `root` down, emitting one
/// [`TreeEntry`] per visible row. `effective_expanded` is the
/// union of user-pinned expansions and whatever ancestor-of-
/// active-tab expansions the query layer decided on this tick.
/// Pure — no state mutation; the result is the memo's output.
pub fn walk_tree(fs: &FsTree, effective_expanded: &HashSet<CanonPath>) -> Vec<TreeEntry> {
    let mut out: Vec<TreeEntry> = Vec::new();
    if let Some(root) = fs.root.as_ref() {
        emit_children_of(fs, effective_expanded, root, 0, &mut out);
    }
    out
}

/// Compute the ancestor chain of `active_path` up to (but not
/// including) `fs.root`, excluding any directories already in
/// `user_expanded` (the two buckets stay disjoint so the
/// ancestor set is the genuinely-auto-expanded extra). Returns
/// an empty set when there's no active path, no root, or the
/// path isn't inside the root.
pub fn ancestors_of(
    fs: &FsTree,
    user_expanded: &HashSet<CanonPath>,
    active_path: Option<&CanonPath>,
) -> HashSet<CanonPath> {
    let mut out: HashSet<CanonPath> = HashSet::default();
    let (Some(root), Some(p)) = (fs.root.as_ref(), active_path) else {
        return out;
    };
    let mut cur = p.as_path().parent();
    while let Some(parent) = cur {
        if parent == root.as_path() {
            break;
        }
        if !parent.starts_with(root.as_path()) {
            break;
        }
        let canon = led_core::UserPath::new(parent).canonicalize();
        if !user_expanded.contains(&canon) {
            out.insert(canon);
        }
        cur = parent.parent();
    }
    out
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

    fn seeded() -> FsTree {
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
        fs
    }

    // ── walk_tree ───────────────────────────────────────────

    #[test]
    fn default_browser_ui_is_empty_and_visible() {
        let ui = BrowserUi::default();
        assert!(ui.expanded_dirs.is_empty());
        assert_eq!(ui.selected_path, None);
        assert!(ui.visible);
        assert_eq!(ui.focus, Focus::Main);
    }

    #[test]
    fn walk_without_root_is_empty() {
        let fs = FsTree::default();
        let expanded = HashSet::default();
        assert!(walk_tree(&fs, &expanded).is_empty());
    }

    #[test]
    fn walk_sorts_dirs_first_then_files_alphabetically() {
        let fs = seeded();
        let expanded = HashSet::default();
        let entries = walk_tree(&fs, &expanded);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "sub");
        assert_eq!(entries[1].name, "alpha.txt");
        assert_eq!(entries[2].name, "beta.txt");
    }

    #[test]
    fn walk_recurses_into_expanded_dirs() {
        let fs = seeded();
        let mut expanded = HashSet::default();
        expanded.insert(canon("/project/sub"));
        let entries = walk_tree(&fs, &expanded);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, "sub");
        assert_eq!(entries[1].name, "inner.txt");
        assert_eq!(entries[1].depth, 1);
    }

    #[test]
    fn walk_filters_hidden_entries() {
        let fs = seeded();
        let expanded = HashSet::default();
        let entries = walk_tree(&fs, &expanded);
        assert!(entries.iter().all(|e| !e.name.starts_with('.')));
    }

    // ── ancestors_of ────────────────────────────────────────

    #[test]
    fn ancestors_none_without_active_path() {
        let fs = seeded();
        let user = HashSet::default();
        assert!(ancestors_of(&fs, &user, None).is_empty());
    }

    #[test]
    fn ancestors_chain_to_just_below_root() {
        let fs = seeded();
        let user = HashSet::default();
        let a = ancestors_of(&fs, &user, Some(&canon("/project/sub/inner.txt")));
        assert_eq!(a.len(), 1);
        assert!(a.contains(&canon("/project/sub")));
    }

    #[test]
    fn ancestors_exclude_user_pinned() {
        let fs = seeded();
        let mut user = HashSet::default();
        user.insert(canon("/project/sub"));
        let a = ancestors_of(&fs, &user, Some(&canon("/project/sub/inner.txt")));
        assert!(a.is_empty(), "user-pinned ancestors stay out of auto set");
    }

    #[test]
    fn ancestors_empty_when_path_outside_root() {
        let fs = seeded();
        let user = HashSet::default();
        let a = ancestors_of(&fs, &user, Some(&canon("/elsewhere/x.txt")));
        assert!(a.is_empty());
    }
}
