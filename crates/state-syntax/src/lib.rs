//! Per-buffer syntax state for M15 highlighting.
//!
//! `SyntaxState` carries the most recent `tree-sitter` tree + the
//! token spans extracted via the language's highlight query, plus
//! the buffer version the parse was run against. The driver
//! (`driver-syntax/`) produces `SyntaxOut` values that replace the
//! state when they arrive; dispatch queues parse requests after
//! every edit that changes the rope.
//!
//! Tokens hold 0-indexed char offsets into the rope. Between parse
//! completions the runtime rebases these through the edit log so
//! typing doesn't flicker — the token colours smear forward through
//! the user's edits until the authoritative reparse lands.

use std::sync::Arc;

use imbl::HashMap;
use led_core::{CanonPath, PathChain};
use tree_sitter::Tree;

/// Programming language identity. One per grammar the rewrite
/// supports. Extended as grammars come online; unknown extensions
/// map to `None` (no highlighting, falls back to plain body text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Bash,
    Markdown,
    Json,
    Toml,
    C,
    Cpp,
    Ruby,
    Swift,
    Make,
}

impl Language {
    /// Best-effort language detection from one path: try the
    /// extension, fall back to the well-known-filename table
    /// (`Makefile`, `Gemfile`, `.bashrc`, …). Prefer
    /// [`Language::from_chain`] when a [`PathChain`] is available —
    /// that walks the user-typed name first and handles the
    /// symlink case correctly.
    pub fn from_path(path: &CanonPath) -> Option<Self> {
        Self::from_fs_path(path.as_path())
    }

    /// Walk a [`PathChain`] in its canonical order
    /// (`user → intermediates → resolved`) and return the language
    /// of the first path that resolves — matches legacy led's
    /// `LanguageId::from_chain`. This makes user-typed names win
    /// over symlink targets: `foo.rs` → `bar` still renders as
    /// Rust even though the canonical tail has no extension.
    pub fn from_chain(chain: &PathChain) -> Option<Self> {
        chain.iter_paths().find_map(Self::from_fs_path)
    }

    fn from_fs_path(path: &std::path::Path) -> Option<Self> {
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && let Some(lang) = Self::from_extension(ext)
        {
            return Some(lang);
        }
        let filename = path.file_name().and_then(|n| n.to_str())?;
        Self::from_filename(filename)
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "rs" => Self::Rust,
            "ts" | "tsx" => Self::TypeScript,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "py" | "pyi" => Self::Python,
            "sh" | "bash" | "zsh" => Self::Bash,
            "md" | "markdown" => Self::Markdown,
            "json" => Self::Json,
            "toml" => Self::Toml,
            "c" | "h" => Self::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hh" => Self::Cpp,
            "rb" => Self::Ruby,
            "swift" => Self::Swift,
            "makefile" | "mk" => Self::Make,
            _ => return None,
        })
    }

    /// Well-known filenames that stand in for an extension. Ports
    /// legacy led's `filename_to_extension` table (crates/core/src/language.rs
    /// on main, circa line 158) — case-sensitive comparisons
    /// because most of these are conventional spellings users
    /// expect to match (`Makefile`, not `makefile`).
    pub fn from_filename(name: &str) -> Option<Self> {
        Some(match name {
            // Make
            "Makefile" | "makefile" | "GNUmakefile" | "BSDmakefile" => Self::Make,
            // Ruby project files
            "Gemfile" | "Rakefile" | "Guardfile" | "Capfile" | "Vagrantfile" | "Podfile"
            | "Brewfile" | "Fastfile" => Self::Ruby,
            // Bash dotfiles + scripts-with-no-ext
            ".bashrc" | ".bash_profile" | ".bash_aliases" | ".profile" | ".zshrc"
            | ".zprofile" | ".envrc" | "PKGBUILD" | ".bash_logout" => Self::Bash,
            // Python scripts-with-no-ext
            "SConstruct" | "SConscript" | "Snakefile" | "wscript" => Self::Python,
            // JSON-ish dotfiles
            ".babelrc" | ".eslintrc" | ".prettierrc" => Self::Json,
            // TOML-ish
            "Pipfile" | "Cargo.lock" => Self::Toml,
            _ => return None,
        })
    }
}

/// Semantic kind of a token, mapped from tree-sitter highlight
/// capture names. The painter looks each variant up in the theme's
/// `[syntax]` section.
///
/// Broader than legacy's set so a single `TokenKind` handles the
/// typical captures across languages (e.g. `keyword.return` and
/// `keyword.function` both become `Keyword`). Finer-grained
/// distinctions (e.g. builtin vs user type) can be added later
/// without breaking callers — unknown captures fall to `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Keyword,
    Type,
    Function,
    String,
    Number,
    Boolean,
    Comment,
    Operator,
    Punctuation,
    Variable,
    Property,
    Attribute,
    Tag,
    Label,
    Constant,
    Escape,
    /// Unclassified — renders with the default body style.
    Default,
}

/// One contiguous run of characters in a buffer that share a
/// single `TokenKind`. Offsets are 0-indexed char positions into
/// the rope. `char_end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSpan {
    pub char_start: usize,
    pub char_end: usize,
    pub kind: TokenKind,
}

/// What the syntax driver produces per file. Echoes the request's
/// version so the runtime can drop stale completions — if the
/// rope moved past this version while the parser was busy, the
/// tokens don't apply to the current content.
#[derive(Debug, Clone)]
pub struct SyntaxOut {
    pub path: CanonPath,
    pub version: u64,
    pub language: Language,
    pub tree: Arc<Tree>,
    pub tokens: Arc<Vec<TokenSpan>>,
    /// Rope the parse was performed against. The runtime stores
    /// this on `SyntaxState.tree_rope` so the next parse can
    /// ship it as `SyntaxCmd.prev_rope` and tree-sitter can run
    /// incremental parsing.
    pub tree_rope: Arc<ropey::Rope>,
}

/// Per-buffer syntax state. `None` in the `Atoms.syntax` map when
/// the runtime hasn't seen a buffer yet OR the language couldn't
/// be identified.
#[derive(Debug, Clone)]
pub struct SyntaxState {
    pub language: Language,
    /// Most recent tree the parser returned. `None` until the
    /// first parse completes.
    pub tree: Option<Arc<Tree>>,
    /// Rope snapshot `tree` was parsed from. Shipped back as
    /// `SyntaxCmd.prev_rope` on the next dispatch so the worker
    /// can resolve each edit's byte offset against the correct
    /// coordinate space. `None` in lock-step with `tree`.
    pub tree_rope: Option<Arc<ropey::Rope>>,
    /// Tokens extracted from `tree` via the language's highlight
    /// query. Empty until the first parse completes.
    pub tokens: Arc<Vec<TokenSpan>>,
    /// The rope version that produced `tree` + `tokens`. The
    /// runtime rebases token positions through any edits between
    /// this version and the buffer's current version before the
    /// painter consumes them.
    pub version: u64,
    /// The buffer version currently being parsed by the worker, if
    /// any. Set by the runtime when a `SyntaxCmd` is dispatched,
    /// cleared when a `SyntaxOut` at or past that version arrives.
    /// Prevents re-queuing the same parse on every main-loop tick
    /// while the worker is still busy. `None` means nothing is in
    /// flight — queue a fresh parse if the buffer is ahead of
    /// `version`.
    pub in_flight_version: Option<u64>,
    /// `history.applied_ops().count()` at the moment the parse
    /// request was dispatched. The render pipeline subtracts this
    /// from the current count to know which edit ops to rebase
    /// through before painting. Advances in lock-step with
    /// `version` on each applied `SyntaxOut`.
    pub applied_at_parse: usize,
    /// `history.applied_ops().count()` snapshotted at dispatch
    /// time; pinned here while the parse is in flight and copied
    /// to `applied_at_parse` when the matching completion arrives.
    pub in_flight_applied: Option<usize>,
}

impl SyntaxState {
    pub fn new(language: Language) -> Self {
        Self {
            language,
            tree: None,
            tree_rope: None,
            tokens: Arc::new(Vec::new()),
            version: 0,
            in_flight_version: None,
            applied_at_parse: 0,
            in_flight_applied: None,
        }
    }
}

/// The `Atoms.syntax` source — one `SyntaxState` per loaded
/// buffer, keyed by canonical path. `imbl::HashMap` for the usual
/// pointer-clone cache-friendliness in memos.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyntaxStates {
    pub by_path: HashMap<CanonPath, SyntaxState>,
}

// Manual PartialEq because `SyntaxState` holds an `Arc<Tree>` and
// `tree_sitter::Tree` doesn't implement Eq. Pointer-eq via Arc is
// the right semantic for drv memo invalidation anyway — same tree
// means same tokens.
impl PartialEq for SyntaxState {
    fn eq(&self, other: &Self) -> bool {
        self.language == other.language
            && self.version == other.version
            && match (&self.tree, &other.tree) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
            && Arc::ptr_eq(&self.tokens, &other.tokens)
    }
}

/// Rebase a token list through the edit ops applied since it was
/// produced. Inserts widen any span that contains the insertion
/// point and push later spans right; deletes narrow or drop spans
/// depending on overlap. Positions after a delete shift left.
///
/// Used by the runtime between a user edit and the arrival of a
/// fresh parse — keeps the visible colour "smear" in sync with
/// the rope so typing inside an identifier stays that colour.
pub fn rebase_tokens(
    tokens: &[TokenSpan],
    ops: impl IntoIterator<Item = RebaseOp>,
) -> Vec<TokenSpan> {
    let mut out: Vec<TokenSpan> = tokens.to_vec();
    for op in ops {
        match op {
            RebaseOp::Insert { at, len } => {
                for t in out.iter_mut() {
                    if t.char_start >= at {
                        t.char_start += len;
                        t.char_end += len;
                    } else if t.char_end > at {
                        // Edit inside the span — stretch it.
                        t.char_end += len;
                    }
                }
            }
            RebaseOp::Delete { at, len } => {
                let end = at + len;
                out.retain(|t| {
                    // Drop spans fully contained in the deleted range.
                    !(t.char_start >= at && t.char_end <= end)
                });
                for t in out.iter_mut() {
                    if t.char_start >= end {
                        t.char_start -= len;
                        t.char_end -= len;
                    } else if t.char_end <= at {
                        // Entirely before the delete — no change.
                    } else if t.char_start < at && t.char_end > end {
                        // Delete sits inside the span — shrink it.
                        t.char_end -= len;
                    } else if t.char_start < at {
                        // Span overlaps from the left; truncate.
                        t.char_end = at;
                    } else {
                        // Span overlaps from the right; truncate +
                        // shift to the delete's start.
                        let kept = t.char_end.saturating_sub(end);
                        t.char_start = at;
                        t.char_end = at + kept;
                    }
                }
            }
        }
    }
    out
}

/// Narrow rebase op — simpler than reusing the full `EditOp` from
/// buffer-edits because we only need start + length for each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseOp {
    Insert { at: usize, len: usize },
    Delete { at: usize, len: usize },
}

/// Derive the sequence of `RebaseOp`s from an
/// [`led_state_buffer_edits::History`]'s op log between two
/// versions. Walks `applied_ops()` and emits an op per applied
/// edit; callers that know the exact boundary pass the returned
/// iterator through `rebase_tokens`.
pub fn rebase_ops_since_version(
    history: &led_state_buffer_edits::History,
    since_applied_count: usize,
) -> Vec<RebaseOp> {
    history
        .applied_ops()
        .skip(since_applied_count)
        .map(|op| match op {
            led_state_buffer_edits::EditOp::Insert { at, text } => RebaseOp::Insert {
                at: *at,
                len: text.chars().count(),
            },
            led_state_buffer_edits::EditOp::Delete { at, text } => RebaseOp::Delete {
                at: *at,
                len: text.chars().count(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(s: usize, e: usize, k: TokenKind) -> TokenSpan {
        TokenSpan {
            char_start: s,
            char_end: e,
            kind: k,
        }
    }

    #[test]
    fn rebase_insert_inside_span_extends_it() {
        let tokens = vec![span(5, 10, TokenKind::Keyword)];
        let out = rebase_tokens(&tokens, [RebaseOp::Insert { at: 7, len: 2 }]);
        assert_eq!(out, vec![span(5, 12, TokenKind::Keyword)]);
    }

    #[test]
    fn rebase_insert_before_span_pushes_it_right() {
        let tokens = vec![span(5, 10, TokenKind::Keyword)];
        let out = rebase_tokens(&tokens, [RebaseOp::Insert { at: 0, len: 3 }]);
        assert_eq!(out, vec![span(8, 13, TokenKind::Keyword)]);
    }

    #[test]
    fn rebase_delete_inside_span_shrinks_it() {
        let tokens = vec![span(5, 15, TokenKind::String)];
        let out = rebase_tokens(&tokens, [RebaseOp::Delete { at: 8, len: 3 }]);
        assert_eq!(out, vec![span(5, 12, TokenKind::String)]);
    }

    fn canon(rel: &str) -> CanonPath {
        led_core::UserPath::new(rel).canonicalize()
    }

    #[test]
    fn language_from_path_maps_common_extensions() {
        assert_eq!(Language::from_path(&canon("main.rs")), Some(Language::Rust));
        assert_eq!(
            Language::from_path(&canon("app.tsx")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::from_path(&canon("widget.jsx")),
            Some(Language::JavaScript)
        );
        assert_eq!(
            Language::from_path(&canon("script.py")),
            Some(Language::Python)
        );
        assert_eq!(Language::from_path(&canon("setup.toml")), Some(Language::Toml));
        assert_eq!(Language::from_path(&canon("README.md")), Some(Language::Markdown));
    }

    #[test]
    fn language_from_path_returns_none_for_unknown_extensions() {
        assert_eq!(Language::from_path(&canon("notes.unknownext")), None);
        assert_eq!(Language::from_path(&canon("no-extension")), None);
    }

    #[test]
    fn well_known_filenames_detect_without_extension() {
        assert_eq!(Language::from_filename("Makefile"), Some(Language::Make));
        assert_eq!(Language::from_filename("GNUmakefile"), Some(Language::Make));
        assert_eq!(Language::from_filename("Gemfile"), Some(Language::Ruby));
        assert_eq!(Language::from_filename("Rakefile"), Some(Language::Ruby));
        assert_eq!(Language::from_filename(".bashrc"), Some(Language::Bash));
        assert_eq!(Language::from_filename(".profile"), Some(Language::Bash));
        assert_eq!(Language::from_filename("PKGBUILD"), Some(Language::Bash));
        assert_eq!(Language::from_filename("Snakefile"), Some(Language::Python));
        assert_eq!(Language::from_filename("Pipfile"), Some(Language::Toml));
        assert_eq!(Language::from_filename("notafile.xyz"), None);
    }

    #[test]
    fn language_from_path_recognises_well_known_basename_without_ext() {
        // Path has no extension → fall through to filename table.
        assert_eq!(Language::from_path(&canon("/etc/Makefile")), Some(Language::Make));
        assert_eq!(Language::from_path(&canon("/home/u/.bashrc")), Some(Language::Bash));
        // `.profile` is a Rust `Path::extension` = None case (starts
        // with `.`, no other `.` within). Must route via filename.
        assert_eq!(Language::from_path(&canon("/home/u/.profile")), Some(Language::Bash));
    }

    #[test]
    fn from_chain_prefers_user_path_extension_over_symlink_target() {
        // User typed `foo.rs` which symlinks to `bar` (no ext).
        // Legacy + rewrite: detect Rust from the user name.
        use led_core::PathChain;
        let chain = PathChain {
            user: led_core::UserPath::new("foo.rs"),
            intermediates: Vec::new(),
            resolved: canon("bar"),
        };
        assert_eq!(Language::from_chain(&chain), Some(Language::Rust));
    }

    #[test]
    fn from_chain_falls_through_when_user_has_no_match() {
        // `local_script` → `/usr/local/bin/python3.11` — user path
        // has nothing useful, resolved path's extension matches.
        use led_core::PathChain;
        let chain = PathChain {
            user: led_core::UserPath::new("local_script"),
            intermediates: Vec::new(),
            resolved: canon("/usr/bin/python3.py"),
        };
        assert_eq!(Language::from_chain(&chain), Some(Language::Python));
    }

    #[test]
    fn from_chain_intermediate_wins_when_user_and_tail_are_mute() {
        // `edit` → `Makefile` → `/abs/Makefile.real` — neither
        // the user nor the resolved path has an ext, but an
        // intermediate has a well-known filename.
        use led_core::PathChain;
        let chain = PathChain {
            user: led_core::UserPath::new("edit"),
            intermediates: vec![std::path::PathBuf::from("/abs/Makefile")],
            resolved: canon("/abs/Makefile.real"),
        };
        assert_eq!(Language::from_chain(&chain), Some(Language::Make));
    }

    #[test]
    fn rebase_delete_fully_covering_span_drops_it() {
        let tokens = vec![
            span(5, 10, TokenKind::Keyword),
            span(15, 20, TokenKind::String),
        ];
        let out = rebase_tokens(&tokens, [RebaseOp::Delete { at: 3, len: 10 }]);
        // First span consumed; second shifts left by 10.
        assert_eq!(out, vec![span(5, 10, TokenKind::String)]);
    }
}
