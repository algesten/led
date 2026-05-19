//! Sync core of the directory-listing driver — strictly isolated.
//!
//! Knows only its own ABI: `ListCmd`, `ListDone`, `DirEntry` (re-
//! exported from `state-browser` so the worker can emit them
//! directly), the driver-owned [`FsListState`] tracking in-flight
//! listings, and the `Trace` hook. The runtime wires it up and
//! owns whatever cross-source logic decides *when* to list.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::CanonPath;

/// Driver-owned in-flight tracking. Path is added on `execute`
/// (sync, before tx.send) and removed on `process` when the
/// matching `Done` arrives. Memos read this to avoid re-emitting
/// `ListCmd::List(p)` while p is outstanding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsListState {
    pub in_flight: imbl::HashSet<CanonPath>,
}

/// A single child entry from a directory listing. Structurally owned
/// by this driver because it IS the driver's ABI — state / runtime
/// consumers depend on this crate for the shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: CanonPath,
    pub kind: DirEntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListCmd {
    List(CanonPath),
}

#[derive(Debug)]
pub struct ListDone {
    pub path: CanonPath,
    pub result: Result<Vec<DirEntry>, String>,
}

/// `--golden-trace` hook for FS-list events.
pub trait Trace: Send + Sync {
    fn list_start(&self, path: &CanonPath);
    fn list_done(&self, path: &CanonPath, result: &Result<Vec<DirEntry>, String>);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn list_start(&self, _: &CanonPath) {}
    fn list_done(&self, _: &CanonPath, _: &Result<Vec<DirEntry>, String>) {}
}

/// Sync driver: runtime calls `execute(actions)` to enqueue listings
/// and `process()` to drain completions.
pub struct FsListDriver {
    cmd_tx: Sender<ListCmd>,
    done_rx: Receiver<ListDone>,
    trace: Arc<dyn Trace>,
}

impl FsListDriver {
    pub fn new(cmd_tx: Sender<ListCmd>, done_rx: Receiver<ListDone>, trace: Arc<dyn Trace>) -> Self {
        Self {
            cmd_tx,
            done_rx,
            trace,
        }
    }

    pub fn execute<'a, I>(&self, cmds: I, state: &mut FsListState)
    where
        I: IntoIterator<Item = &'a ListCmd>,
    {
        for cmd in cmds {
            match cmd {
                ListCmd::List(path) => {
                    state.in_flight.insert(path.clone());
                    self.trace.list_start(path);
                }
            }
            // Clone once; the worker owns it from here.
            if self.cmd_tx.send(cmd.clone()).is_err() {
                // Worker gone — next tick's drivers will observe the
                // same and the runtime can shut down cleanly.
                return;
            }
        }
    }

    pub fn process(&self, state: &mut FsListState) -> Vec<ListDone> {
        let mut out: Vec<ListDone> = Vec::new();
        while let Ok(done) = self.done_rx.try_recv() {
            state.in_flight.remove(&done.path);
            self.trace.list_done(&done.path, &done.result);
            out.push(done);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn process_returns_empty_when_nothing_queued() {
        let (_cmd_tx, cmd_rx) = mpsc::channel::<ListCmd>();
        let (_done_tx, done_rx) = mpsc::channel::<ListDone>();
        let _ = cmd_rx; // keep receiver alive
        let drv = FsListDriver::new(_cmd_tx_noop(), done_rx, Arc::new(NoopTrace));
        let mut state = FsListState::default();
        assert!(drv.process(&mut state).is_empty());
    }

    fn _cmd_tx_noop() -> Sender<ListCmd> {
        // Throwaway sender — tests don't exercise the cmd path here.
        let (tx, _rx) = mpsc::channel();
        tx
    }

    #[test]
    fn process_drains_a_result_and_clears_in_flight() {
        use led_core::UserPath;
        let (cmd_tx, _cmd_rx) = mpsc::channel::<ListCmd>();
        let (done_tx, done_rx) = mpsc::channel::<ListDone>();
        let drv = FsListDriver::new(cmd_tx, done_rx, Arc::new(NoopTrace));
        let path = UserPath::new("/x").canonicalize();
        let mut state = FsListState::default();
        state.in_flight.insert(path.clone());
        done_tx
            .send(ListDone {
                path: path.clone(),
                result: Ok(Vec::new()),
            })
            .unwrap();
        let batch = drv.process(&mut state);
        assert_eq!(batch.len(), 1);
        assert!(!state.in_flight.contains(&path));
    }

    #[test]
    fn execute_marks_path_in_flight_before_send() {
        use led_core::UserPath;
        let (cmd_tx, cmd_rx) = mpsc::channel::<ListCmd>();
        let (_done_tx, done_rx) = mpsc::channel::<ListDone>();
        let drv = FsListDriver::new(cmd_tx, done_rx, Arc::new(NoopTrace));
        let path = UserPath::new("/x").canonicalize();
        let mut state = FsListState::default();
        drv.execute([&ListCmd::List(path.clone())], &mut state);
        assert!(state.in_flight.contains(&path));
        assert!(matches!(cmd_rx.try_recv(), Ok(ListCmd::List(p)) if p == path));
    }
}
