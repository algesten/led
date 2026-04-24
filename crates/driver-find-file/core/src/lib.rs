//! Sync core of the find-file driver.
//!
//! ABI types at the driver boundary + the main-loop-facing
//! [`FindFileDriver`]. The async worker (real `fs::read_dir` in
//! `*-native`, mock in tests) lives on the other side of the mpsc
//! channels.
//!
//! The driver's contract: take a [`FindFileCmd`] (`dir` + `prefix` +
//! `show_hidden`), read the directory, case-insensitively filter by
//! leaf-name prefix, sort dirs-first then alphabetically, and return a
//! [`FindFileListed`] with the entries. Failures return an empty
//! entry list — the overlay treats "no completions" the same way for
//! "directory missing" and "directory empty".

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::CanonPath;

/// One completion-list entry. `name` has a trailing `/` for
/// directories so the renderer doesn't need to inspect `is_dir`;
/// `full` is the canonicalized target for open / save requests.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct FindFileEntry {
    pub name: String,
    pub full: CanonPath,
    pub is_dir: bool,
}

/// Command to the worker: list `dir`, keep only leaves that
/// case-insensitively start with `prefix`, optionally include
/// dotfiles.
#[derive(Debug, Clone, PartialEq, Eq, drv::Input)]
pub struct FindFileCmd {
    pub dir: CanonPath,
    pub prefix: String,
    pub show_hidden: bool,
}

/// Completion back to the runtime. `dir` + `prefix` are echoed so
/// late-arriving results that no longer match the current input can
/// be dropped (legacy's "expected_dir" discipline).
#[derive(Debug, Clone)]
pub struct FindFileListed {
    pub dir: CanonPath,
    pub prefix: String,
    pub entries: Vec<FindFileEntry>,
}

/// Trace hook — driver-specific. The runtime's `Trace` delegates
/// here via the adapter pattern used by the other drivers.
pub trait Trace: Send + Sync {
    fn find_file_start(&self, cmd: &FindFileCmd);
    fn find_file_done(&self, dir: &CanonPath, prefix: &str, ok: bool);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn find_file_start(&self, _: &FindFileCmd) {}
    fn find_file_done(&self, _: &CanonPath, _: &str, _: bool) {}
}

/// The main-loop-facing half. Owns the `Sender` for commands and the
/// `Receiver` for completions; the async worker holds the opposite
/// ends.
pub struct FindFileDriver {
    tx: Sender<FindFileCmd>,
    rx: Receiver<FindFileListed>,
    trace: Arc<dyn Trace>,
}

impl FindFileDriver {
    pub fn new(
        tx: Sender<FindFileCmd>,
        rx: Receiver<FindFileListed>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self { tx, rx, trace }
    }

    /// Ship each command to the worker. Emits a `find_file_start`
    /// trace per command. Failed sends (worker gone) silently drop —
    /// mirrors every other driver's behaviour on shutdown races.
    pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a FindFileCmd>) {
        for cmd in cmds {
            self.trace.find_file_start(cmd);
            let _ = self.tx.send(cmd.clone());
        }
    }

    /// Drain ready completions. Returns an empty `Vec` on idle ticks —
    /// zero heap alloc.
    pub fn process(&self) -> Vec<FindFileListed> {
        let mut out = Vec::new();
        while let Ok(done) = self.rx.try_recv() {
            self.trace.find_file_done(&done.dir, &done.prefix, true);
            out.push(done);
        }
        out
    }
}
