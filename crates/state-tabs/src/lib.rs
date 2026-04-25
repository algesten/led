//! The `Tabs` source — which files are open and which is active.
//!
//! User-decision state: mutated by dispatch in response to user input
//! and, at startup, by CLI parsing. Other crates reach in with their
//! own `#[drv::input]` projections + `new(&Tabs)` constructors (the
//! drv pattern for cross-crate projections).

use led_core::{CanonPath, SubLine};

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, drv::Input)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
    pub preferred_col: usize,
}

/// Viewport scroll offset. `top` is the first logical line whose
/// sub-line `top_sub_line` sits at the top of the body — the
/// anchor lives in logical-line × sub-line space so soft-wrapped
/// buffers scroll one visual row at a time instead of jumping by
/// whole logical lines. Persists per tab across tab switches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, drv::Input)]
pub struct Scroll {
    pub top: usize,
    pub top_sub_line: SubLine,
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
    /// For preview tabs only: the tab id that was active when the
    /// preview first opened. `close_preview` restores this on
    /// directory-nav / abort so the user returns to whatever they
    /// were looking at before arrow-scanning. Mirrors legacy's
    /// `Tab::previous_tab` (`/led/led/src/model/action/preview.rs`).
    /// `None` on non-preview tabs or when no tab was active.
    pub previous_tab: Option<TabId>,
    /// Last committed isearch query for this tab. Stashed by
    /// `search_accept` / `search_cancel` so `Ctrl-s` on an empty
    /// query recalls it. Per-tab (not global) because users
    /// typically want per-buffer search history. M13.
    pub last_search: Option<String>,
    /// Cursor to apply once the buffer at this tab's path has
    /// been loaded into [`led_state_buffer_edits::BufferEdits`].
    /// Set when a tab is opened with a target the user wants
    /// the cursor at — session restore (M21), Alt-Enter
    /// goto-def into a not-yet-open file, Alt-./Alt-, into a
    /// not-yet-open file. The load-completion ingest hook
    /// applies this (clamped to the rope) and clears the
    /// field. None on tabs that don't need a deferred cursor.
    pub pending_cursor: Option<Cursor>,
    /// Companion to `pending_cursor`: the scroll anchor to
    /// apply at the same moment. Session restore carries the
    /// exact pre-quit scroll; the goto-def / issue-nav paths
    /// leave this `None` and let the load-completion hook
    /// recenter via `dispatch::center_on_cursor`.
    pub pending_scroll: Option<Scroll>,
}

/// Source: which tabs are open, which is active.
///
/// Invariants (maintained by dispatch, debug-asserted in tests):
/// - `active.is_some()` iff `!open.is_empty()`
/// - when `Some`, `active` is the id of exactly one [`Tab`] in `open`
///
/// Carries `#[derive(drv::Input)]` so it can be used as a memo
/// input directly (see [`desired_loaded_paths`] below). A
/// narrower projection (via its own `#[derive(drv::Input)]`
/// struct in `runtime::query`) is still preferable when a memo
/// only reads a subset of the fields — that path invalidates
/// less often as other fields churn.
#[derive(Debug, Clone, PartialEq, Default, drv::Input)]
pub struct Tabs {
    pub open: imbl::Vector<Tab>,
    pub active: Option<TabId>,
}

/// "What files should be loaded right now?" — the set of paths
/// referenced by any open tab. Trivial for milestone 1 (every tab);
/// later milestones may prune (e.g., active + neighbours only).
///
/// Uses `&Tabs` as the memo input directly — the projection is
/// the whole struct. A narrower projection is warranted only if
/// memo-recompute on `active` changes becomes measurable.
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
