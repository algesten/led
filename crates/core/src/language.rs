use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::path::{CanonPath, PathChain};

// ── LanguageId ──

/// Language identifier. Copy-type enum used throughout the editor to identify
/// which language a file belongs to, and which LSP server to use.
///
/// Covers every language the syntax crate can highlight, even those without
/// a real LSP server (e.g. Markdown, Make). LSP server lookup just returns
/// `None` for those.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum LanguageId {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    C,
    Cpp,
    Swift,
    Toml,
    Json,
    Bash,
    Ruby,
    Markdown,
    Make,
}

impl LanguageId {
    /// The string sent in LSP protocol `textDocument/didOpen` messages.
    pub fn as_lsp_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Swift => "swift",
            Self::Toml => "toml",
            Self::Json => "json",
            Self::Bash => "shellscript",
            Self::Ruby => "ruby",
            Self::Markdown => "markdown",
            Self::Make => "make",
        }
    }

    /// Map a file extension to a language. Returns `None` for unknown extensions.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" | "mjs" => Some(Self::JavaScript),
            "py" => Some(Self::Python),
            "c" | "h" => Some(Self::C),
            "cpp" | "hpp" | "cc" | "cxx" | "hxx" => Some(Self::Cpp),
            "swift" => Some(Self::Swift),
            "toml" => Some(Self::Toml),
            "json" => Some(Self::Json),
            "sh" | "bash" => Some(Self::Bash),
            "rb" => Some(Self::Ruby),
            "md" | "markdown" => Some(Self::Markdown),
            "mk" => Some(Self::Make),
            _ => None,
        }
    }

    /// Map a well-known filename (e.g. `.profile`, `Gemfile`, `Makefile`)
    /// to a language. Private — only [`from_chain`] uses it.
    fn from_filename(name: &str) -> Option<Self> {
        filename_to_extension(name).and_then(Self::from_extension)
    }

    /// Walk a path chain and return the first link that yields a known
    /// language. Each link is checked first by extension, then by
    /// well-known filename. The chain order (user, intermediaries,
    /// resolved) means a user-typed dotfile name like `.profile` wins
    /// over its non-well-known canonical target.
    pub fn from_chain(chain: &PathChain) -> Option<Self> {
        chain.iter_paths().find_map(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .and_then(Self::from_extension)
                .or_else(|| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .and_then(Self::from_filename)
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::UserPath;
    use std::collections::VecDeque;
    use std::path::PathBuf;

    fn chain_of(user: &str, resolved: &str) -> PathChain {
        let user = UserPath::new(PathBuf::from(user));
        // Build a degenerate chain (no intermediates) for tests that just
        // want to verify which name wins between user and resolved.
        let resolved_canon = UserPath::new(PathBuf::from(resolved)).canonicalize();
        PathChain {
            user,
            intermediaries: VecDeque::new(),
            resolved: resolved_canon,
        }
    }

    #[test]
    fn from_chain_picks_extension_first() {
        let chain = chain_of("/tmp/foo.rs", "/tmp/foo.rs");
        assert_eq!(LanguageId::from_chain(&chain), Some(LanguageId::Rust));
    }

    #[test]
    fn from_chain_picks_well_known_filename_when_no_extension() {
        let chain = chain_of("/home/x/.profile", "/home/x/.profile");
        assert_eq!(LanguageId::from_chain(&chain), Some(LanguageId::Bash));
    }

    #[test]
    fn from_chain_user_name_wins_over_resolved() {
        // The user typed `.profile` (well-known); resolved is `profile`
        // (no extension, not in the well-known list). Walking the chain
        // in order gives `.profile` first → Bash.
        let chain = chain_of("/home/x/.profile", "/home/x/dotfiles/profile");
        assert_eq!(LanguageId::from_chain(&chain), Some(LanguageId::Bash));
    }

    #[test]
    fn from_chain_returns_none_for_unknown() {
        let chain = chain_of("/tmp/randomfile", "/tmp/randomfile");
        assert_eq!(LanguageId::from_chain(&chain), None);
    }

    #[test]
    fn filename_to_extension_covers_well_known_names() {
        assert_eq!(filename_to_extension(".profile"), Some("sh"));
        assert_eq!(filename_to_extension(".bashrc"), Some("sh"));
        assert_eq!(filename_to_extension("Makefile"), Some("mk"));
        assert_eq!(filename_to_extension("Gemfile"), Some("rb"));
        assert_eq!(filename_to_extension("Pipfile"), Some("toml"));
        assert_eq!(filename_to_extension(".babelrc"), Some("json"));
        assert_eq!(filename_to_extension("unknown"), None);
    }
}

/// Map a well-known filename to a conventional file extension.
///
/// Single source of truth for the syntax crate's `lang_for_filename` and
/// the LSP layer's chain-based detection. Both delegate here.
pub fn filename_to_extension(name: &str) -> Option<&'static str> {
    match name {
        "Makefile" | "makefile" | "GNUmakefile" | "BSDmakefile" => Some("mk"),
        "Gemfile" | "Rakefile" | "Vagrantfile" | "Guardfile" | "Podfile" | "Capfile"
        | "Brewfile" | "Thorfile" | "Dangerfile" | "Berksfile" | "Puppetfile" | "Steepfile"
        | "Fastfile" | "Appfile" | "Matchfile" | "Deliverfile" | "Snapfile" | "Scanfile"
        | "Gymfile" => Some("rb"),
        ".bashrc" | ".bash_profile" | ".bash_logout" | ".bash_aliases" | ".profile" | ".envrc"
        | "PKGBUILD" => Some("sh"),
        "SConstruct" | "SConscript" | "Snakefile" | "wscript" => Some("py"),
        "Pipfile" => Some("toml"),
        ".babelrc" => Some("json"),
        _ => None,
    }
}

// ── LspContextId ──

/// Identifies an LSP server instance: a (root, language) pair.
///
/// Arc-backed for cheap cloning. Hash/Eq delegate to inner data so this
/// can be used as a HashMap key.
#[derive(Debug)]
pub struct LspContextId(Arc<LspContextIdData>);

#[derive(Debug, Hash, Eq, PartialEq)]
struct LspContextIdData {
    root: CanonPath,
    language: LanguageId,
}

impl LspContextId {
    pub fn new(root: CanonPath, language: LanguageId) -> Self {
        Self(Arc::new(LspContextIdData { root, language }))
    }

    pub fn root(&self) -> &CanonPath {
        &self.0.root
    }

    pub fn language(&self) -> LanguageId {
        self.0.language
    }
}

impl Clone for LspContextId {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl Hash for LspContextId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl PartialEq for LspContextId {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for LspContextId {}
