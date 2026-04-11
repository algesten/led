//! Single source of truth for "issue/status" categories surfaced by the editor.
//!
//! Used by:
//!   - Alt-. NextIssue navigation (via [`IssueCategory::NAV_LEVELS`] and [`IssueCategory::at_level`])
//!   - File browser coloring/letters (via [`CategoryInfo::theme_key`] and [`CategoryInfo::browser_letter`])
//!   - Gutter line coloring (via [`CategoryInfo::theme_key`])
//!
//! Adding a variant requires updating [`IssueCategory::info`] and every match
//! site — the compiler enforces exhaustiveness.

use std::collections::{HashMap, HashSet};

use crate::CanonPath;

/// The single canonical category enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IssueCategory {
    /// LSP error diagnostic.
    LspError,
    /// LSP warning diagnostic.
    LspWarning,
    /// Worktree differs from index — local edit not yet `git add`'d.
    Unstaged,
    /// Index differs from HEAD, file existed in HEAD.
    StagedModified,
    /// Index differs from HEAD, file did not exist in HEAD.
    StagedNew,
    /// File is unknown to git.
    Untracked,
    /// PR review comment on a line.
    PrComment,
    /// Line is part of the PR diff.
    PrDiff,
}

#[derive(Debug, Clone, Copy)]
pub struct CategoryInfo {
    /// Theme key for the color used by browser, gutter, and overlays.
    pub theme_key: &'static str,
    /// Letter shown in the file browser. `None` means render a bullet (•).
    pub browser_letter: Option<char>,
    /// Alt-. NextIssue level. Lower = higher priority.
    /// `None` = not navigable (e.g. `Untracked` has no specific lines).
    pub nav_level: Option<u8>,
    /// Human-readable category label, used in status messages.
    pub label: &'static str,
}

impl IssueCategory {
    /// The canonical mapping. **Single source of truth** — do not duplicate
    /// theme keys, letters, or nav levels anywhere else.
    pub const fn info(self) -> CategoryInfo {
        use IssueCategory::*;
        match self {
            LspError => CategoryInfo {
                theme_key: "diagnostics.error",
                browser_letter: None,
                nav_level: Some(1),
                label: "Error",
            },
            LspWarning => CategoryInfo {
                theme_key: "diagnostics.warning",
                browser_letter: None,
                nav_level: Some(2),
                label: "Warning",
            },
            Unstaged => CategoryInfo {
                theme_key: "git.modified",
                browser_letter: Some('M'),
                nav_level: Some(3),
                label: "Unstaged",
            },
            StagedModified => CategoryInfo {
                theme_key: "git.added",
                browser_letter: Some('M'),
                nav_level: Some(4),
                label: "Staged",
            },
            StagedNew => CategoryInfo {
                theme_key: "git.added",
                browser_letter: Some('A'),
                nav_level: Some(4),
                label: "Staged",
            },
            Untracked => CategoryInfo {
                theme_key: "git.untracked",
                browser_letter: Some('U'),
                nav_level: None,
                label: "Untracked",
            },
            PrComment => CategoryInfo {
                theme_key: "pr.comment",
                browser_letter: None,
                nav_level: Some(5),
                label: "Comment",
            },
            PrDiff => CategoryInfo {
                theme_key: "pr.diff",
                browser_letter: None,
                nav_level: Some(5),
                label: "PR diff",
            },
        }
    }

    /// All categories at a given nav level.
    pub fn at_level(level: u8) -> &'static [IssueCategory] {
        match level {
            1 => &[IssueCategory::LspError],
            2 => &[IssueCategory::LspWarning],
            3 => &[IssueCategory::Unstaged],
            4 => &[IssueCategory::StagedModified, IssueCategory::StagedNew],
            5 => &[IssueCategory::PrComment, IssueCategory::PrDiff],
            _ => &[],
        }
    }

    /// All defined nav levels in order.
    pub const NAV_LEVELS: &'static [u8] = &[1, 2, 3, 4, 5];

    /// Priority for tie-breaking when multiple categories apply to the same
    /// file or line. Lower number = higher precedence (wins both letter & color).
    ///
    /// Order: Unstaged > StagedNew > StagedModified > Untracked
    /// (Unstaged is the most recent / loudest action item.)
    pub const fn precedence(self) -> u8 {
        use IssueCategory::*;
        match self {
            LspError => 0,
            LspWarning => 1,
            Unstaged => 2,
            StagedNew => 3,
            StagedModified => 4,
            Untracked => 5,
            PrComment => 6,
            PrDiff => 7,
        }
    }
}

pub struct StatusDisplay {
    pub letter: char,
    pub theme_key: &'static str,
}

/// Resolve a set of categories into a single display (browser file row).
/// The category with the highest precedence (lowest number) wins both letter
/// and color. Categories without a letter (e.g. PR-only) fall back to a
/// bullet rendered by the caller.
pub fn resolve_display(categories: &HashSet<IssueCategory>) -> Option<StatusDisplay> {
    let winner = categories.iter().min_by_key(|c| c.precedence())?;
    let info = winner.info();
    Some(StatusDisplay {
        letter: info.browser_letter.unwrap_or('\u{2022}'),
        theme_key: info.theme_key,
    })
}

/// Aggregate categories for all files under a directory.
pub fn directory_categories(
    file_categories: &HashMap<CanonPath, HashSet<IssueCategory>>,
    dir: &CanonPath,
) -> HashSet<IssueCategory> {
    let mut result = HashSet::new();
    for (path, cats) in file_categories {
        if path.starts_with(dir) && path != dir {
            result.extend(cats);
        }
    }
    result
}
