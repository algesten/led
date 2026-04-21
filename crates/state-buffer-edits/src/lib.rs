//! The `BufferEdits` source — the user-edited view of each open
//! buffer.
//!
//! User-decision state: mutated by dispatch on character / deletion /
//! newline keypresses. Seeded from [`BufferStore`] by the runtime when
//! a file finishes loading (the [`LoadCompletion`] pass). The rope
//! here is **always** the current view the user sees; `BufferStore`
//! retains the pristine disk snapshot for dirty/save/reload questions.
//!
//! No driver: there is no async side to user edits. Same shape as
//! `state-tabs`. Future async work (save, reload) will live in a
//! sibling `driver-buffer-writes` crate that watches this source via
//! memos.
//!
//! Other crates reach in with their own `#[drv::input]` projections
//! (typically two: one for the rope — consumed by body_model — and
//! one for just the dirty flags — consumed by tab_bar_model).

use imbl::{HashMap, HashSet};
use led_core::CanonPath;
use ropey::Rope;
use std::sync::Arc;

pub mod history;
pub use history::{EditGroup, EditOp, History, rebase_char_index};

/// One open buffer's editable state.
#[derive(Debug, Clone, PartialEq)]
pub struct EditedBuffer {
    /// Current rope = disk base + all user edits applied.
    ///
    /// `Arc`-wrapped so the `body_model` memo input projection is a
    /// pointer copy, and cache-hit clones of `Frame` don't deep-copy
    /// the rope tree.
    pub rope: Arc<Rope>,
    /// Monotonically increasing; bumped on every edit. Doubles as
    /// (a) a cheap input-change key for memos that don't need the
    /// rope itself (dirty badge, status line) and (b) the anchor
    /// future rebase queries will translate coordinates against.
    pub version: u64,
    /// The `version` value last persisted to disk. Starts at 0 —
    /// matching `version` on a fresh load, so a just-loaded buffer
    /// reports `dirty() == false`. Advances on every successful
    /// save completion. A save that races behind a newer edit does
    /// not clear the dirty flag, because `version > saved_version`
    /// still holds.
    pub saved_version: u64,
    /// Undo / redo history. See [`History`]. Grows unbounded for
    /// the session; persistence is deferred to M21.
    pub history: History,
}

impl EditedBuffer {
    /// True iff the rope has been modified since the last save /
    /// load completion.
    pub fn dirty(&self) -> bool {
        self.version > self.saved_version
    }

    /// Fresh, clean seed for a buffer whose disk rope just arrived.
    pub fn fresh(rope: Arc<Rope>) -> Self {
        Self {
            rope,
            version: 0,
            saved_version: 0,
            history: History::default(),
        }
    }
}

/// Source: per-path edited buffer state + the set of paths the user
/// has asked to save but whose write hasn't been dispatched yet.
///
/// Invariants (maintained by dispatch + runtime seeding):
/// - An entry in `buffers` exists iff the runtime has observed a
///   `Ready` load completion for that path.
/// - `pending_saves ⊆ keys(buffers)` — dispatch only inserts paths
///   for which `buffers.get(path)` returned `Some` and reported
///   `dirty()`. Entries are cleared synchronously in the execute
///   phase, so the next tick's save query emits an empty list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BufferEdits {
    pub buffers: HashMap<CanonPath, EditedBuffer>,
    pub pending_saves: HashSet<CanonPath>,
    /// Map from the active buffer's path (`from`) to a fresh
    /// target (`to`) for a find-file SaveAs commit. Dispatch
    /// inserts here when `Enter` lands in SaveAs mode; the runtime
    /// drains the map in the query phase, turns each entry into a
    /// `SaveAction::SaveAs`, and sync-clears it before dispatching
    /// to the driver.
    pub pending_save_as: HashMap<CanonPath, CanonPath>,
    /// Queued project-wide replace-all requests. Dispatch pushes
    /// one when the user hits Alt+Enter in the file-search overlay;
    /// the runtime drains + ships to `driver-file-search`. Lives
    /// here (not on `FileSearchState`) so the overlay can close
    /// before the driver finishes — the pending cmd survives
    /// deactivation.
    pub pending_replace_all: Vec<PendingReplaceAll>,
    /// In-memory replacement counts staged by dispatch for the most
    /// recent replace, indexed by path. The runtime aggregates
    /// these with the driver's on-disk count to build the "Replaced
    /// N occurrences in M files" alert.
    pub pending_replace_in_memory: Vec<InMemoryReplace>,
    /// Queued on-disk single-hit replace commands. Each
    /// corresponds to a Right-arrow on a result row whose file
    /// isn't loaded as a buffer. Main loop drains + ships to the
    /// driver's single-replace lane.
    pub pending_single_replace: Vec<PendingSingleReplace>,
}

/// A pending on-disk replace-all request. Carries only the data
/// driver-file-search needs; the runtime materialises a
/// `FileSearchReplaceCmd` from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReplaceAll {
    pub root: CanonPath,
    pub query: String,
    pub replacement: String,
    pub case_sensitive: bool,
    pub use_regex: bool,
    /// Paths the runtime is already replacing in-memory — driver
    /// must skip these so it doesn't clobber unsaved changes.
    pub skip_paths: Vec<CanonPath>,
}

/// One in-memory replacement already applied by dispatch (to a
/// loaded buffer's rope). The runtime tallies these up with the
/// driver's reply to report the total count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InMemoryReplace {
    pub path: CanonPath,
    pub count: usize,
}

/// A pending on-disk single-hit replace. Dispatch queues one when
/// Right-arrow lands on a result row whose file isn't loaded; the
/// runtime ships it to the driver's single-replace lane. Byte
/// offsets are line-relative, matching `FileSearchHit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSingleReplace {
    pub path: CanonPath,
    pub line: usize,
    pub match_start: usize,
    pub match_end: usize,
    pub original: String,
    pub replacement: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn default_is_empty() {
        let e = BufferEdits::default();
        assert!(e.buffers.is_empty());
    }

    #[test]
    fn fresh_is_clean() {
        let rope = Arc::new(Rope::from_str("hello"));
        let eb = EditedBuffer::fresh(rope.clone());
        assert_eq!(eb.version, 0);
        assert_eq!(eb.saved_version, 0);
        assert!(!eb.dirty());
        assert!(Arc::ptr_eq(&eb.rope, &rope));
    }

    #[test]
    fn dirty_flips_when_version_exceeds_saved_version() {
        let mut eb = EditedBuffer::fresh(Arc::new(Rope::from_str("hi")));
        eb.version = 3;
        assert!(eb.dirty());
        eb.saved_version = 3;
        assert!(!eb.dirty());
        eb.version = 4; // user edited after save
        assert!(eb.dirty());
    }

    #[test]
    fn dirty_tracks_saved_version_not_a_flag() {
        // A save at version 2 that completes after the user has
        // edited to version 4 must leave the buffer dirty.
        let mut eb = EditedBuffer::fresh(Arc::new(Rope::from_str("x")));
        eb.version = 4;
        eb.saved_version = 2; // write finished; recorded older version
        assert!(eb.dirty());
    }

    #[test]
    fn entries_keyed_by_canon_path() {
        let mut e = BufferEdits::default();
        let p = canon("a.rs");
        e.buffers
            .insert(p.clone(), EditedBuffer::fresh(Arc::new(Rope::from_str("x"))));
        assert!(e.buffers.contains_key(&p));
    }
}
