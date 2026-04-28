//! Sync core of the file-watch driver (M26).
//!
//! One driver services every filesystem-watch consumer in led:
//!
//! - **Workspace root recursive watch** — sidebar refresh + git
//!   rescan on Create/Remove of any path under the project root.
//! - **`<config>/notify/` non-recursive watch** — cross-instance
//!   sync touch files (M21 wrote them; M26 reads them back).
//! - **Per-buffer parent-dir watches** — open-file external-change
//!   detection for the docstore reload-or-prompt branch.
//!
//! Each registration is keyed by a [`WatchSeq`] minted by the
//! runtime; the driver internally fans events out to the requesting
//! id only. The `notify::Watcher` instance is shared across all
//! registrations on the native side.
//!
//! # Driver-owned source
//!
//! Per `EXAMPLE-ARCH.md` § "Stateless drivers still need an
//! in-flight source" the driver carries [`FileWatchState`] — its
//! view of "what's currently watched" plus "what fired since the
//! last drain". Memos in the runtime read this struct to derive
//! both the desired/actual diff (→ Watch / Unwatch commands) and
//! the per-event fan-out (→ reread / sync-check / browser-refresh
//! dispatches).
//!
//! # Trace
//!
//! Watch events are input-side and would add per-keystroke noise
//! to dispatched.snap on platforms that echo our own writes back
//! through the watcher. The [`Trace::file_watch_event`] hook
//! exists for verbose investigation builds; the runtime's golden
//! trace ignores it.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use imbl::HashMap;
use led_core::CanonPath;
pub use led_core::WatchSeq;

// ── ABI ─────────────────────────────────────────────────────────

/// Per-event filesystem change kinds, packed into a `u8` bitmask.
/// `notify` can fire `Created+Modified` within one debounce
/// window for the same path; we collapse to a single event with
/// the union of bits set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChangeKinds(u8);

impl ChangeKinds {
    pub const CREATED: u8 = 0b001;
    pub const MODIFIED: u8 = 0b010;
    pub const REMOVED: u8 = 0b100;

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn from_bits(b: u8) -> Self {
        Self(b & 0b111)
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn contains_any(self, mask: u8) -> bool {
        self.0 & mask != 0
    }

    pub fn insert(&mut self, mask: u8) {
        self.0 |= mask & 0b111;
    }
}

/// Runtime → driver commands.
#[derive(Debug, Clone)]
pub enum FileWatchCmd {
    /// Watch `path` (file or directory). `recursive=true` includes
    /// descendants. Idempotent: re-issuing the same `(id, path)`
    /// is a no-op.
    Watch {
        id: WatchSeq,
        path: CanonPath,
        recursive: bool,
        /// 0 = no debounce. Otherwise the worker waits this many
        /// ms of quiet on the same path before emitting. The
        /// `<config>/notify/` watch sets 100 ms; per-buffer and
        /// root watches set 0 (FSEvents/inotify already coalesce
        /// at their own native cadence).
        debounce_ms: u32,
    },
    /// Drop a previously-registered watch. Idempotent.
    Unwatch { id: WatchSeq },
}

/// Driver → runtime events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileWatchEvent {
    /// One filesystem change, post-debounce + path-filter.
    Changed {
        id: WatchSeq,
        path: CanonPath,
        kinds: ChangeKinds,
    },
    /// Backend reported a fatal error (rare — usually a platform
    /// watcher running out of fds). Per-id so the runtime can
    /// decide whether to fall back per-registration.
    Failed {
        id: WatchSeq,
        message: String,
    },
}

// ── Driver-owned source ────────────────────────────────────────

/// What the driver knows about its own state.
///
/// The `registry` half is the **actual** side of the desired/actual
/// diff: a `desired_watch_set` memo in the runtime computes what
/// *should* be registered, the `watch_actions` memo diffs against
/// this map, and the runtime's execute phase ships the resulting
/// `Vec<FileWatchCmd>` to `FileWatchDriver::execute`.
///
/// The `recent_events` half is the queue of post-debounce events
/// the worker emitted since the last `process()` drain. Memos
/// in the runtime read it during the Query phase; the runtime
/// calls [`FileWatchState::clear_events`] at the end of every
/// Execute phase so each event drives at most one tick of
/// dispatches.
///
/// `imbl` collections everywhere: input projections in memos are
/// pointer copies, idle ticks cache-hit (G14).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileWatchState {
    pub registry: HashMap<WatchSeq, Registration>,
    pub recent_events: HashMap<WatchSeq, imbl::Vector<FileWatchEvent>>,
    pub backend: BackendStatus,
}

impl FileWatchState {
    /// Drop all queued events. Called at the end of the runtime's
    /// Execute phase so the next Query tick sees a clean slate.
    pub fn clear_events(&mut self) {
        self.recent_events.clear();
    }

    /// Synthesise a "this path changed externally" signal — used
    /// by the SessionEvent::SyncResult ingest arm when chain or
    /// content-hash mismatch forces a reread fallback. Routing
    /// through the same source the watcher writes to means the
    /// `external_reread_targets` memo handles both cases
    /// uniformly.
    pub fn synthesize_modified(&mut self, id: WatchSeq, path: CanonPath) {
        let entry = self.recent_events.entry(id).or_default();
        entry.push_back(FileWatchEvent::Changed {
            id,
            path,
            kinds: ChangeKinds::from_bits(ChangeKinds::MODIFIED),
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub path: CanonPath,
    pub recursive: bool,
    pub debounce_ms: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BackendStatus {
    /// Steady state. `notify::Watcher` constructed and polling.
    #[default]
    Healthy,
    /// Platform unsupported or watcher init failed. Every `Watch`
    /// silently no-ops; events never fire. The runtime tolerates
    /// this — code paths gated on watcher events also work in
    /// "no events ever" mode.
    Inert,
    /// A previously-healthy backend reported a fatal error.
    Failed {
        message: String,
    },
}

// ── Trace ──────────────────────────────────────────────────────

/// `--golden-trace` hook for debug investigation builds. Watch
/// events are input-side and *not* emitted into the goldens'
/// dispatched.snap.
pub trait Trace: Send + Sync {
    fn file_watch_event(&self, id: WatchSeq, path: &CanonPath, kinds: ChangeKinds);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn file_watch_event(&self, _: WatchSeq, _: &CanonPath, _: ChangeKinds) {}
}

// ── Driver handle ──────────────────────────────────────────────

/// Main-loop-facing half. Owns the `Sender` for commands and the
/// `Receiver` for events. Constructed by the native `spawn`
/// alongside the lifetime marker.
pub struct FileWatchDriver {
    tx: Sender<FileWatchCmd>,
    rx: Receiver<FileWatchEvent>,
    trace: Arc<dyn Trace>,
}

impl FileWatchDriver {
    pub fn new(
        tx: Sender<FileWatchCmd>,
        rx: Receiver<FileWatchEvent>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self { tx, rx, trace }
    }

    /// Execute pattern (G3): writes intent into
    /// `state.registry` *synchronously* for each `Watch` /
    /// `Unwatch`, then forwards the command to the native worker
    /// for the async `notify::Watcher::watch`/`unwatch` calls.
    /// The synchronous source write closes the loop —
    /// `desired_watch_set == registry` on the next tick, so the
    /// `watch_actions` memo returns an empty diff and the same
    /// command is not re-dispatched.
    pub fn execute<'a, I>(&self, cmds: I, state: &mut FileWatchState)
    where
        I: IntoIterator<Item = &'a FileWatchCmd>,
    {
        for cmd in cmds {
            match cmd {
                FileWatchCmd::Watch {
                    id,
                    path,
                    recursive,
                    debounce_ms,
                } => {
                    state.registry.insert(
                        *id,
                        Registration {
                            path: path.clone(),
                            recursive: *recursive,
                            debounce_ms: *debounce_ms,
                        },
                    );
                }
                FileWatchCmd::Unwatch { id } => {
                    state.registry.remove(id);
                }
            }
            if self.tx.send(cmd.clone()).is_err() {
                return;
            }
        }
    }

    /// Drain worker-emitted events into `state.recent_events`.
    /// The runtime's Query phase reads from there; Execute phase
    /// calls [`FileWatchState::clear_events`] after dispatching.
    pub fn process(&self, state: &mut FileWatchState) {
        while let Ok(ev) = self.rx.try_recv() {
            match &ev {
                FileWatchEvent::Changed { id, path, kinds } => {
                    self.trace.file_watch_event(*id, path, *kinds);
                    let entry = state.recent_events.entry(*id).or_default();
                    entry.push_back(ev);
                }
                FileWatchEvent::Failed { id, message } => {
                    state.backend = BackendStatus::Failed {
                        message: message.clone(),
                    };
                    let entry = state.recent_events.entry(*id).or_default();
                    entry.push_back(ev);
                }
            }
        }
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
    fn execute_writes_intent_synchronously_for_watch() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<FileWatchCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<FileWatchEvent>();
        let drv = FileWatchDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut state = FileWatchState::default();

        let id = WatchSeq(7);
        drv.execute(
            [&FileWatchCmd::Watch {
                id,
                path: canon("/x"),
                recursive: false,
                debounce_ms: 0,
            }],
            &mut state,
        );

        // (1) Sync state was written immediately — desired/actual
        // diff next tick produces no command.
        assert!(state.registry.contains_key(&id));
        // (2) The command landed on the ABI boundary.
        let cmd = rx_cmd.try_recv().expect("cmd was sent");
        assert!(matches!(cmd, FileWatchCmd::Watch { .. }));
    }

    #[test]
    fn execute_writes_intent_synchronously_for_unwatch() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<FileWatchCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<FileWatchEvent>();
        let drv = FileWatchDriver::new(tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut state = FileWatchState::default();
        let id = WatchSeq(3);
        state.registry.insert(
            id,
            Registration {
                path: canon("/y"),
                recursive: true,
                debounce_ms: 0,
            },
        );

        drv.execute([&FileWatchCmd::Unwatch { id }], &mut state);
        assert!(!state.registry.contains_key(&id));
        let cmd = rx_cmd.try_recv().expect("cmd was sent");
        assert!(matches!(cmd, FileWatchCmd::Unwatch { .. }));
    }

    #[test]
    fn process_drains_changed_into_state() {
        let (_tx_cmd, _rx_cmd) = mpsc::channel::<FileWatchCmd>();
        let (tx_ev, rx_ev) = mpsc::channel::<FileWatchEvent>();
        let drv = FileWatchDriver::new(_tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut state = FileWatchState::default();

        let id = WatchSeq(1);
        tx_ev
            .send(FileWatchEvent::Changed {
                id,
                path: canon("/x/y"),
                kinds: ChangeKinds::from_bits(ChangeKinds::MODIFIED),
            })
            .unwrap();
        drv.process(&mut state);

        let queue = state.recent_events.get(&id).expect("event queued");
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn process_returns_empty_when_channel_quiet() {
        let (_tx_cmd, _rx_cmd) = mpsc::channel::<FileWatchCmd>();
        let (_tx_ev, rx_ev) = mpsc::channel::<FileWatchEvent>();
        let drv = FileWatchDriver::new(_tx_cmd, rx_ev, Arc::new(NoopTrace));
        let mut state = FileWatchState::default();
        drv.process(&mut state);
        assert!(state.recent_events.is_empty());
    }

    #[test]
    fn change_kinds_bitset_combines() {
        let mut k = ChangeKinds::from_bits(ChangeKinds::CREATED);
        k.insert(ChangeKinds::MODIFIED);
        assert!(k.contains_any(ChangeKinds::CREATED));
        assert!(k.contains_any(ChangeKinds::MODIFIED));
        assert!(!k.contains_any(ChangeKinds::REMOVED));
    }

    #[test]
    fn synthesize_modified_routes_into_recent_events() {
        let mut state = FileWatchState::default();
        let id = WatchSeq(2);
        state.synthesize_modified(id, canon("/y"));
        let queue = state.recent_events.get(&id).unwrap();
        assert_eq!(queue.len(), 1);
        match &queue[0] {
            FileWatchEvent::Changed { kinds, .. } => {
                assert!(kinds.contains_any(ChangeKinds::MODIFIED));
            }
            _ => panic!("expected Changed"),
        }
    }

    #[test]
    fn clear_events_drops_all_queued() {
        let mut state = FileWatchState::default();
        state.synthesize_modified(WatchSeq(1), canon("/a"));
        state.synthesize_modified(WatchSeq(2), canon("/b"));
        assert_eq!(state.recent_events.len(), 2);
        state.clear_events();
        assert!(state.recent_events.is_empty());
    }
}
