//! Sync core of the session driver.
//!
//! ABI mirrors legacy `WorkspaceOut` / `WorkspaceIn` shapes for
//! the persistence-relevant commands (Init / SaveSession /
//! FlushUndo / ClearUndo) so the rewrite's storage lifecycle
//! lines up with legacy's design (`docs/spec/persistence.md`,
//! `docs/spec/lifecycle.md`).
//!
//! Native I/O lives in `driver-session-native`.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::{CanonPath, PersistedContentHash};
use led_state_buffer_edits::EditGroup;
use led_state_session::SessionData;

#[derive(Debug, Clone)]
pub enum SessionCmd {
    /// One-shot startup: open the DB at `<config_dir>/db.sqlite`,
    /// attempt to acquire the primary flock at
    /// `<config_dir>/primary/<hash(root)>`, load the session row +
    /// per-buffer undo state, and emit a [`SessionEvent::Restored`].
    Init {
        root: CanonPath,
        config_dir: CanonPath,
    },
    /// Persist the full session payload (workspaces row, all
    /// buffer rows, kv pairs) — the equivalent of legacy's
    /// `WorkspaceOut::SaveSession`. No-op for non-primaries.
    SaveSession { data: SessionData },
    /// Append undo entries for one buffer + update its
    /// `buffer_undo_state` row. Mirrors legacy
    /// `WorkspaceOut::FlushUndo`. Caller passes only entries it
    /// hasn't flushed yet (the `last_seq` returned in the
    /// `UndoFlushed` event tells the runtime where to resume).
    FlushUndo {
        path: CanonPath,
        chain_id: String,
        content_hash: PersistedContentHash,
        undo_cursor: usize,
        distance_from_save: i32,
        entries: Vec<EditGroup>,
    },
    /// Drop a path's persisted undo state — the
    /// `WorkspaceClearUndo` legacy command. Fired post-save:
    /// the saved bytes are the new disk baseline, so the prior
    /// undo chain (computed against the old content) is stale
    /// relative to disk and gets wiped from `buffer_undo_state`
    /// + `undo_entries`.
    ClearUndo { path: CanonPath },
    /// Drop the flock + close the DB. Sent on the
    /// `Phase::Exiting` → break transition.
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// First message after [`SessionCmd::Init`].
    Restored {
        primary: bool,
        restored: Option<SessionData>,
    },
    /// Acknowledgement of a successful `SaveSession`.
    SessionSaved,
    /// Acknowledgement of a successful `FlushUndo`. Carries the
    /// max seq inserted (so the runtime can advance its
    /// last-flushed mark) and echoes the path + chain_id +
    /// `persisted_undo_len` for the matching call site.
    UndoFlushed {
        path: CanonPath,
        chain_id: String,
        persisted_undo_len: usize,
        last_seq: i64,
    },
    /// Non-fatal error during open / save / flush. The runtime
    /// surfaces the message as a warn alert.
    Failed { message: String },
}

pub trait Trace: Send + Sync {
    fn session_init_start(&self, root: &CanonPath);
    fn session_save_start(&self);
    fn session_save_done(&self, ok: bool);
    fn session_drop_undo(&self, path: &CanonPath);
    /// Per-flush undo persist: emitted as
    /// `WorkspaceFlushUndo\tpath=<p> chain=<id>` in
    /// `dispatched.snap`. Mirrors legacy's same-named line.
    fn session_flush_undo(&self, path: &CanonPath, chain_id: &str);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn session_init_start(&self, _: &CanonPath) {}
    fn session_save_start(&self) {}
    fn session_save_done(&self, _: bool) {}
    fn session_drop_undo(&self, _: &CanonPath) {}
    fn session_flush_undo(&self, _: &CanonPath, _: &str) {}
}

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
                SessionCmd::Init { root, .. } => self.trace.session_init_start(root),
                SessionCmd::SaveSession { .. } => self.trace.session_save_start(),
                SessionCmd::ClearUndo { path } => self.trace.session_drop_undo(path),
                SessionCmd::FlushUndo { path, chain_id, .. } => {
                    self.trace.session_flush_undo(path, chain_id);
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
            if matches!(ev, SessionEvent::SessionSaved) {
                self.trace.session_save_done(true);
            } else if matches!(ev, SessionEvent::Failed { .. }) {
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
    fn process_drains_session_saved_event() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        tx_ev.send(SessionEvent::SessionSaved).unwrap();
        let batch = drv.process();
        assert_eq!(batch.len(), 1);
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
