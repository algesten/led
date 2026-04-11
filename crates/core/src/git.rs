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

/// Binary search for the line status covering `row`.
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
