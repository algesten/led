//! Sync core of the project-wide file-search driver.
//!
//! ABI types at the driver boundary + the main-loop-facing
//! [`FileSearchDriver`]. The async worker (ripgrep over the workspace
//! in `*-native`, mock in tests) lives on the other side of the mpsc
//! channels.
//!
//! The driver's contract: take a [`FileSearchCmd`] (`root` + `query` +
//! toggles), walk the workspace honouring `.gitignore`, and return a
//! [`FileSearchOut`] with per-file hit groups. Failures / empty trees
//! return empty groups — the overlay treats "no hits" the same way
//! for all outcomes.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::CanonPath;

/// One match inside a file. Positions are all 1-indexed to match
/// ripgrep's output conventions; `match_start` / `match_end` are
/// byte offsets into `preview` (kept for later rendering of the
/// hit inside the preview line, and for the replace flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchHit {
    pub path: CanonPath,
    /// 1-indexed line number.
    pub line: usize,
    /// 1-indexed column of the first char of the match.
    pub col: usize,
    /// Single-line preview (the matched line with its newline
    /// trimmed). The UI renders this as-is.
    pub preview: String,
    /// Byte offsets inside `preview` — the highlight span.
    pub match_start: usize,
    pub match_end: usize,
}

/// All hits in a single file. `relative` is the file's path
/// rendered relative to the search root; the UI shows this as the
/// group header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchGroup {
    pub path: CanonPath,
    pub relative: String,
    pub hits: Vec<FileSearchHit>,
}

/// One search request, shaped exactly as the runtime dispatches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchCmd {
    pub root: CanonPath,
    pub query: String,
    pub case_sensitive: bool,
    pub use_regex: bool,
}

/// One completion back from the worker. `query` + toggles are echoed
/// so the runtime can drop late arrivals (user typed further or
/// flipped a toggle since the request went out).
#[derive(Debug, Clone)]
pub struct FileSearchOut {
    pub query: String,
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub groups: Vec<FileSearchGroup>,
    /// `groups[..].hits` concatenated in order — exists so the
    /// runtime doesn't re-walk the tree when projecting the cursor
    /// between hits.
    pub flat: Vec<FileSearchHit>,
}

/// Driver-scoped trace. The runtime's `Trace` delegates here via the
/// adapter pattern used by every other driver.
pub trait Trace: Send + Sync {
    fn file_search_start(&self, cmd: &FileSearchCmd);
    fn file_search_done(&self, query: &str, ok: bool);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn file_search_start(&self, _: &FileSearchCmd) {}
    fn file_search_done(&self, _: &str, _: bool) {}
}

/// Main-loop-facing half. Owns the `Sender` for commands and the
/// `Receiver` for results; the async worker holds the opposite ends.
pub struct FileSearchDriver {
    tx: Sender<FileSearchCmd>,
    rx: Receiver<FileSearchOut>,
    trace: Arc<dyn Trace>,
}

impl FileSearchDriver {
    pub fn new(
        tx: Sender<FileSearchCmd>,
        rx: Receiver<FileSearchOut>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self { tx, rx, trace }
    }

    /// Ship each command to the worker. A `file_search_start` trace
    /// fires per command. Failed sends (worker gone) silently drop —
    /// mirrors every other driver's shutdown-race handling.
    pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a FileSearchCmd>) {
        for cmd in cmds {
            self.trace.file_search_start(cmd);
            let _ = self.tx.send(cmd.clone());
        }
    }

    /// Drain ready results. Empty `Vec` on idle ticks — zero heap
    /// alloc on the happy path.
    pub fn process(&self) -> Vec<FileSearchOut> {
        let mut out = Vec::new();
        while let Ok(done) = self.rx.try_recv() {
            self.trace.file_search_done(&done.query, true);
            out.push(done);
        }
        out
    }
}
