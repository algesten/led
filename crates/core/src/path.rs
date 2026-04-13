use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Components, Path, PathBuf};

use serde::{Deserialize, Serialize};

// ── symlink_chain ──

/// Walk the symlink chain starting at `path`. The first entry is `path`
/// itself; subsequent entries are the targets of each symlink, resolved
/// relative to the link's parent. Stops on the first non-symlink (regular
/// file or `read_link` failure) and on cycles.
pub fn symlink_chain(path: &Path) -> Vec<PathBuf> {
    let mut chain = vec![path.to_path_buf()];
    let mut current = path.to_path_buf();
    loop {
        match std::fs::read_link(&current) {
            Ok(target) => {
                let resolved = if target.is_relative() {
                    current.parent().unwrap_or(Path::new(".")).join(&target)
                } else {
                    target
                };
                if chain.contains(&resolved) {
                    break;
                }
                chain.push(resolved.clone());
                current = resolved;
            }
            Err(_) => break,
        }
    }
    chain
}

// ── CanonPath ──

/// A canonicalized path. The only way to create one is [`UserPath::canonicalize`].
///
/// Used for all internal state: buffer keys, tab identity, file watcher
/// registrations, git status maps, LSP document tracking, etc.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CanonPath(PathBuf);

impl CanonPath {
    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn parent(&self) -> Option<CanonPath> {
        self.0.parent().map(|p| CanonPath(p.to_path_buf()))
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }

    pub fn extension(&self) -> Option<&OsStr> {
        self.0.extension()
    }

    pub fn starts_with(&self, base: &CanonPath) -> bool {
        self.0.starts_with(&base.0)
    }

    pub fn ends_with(&self, suffix: impl AsRef<Path>) -> bool {
        self.0.ends_with(suffix)
    }

    pub fn join(&self, component: impl AsRef<Path>) -> CanonPath {
        CanonPath(self.0.join(component))
    }

    pub fn strip_prefix(&self, base: &CanonPath) -> Option<&Path> {
        self.0.strip_prefix(&base.0).ok()
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }

    pub fn to_str(&self) -> Option<&str> {
        self.0.to_str()
    }

    pub fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        self.0.to_string_lossy()
    }

    pub fn exists(&self) -> bool {
        self.0.exists()
    }

    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }

    /// Derive a user-facing path by replacing the canonical root prefix
    /// with the original user-provided root. Falls back to `self` if
    /// the canonical root is not a prefix.
    pub fn to_user_path(&self, canon_root: &CanonPath, user_root: &UserPath) -> UserPath {
        if let Some(suffix) = self.strip_prefix(canon_root) {
            user_root.join(suffix)
        } else {
            UserPath::new(self.0.clone())
        }
    }
}

impl AsRef<Path> for CanonPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for CanonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

// ── UserPath ──

/// A user-provided path that has not been canonicalized.
///
/// Used at input edges: CLI arguments, config directories, session
/// persistence, find-file input text.  Call [`UserPath::canonicalize`]
/// to obtain a [`CanonPath`] for internal use.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserPath(PathBuf);

impl UserPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// Canonicalize this path.  Falls back to the original path if
    /// canonicalization fails (e.g. file does not exist yet).
    pub fn canonicalize(&self) -> CanonPath {
        let canonical = std::fs::canonicalize(&self.0).unwrap_or_else(|_| self.0.clone());
        CanonPath(canonical)
    }

    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn parent(&self) -> Option<UserPath> {
        self.0.parent().map(|p| UserPath(p.to_path_buf()))
    }

    pub fn join(&self, component: impl AsRef<Path>) -> UserPath {
        UserPath(self.0.join(component))
    }

    pub fn with_extension(&self, ext: impl AsRef<OsStr>) -> UserPath {
        UserPath(self.0.with_extension(ext))
    }

    pub fn push(&mut self, component: impl AsRef<Path>) {
        self.0.push(component);
    }

    pub fn components(&self) -> Components<'_> {
        self.0.components()
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }

    pub fn extension(&self) -> Option<&OsStr> {
        self.0.extension()
    }

    pub fn starts_with(&self, base: impl AsRef<Path>) -> bool {
        self.0.starts_with(base)
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }

    pub fn to_str(&self) -> Option<&str> {
        self.0.to_str()
    }

    pub fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        self.0.to_string_lossy()
    }

    pub fn exists(&self) -> bool {
        self.0.exists()
    }

    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }
}

impl From<PathBuf> for UserPath {
    fn from(p: PathBuf) -> Self {
        Self(p)
    }
}

impl From<String> for UserPath {
    fn from(s: String) -> Self {
        Self(PathBuf::from(s))
    }
}

impl From<&str> for UserPath {
    fn from(s: &str) -> Self {
        Self(PathBuf::from(s))
    }
}

impl AsRef<Path> for UserPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for UserPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[cfg(all(test, unix))]
mod resolve_chain_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn resolve_chain_no_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("regular");
        std::fs::write(&target, "x").unwrap();
        let chain = UserPath::new(target.clone()).resolve_chain();
        assert_eq!(chain.user.as_path(), target);
        assert!(chain.intermediaries.is_empty());
        // `resolved` is the canonical form (resolves symlinked parent
        // components like macOS's /var → /private/var).
        assert_eq!(
            chain.resolved.as_path(),
            std::fs::canonicalize(&target).unwrap()
        );
    }

    #[test]
    fn resolve_chain_single_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("profile");
        std::fs::write(&target, "x").unwrap();
        let link = tmp.path().join(".profile");
        symlink(&target, &link).unwrap();

        let chain = UserPath::new(link.clone()).resolve_chain();
        assert_eq!(chain.user.as_path(), link);
        // No intermediates — `link` points directly at the regular file.
        assert!(chain.intermediaries.is_empty());
        assert_eq!(
            chain.resolved.as_path(),
            std::fs::canonicalize(&link).unwrap()
        );
    }

    #[test]
    fn resolve_chain_two_symlinks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("end");
        std::fs::write(&target, "x").unwrap();
        let mid = tmp.path().join("middle");
        symlink(&target, &mid).unwrap();
        let head = tmp.path().join(".profile");
        symlink(&mid, &head).unwrap();

        let chain = UserPath::new(head.clone()).resolve_chain();
        assert_eq!(chain.user.as_path(), head);
        assert_eq!(chain.intermediaries.len(), 1);
        // Intermediate is `middle` (canonicalized).
        assert_eq!(
            chain.intermediaries[0].as_path(),
            std::fs::canonicalize(&mid).unwrap()
        );
        assert_eq!(
            chain.resolved.as_path(),
            std::fs::canonicalize(&head).unwrap()
        );
    }

    #[test]
    fn iter_paths_preserves_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("end");
        std::fs::write(&target, "x").unwrap();
        let head = tmp.path().join(".profile");
        symlink(&target, &head).unwrap();

        let chain = UserPath::new(head.clone()).resolve_chain();
        let names: Vec<&std::ffi::OsStr> =
            chain.iter_paths().filter_map(|p| p.file_name()).collect();
        // user first, then resolved.
        assert_eq!(names.first().unwrap(), &".profile");
        assert_eq!(names.last().unwrap(), &"end");
    }
}

// ── PathChain ──

/// The full symlink chain for a buffer's path.
///
/// Built once at buffer creation by [`UserPath::resolve_chain`]. Carries
/// the user-facing name (whatever was typed or clicked), the intermediate
/// symlink targets (zero or more), and the final canonical path.
///
/// Used for language detection: walking the chain in order lets a
/// well-known dotfile name like `.profile` win over its non-well-known
/// canonical target (e.g. `dotfiles/profile`). The first link that yields
/// a known language wins.
#[derive(Clone, Debug)]
pub struct PathChain {
    pub user: UserPath,
    pub intermediaries: VecDeque<CanonPath>,
    pub resolved: CanonPath,
}

impl PathChain {
    /// Trivial chain (no symlinks). Used for buffers whose path comes from
    /// internal drivers (gh PR, git status, LSP, file_search, session
    /// restore) where only a `CanonPath` is known.
    pub fn from_canon(canon: CanonPath) -> Self {
        Self {
            user: UserPath::new(canon.as_path()),
            intermediaries: VecDeque::new(),
            resolved: canon,
        }
    }

    /// Iterate the chain in order: user, intermediaries, resolved.
    /// The first link with a known extension or well-known filename
    /// determines the language.
    pub fn iter_paths(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.user.as_path())
            .chain(self.intermediaries.iter().map(|c| c.as_path()))
            .chain(std::iter::once(self.resolved.as_path()))
    }
}

impl UserPath {
    /// Walk the symlink chain forward. Each `read_link` step appends the
    /// target to `intermediaries`; the final non-symlink (or
    /// `canonicalize` fallback when the path doesn't exist) becomes
    /// `resolved`. When `self` is not a symlink, `intermediaries` is
    /// empty and `resolved == self.canonicalize()`.
    pub fn resolve_chain(&self) -> PathChain {
        let chain = symlink_chain(self.as_path());
        let resolved = self.canonicalize();
        // chain[0] is `self`; intermediates are chain[1..n-1]; the final
        // entry corresponds to the target — but we use `resolved` for the
        // canonical form regardless.
        let intermediaries: VecDeque<CanonPath> = chain
            .iter()
            .skip(1)
            .take(chain.len().saturating_sub(2))
            .map(|p| UserPath::new(p.clone()).canonicalize())
            .collect();
        PathChain {
            user: self.clone(),
            intermediaries,
            resolved,
        }
    }
}
