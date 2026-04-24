//! Single source of truth for "issue/status" categories surfaced by the editor.
//!
//! Used by:
//! - Alt-./Alt-, NextIssue navigation (via [`IssueCategory::NAV_LEVELS`] +
//!   [`IssueCategory::at_level`]) — lands in M20a once git+PR are present.
//! - File browser coloring / letters (`CategoryInfo::browser_letter`).
//! - Editor gutter line coloring.
//! - Cross-pane display of the winning category via [`resolve_display`].
//!
//! Ported from legacy `/Users/martin/dev/led/crates/core/src/issue.rs`. The
//! enum shape, precedence, letters, nav levels, and aggregation semantics
//! match legacy **verbatim**, so ported paint/nav behaviour lines up cell-
//! for-cell with the main branch binary.
//!
//! Adding a variant requires updating [`IssueCategory::info`],
//! [`IssueCategory::precedence`], and [`IssueCategory::at_level`] — the
//! compiler enforces exhaustiveness.

use imbl::{HashMap as ImblHashMap, HashSet as ImblHashSet};

use crate::CanonPath;

/// The single canonical category enum.
///
/// Only `LspError` / `LspWarning` are populated today (M16 scope); git and
/// PR variants are plumbed so the browser painter, category resolver, and
/// nav-level machinery are ready when those milestones land (M19 + M27).
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
    /// file or line. Lower number = higher precedence (wins both letter +
    /// colour).
    ///
    /// Order (highest precedence first): `LspError` > `LspWarning` >
    /// `Unstaged` > `StagedNew` > `StagedModified` > `Untracked` >
    /// `PrComment` > `PrDiff`. `Unstaged` outranks the staged variants
    /// because it's the most recent / loudest action item.
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

/// What the painter needs to render one browser row's status column:
/// the letter (or bullet fallback) and the winning category so the painter
/// can pick a colour.
#[derive(Debug, Clone, Copy)]
pub struct StatusDisplay {
    pub letter: char,
    pub category: IssueCategory,
}

/// Resolve a set of categories into a single display (browser file row).
/// The category with the highest precedence (lowest number) wins both
/// letter and colour. Categories without a letter (e.g. PR-only) fall back
/// to a bullet.
pub fn resolve_display(categories: &ImblHashSet<IssueCategory>) -> Option<StatusDisplay> {
    let winner = *categories.iter().min_by_key(|c| c.precedence())?;
    let info = winner.info();
    Some(StatusDisplay {
        letter: info.browser_letter.unwrap_or('\u{2022}'),
        category: winner,
    })
}

/// Aggregate categories for all files under a directory. Matches legacy
/// `directory_categories` — shallow union over the `file_categories` map,
/// including every descendant file's categories. Excludes the dir's own
/// entry when the map happens to carry one.
pub fn directory_categories(
    file_categories: &ImblHashMap<CanonPath, ImblHashSet<IssueCategory>>,
    dir: &CanonPath,
) -> ImblHashSet<IssueCategory> {
    let mut result = ImblHashSet::default();
    for (path, cats) in file_categories.iter() {
        if path.as_path().starts_with(dir.as_path()) && path != dir {
            for c in cats.iter() {
                result.insert(*c);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn precedence_ordering_matches_legacy_exactly() {
        assert!(IssueCategory::LspError.precedence() < IssueCategory::LspWarning.precedence());
        assert!(IssueCategory::LspWarning.precedence() < IssueCategory::Unstaged.precedence());
        assert!(IssueCategory::Unstaged.precedence() < IssueCategory::StagedNew.precedence());
        assert!(IssueCategory::StagedNew.precedence() < IssueCategory::StagedModified.precedence());
        assert!(IssueCategory::StagedModified.precedence() < IssueCategory::Untracked.precedence());
        assert!(IssueCategory::Untracked.precedence() < IssueCategory::PrComment.precedence());
        assert!(IssueCategory::PrComment.precedence() < IssueCategory::PrDiff.precedence());
    }

    #[test]
    fn resolve_display_picks_highest_precedence() {
        let mut cats = ImblHashSet::default();
        cats.insert(IssueCategory::Unstaged);
        cats.insert(IssueCategory::LspWarning);
        cats.insert(IssueCategory::LspError);
        let d = resolve_display(&cats).expect("non-empty");
        assert_eq!(d.category, IssueCategory::LspError);
    }

    #[test]
    fn resolve_display_renders_bullet_for_letterless_categories() {
        let mut cats = ImblHashSet::default();
        cats.insert(IssueCategory::LspError);
        let d = resolve_display(&cats).unwrap();
        assert_eq!(d.letter, '\u{2022}');
    }

    #[test]
    fn resolve_display_renders_letter_when_available() {
        let mut cats = ImblHashSet::default();
        cats.insert(IssueCategory::Untracked);
        let d = resolve_display(&cats).unwrap();
        assert_eq!(d.letter, 'U');
    }

    #[test]
    fn resolve_display_returns_none_on_empty_set() {
        let cats = ImblHashSet::default();
        assert!(resolve_display(&cats).is_none());
    }

    #[test]
    fn directory_categories_unions_descendant_categories() {
        let mut map: ImblHashMap<CanonPath, ImblHashSet<IssueCategory>> =
            ImblHashMap::default();
        let mut errs = ImblHashSet::default();
        errs.insert(IssueCategory::LspError);
        map.insert(canon("/root/sub/a.rs"), errs);
        let mut mods = ImblHashSet::default();
        mods.insert(IssueCategory::Unstaged);
        map.insert(canon("/root/sub/deep/b.rs"), mods);
        let agg = directory_categories(&map, &canon("/root/sub"));
        assert!(agg.contains(&IssueCategory::LspError));
        assert!(agg.contains(&IssueCategory::Unstaged));
    }

    #[test]
    fn directory_categories_excludes_unrelated_paths() {
        let mut map: ImblHashMap<CanonPath, ImblHashSet<IssueCategory>> =
            ImblHashMap::default();
        let mut errs = ImblHashSet::default();
        errs.insert(IssueCategory::LspError);
        map.insert(canon("/elsewhere/z.rs"), errs);
        let agg = directory_categories(&map, &canon("/root/sub"));
        assert!(agg.is_empty());
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
