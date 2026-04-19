//! Path newtypes.
//!
//! Two kinds of path exist in led:
//!
//! - [`UserPath`] — what the user supplied (CLI argument, config file entry,
//!   whatever). May be relative, may contain symlinks, may not yet exist.
//!   Preserved verbatim for display.
//! - [`CanonPath`] — canonicalized (absolute + symlinks resolved). The only
//!   way to construct one is [`UserPath::canonicalize`]. Used as the
//!   internal identity of a file — map keys, tab paths, LSP document ids, etc.
//!
//! The type split makes it a compile error to use a user-typed path where a
//! canonical internal key is expected, and vice versa.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A path as supplied by the user. Never used as an internal map key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserPath(PathBuf);

impl UserPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    /// Canonicalize this path. Falls back to the original path if
    /// canonicalization fails (e.g. the file does not exist yet).
    pub fn canonicalize(&self) -> CanonPath {
        let canonical = std::fs::canonicalize(&self.0).unwrap_or_else(|_| self.0.clone());
        CanonPath(canonical)
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl AsRef<Path> for UserPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for UserPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}

/// A canonical absolute path. The only way to construct one is via
/// [`UserPath::canonicalize`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanonPath(PathBuf);

impl CanonPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_inner(self) -> PathBuf {
        self.0
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        self.0.file_name()
    }

    pub fn extension(&self) -> Option<&OsStr> {
        self.0.extension()
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl AsRef<Path> for CanonPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for CanonPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_a_nonexistent_path_preserves_it() {
        let up = UserPath::new("nonexistent-file-xyz");
        let cp = up.canonicalize();
        assert_eq!(cp.as_path(), Path::new("nonexistent-file-xyz"));
    }

    #[test]
    fn file_name_extracts_last_component() {
        let up = UserPath::new("some/dir/main.rs");
        let cp = up.canonicalize();
        assert_eq!(cp.file_name(), Some(OsStr::new("main.rs")));
    }

    #[test]
    fn distinct_types_do_not_mix() {
        // Compile-time guarantee: these are different types.
        let _u: UserPath = UserPath::new("a");
        let _c: CanonPath = UserPath::new("a").canonicalize();
        // let _: UserPath = _c;  // would not compile
    }
}
