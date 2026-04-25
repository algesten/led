//! Sync core of the session driver (M21).
//!
//! Three commands cross the ABI:
//!
//! - [`SessionCmd::Init`] — open the SQLite DB, attempt to
//!   acquire the workspace's primary flock, and emit the
//!   restored session (or `None` for a fresh / non-primary
//!   workspace). Fires once per session, at startup.
//! - [`SessionCmd::Save`] — persist a [`SessionData`] payload
//!   into the workspace's row. Fires on the `Phase::Exiting`
//!   transition (and, in future, on a debounce after edits).
//! - [`SessionCmd::Shutdown`] — graceful close: flush any
//!   pending writes and drop the flock.
//!
//! Native I/O lives in `driver-session-native`. This crate
//! holds only the wire types so the runtime can compile
//! against a narrow surface and tests can substitute a mock
//! worker over a plain channel pair.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::CanonPath;
use led_state_session::SessionData;

// ── ABI ─────────────────────────────────────────────────────────

/// Runtime → driver commands.
#[derive(Debug, Clone)]
pub enum SessionCmd {
    /// One-shot startup: open the DB at `<config_dir>/db.sqlite`,
    /// attempt to acquire the primary flock at
    /// `<config_dir>/primary/<hash(root)>`, and emit a
    /// [`SessionEvent::Restored`].
    Init {
        root: CanonPath,
        config_dir: CanonPath,
    },
    /// Persist `data` into the workspace's row, replacing the
    /// previous snapshot. No-op for non-primary instances —
    /// the runtime gates this command on `session.primary`.
    Save { data: SessionData },
    /// Drop the persisted undo blob for `path` from the
    /// `undo_state` table. Fired after a successful disk save:
    /// the saved bytes become the new disk baseline, so the
    /// previously-persisted undo chain (computed against the
    /// pre-save content) is now stale relative to disk. The
    /// in-memory `History` stays intact — the user can still
    /// Ctrl-/. Maps 1:1 to the `WorkspaceClearUndo` trace line.
    DropUndo { path: CanonPath },
    /// Drop the flock + close the DB. Sent on the
    /// `Phase::Exiting` → break transition.
    Shutdown,
}

/// Driver → runtime events.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// First message after [`SessionCmd::Init`]. `primary`
    /// reflects the flock outcome; `restored` carries the
    /// loaded session for primaries with a prior session row,
    /// `None` otherwise (fresh workspace, or running
    /// secondary).
    Restored {
        primary: bool,
        restored: Option<SessionData>,
    },
    /// Acknowledgement of a successful [`SessionCmd::Save`].
    /// The runtime flips `session.saved = true` on this event;
    /// the Quit gate observes it on the next iteration.
    Saved,
    /// Non-fatal error during open / save. The runtime surfaces
    /// the message as a warn alert. Kept distinct from `Saved`
    /// so the Quit gate doesn't accidentally clear on a
    /// failed write.
    Failed { message: String },
}

// ── Trace ──────────────────────────────────────────────────────

pub trait Trace: Send + Sync {
    fn session_init_start(&self, root: &CanonPath);
    fn session_save_start(&self);
    fn session_save_done(&self, ok: bool);
    /// Runtime asked the session driver to drop a single
    /// path's persisted undo blob. Bound to the
    /// `WorkspaceClearUndo` line in `dispatched.snap`.
    fn session_drop_undo(&self, path: &CanonPath);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn session_init_start(&self, _: &CanonPath) {}
    fn session_save_start(&self) {}
    fn session_save_done(&self, _: bool) {}
    fn session_drop_undo(&self, _: &CanonPath) {}
}

// ── Driver handle ──────────────────────────────────────────────

pub struct SessionDriver {
    tx: Sender<SessionCmd>,
    rx: Receiver<SessionEvent>,
    trace: Arc<dyn Trace>,
}

impl SessionDriver {
    pub fn new(
        tx: Sender<SessionCmd>,
        rx: Receiver<SessionEvent>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self { tx, rx, trace }
    }

    pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a SessionCmd>) {
        for cmd in cmds {
            match cmd {
                SessionCmd::Init { root, .. } => {
                    self.trace.session_init_start(root);
                }
                SessionCmd::Save { .. } => {
                    self.trace.session_save_start();
                }
                SessionCmd::DropUndo { path } => {
                    self.trace.session_drop_undo(path);
                }
                SessionCmd::Shutdown => {}
            }
            if self.tx.send(cmd.clone()).is_err() {
                return;
            }
        }
    }

    pub fn process(&self) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            if let SessionEvent::Saved = &ev {
                self.trace.session_save_done(true);
            } else if let SessionEvent::Failed { .. } = &ev {
                self.trace.session_save_done(false);
            }
            out.push(ev);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::UserPath;
    use std::sync::mpsc;

    fn canon(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    #[test]
    fn process_returns_empty_when_quiet() {
        let (_tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(_tx_cmd, rx_ev, Arc::new(NoopTrace));
        assert!(drv.process().is_empty());
    }

    #[test]
    fn process_drains_events() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        tx_ev
            .send(SessionEvent::Restored {
                primary: true,
                restored: None,
            })
            .unwrap();
        tx_ev.send(SessionEvent::Saved).unwrap();
        let batch = drv.process();
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn execute_forwards_init() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<SessionCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        drv.execute([&SessionCmd::Init {
            root: canon("/p"),
            config_dir: canon("/c"),
        }]);
        let cmd = rx_cmd.try_recv().expect("init dispatched");
        matches!(cmd, SessionCmd::Init { .. });
    }
}
