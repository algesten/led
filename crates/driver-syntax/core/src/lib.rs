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

/// One parse request. When `prev_tree`, `prev_rope`, and
/// `edits_since_prev` are all present, the worker runs
/// tree-sitter's incremental parse: it replays each edit
/// against a mutable clone of `prev_rope` to derive the byte +
/// point positions tree-sitter needs, calls `tree.edit(...)` for
/// each, then `parser.parse(&new_bytes, Some(&edited_tree))`.
/// Without the extras (e.g. first parse of a buffer), it falls
/// back to `parser.parse(&new_bytes, None)` — a full parse.
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
    /// Rope snapshot the `prev_tree` was parsed from. Used by
    /// the worker to map each edit's `char_start` to its byte
    /// offset in `prev_tree`'s coordinate space.
    pub prev_rope: Option<Arc<Rope>>,
    /// Edits applied between `prev_tree`'s version and this
    /// cmd's version, in char-offset form. The worker translates
    /// to tree-sitter's `InputEdit` (byte offsets + row/col)
    /// before calling `tree.edit` on a mutable clone of
    /// `prev_tree`. Carries the inserted text so the worker can
    /// keep a mutable clone of `prev_rope` in lock-step, which
    /// is how sequential-edit byte offsets get resolved
    /// correctly (each edit's positions are relative to the rope
    /// state after prior edits).
    pub edits_since_prev: Vec<RopeEdit>,
}

/// A rope edit, in char-offset form the runtime already carries
/// in its history log. `Insert` carries the actual text so the
/// worker can compute byte length and mirror the edit onto its
/// mutable `prev_rope` clone; `Delete` only needs the removed
/// char count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RopeEdit {
    Insert {
        char_start: usize,
        text: Arc<str>,
    },
    Delete {
        char_start: usize,
        removed_chars: usize,
    },
}

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
