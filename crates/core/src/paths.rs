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
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, drv::Input)]
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

impl UserPath {
    /// Walk the symlink chain starting from this path. Returns a
    /// [`PathChain`] with the user-typed path at the head,
    /// symlink intermediates in between, and the final canonical
    /// path at the tail.
    ///
    /// Used by language detection — legacy led routes
    /// `Language::from_chain` over every link in the chain
    /// (user → intermediaries → resolved), first match wins. That
    /// way `foo.rs` symlinked to `bar` still detects Rust off the
    /// user-typed name, and `.profile` → `dotfiles/profile` still
    /// detects Bash.
    ///
    /// Failed reads at any step collapse the remainder to the
    /// canonicalized path and return early — on a broken link the
    /// chain still yields a usable tail.
    pub fn resolve_chain(&self) -> PathChain {
        let mut intermediates: Vec<PathBuf> = Vec::new();
        let mut cursor = self.0.clone();
        // Bounded to avoid runaway on a cyclic symlink (POSIX's
        // own link-chain limit is usually 40).
        for _ in 0..40 {
            match std::fs::read_link(&cursor) {
                Ok(target) => {
                    // Symlink targets may be relative to the
                    // symlink's parent dir — resolve as such.
                    let next = if target.is_absolute() {
                        target
                    } else if let Some(parent) = cursor.parent() {
                        parent.join(target)
                    } else {
                        target
                    };
                    intermediates.push(next.clone());
                    cursor = next;
                }
                Err(_) => break,
            }
        }
        let resolved = std::fs::canonicalize(&self.0)
            .or_else(|_| std::fs::canonicalize(&cursor))
            .unwrap_or_else(|_| cursor.clone());
        PathChain {
            user: self.clone(),
            intermediates,
            resolved: CanonPath(resolved),
        }
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
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, drv::Input,
    serde::Serialize, serde::Deserialize,
)]
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

/// The full symlink resolution story for one user-typed path.
///
/// Legacy led uses this triple for language detection — and for
/// the rewrite's mirror, see
/// [`led_state_syntax::Language::from_chain`]. Walks in order
/// `user → intermediates → resolved`; the first entry whose
/// filename tells us the language wins. Keeping the user-typed
/// name at the head means `foo.rs` symlinked to `bar` still
/// renders as Rust, not as "plain text".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChain {
    pub user: UserPath,
    /// Symlink targets between `user` and `resolved`. Empty when
    /// the user-typed path is not a symlink. Each entry is a
    /// `PathBuf` (not a newtype) because the intermediates are
    /// themselves raw fs paths — they're neither the canonical id
    /// nor user-typed.
    pub intermediates: Vec<PathBuf>,
    pub resolved: CanonPath,
}

impl PathChain {
    /// Iterate every path in the chain from head to tail —
    /// `user`, each intermediate symlink target, then `resolved`.
    /// The order matters: callers that pick the "first match
    /// wins" (e.g. language detection) rely on the user-typed
    /// name being tried before any symlink target.
    pub fn iter_paths(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.user.as_path())
            .chain(self.intermediates.iter().map(|p| p.as_path()))
            .chain(std::iter::once(self.resolved.as_path()))
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

    #[test]
    fn resolve_chain_yields_only_self_for_non_symlink() {
        use std::fs;
        let base = std::env::temp_dir().join(format!(
            "led-pathchain-test.{}.nonlink",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let real = base.join("file.rs");
        fs::write(&real, b"").unwrap();

        let user = UserPath::new(&real);
        let chain = user.resolve_chain();
        assert!(chain.intermediates.is_empty(), "got {:?}", chain.intermediates);
        assert_eq!(chain.user.as_path(), &real);
        // Canonical resolves symlinks in parents (e.g. macOS
        // /var → /private/var) so we just assert the basename.
        assert_eq!(chain.resolved.as_path().file_name(), Some(OsStr::new("file.rs")));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_chain_resolved_matches_canonicalize_for_real_file() {
        // Regression: the rewrite's active-tab browser marker
        // depends on `tab.path == browser_entry.path`. Tabs open
        // via `UserPath::resolve_chain().resolved`; browser
        // entries open via `UserPath::canonicalize()`. Both paths
        // must produce byte-identical `CanonPath` for the same
        // file or the marker never fires.
        use std::fs;
        let base = std::env::temp_dir().join(format!(
            "led-pathchain-test.{}.canoneq",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let real = base.join("main.rs");
        fs::write(&real, b"").unwrap();

        // Tab-path style: resolve_chain → .resolved.
        let chain = UserPath::new(&real).resolve_chain();
        let tab_canon = chain.resolved.clone();

        // Browser-path style: enumerate via readdir → canonicalize.
        let browser_canon = fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name() == "main.rs")
            .map(|e| UserPath::new(e.path()).canonicalize())
            .expect("main.rs in the readdir");

        assert_eq!(
            tab_canon, browser_canon,
            "tab + browser must produce identical CanonPath for the same file"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_chain_walks_one_symlink_hop() {
        use std::fs;
        let base = std::env::temp_dir().join(format!(
            "led-pathchain-test.{}.onelink",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let real = base.join("bar");
        fs::write(&real, b"").unwrap();
        let link = base.join("foo.rs");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let user = UserPath::new(&link);
        let chain = user.resolve_chain();
        assert_eq!(chain.user.as_path(), &link);
        assert_eq!(chain.intermediates.len(), 1, "got {:?}", chain.intermediates);
        assert_eq!(
            chain.intermediates[0].file_name(),
            Some(OsStr::new("bar"))
        );
        assert_eq!(
            chain.resolved.as_path().file_name(),
            Some(OsStr::new("bar"))
        );
        // iter_paths hits head, intermediate, then tail.
        let names: Vec<_> = chain
            .iter_paths()
            .map(|p| p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string())
            .collect();
        assert_eq!(names, vec!["foo.rs", "bar", "bar"]);

        let _ = fs::remove_dir_all(&base);
    }
}
