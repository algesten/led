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
use led_core::CanonPath;
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
    /// Best-effort language detection from a path's extension.
    /// Legacy also checks shebang / modeline — those are deferred.
    pub fn from_path(path: &CanonPath) -> Option<Self> {
        let ext = path.as_path().extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
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
