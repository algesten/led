//! Git-specific data types: per-line status ranges.
//!
//! The category enum itself lives in [`crate::issue`] (re-exported
//! as [`crate::IssueCategory`]) since it's shared across git, LSP,
//! PR, and browser concerns. This module adds the per-line shape
//! the git driver emits and the two lookup helpers that the gutter
//! paint pass relies on.
//!
//! Ported verbatim from legacy `led/crates/core/src/git.rs` so
//! the new gutter painter lines up byte-for-byte with the
//! reference implementation.

use std::ops::Range;

use crate::IssueCategory;

/// One contiguous run of rows sharing a single [`IssueCategory`].
/// `rows` is `[start, end)`; a single changed line is
/// `rows: row..row + 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineStatus {
    pub category: IssueCategory,
    pub rows: Range<usize>,
}

/// Binary search for the line status covering `row`. Expects a
/// **non-overlapping** sorted list — each row sits in at most one
/// range. For merged lists where ranges can overlap (git unstaged
/// vs staged on the same row, or git + PR later), use
/// [`best_category_at`].
pub fn line_category_at(statuses: &[LineStatus], row: usize) -> Option<IssueCategory> {
    let idx = statuses
        .binary_search_by(|s| {
            if row < s.rows.start {
                std::cmp::Ordering::Greater
            } else if row >= s.rows.end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()?;
    Some(statuses[idx].category)
}

/// Highest-precedence category covering `row` in a potentially-
/// overlapping merged list. Linear scan — O(n) in the list length.
///
/// Used by the gutter paint when the same row might carry both
/// unstaged and staged hunks (or, at M27, a PR diff range on top
/// of a git line status). [`IssueCategory::precedence`] decides
/// the winner.
pub fn best_category_at(statuses: &[LineStatus], row: usize) -> Option<IssueCategory> {
    statuses
        .iter()
        .filter(|s| s.rows.contains(&row))
        .map(|s| s.category)
        .min_by_key(|c| c.precedence())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ls(cat: IssueCategory, a: usize, b: usize) -> LineStatus {
        LineStatus {
            category: cat,
            rows: a..b,
        }
    }

    #[test]
    fn line_category_at_finds_covering_range() {
        let statuses = vec![
            ls(IssueCategory::Unstaged, 3, 5),
            ls(IssueCategory::StagedModified, 10, 12),
        ];
        assert_eq!(
            line_category_at(&statuses, 3),
            Some(IssueCategory::Unstaged),
        );
        assert_eq!(
            line_category_at(&statuses, 4),
            Some(IssueCategory::Unstaged),
        );
        assert_eq!(
            line_category_at(&statuses, 11),
            Some(IssueCategory::StagedModified),
        );
    }

    #[test]
    fn line_category_at_returns_none_outside_ranges() {
        let statuses = vec![ls(IssueCategory::Unstaged, 3, 5)];
        assert_eq!(line_category_at(&statuses, 2), None);
        assert_eq!(line_category_at(&statuses, 5), None);
        assert_eq!(line_category_at(&statuses, 100), None);
    }

    #[test]
    fn best_category_at_picks_unstaged_over_staged_on_overlap() {
        let statuses = vec![
            ls(IssueCategory::StagedModified, 5, 7),
            ls(IssueCategory::Unstaged, 5, 7),
        ];
        assert_eq!(
            best_category_at(&statuses, 6),
            Some(IssueCategory::Unstaged),
        );
    }

    #[test]
    fn best_category_at_picks_lsp_error_over_git() {
        let statuses = vec![
            ls(IssueCategory::Unstaged, 0, 2),
            ls(IssueCategory::LspError, 0, 2),
        ];
        assert_eq!(
            best_category_at(&statuses, 1),
            Some(IssueCategory::LspError),
        );
    }

    #[test]
    fn best_category_at_returns_none_when_no_range_covers() {
        let statuses = vec![ls(IssueCategory::Unstaged, 0, 2)];
        assert_eq!(best_category_at(&statuses, 3), None);
    }
}
