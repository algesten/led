//! Git state atom — branch, per-file `IssueCategory` sets, and
//! per-buffer line-status lists.
//!
//! Populated by the runtime when it folds in driver events:
//!
//! - `GitEvent::FileStatuses` → replace [`GitState::branch`] +
//!   [`GitState::file_statuses`] wholesale. Every non-empty
//!   scan replaces the previous map; a path whose categories
//!   dropped to zero is absent from the new map.
//! - `GitEvent::LineStatuses` → insert or remove per-path. An
//!   empty `statuses: []` is the driver's clear-event and
//!   removes the key.
//!
//! Consumers:
//!
//! - `query::file_categories_map` unions `file_statuses` with
//!   the LSP-derived category set so the browser painter
//!   resolves the winning display via `resolve_display`.
//! - `query::body_model` reads `line_statuses.get(&path)` and
//!   merges it into the gutter's category ladder alongside
//!   LSP diagnostics.
//! - `query::status_bar_model` reads `branch` and prepends
//!   ` {name}` to the default left-string segment.

use std::sync::Arc;

use imbl::{HashMap, HashSet};
use led_core::git::LineStatus;
use led_core::{CanonPath, IssueCategory, PersistedContentHash};

/// Per-path line-status payload plus the buffer hash the scan
/// was computed against.
///
/// The runtime stamps `anchor_hash` with the buffer's
/// `disk_content_hash` at the time the [`GitEvent::LineStatuses`]
/// event arrives (git scans against the worktree, so disk hash
/// IS what the markers describe). Renderers consult
/// `EditedBuffer::row_delta_for(anchor_hash)` to invalidate /
/// translate marker rows that have been edited since.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitLineStatuses {
    pub anchor_hash: PersistedContentHash,
    pub statuses: Arc<Vec<LineStatus>>,
}

/// Full git state surface in one atom. `imbl` collections so
/// `Clone` is a pointer copy — the drv memos that project over
/// `GitStateInput` only invalidate on identity change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitState {
    /// Current branch shorthand (`"main"`, `"feature/foo"`). `None`
    /// means detached HEAD, no HEAD, or no repo.
    pub branch: Option<String>,
    /// Every path the last scan reported non-empty categories for.
    /// Keyed by canonical path so browser-row matching on file
    /// entries is trivial.
    pub file_statuses: HashMap<CanonPath, HashSet<IssueCategory>>,
    /// Per-buffer line-level statuses. `GitLineStatuses` carries
    /// both the `Arc<Vec<LineStatus>>` payload and the buffer hash
    /// the scan was computed against, so renderers can do
    /// per-line invalidation against the current buffer state.
    pub line_statuses: HashMap<CanonPath, GitLineStatuses>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn default_state_is_empty() {
        let s = GitState::default();
        assert!(s.branch.is_none());
        assert!(s.file_statuses.is_empty());
        assert!(s.line_statuses.is_empty());
    }

    #[test]
    fn file_statuses_insert_round_trip() {
        let mut s = GitState::default();
        let p = canon("/x/y.rs");
        let mut cats = HashSet::default();
        cats.insert(IssueCategory::Unstaged);
        s.file_statuses.insert(p.clone(), cats);
        assert!(s.file_statuses.get(&p).is_some());
        assert!(
            s.file_statuses
                .get(&p)
                .unwrap()
                .contains(&IssueCategory::Unstaged)
        );
    }

    #[test]
    fn branch_round_trip() {
        let s = GitState {
            branch: Some("main".to_string()),
            ..Default::default()
        };
        assert_eq!(s.branch.as_deref(), Some("main"));
    }

    #[test]
    fn line_statuses_insert_and_remove() {
        let mut s = GitState::default();
        let p = canon("/x/y.rs");
        let statuses = vec![LineStatus {
            category: IssueCategory::Unstaged,
            rows: 3..5,
        }];
        s.line_statuses.insert(
            p.clone(),
            GitLineStatuses {
                anchor_hash: PersistedContentHash::default(),
                statuses: Arc::new(statuses),
            },
        );
        assert!(s.line_statuses.get(&p).is_some());
        s.line_statuses.remove(&p);
        assert!(s.line_statuses.get(&p).is_none());
    }
}
