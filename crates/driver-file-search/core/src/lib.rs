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

/// Project-wide replace-all request. Runs independently of any
/// cached search results — the worker does its own tree walk.
///
/// `skip_paths` is the set of files the runtime is rewriting
/// in-memory (loaded buffers). The worker skips them so the session
/// view stays the source of truth for those; the runtime applies
/// the replacement to their rope in dispatch instead of letting the
/// driver overwrite them on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchReplaceCmd {
    pub root: CanonPath,
    pub query: String,
    pub replacement: String,
    pub case_sensitive: bool,
    pub use_regex: bool,
    pub skip_paths: Vec<CanonPath>,
}

/// One replace-all completion. `files_changed` = number of files
/// whose content differed after regex substitution (and therefore
/// got rewritten). `total_replacements` = total number of matches
/// the worker replaced across all files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchReplaceOut {
    pub query: String,
    pub files_changed: usize,
    pub total_replacements: usize,
}

/// Driver-scoped trace. The runtime's `Trace` delegates here via the
/// adapter pattern used by every other driver.
pub trait Trace: Send + Sync {
    fn file_search_start(&self, cmd: &FileSearchCmd);
    fn file_search_done(&self, query: &str, ok: bool);
    fn file_search_replace_start(&self, cmd: &FileSearchReplaceCmd);
    fn file_search_replace_done(
        &self,
        query: &str,
        files_changed: usize,
        total_replacements: usize,
    );
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn file_search_start(&self, _: &FileSearchCmd) {}
    fn file_search_done(&self, _: &str, _: bool) {}
    fn file_search_replace_start(&self, _: &FileSearchReplaceCmd) {}
    fn file_search_replace_done(&self, _: &str, _: usize, _: usize) {}
}

/// Main-loop-facing half. Owns two channel pairs — one for search
/// (the live-typing feed) and one for replace-all. Separating them
/// keeps the two loops independent: a pending replace never delays
/// a search response, and vice versa.
pub struct FileSearchDriver {
    search_tx: Sender<FileSearchCmd>,
    search_rx: Receiver<FileSearchOut>,
    replace_tx: Sender<FileSearchReplaceCmd>,
    replace_rx: Receiver<FileSearchReplaceOut>,
    trace: Arc<dyn Trace>,
}

impl FileSearchDriver {
    pub fn new(
        search_tx: Sender<FileSearchCmd>,
        search_rx: Receiver<FileSearchOut>,
        replace_tx: Sender<FileSearchReplaceCmd>,
        replace_rx: Receiver<FileSearchReplaceOut>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self {
            search_tx,
            search_rx,
            replace_tx,
            replace_rx,
            trace,
        }
    }

    /// Ship each search command to the worker. A `file_search_start`
    /// trace fires per command. Failed sends (worker gone) silently
    /// drop — mirrors every other driver's shutdown-race handling.
    pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a FileSearchCmd>) {
        for cmd in cmds {
            self.trace.file_search_start(cmd);
            let _ = self.search_tx.send(cmd.clone());
        }
    }

    /// Ship a replace-all request. One trace line per command.
    pub fn execute_replace<'a>(
        &self,
        cmds: impl IntoIterator<Item = &'a FileSearchReplaceCmd>,
    ) {
        for cmd in cmds {
            self.trace.file_search_replace_start(cmd);
            let _ = self.replace_tx.send(cmd.clone());
        }
    }

    /// Drain ready search results. Empty `Vec` on idle ticks — zero
    /// heap alloc on the happy path.
    pub fn process(&self) -> Vec<FileSearchOut> {
        let mut out = Vec::new();
        while let Ok(done) = self.search_rx.try_recv() {
            self.trace.file_search_done(&done.query, true);
            out.push(done);
        }
        out
    }

    /// Drain ready replace completions. Same zero-alloc-on-idle
    /// discipline as `process`.
    pub fn process_replace(&self) -> Vec<FileSearchReplaceOut> {
        let mut out = Vec::new();
        while let Ok(done) = self.replace_rx.try_recv() {
            self.trace.file_search_replace_done(
                &done.query,
                done.files_changed,
                done.total_replacements,
            );
            out.push(done);
        }
        out
    }
}
