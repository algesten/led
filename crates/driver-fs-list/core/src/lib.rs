//! Sync core of the directory-listing driver — strictly isolated.
//!
//! Owns:
//!  - The wire ABI: `ListCmd`, `ListDone`, the trace hook, and the
//!    main-loop-facing [`FsListDriver`].
//!  - The driver-owned [`FsListState`] tracking in-flight listings.
//!  - The **external-fact source** [`FsTree`] (relocated here from
//!    `state-browser` per the EXAMPLE-ARCH audit: wholly external-
//!    fact sources belong with the driver that fills them) plus its
//!    pure tree-walk helpers `walk_tree` / `ancestors_of` and their
//!    output types `TreeEntry` / `TreeEntryKind`.
//!  - The re-exported leaf-crate types `DirEntry` / `DirEntryKind`
//!    from `led-abi-fs-list` — kept in the leaf crate so they remain
//!    importable from elsewhere without depending on this driver.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use imbl::{HashMap, HashSet, Vector};
use led_core::CanonPath;

pub use led_abi_fs_list::{DirEntry, DirEntryKind};

/// Driver-owned in-flight tracking. Path is added on `execute`
/// (sync, before tx.send) and removed on `process` when the
/// matching `Done` arrives. Memos read this to avoid re-emitting
/// `ListCmd::List(p)` while p is outstanding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsListState {
    pub in_flight: imbl::HashSet<CanonPath>,
}

// ── External-fact source: the FS view of the workspace ────────

/// **External-fact** source: the file-system view of the workspace.
/// Written exclusively by the FS-list driver's ingest path; never
/// mutated by dispatch. Lives in the driver crate because every
/// field is driver-discovered — relocating per the EXAMPLE-ARCH
/// audit ("wholly external-fact sources belong in the driver that
/// fills them").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsTree {
    pub root: Option<CanonPath>,
    /// Per-directory listing cache, filled by the FS driver.
    pub dir_contents: HashMap<CanonPath, Vector<DirEntry>>,
    /// Directories whose last listing attempt failed (missing,
    /// permission denied, etc). Mirrors `BufferStore`'s
    /// `LoadState::Error` discipline: keeping the failure tracked
    /// here is what lets `file_list_action` skip the path on
    /// subsequent ticks instead of re-firing forever. In-memory
    /// only — never persisted, so a transient failure (network
    /// mount, etc.) gets one fresh attempt per session, and a
    /// stale persisted `expanded_dirs` entry pointing at a deleted
    /// dir burns one `read_dir` call per session instead of
    /// spinning the loop. Cleared by `invalidate_subtree` and by
    /// the `apply_workspace_tree_delta` CREATE path so a re-mkdir
    /// or git checkout under the recursive root recovers without
    /// user action.
    pub failed_dirs: HashSet<CanonPath>,
}

/// One row in the flattened browser tree. Pure output of
/// [`walk_tree`]; lives alongside `FsTree` because it's the
/// projection of an FS state and no UI fields contribute to its
/// shape (the expansion bit is the *effective* expansion union
/// supplied by the caller).
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListCmd {
    List(CanonPath),
}

#[derive(Debug)]
pub struct ListDone {
    pub path: CanonPath,
    pub result: Result<Vec<DirEntry>, String>,
}

/// `--golden-trace` hook for FS-list events.
pub trait Trace: Send + Sync {
    fn list_start(&self, path: &CanonPath);
    fn list_done(&self, path: &CanonPath, result: &Result<Vec<DirEntry>, String>);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn list_start(&self, _: &CanonPath) {}
    fn list_done(&self, _: &CanonPath, _: &Result<Vec<DirEntry>, String>) {}
}

/// Sync driver: runtime calls `execute(actions)` to enqueue listings
/// and `process()` to drain completions.
pub struct FsListDriver {
    cmd_tx: Sender<ListCmd>,
    done_rx: Receiver<ListDone>,
    trace: Arc<dyn Trace>,
}

impl FsListDriver {
    pub fn new(cmd_tx: Sender<ListCmd>, done_rx: Receiver<ListDone>, trace: Arc<dyn Trace>) -> Self {
        Self {
            cmd_tx,
            done_rx,
            trace,
        }
    }

    pub fn execute<'a, I>(&self, cmds: I, state: &mut FsListState)
    where
        I: IntoIterator<Item = &'a ListCmd>,
    {
        for cmd in cmds {
            match cmd {
                ListCmd::List(path) => {
                    state.in_flight.insert(path.clone());
                    self.trace.list_start(path);
                }
            }
            // Clone once; the worker owns it from here.
            if self.cmd_tx.send(cmd.clone()).is_err() {
                // Worker gone — next tick's drivers will observe the
                // same and the runtime can shut down cleanly.
                return;
            }
        }
    }

    pub fn process(&self, state: &mut FsListState) -> Vec<ListDone> {
        let mut out: Vec<ListDone> = Vec::new();
        while let Ok(done) = self.done_rx.try_recv() {
            state.in_flight.remove(&done.path);
            self.trace.list_done(&done.path, &done.result);
            out.push(done);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn process_returns_empty_when_nothing_queued() {
        let (_cmd_tx, cmd_rx) = mpsc::channel::<ListCmd>();
        let (_done_tx, done_rx) = mpsc::channel::<ListDone>();
        let _ = cmd_rx; // keep receiver alive
        let drv = FsListDriver::new(_cmd_tx_noop(), done_rx, Arc::new(NoopTrace));
        let mut state = FsListState::default();
        assert!(drv.process(&mut state).is_empty());
    }

    fn _cmd_tx_noop() -> Sender<ListCmd> {
        // Throwaway sender — tests don't exercise the cmd path here.
        let (tx, _rx) = mpsc::channel();
        tx
    }

    #[test]
    fn process_drains_a_result_and_clears_in_flight() {
        use led_core::UserPath;
        let (cmd_tx, _cmd_rx) = mpsc::channel::<ListCmd>();
        let (done_tx, done_rx) = mpsc::channel::<ListDone>();
        let drv = FsListDriver::new(cmd_tx, done_rx, Arc::new(NoopTrace));
        let path = UserPath::new("/x").canonicalize();
        let mut state = FsListState::default();
        state.in_flight.insert(path.clone());
        done_tx
            .send(ListDone {
                path: path.clone(),
                result: Ok(Vec::new()),
            })
            .unwrap();
        let batch = drv.process(&mut state);
        assert_eq!(batch.len(), 1);
        assert!(!state.in_flight.contains(&path));
    }

    #[test]
    fn execute_marks_path_in_flight_before_send() {
        use led_core::UserPath;
        let (cmd_tx, cmd_rx) = mpsc::channel::<ListCmd>();
        let (_done_tx, done_rx) = mpsc::channel::<ListDone>();
        let drv = FsListDriver::new(cmd_tx, done_rx, Arc::new(NoopTrace));
        let path = UserPath::new("/x").canonicalize();
        let mut state = FsListState::default();
        drv.execute([&ListCmd::List(path.clone())], &mut state);
        assert!(state.in_flight.contains(&path));
        assert!(matches!(cmd_rx.try_recv(), Ok(ListCmd::List(p)) if p == path));
    }

    // ── FsTree / walk_tree / ancestors_of ─────────────────────

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
