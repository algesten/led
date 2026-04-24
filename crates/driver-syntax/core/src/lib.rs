//! Sync core of the tree-sitter syntax driver.
//!
//! The runtime queues a [`SyntaxCmd`] per buffer whenever the rope
//! changes; the native worker parses and posts a [`SyntaxOut`]
//! back. Drop-stale-requests is handled on both sides: the runtime
//! ignores completions whose `version` is older than the current
//! rope version, and the worker coalesces queued commands by
//! keeping only the latest per path.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::CanonPath;
use led_state_syntax::{Language, SyntaxOut};
use ropey::Rope;
use tree_sitter::Tree;

/// One parse request. When `prev_tree` and `prev_rope` are
/// both present, the worker runs tree-sitter's incremental
/// parse: it derives a single `RopeDiff` from
/// `(prev_rope, rope)`, converts it to one `InputEdit`,
/// calls `tree.edit(...)`, then
/// `parser.parse(&new_bytes, Some(&edited_tree))`. Without
/// them (first parse, or previous parse errored), it falls
/// back to `parser.parse(&new_bytes, None)` — a full parse.
///
/// Deriving the diff from the two ropes rather than tracking
/// a history counter keeps the tree a pure function of the
/// current rope atom: undo/redo that shrinks or reshuffles
/// the op log can't put the tree and the source bytes out
/// of sync.
///
/// `rope` and `prev_rope` are `Arc<Rope>` so shipping a request
/// is a pointer clone; the worker reads them without copying.
#[derive(Debug, Clone)]
pub struct SyntaxCmd {
    pub path: CanonPath,
    pub version: u64,
    pub rope: Arc<Rope>,
    pub language: Language,
    pub prev_tree: Option<Arc<Tree>>,
    /// Rope snapshot the `prev_tree` was parsed from. The
    /// worker diffs this against `rope` via
    /// [`RopeDiff::between`] to derive tree-sitter's
    /// `InputEdit`. `None` (or a mismatched tree) → full parse.
    pub prev_rope: Option<Arc<Rope>>,
}

// `RopeDiff` lives in `led-state-syntax` — it's the shape both
// the driver worker and the runtime's rebase path consume.
pub use led_state_syntax::RopeDiff;

/// Driver-scoped trace hook. The runtime's top-level Trace
/// delegates here via an adapter, matching every other driver.
pub trait Trace: Send + Sync {
    fn syntax_parse_start(&self, path: &CanonPath, version: u64, language: Language);
    fn syntax_parse_done(&self, path: &CanonPath, version: u64, ok: bool);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn syntax_parse_start(&self, _: &CanonPath, _: u64, _: Language) {}
    fn syntax_parse_done(&self, _: &CanonPath, _: u64, _: bool) {}
}

/// Main-loop-facing half of the driver. Holds the `Sender` for
/// commands and the `Receiver` for completions.
pub struct SyntaxDriver {
    tx: Sender<SyntaxCmd>,
    rx: Receiver<SyntaxOut>,
    trace: Arc<dyn Trace>,
}

impl SyntaxDriver {
    pub fn new(
        tx: Sender<SyntaxCmd>,
        rx: Receiver<SyntaxOut>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self { tx, rx, trace }
    }

    /// Ship each cmd to the worker. One `syntax_parse_start`
    /// trace per dispatched request.
    pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a SyntaxCmd>) {
        for cmd in cmds {
            self.trace.syntax_parse_start(&cmd.path, cmd.version, cmd.language);
            let _ = self.tx.send(cmd.clone());
        }
    }

    /// Drain ready completions. Caller version-gates them.
    pub fn process(&self) -> Vec<SyntaxOut> {
        let mut out = Vec::new();
        while let Ok(done) = self.rx.try_recv() {
            self.trace.syntax_parse_done(&done.path, done.version, true);
            out.push(done);
        }
        out
    }
}
