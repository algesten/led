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

use imbl::HashMap;
use led_core::CanonPath;
use ropey::Rope;
use std::sync::Arc;

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
    /// `false` while rope == disk base; flips to `true` on the first
    /// edit. Reset by save (M4) / reload (later).
    pub dirty: bool,
}

impl EditedBuffer {
    /// Fresh, clean seed for a buffer whose disk rope just arrived.
    pub fn fresh(rope: Arc<Rope>) -> Self {
        Self {
            rope,
            version: 0,
            dirty: false,
        }
    }
}

/// Source: per-path edited buffer state.
///
/// Invariants (maintained by dispatch + runtime seeding):
/// - An entry exists iff the runtime has observed a `Ready` load
///   completion for that path.
/// - `dirty == (version > 0)` for buffers that haven't been saved or
///   reloaded yet. Post-M4, saves reset `version`-vs-disk tracking
///   independently of the counter.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BufferEdits {
    pub buffers: HashMap<CanonPath, EditedBuffer>,
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
        assert!(!eb.dirty);
        assert!(Arc::ptr_eq(&eb.rope, &rope));
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
