use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ── LanguageId ──

/// Language identifier. Copy-type enum used throughout the editor to identify
/// which language a file belongs to, and which LSP server to use.
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
        }
    }

    /// Map a file extension to a language. Returns `None` for unknown extensions.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "ts" | "tsx" => Some(Self::TypeScript),
            "js" | "jsx" => Some(Self::JavaScript),
            "py" => Some(Self::Python),
            "c" | "h" => Some(Self::C),
            "cpp" | "hpp" | "cc" | "cxx" => Some(Self::Cpp),
            "swift" => Some(Self::Swift),
            "toml" => Some(Self::Toml),
            "json" => Some(Self::Json),
            "sh" | "bash" => Some(Self::Bash),
            _ => None,
        }
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
    root: PathBuf,
    language: LanguageId,
}

impl LspContextId {
    pub fn new(root: PathBuf, language: LanguageId) -> Self {
        Self(Arc::new(LspContextIdData { root, language }))
    }

    pub fn root(&self) -> &Path {
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
