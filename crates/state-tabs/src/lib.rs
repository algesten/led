//! The `Tabs` source — which files are open and which is active.
//!
//! User-decision state: mutated by dispatch in response to user input
//! and, at startup, by CLI parsing. Other crates reach in with their
//! own `#[drv::input]` projections + `new(&Tabs)` constructors (the
//! drv pattern for cross-crate projections).

use led_core::CanonPath;

led_core::id_newtype!(TabId);

/// Buffer-coordinate cursor position. Stored on [`Tab`] so two tabs
/// viewing the same file can hold independent cursors.
///
/// `line` / `col` are zero-based indices; `col` is a character index
/// (not a display column — revisit for wide/combining characters when
/// syntax work comes online).
///
/// `preferred_col` is the user's horizontal "goal" — preserved across
/// `Up` / `Down` / `PageUp` / `PageDown` so that traversing a short
/// line and continuing onto a long line restores the original column.
/// Any explicit horizontal move (`Left` / `Right` / `Home` / `End`)
/// resets `preferred_col` to match the new `col`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
    pub preferred_col: usize,
}

/// Viewport scroll offset. `top` is the first buffer row shown at the
/// top of the body. Persists per tab across tab switches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll {
    pub top: usize,
}

/// One open tab. Stored in-line inside [`Tabs::open`] rather than
/// through a separate map so the source's invariants are local.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Tab {
    pub id: TabId,
    pub path: CanonPath,
    pub cursor: Cursor,
    pub scroll: Scroll,
    /// Second anchor for a region. Set by `SetMark`; cleared by
    /// `Abort` or consumed by `KillRegion`. Stays in raw buffer
    /// coordinates across edits — clamped on read, not rebased.
    pub mark: Option<Cursor>,
    /// `true` while this tab is a file-browser preview — opened by
    /// arrow-scanning the sidebar and flipped to a real tab on
    /// `OpenSelected`. M11. At most one preview tab exists at a time;
    /// dispatch enforces that invariant.
    pub preview: bool,
    /// Last committed isearch query for this tab. Stashed by
    /// `search_accept` / `search_cancel` so `Ctrl-s` on an empty
    /// query recalls it. Per-tab (not global) because users
    /// typically want per-buffer search history. M13.
    pub last_search: Option<String>,
}

/// Source: which tabs are open, which is active.
///
/// Invariants (maintained by dispatch, debug-asserted in tests):
/// - `active.is_some()` iff `!open.is_empty()`
/// - when `Some`, `active` is the id of exactly one [`Tab`] in `open`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tabs {
    pub open: imbl::Vector<Tab>,
    pub active: Option<TabId>,
}

/// "What files should be loaded right now?" — the set of paths
/// referenced by any open tab. Trivial for milestone 1 (every tab);
/// later milestones may prune (e.g., active + neighbours only).
///
/// Uses `&Tabs` as the memo input directly — the projection is the
/// whole struct. A narrower `#[drv::input]` would be warranted only
/// if memo-recompute on `active` changes becomes measurable.
#[drv::memo(single)]
pub fn desired_loaded_paths(tabs: &Tabs) -> imbl::HashSet<CanonPath> {
    tabs.open.iter().map(|t| t.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn tab(id: u64, path: &str) -> Tab {
        Tab {
            id: TabId(id),
            path: canon(path),
            ..Default::default()
        }
    }

    #[test]
    fn default_tabs_is_empty_and_inactive() {
        let t = Tabs::default();
        assert!(t.open.is_empty());
        assert!(t.active.is_none());
    }

    #[test]
    fn tabs_can_hold_entries() {
        let mut t = Tabs::default();
        let id = TabId(1);
        t.open.push_back(Tab { id, path: canon("a.rs"), ..Default::default() });
        t.active = Some(id);

        assert_eq!(t.open.len(), 1);
        assert_eq!(t.open[0].id, id);
        assert_eq!(t.active, Some(id));
    }

    #[test]
    fn desired_loaded_paths_is_union_of_open_paths() {
        let mut t = Tabs::default();
        t.open.push_back(tab(1, "a.rs"));
        t.open.push_back(tab(2, "b.rs"));

        let paths = desired_loaded_paths(&t);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&canon("a.rs")));
        assert!(paths.contains(&canon("b.rs")));
    }

    #[test]
    fn desired_loaded_paths_is_empty_when_no_tabs() {
        let t = Tabs::default();
        assert!(desired_loaded_paths(&t).is_empty());
    }
}
