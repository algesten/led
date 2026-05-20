//! Single source of truth for "issue/status" categories surfaced by the editor.
//!
//! Used by:
//! - Alt-./Alt-, NextIssue navigation (via [`IssueCategory::NAV_LEVELS`] +
//!   [`IssueCategory::at_level`]).
//! - File browser coloring / letters (`CategoryInfo::browser_letter`).
//! - Editor gutter line coloring.
//! - Cross-pane display via the runtime's `query::issues` helpers
//!   (`resolve_display`, `directory_categories`), which walk the
//!   diagnostics + git aggregation map.
//!
//! Adding a variant requires updating [`IssueCategory::info`],
//! [`IssueCategory::precedence`], and [`IssueCategory::at_level`] — the
//! compiler enforces exhaustiveness.

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
}

/// Static per-category metadata. The `theme_key` is kept as a `&'static str`
/// for diagnostic / trace purposes; paint sites resolve colour through
/// `category_style` helpers that match on the enum directly (the rewrite
/// has no string-keyed theme lookup).
#[derive(Debug, Clone, Copy)]
pub struct CategoryInfo {
    pub theme_key: &'static str,
    /// Letter shown in the file browser. `None` → render a bullet (•).
    pub browser_letter: Option<char>,
    /// Alt-./Alt-, NextIssue level. Lower = higher priority.
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
        }
    }

    /// All categories at a given nav level.
    pub fn at_level(level: u8) -> &'static [IssueCategory] {
        match level {
            1 => &[IssueCategory::LspError],
            2 => &[IssueCategory::LspWarning],
            3 => &[IssueCategory::Unstaged],
            4 => &[IssueCategory::StagedModified, IssueCategory::StagedNew],
            _ => &[],
        }
    }

    /// All defined nav levels in order.
    pub const NAV_LEVELS: &'static [u8] = &[1, 2, 3, 4];

    /// Priority for tie-breaking when multiple categories apply to the same
    /// file or line. Lower number = higher precedence (wins both letter +
    /// colour).
    ///
    /// Order (highest precedence first): `LspError` > `LspWarning` >
    /// `Unstaged` > `StagedNew` > `StagedModified` > `Untracked`. `Unstaged`
    /// outranks the staged variants because it's the most recent / loudest
    /// action item.
    pub const fn precedence(self) -> u8 {
        use IssueCategory::*;
        match self {
            LspError => 0,
            LspWarning => 1,
            Unstaged => 2,
            StagedNew => 3,
            StagedModified => 4,
            Untracked => 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_ordering() {
        assert!(IssueCategory::LspError.precedence() < IssueCategory::LspWarning.precedence());
        assert!(IssueCategory::LspWarning.precedence() < IssueCategory::Unstaged.precedence());
        assert!(IssueCategory::Unstaged.precedence() < IssueCategory::StagedNew.precedence());
        assert!(IssueCategory::StagedNew.precedence() < IssueCategory::StagedModified.precedence());
        assert!(IssueCategory::StagedModified.precedence() < IssueCategory::Untracked.precedence());
    }

    #[test]
    fn nav_levels_cover_all_navigable_categories() {
        for level in IssueCategory::NAV_LEVELS {
            assert!(
                !IssueCategory::at_level(*level).is_empty(),
                "NAV_LEVELS level {level} had no categories"
            );
        }
    }
}
