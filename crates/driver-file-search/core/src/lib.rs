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

// ── Driver-owned source ────────────────────────────────────────

/// Driver-owned source per `EXAMPLE-ARCH.md` § "Stateless drivers
/// still need an in-flight source". Tracks the last-issued command
/// in each of the three lanes (live search / replace-all / single
/// on-disk point replace). Written synchronously by the matching
/// `execute*` method; cleared by the matching `process*` method
/// when its `Done` arrives.
///
/// For search + replace-all the slot is a single `Option` — the
/// overlay never has more than one in flight at a time (a fresh
/// keystroke or replace request supersedes the previous one).
/// Single-replace can fan out across hits so its slot is a
/// per-path count; the ABI's `FileSearchSingleReplaceOut` carries
/// only `path` + `ok`, which is too coarse for a richer key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSearchDriverState {
    /// Last-issued live search. Cleared when a matching `Out`
    /// arrives (same `query` + toggle echoes).
    pub search_in_flight: Option<FileSearchCmd>,
    /// Last-issued replace-all. Cleared when a matching `Out`
    /// arrives (echoes the same `query`).
    pub replace_in_flight: Option<FileSearchReplaceCmd>,
    /// Per-path count of outstanding single-replace requests.
    /// Bumped in `execute_single_replace`, decremented in
    /// `process_single_replace`. A path with count == 0 is
    /// removed from the map so memos can read `contains_key`
    /// as "currently in flight".
    pub single_in_flight: imbl::HashMap<CanonPath, usize>,
    pub last_error: Option<Arc<str>>,
}

pub use led_abi_file_search::{FileSearchGroup, FileSearchHit};

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
///
/// `error` is `Some(msg)` when the worker hit a terminal failure
/// (bad regex, walker init error). Per
/// `feedback_driver_failure_state.md` the driver records this on
/// `FileSearchDriverState.last_error` rather than silently
/// collapsing to "0 results."
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
    pub error: Option<Arc<str>>,
}

/// One-shot point replacement for a single hit on disk. Used when
/// the user Right-arrows on a result whose file isn't currently
/// loaded as a buffer — dispatch optimistically removes the hit
/// from the display, and the driver does the on-disk splice.
///
/// The `original` field lets the worker abort when the target
/// bytes don't look like what we expected (file changed under us
/// between search and replace). Byte offsets are line-relative —
/// same form ripgrep / `FileSearchHit` already use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchSingleReplaceCmd {
    pub path: CanonPath,
    /// 1-indexed line number in the file.
    pub line: usize,
    /// Byte offset inside the line where the match starts.
    pub match_start: usize,
    /// Byte offset inside the line where the match ends.
    pub match_end: usize,
    /// Expected content at `[match_start..match_end]`. The worker
    /// refuses the edit and reports `ok=false` when the file has
    /// changed.
    pub original: String,
    pub replacement: String,
}

/// Completion for a single on-disk point replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchSingleReplaceOut {
    pub path: CanonPath,
    /// `true` when the edit was written to disk successfully.
    /// `false` when the file was missing, unreadable, or the
    /// target bytes didn't match `original` (stale hit).
    pub ok: bool,
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
    fn file_search_single_replace_start(&self, cmd: &FileSearchSingleReplaceCmd);
    fn file_search_single_replace_done(&self, path: &CanonPath, ok: bool);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn file_search_start(&self, _: &FileSearchCmd) {}
    fn file_search_done(&self, _: &str, _: bool) {}
    fn file_search_replace_start(&self, _: &FileSearchReplaceCmd) {}
    fn file_search_replace_done(&self, _: &str, _: usize, _: usize) {}
    fn file_search_single_replace_start(&self, _: &FileSearchSingleReplaceCmd) {}
    fn file_search_single_replace_done(&self, _: &CanonPath, _: bool) {}
}

/// Main-loop-facing half. Owns three channel pairs — live-typing
/// search, bulk replace-all, and single on-disk point replace.
/// Separating them keeps the loops independent: a pending replace
/// never delays a search response, and a slow single-point replace
/// never blocks a bulk operation.
pub struct FileSearchDriver {
    search_tx: Sender<FileSearchCmd>,
    search_rx: Receiver<FileSearchOut>,
    replace_tx: Sender<FileSearchReplaceCmd>,
    replace_rx: Receiver<FileSearchReplaceOut>,
    single_tx: Sender<FileSearchSingleReplaceCmd>,
    single_rx: Receiver<FileSearchSingleReplaceOut>,
    trace: Arc<dyn Trace>,
}

impl FileSearchDriver {
    pub fn new(
        search_tx: Sender<FileSearchCmd>,
        search_rx: Receiver<FileSearchOut>,
        replace_tx: Sender<FileSearchReplaceCmd>,
        replace_rx: Receiver<FileSearchReplaceOut>,
        single_tx: Sender<FileSearchSingleReplaceCmd>,
        single_rx: Receiver<FileSearchSingleReplaceOut>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self {
            search_tx,
            search_rx,
            replace_tx,
            replace_rx,
            single_tx,
            single_rx,
            trace,
        }
    }

    /// Ship each search command to the worker. Writes the last cmd
    /// into `state.search_in_flight` synchronously before the async
    /// dispatch — the overlay only ever has one query outstanding,
    /// so each new keystroke replaces the previous in-flight cmd.
    pub fn execute<'a>(
        &self,
        cmds: impl IntoIterator<Item = &'a FileSearchCmd>,
        state: &mut FileSearchDriverState,
    ) {
        for cmd in cmds {
            state.search_in_flight = Some(cmd.clone());
            self.trace.file_search_start(cmd);
            let _ = self.search_tx.send(cmd.clone());
        }
    }

    /// Ship a replace-all request. Writes the last cmd into
    /// `state.replace_in_flight` synchronously before the async
    /// dispatch.
    pub fn execute_replace<'a>(
        &self,
        cmds: impl IntoIterator<Item = &'a FileSearchReplaceCmd>,
        state: &mut FileSearchDriverState,
    ) {
        for cmd in cmds {
            state.replace_in_flight = Some(cmd.clone());
            self.trace.file_search_replace_start(cmd);
            let _ = self.replace_tx.send(cmd.clone());
        }
    }

    /// Ship a single on-disk point-replace. Bumps the per-path
    /// in-flight count synchronously.
    pub fn execute_single_replace<'a>(
        &self,
        cmds: impl IntoIterator<Item = &'a FileSearchSingleReplaceCmd>,
        state: &mut FileSearchDriverState,
    ) {
        for cmd in cmds {
            *state
                .single_in_flight
                .entry(cmd.path.clone())
                .or_insert(0) += 1;
            self.trace.file_search_single_replace_start(cmd);
            let _ = self.single_tx.send(cmd.clone());
        }
    }

    /// Drain ready search results. Clears `state.search_in_flight`
    /// when the completed `(query, case_sensitive, use_regex)`
    /// triple matches the latest outstanding cmd; stale results
    /// from superseded queries still propagate to the caller (the
    /// runtime's matching discipline handles drop semantics) but
    /// don't clear the in-flight slot. Empty `Vec` on idle ticks.
    pub fn process(&self, state: &mut FileSearchDriverState) -> Vec<FileSearchOut> {
        let mut out = Vec::new();
        while let Ok(done) = self.search_rx.try_recv() {
            if let Some(in_flight) = &state.search_in_flight
                && in_flight.query == done.query
                && in_flight.case_sensitive == done.case_sensitive
                && in_flight.use_regex == done.use_regex
            {
                state.search_in_flight = None;
            }
            let ok = match &done.error {
                Some(msg) => {
                    state.last_error = Some(msg.clone());
                    false
                }
                None => {
                    state.last_error = None;
                    true
                }
            };
            self.trace.file_search_done(&done.query, ok);
            out.push(done);
        }
        out
    }

    /// Drain ready replace completions. Clears
    /// `state.replace_in_flight` when the completed `query`
    /// matches the latest outstanding cmd.
    pub fn process_replace(
        &self,
        state: &mut FileSearchDriverState,
    ) -> Vec<FileSearchReplaceOut> {
        let mut out = Vec::new();
        while let Ok(done) = self.replace_rx.try_recv() {
            if let Some(in_flight) = &state.replace_in_flight
                && in_flight.query == done.query
            {
                state.replace_in_flight = None;
            }
            self.trace.file_search_replace_done(
                &done.query,
                done.files_changed,
                done.total_replacements,
            );
            out.push(done);
        }
        out
    }

    /// Drain single-replace completions. Decrements the per-path
    /// in-flight count, removing the entry when it reaches zero.
    pub fn process_single_replace(
        &self,
        state: &mut FileSearchDriverState,
    ) -> Vec<FileSearchSingleReplaceOut> {
        let mut out = Vec::new();
        while let Ok(done) = self.single_rx.try_recv() {
            if let Some(count) = state.single_in_flight.get_mut(&done.path) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.single_in_flight.remove(&done.path);
                }
            }
            self.trace
                .file_search_single_replace_done(&done.path, done.ok);
            out.push(done);
        }
        out
    }
}
