//! Cross-source issue / status helpers.
//!
//! These functions consume `ImblHashMap<CanonPath, ImblHashSet<IssueCategory>>`
//! — the cross-source aggregation that combines diagnostic + git
//! per-path categories. The `IssueCategory` enum and its static metadata
//! live in `led-core` (no map dependency, no aggregation logic); the
//! map-walking + display-resolution shaped here is runtime-query
//! territory.

use imbl::{HashMap as ImblHashMap, HashSet as ImblHashSet};
use led_core::{CanonPath, IssueCategory};

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
    use led_core::UserPath;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
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
}
