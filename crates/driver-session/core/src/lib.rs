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

use led_core::{CanonPath, ChainId, PersistedContentHash, SessionUuid, UndoDbSeq};
use led_state_buffer_edits::EditGroup;

pub mod session_state;

pub use session_state::{
    DraftSession, PersistedSession, SessionBuffer, SessionData, SessionState, UndoRestoreData,
};

pub mod chat;

pub use chat::{
    ChatMessageKind, ChatMessageRow, ChatRole, ChatRow, ChatStatus, ChatStore,
};

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
        chain_id: ChainId,
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
    /// Cross-instance sync probe (M26). Triggered when the
    /// notify-dir watcher reports a touch on
    /// `<config>/notify/<path_hash>` whose hash maps to an open
    /// buffer. Reads `WHERE seq > last_seen_seq` and returns one
    /// of three [`SyncResultKind`]s.
    CheckSync {
        path: CanonPath,
        last_seen_seq: UndoDbSeq,
        current_chain_id: ChainId,
    },

    // ── Claude chat persistence ────────────────────────────────
    //
    // Cmds emitted by the runtime's `pending_persist_writes`
    // memo as it diffs `ChatTranscripts` (live) against
    // `ChatStore` (persisted). Write-through: no ack event for
    // success, errors propagate via `Failed`.

    /// Insert a fresh `claude_sessions` row. Fired once per
    /// session lifetime, on the first transcript event for a
    /// previously-unknown session.
    InsertChatRow { row: ChatRow },
    /// Append one `claude_messages` row for `session`. `seq`
    /// must be `ChatStore::max_seq(session) + 1`.
    AppendChatMessage { message: ChatMessageRow },
    /// Update the `short_label` / `long_summary` columns after
    /// the auto-label query lands. Either field can be `None`
    /// to clear.
    UpdateChatLabels {
        id: SessionUuid,
        short_label: Option<String>,
        long_summary: Option<String>,
    },
    /// Bump `last_active_at` and optionally refresh
    /// `last_usage_json`. Called once per turn.
    UpdateChatLastActive {
        id: SessionUuid,
        at: i64,
        usage_json: Option<String>,
    },
    /// Mark a session `active` or `orphaned`. Orphaned means
    /// the CLI's transcript for this id is gone — led keeps
    /// the row for the picker but won't try to respawn.
    UpdateChatStatus { id: SessionUuid, status: ChatStatus },
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
        chain_id: ChainId,
        persisted_undo_len: usize,
        last_seq: UndoDbSeq,
    },
    /// Non-fatal error during open / save / flush. The runtime
    /// surfaces the message as a warn alert.
    Failed { message: String },
    /// Result of a [`SessionCmd::CheckSync`] probe. Carries one
    /// of three discriminants (M26).
    SyncResult { kind: SyncResultKind },
    /// Bulk-load of all chat rows + messages for the active
    /// workspace. Fired once on `SessionCmd::Init`, after the
    /// regular `Restored` event. The runtime folds these into
    /// `ChatStore::apply_loaded`.
    ChatsLoaded {
        rows: Vec<ChatRow>,
        messages: Vec<ChatMessageRow>,
    },
}

/// Discriminant for [`SessionEvent::SyncResult`]. The runtime
/// reduces each variant differently:
///
/// - `SyncEntries` — peer wrote new undo entries since our
///   `last_seen_seq`. Validate `chain_id` + `content_hash`
///   against the live buffer; on match, apply each `EditGroup`
///   to the rope; on mismatch, queue a synthetic
///   `FileWatchEvent::Changed { kinds: MODIFIED }` so the
///   next-tick reconcile arm picks it up.
/// - `ExternalSave` — peer saved + cleared its undo state (the
///   `buffer_undo_state` row vanished). Equivalent to "the disk
///   bytes moved underneath us"; the runtime synthesises a
///   reread the same way.
/// - `NoChange` — chain matches and there are no new entries.
///   Includes the common self-echo case (our own `FlushUndo`
///   touched `<config>/notify/<hash>`, the watcher fired, we
///   probed back and got NoChange).
#[derive(Debug, Clone)]
pub enum SyncResultKind {
    SyncEntries {
        path: CanonPath,
        chain_id: ChainId,
        content_hash: PersistedContentHash,
        entries: Vec<EditGroup>,
        new_last_seen_seq: UndoDbSeq,
    },
    ExternalSave {
        path: CanonPath,
    },
    NoChange {
        path: CanonPath,
    },
}

// ── Driver-owned source ────────────────────────────────────────

/// Driver-owned source per `EXAMPLE-ARCH.md` § "Stateless drivers
/// still need an in-flight source". Written synchronously by
/// [`SessionDriver::execute`] when a command is dispatched; cleared
/// by [`SessionDriver::process`] when the matching completion event
/// arrives.
///
/// # `clear_undo_in_flight` caveat
///
/// `ClearUndo` has no ack event in the current ABI. The driver sets
/// the in-flight bit when the command is issued (so the same-tick
/// downstream query sees the op as outstanding) but cannot clear
/// it on a per-path basis from `process`. The bit is cleared
/// pessimistically on any `Failed` event (the driver doesn't know
/// which op failed), but otherwise stays set until the next call
/// to `execute` removes it — i.e. it is best treated as
/// "fired once" rather than "currently in flight".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDriverState {
    pub init_in_flight: bool,
    pub save_in_flight: bool,
    pub shutdown_in_flight: bool,
    /// Per-path `FlushUndo` in-flight, value is the `chain_id` so
    /// the runtime can correlate the matching `UndoFlushed` event.
    pub flush_in_flight: imbl::HashMap<CanonPath, ChainId>,
    /// Per-path `ClearUndo` in-flight. See struct-level docs for
    /// the no-ack caveat.
    pub clear_undo_in_flight: imbl::HashSet<CanonPath>,
    /// Per-path `CheckSync` in-flight. Cleared in `process` on any
    /// [`SyncResultKind`] variant (each carries `path`).
    pub check_sync_in_flight: imbl::HashSet<CanonPath>,
    pub last_error: Option<Arc<str>>,
    /// `true` once the runtime has dispatched `SessionCmd::Save`
    /// for the active Exiting transition, so we don't spam Save
    /// every tick while waiting for the `Saved` event.
    /// Driver-outbound bookkeeping on the driver source rather
    /// than mirrored on AppState.
    pub save_dispatched: bool,
}

pub trait Trace: Send + Sync {
    fn session_init_start(&self, root: &CanonPath);
    fn session_save_start(&self);
    fn session_save_done(&self, ok: bool);
    fn session_drop_undo(&self, path: &CanonPath);
    /// Per-flush undo persist: emitted as
    /// `WorkspaceFlushUndo\tpath=<p> chain=<id>` in
    /// `dispatched.snap`. Mirrors legacy's same-named line.
    fn session_flush_undo(&self, path: &CanonPath, chain_id: &ChainId);
    /// Per cross-instance sync probe (M26). Emitted as
    /// `WorkspaceCheckSync\tpath=<p>` in `dispatched.snap`.
    fn session_check_sync(&self, path: &CanonPath);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn session_init_start(&self, _: &CanonPath) {}
    fn session_save_start(&self) {}
    fn session_save_done(&self, _: bool) {}
    fn session_drop_undo(&self, _: &CanonPath) {}
    fn session_flush_undo(&self, _: &CanonPath, _: &ChainId) {}
    fn session_check_sync(&self, _: &CanonPath) {}
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

    /// Forward each command to the SQLite worker, writing intent
    /// into `state` synchronously *before* the async dispatch so
    /// downstream queries on the same tick see the op as in-flight.
    pub fn execute<'a>(
        &self,
        cmds: impl IntoIterator<Item = &'a SessionCmd>,
        state: &mut SessionDriverState,
    ) {
        for cmd in cmds {
            match cmd {
                SessionCmd::Init { root, .. } => {
                    state.init_in_flight = true;
                    self.trace.session_init_start(root);
                }
                SessionCmd::SaveSession { .. } => {
                    state.save_in_flight = true;
                    state.save_dispatched = true;
                    self.trace.session_save_start();
                }
                SessionCmd::ClearUndo { path } => {
                    state.clear_undo_in_flight.insert(path.clone());
                    self.trace.session_drop_undo(path);
                }
                SessionCmd::FlushUndo { path, chain_id, .. } => {
                    state
                        .flush_in_flight
                        .insert(path.clone(), chain_id.clone());
                    self.trace.session_flush_undo(path, chain_id);
                }
                SessionCmd::CheckSync { path, .. } => {
                    state.check_sync_in_flight.insert(path.clone());
                    self.trace.session_check_sync(path);
                }
                SessionCmd::Shutdown => {
                    state.shutdown_in_flight = true;
                }
                // Chat persistence cmds are write-through with no
                // per-cmd trace hook (yet) — the SQLite writes are
                // small and their effects are visible on
                // `ChatStore` immediately via the optimistic
                // mirror the runtime applies before dispatch.
                SessionCmd::InsertChatRow { .. }
                | SessionCmd::AppendChatMessage { .. }
                | SessionCmd::UpdateChatLabels { .. }
                | SessionCmd::UpdateChatLastActive { .. }
                | SessionCmd::UpdateChatStatus { .. } => {}
            }
            if self.tx.send(cmd.clone()).is_err() {
                return;
            }
        }
    }

    /// Drain completions from the SQLite worker, clearing the
    /// matching in-flight slot on `state` per event variant.
    /// `Failed` has no per-op discriminator in the ABI, so it
    /// pessimistically clears every in-flight flag and records
    /// the message on `state.last_error`.
    pub fn process(&self, state: &mut SessionDriverState) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            match &ev {
                SessionEvent::Restored { .. } => {
                    state.init_in_flight = false;
                }
                SessionEvent::SessionSaved => {
                    state.save_in_flight = false;
                    self.trace.session_save_done(true);
                }
                SessionEvent::UndoFlushed { path, .. } => {
                    state.flush_in_flight.remove(path);
                }
                SessionEvent::Failed { message } => {
                    // ABI carries no per-op discriminator; clear
                    // every in-flight slot pessimistically.
                    state.init_in_flight = false;
                    state.save_in_flight = false;
                    state.shutdown_in_flight = false;
                    state.flush_in_flight.clear();
                    state.clear_undo_in_flight.clear();
                    state.check_sync_in_flight.clear();
                    state.last_error = Some(Arc::from(message.as_str()));
                    self.trace.session_save_done(false);
                }
                SessionEvent::SyncResult { kind } => match kind {
                    SyncResultKind::SyncEntries { path, .. }
                    | SyncResultKind::ExternalSave { path }
                    | SyncResultKind::NoChange { path } => {
                        state.check_sync_in_flight.remove(path);
                    }
                },
                SessionEvent::ChatsLoaded { .. } => {
                    // Folded into ChatStore by the chat ingest
                    // arm in the runtime. No flight state to
                    // clear here.
                }
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
    fn process_drains_session_saved_event_and_clears_flag() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut state = SessionDriverState {
            save_in_flight: true,
            ..SessionDriverState::default()
        };
        tx_ev.send(SessionEvent::SessionSaved).unwrap();
        let batch = drv.process(&mut state);
        assert_eq!(batch.len(), 1);
        assert!(!state.save_in_flight);
    }

    #[test]
    fn execute_forwards_init_and_writes_in_flight() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<SessionCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut state = SessionDriverState::default();
        drv.execute(
            [&SessionCmd::Init {
                root: canon("/p"),
                config_dir: canon("/c"),
            }],
            &mut state,
        );
        assert!(state.init_in_flight);
        let cmd = rx_cmd.try_recv().expect("init dispatched");
        assert!(matches!(cmd, SessionCmd::Init { .. }));
    }

    #[test]
    fn execute_flush_undo_records_chain_id() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut state = SessionDriverState::default();
        let path = canon("/p/file.rs");
        let chain = ChainId::from("test-chain");
        drv.execute(
            [&SessionCmd::FlushUndo {
                path: path.clone(),
                chain_id: chain.clone(),
                content_hash: PersistedContentHash::default(),
                undo_cursor: 0,
                distance_from_save: 0,
                entries: Vec::new(),
            }],
            &mut state,
        );
        assert_eq!(state.flush_in_flight.get(&path), Some(&chain));
    }

    #[test]
    fn process_clears_flush_in_flight_on_undo_flushed() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let path = canon("/p/file.rs");
        let chain = ChainId::from("test-chain");
        let mut state = SessionDriverState::default();
        state.flush_in_flight.insert(path.clone(), chain.clone());

        tx_ev
            .send(SessionEvent::UndoFlushed {
                path: path.clone(),
                chain_id: chain,
                persisted_undo_len: 0,
                last_seq: UndoDbSeq(1),
            })
            .unwrap();
        drv.process(&mut state);
        assert!(!state.flush_in_flight.contains_key(&path));
    }

    #[test]
    fn process_clears_check_sync_for_each_kind() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let path = canon("/p/file.rs");
        let mut state = SessionDriverState::default();
        state.check_sync_in_flight.insert(path.clone());

        tx_ev
            .send(SessionEvent::SyncResult {
                kind: SyncResultKind::NoChange { path: path.clone() },
            })
            .unwrap();
        drv.process(&mut state);
        assert!(!state.check_sync_in_flight.contains(&path));
    }

    #[test]
    fn process_failed_clears_all_in_flight_and_records_error() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<SessionCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<SessionEvent>();
        let drv = SessionDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let path = canon("/p/file.rs");
        let mut state = SessionDriverState {
            init_in_flight: true,
            save_in_flight: true,
            ..SessionDriverState::default()
        };
        state.flush_in_flight.insert(path.clone(), ChainId::from("c"));
        state.clear_undo_in_flight.insert(path.clone());

        tx_ev
            .send(SessionEvent::Failed {
                message: "db locked".into(),
            })
            .unwrap();
        drv.process(&mut state);
        assert!(!state.init_in_flight);
        assert!(!state.save_in_flight);
        assert!(state.flush_in_flight.is_empty());
        assert!(state.clear_undo_in_flight.is_empty());
        assert_eq!(state.last_error.as_deref(), Some("db locked"));
    }
}
