//! Sync core of the find-file driver.
//!
//! ABI types at the driver boundary + the main-loop-facing
//! [`FindFileDriver`] + a driver-owned [`FindFileDriverState`] tracking
//! the currently in-flight listing per EXAMPLE-ARCH § "Stateless
//! drivers still need an in-flight source". The async worker (real
//! `fs::read_dir` in `*-native`, mock in tests) lives on the other
//! side of the mpsc channels.
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

pub use led_abi_find_file::FindFileEntry;

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

/// Driver-owned in-flight tracking. `in_flight` is `Some` while a
/// command is outstanding; `last_error` records the most recent
/// async-side failure for future diagnostics surfacing.
///
/// The overlay only ever has one query active at a time — a fresh
/// keystroke replaces the previous in-flight cmd rather than
/// queuing — so a single `Option` slot is sufficient.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindFileDriverState {
    pub in_flight: Option<FindFileCmd>,
    pub last_error: Option<std::sync::Arc<str>>,
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

    /// Ship each command to the worker. Writes the command into
    /// `state.in_flight` synchronously *before* dispatching async
    /// so the same tick's downstream code sees the listing as
    /// outstanding. The latest cmd wins — newer user keystrokes
    /// replace older in-flight queries.
    pub fn execute<'a>(
        &self,
        cmds: impl IntoIterator<Item = &'a FindFileCmd>,
        state: &mut FindFileDriverState,
    ) {
        for cmd in cmds {
            state.in_flight = Some(cmd.clone());
            self.trace.find_file_start(cmd);
            let _ = self.tx.send(cmd.clone());
        }
    }

    /// Drain ready completions, clearing `in_flight` when a
    /// completion's `(dir, prefix)` matches the outstanding cmd.
    /// Stale completions (echoed dir/prefix don't match the latest
    /// in_flight) still propagate to the caller — the runtime's
    /// matching discipline handles drop semantics — but they
    /// don't clear the in-flight slot. Returns an empty `Vec` on
    /// idle ticks.
    pub fn process(&self, state: &mut FindFileDriverState) -> Vec<FindFileListed> {
        let mut out = Vec::new();
        while let Ok(done) = self.rx.try_recv() {
            if let Some(in_flight) = &state.in_flight
                && in_flight.dir == done.dir
                && in_flight.prefix == done.prefix
            {
                state.in_flight = None;
            }
            self.trace.find_file_done(&done.dir, &done.prefix, true);
            out.push(done);
        }
        out
    }
}
