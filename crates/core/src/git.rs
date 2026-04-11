//! Git-specific data types: per-line status ranges.
//!
//! The category enum lives in [`crate::issue`] (re-exported as
//! [`crate::IssueCategory`]) since it's shared across git, LSP, PR, and
//! browser concerns.

use std::ops::Range;

use crate::IssueCategory;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineStatus {
    pub category: IssueCategory,
    pub rows: Range<usize>,
}

/// Binary search for the line status covering `row`. Expects a
/// **non-overlapping** sorted list (each row is in at most one range).
/// For merged lists where ranges can overlap, use [`best_category_at`].
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

/// Find the highest-precedence category covering `row` in a
/// potentially-overlapping merged list. Linear scan — O(n).
///
/// Use this when the list contains multiple sources (git line statuses,
/// PR diff, PR comments) that can overlap at the same row.
pub fn best_category_at(statuses: &[LineStatus], row: usize) -> Option<IssueCategory> {
    statuses
        .iter()
        .filter(|s| s.rows.contains(&row))
        .map(|s| s.category)
        .min_by_key(|c| c.precedence())
}
