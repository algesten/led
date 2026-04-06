use std::ffi::OsStr;
use std::fmt;
use std::path::{Components, Path, PathBuf};

use serde::{Deserialize, Serialize};

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
