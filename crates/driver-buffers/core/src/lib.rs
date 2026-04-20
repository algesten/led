//! Sync core of the buffers driver — strictly isolated.
//!
//! Knows only about its own atom ([`BufferStore`]) plus the ABI types
//! it exchanges with the async worker ([`ReadCmd`], [`ReadDone`]) and
//! the sync API the main loop calls ([`FileReadDriver::process`],
//! [`FileReadDriver::execute`]).
//!
//! **Nothing** here references other drivers, `state-tabs`, render
//! models, or the runtime. Cross-driver composition — memos that
//! combine lenses from multiple atoms, the dispatch logic that issues
//! driver operations — lives in `led-runtime`.
//!
//! Testing against this crate is independent: construct the channels,
//! construct a `FileReadDriver`, play the role of the async worker
//! yourself. No threads, no fs, no other drivers involved.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use led_core::CanonPath;
use ropey::Rope;

// ── Atom ───────────────────────────────────────────────────────────────

/// Load state for a single path.
///
/// `Arc<Rope>` / `Arc<String>` give O(1) pointer-equality comparison in
/// the memo cache layer even as content grows.
#[derive(Clone, Debug, PartialEq)]
pub enum LoadState {
    /// Read request in flight.
    Pending,
    /// Content loaded.
    Ready(Arc<Rope>),
    /// Load failed; the string is the `io::Error` message.
    Error(Arc<String>),
}

/// Source: what each file looks like on disk. Managed by `FileReadDriver`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BufferStore {
    pub loaded: imbl::HashMap<CanonPath, LoadState>,
}

/// Command the runtime hands to `FileReadDriver::execute`, produced by
/// a memo in the runtime that diffs desired vs actual state.
#[derive(Clone, Debug, PartialEq)]
pub enum LoadAction {
    Load(CanonPath),
}

// ── ABI boundary ───────────────────────────────────────────────────────

/// Command from the sync driver to the async worker. No explicit "stop"
/// variant — the worker detects shutdown when `FileReadDriver` drops
/// its `Sender` and the receiver returns `Err` on `recv`.
#[derive(Clone, Debug)]
pub enum ReadCmd {
    Read(CanonPath),
}

/// Completion posted by the async worker back to the sync driver.
#[derive(Debug)]
pub struct ReadDone {
    pub path: CanonPath,
    pub result: Result<Arc<Rope>, String>,
}

/// A successful load surfaced by [`FileReadDriver::process`] — the
/// runtime uses this to seed the `BufferEdits` source with a clean,
/// disk-matching rope. Failed loads are not surfaced here; they land
/// in `BufferStore` as `LoadState::Error` and don't belong in
/// `BufferEdits`.
#[derive(Debug, Clone)]
pub struct LoadCompletion {
    pub path: CanonPath,
    pub rope: Arc<Rope>,
}

// ── Trace ──────────────────────────────────────────────────────────────

/// Hook for emitting `--golden-trace` lines. The runtime crate provides
/// the implementation; the driver calls these at the relevant moments.
pub trait Trace: Send + Sync {
    fn file_load_start(&self, path: &CanonPath);
    fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>);
}

/// No-op trace for tests or non-golden runs.
pub struct NoopTrace;
impl Trace for NoopTrace {
    fn file_load_start(&self, _: &CanonPath) {}
    fn file_load_done(&self, _: &CanonPath, _: &Result<Arc<Rope>, String>) {}
}

// ── Sync driver API ────────────────────────────────────────────────────

/// The main-loop-facing half of the driver.
///
/// Constructed with a channel pair whose other ends are owned by the
/// async worker. Tests can play the worker directly on those channels
/// — that's the mock point.
pub struct FileReadDriver {
    tx_cmd: Sender<ReadCmd>,
    rx_done: Receiver<ReadDone>,
    trace: Arc<dyn Trace>,
}

impl FileReadDriver {
    pub fn new(
        tx_cmd: Sender<ReadCmd>,
        rx_done: Receiver<ReadDone>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self {
            tx_cmd,
            rx_done,
            trace,
        }
    }

    /// Drain completions from the async worker into `BufferStore`.
    /// Main-thread, cheap.
    ///
    /// Returns the list of paths whose load transitioned to `Ready`
    /// on this tick so the runtime can seed sibling sources (notably
    /// `BufferEdits`). On idle ticks the returned `Vec` is empty —
    /// `Vec::new()` is zero-alloc.
    pub fn process(&self, store: &mut BufferStore) -> Vec<LoadCompletion> {
        let mut completions: Vec<LoadCompletion> = Vec::new();
        while let Ok(done) = self.rx_done.try_recv() {
            self.trace.file_load_done(&done.path, &done.result);
            let entry = match &done.result {
                Ok(rope) => {
                    completions.push(LoadCompletion {
                        path: done.path.clone(),
                        rope: rope.clone(),
                    });
                    LoadState::Ready(rope.clone())
                }
                Err(msg) => LoadState::Error(Arc::new(msg.clone())),
            };
            store.loaded.insert(done.path, entry);
        }
        completions
    }

    /// Act on `LoadAction`s (produced by the runtime's query layer).
    /// Writes `Pending` synchronously into `BufferStore` before
    /// dispatching async work — without the sync write, the next
    /// tick's query would see the path as absent and re-trigger.
    pub fn execute<'a, I>(&self, actions: I, store: &mut BufferStore)
    where
        I: IntoIterator<Item = &'a LoadAction>,
    {
        for LoadAction::Load(path) in actions {
            store.loaded.insert(path.clone(), LoadState::Pending);
            self.trace.file_load_start(path);
            let _ = self.tx_cmd.send(ReadCmd::Read(path.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    //! All tests stay within this crate's world: BufferStore + its
    //! driver + synthetic workers on the channels. No other drivers,
    //! no `state-tabs`, no runtime.

    use super::*;
    use led_core::UserPath;
    use std::sync::mpsc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn execute_writes_pending_sync_then_sends_read_cmd() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<ReadCmd>();
        let (_tx_done, rx_done) = mpsc::channel::<ReadDone>();
        let driver = FileReadDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let mut store = BufferStore::default();
        let path = canon("example.txt");

        let acts = [LoadAction::Load(path.clone())];
        driver.execute(acts.iter(), &mut store);

        // Sync state updated immediately.
        assert!(matches!(store.loaded.get(&path), Some(LoadState::Pending)));

        // Command landed on the ABI boundary.
        match rx_cmd.try_recv().expect("expected a ReadCmd") {
            ReadCmd::Read(p) => assert_eq!(p, path),
        }
    }

    #[test]
    fn process_applies_worker_completion_to_atom() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<ReadCmd>();
        let (tx_done, rx_done) = mpsc::channel::<ReadDone>();
        let driver = FileReadDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let mut store = BufferStore::default();
        let path = canon("example.txt");
        let rope = Arc::new(Rope::from_str("hello"));

        tx_done
            .send(ReadDone {
                path: path.clone(),
                result: Ok(rope.clone()),
            })
            .expect("send ReadDone");

        driver.process(&mut store);
        match store.loaded.get(&path) {
            Some(LoadState::Ready(r)) => assert!(Arc::ptr_eq(r, &rope)),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn process_applies_worker_error_to_atom() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<ReadCmd>();
        let (tx_done, rx_done) = mpsc::channel::<ReadDone>();
        let driver = FileReadDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let mut store = BufferStore::default();
        let path = canon("missing.rs");

        tx_done
            .send(ReadDone {
                path: path.clone(),
                result: Err("No such file".into()),
            })
            .expect("send ReadDone");

        driver.process(&mut store);
        match store.loaded.get(&path) {
            Some(LoadState::Error(m)) => assert_eq!(m.as_str(), "No such file"),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
