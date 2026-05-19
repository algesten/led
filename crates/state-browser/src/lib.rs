//! File-browser sidebar **user-decision** state.
//!
//! Per EXAMPLE-ARCH § "Sources: two kinds of ground truth", the
//! browser splits into two sources:
//!
//! - [`FsTree`] (in `led-driver-fs-list-core`) — *external fact*,
//!   written by the FS-list driver. Holds the workspace root and the
//!   per-directory listings cache. Relocated from this crate per the
//!   audit theme: wholly external-fact sources belong with the driver
//!   that fills them.
//! - [`BrowserUi`] (here) — *user decision*, mutated by dispatch.
//!   Holds which directories are user-pinned open, the current
//!   selection target (path, not index), scroll offset, the
//!   visible-panel toggle, and focus.
//!
//! **No derived fields:** the flattened entries list, the effective
//! expansion set (user ∪ ancestors-of-active-tab), and the resolved
//! selected index all live in the query layer as memos over
//! `(FsTree, BrowserUi, TabsActiveInput)`. The tree-walk helpers
//! (`walk_tree`, `ancestors_of`) and their output types (`TreeEntry`,
//! `TreeEntryKind`) live alongside `FsTree` in
//! `led-driver-fs-list-core` so this crate stays pure user-decision.

use imbl::HashSet;
use led_core::CanonPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, drv::Input)]
pub enum Focus {
    #[default]
    Main,
    Side,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_browser_ui_is_empty_and_visible() {
        let ui = BrowserUi::default();
        assert!(ui.expanded_dirs.is_empty());
        assert_eq!(ui.selected_path, None);
        assert!(ui.visible);
        assert_eq!(ui.focus, Focus::Main);
    }
}
