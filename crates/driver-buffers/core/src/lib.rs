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

use led_core::{BufferVersion, CanonPath};
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
    /// Initial open. The driver writes `LoadState::Pending` into
    /// `BufferStore`, dispatches the read, and the completion is
    /// surfaced as a [`LoadCompletion`] for the runtime to seed
    /// `BufferEdits`.
    Load(CanonPath),
    /// External-change reread (M26). Triggered when the file-watch
    /// driver reports `MODIFIED` for an already-materialised
    /// buffer's path. Does **not** rewrite `BufferStore.loaded`
    /// to `Pending` — that would visually flip the body to a
    /// loading placeholder. Completion is surfaced as a
    /// [`RereadCompletion`] for the runtime's three-branch
    /// reconcile (clean reload / dirty silent drop / hash-match
    /// no-op).
    Reread(CanonPath),
}

// ── ABI boundary ───────────────────────────────────────────────────────

/// Command from the sync driver to the async worker. No explicit "stop"
/// variant — the worker detects shutdown when `FileReadDriver` drops
/// its `Sender` and the receiver returns `Err` on `recv`.
#[derive(Clone, Debug)]
pub enum ReadCmd {
    /// Initial disk read (M1). Result writes into `BufferStore`.
    Read(CanonPath),
    /// External-change reread (M26). Result is *not* written into
    /// `BufferStore` — the runtime's reconcile branch consumes the
    /// new rope directly and decides whether to update
    /// `EditedBuffer.rope`.
    Reread(CanonPath),
}

/// Whether a [`ReadDone`] is the completion of an initial open
/// (`Initial`) or an external-change reread (`Reread`). The driver
/// echoes this back from the originating [`ReadCmd`] so `process`
/// can route the result to the right consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadKind {
    #[default]
    Initial,
    Reread,
}

/// Completion posted by the async worker back to the sync driver.
#[derive(Debug)]
pub struct ReadDone {
    pub path: CanonPath,
    pub kind: ReadKind,
    pub result: Result<Arc<Rope>, String>,
}

/// A successful initial load surfaced by [`FileReadDriver::process`]
/// — the runtime uses this to seed the `BufferEdits` source with a
/// clean, disk-matching rope. Failed loads are not surfaced here;
/// they land in `BufferStore` as `LoadState::Error` and don't
/// belong in `BufferEdits`.
#[derive(Debug, Clone)]
pub struct LoadCompletion {
    pub path: CanonPath,
    pub rope: Arc<Rope>,
}

/// External-change reread completion (M26). The runtime's
/// reconcile branch reads the fresh rope, compares its content
/// hash against `eb.disk_content_hash`, and decides whether to
/// replace the in-memory rope (clean buffer + content diverges)
/// or silently drop (dirty buffer protects local edits).
#[derive(Debug, Clone)]
pub struct RereadCompletion {
    pub path: CanonPath,
    pub result: Result<Arc<Rope>, String>,
}

/// What [`FileReadDriver::process`] hands back to the runtime:
/// initial-open completions for the `BufferEdits` seeding flow,
/// plus reread completions for the external-change reconcile.
/// On idle ticks both vectors are empty (`Vec::new()` allocates
/// nothing).
#[derive(Debug, Default)]
pub struct ReadCompletions {
    pub initials: Vec<LoadCompletion>,
    pub rereads: Vec<RereadCompletion>,
}

// ── Trace ──────────────────────────────────────────────────────────────

/// Hook for emitting `--golden-trace` lines. The runtime crate provides
/// the implementation; the driver calls these at the relevant moments.
pub trait Trace: Send + Sync {
    fn file_load_start(&self, path: &CanonPath);
    fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>);
    fn file_save_start(&self, path: &CanonPath, version: BufferVersion);
    fn file_save_done(&self, path: &CanonPath, version: BufferVersion, result: &Result<(), String>);
    /// `Ctrl+x Ctrl+w` commit: write the active buffer (`from`) to a
    /// different path (`to`). `FileSaveAs` in legacy's dispatched.snap.
    fn file_save_as_start(&self, from: &CanonPath, to: &CanonPath);
    fn file_save_as_done(&self, from: &CanonPath, to: &CanonPath, result: &Result<(), String>);
    /// External-change reread start. Quiet by default in
    /// dispatched.snap (none of the M26-gated goldens contain a
    /// `FileReread` line — the user-visible effect is the
    /// post-reload `WorkspaceFlushUndo`). Available for verbose
    /// investigation builds.
    fn file_reread_start(&self, path: &CanonPath);
}

/// No-op trace for tests or non-golden runs.
pub struct NoopTrace;
impl Trace for NoopTrace {
    fn file_load_start(&self, _: &CanonPath) {}
    fn file_load_done(&self, _: &CanonPath, _: &Result<Arc<Rope>, String>) {}
    fn file_save_start(&self, _: &CanonPath, _: BufferVersion) {}
    fn file_save_done(&self, _: &CanonPath, _: BufferVersion, _: &Result<(), String>) {}
    fn file_save_as_start(&self, _: &CanonPath, _: &CanonPath) {}
    fn file_save_as_done(&self, _: &CanonPath, _: &CanonPath, _: &Result<(), String>) {}
    fn file_reread_start(&self, _: &CanonPath) {}
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

    /// Drain completions from the async worker.
    ///
    /// Initial loads transition `BufferStore` entries to `Ready`
    /// or `Error` and are surfaced as [`LoadCompletion`]s for the
    /// runtime to seed `BufferEdits`.
    ///
    /// Reread completions (M26) do **not** touch `BufferStore` —
    /// the buffer is already materialised; the runtime's
    /// reconcile branch consumes the fresh rope directly.
    ///
    /// On idle ticks both vectors are empty (`Vec::new()` is
    /// zero-alloc).
    pub fn process(&self, store: &mut BufferStore) -> ReadCompletions {
        let mut out = ReadCompletions::default();
        while let Ok(done) = self.rx_done.try_recv() {
            self.trace.file_load_done(&done.path, &done.result);
            match done.kind {
                ReadKind::Initial => {
                    let entry = match &done.result {
                        Ok(rope) => {
                            out.initials.push(LoadCompletion {
                                path: done.path.clone(),
                                rope: rope.clone(),
                            });
                            LoadState::Ready(rope.clone())
                        }
                        Err(msg) => LoadState::Error(Arc::new(msg.clone())),
                    };
                    store.loaded.insert(done.path, entry);
                }
                ReadKind::Reread => {
                    out.rereads.push(RereadCompletion {
                        path: done.path,
                        result: done.result,
                    });
                }
            }
        }
        out
    }

    /// Act on `LoadAction`s (produced by the runtime's query layer).
    /// For `Load`: writes `Pending` synchronously into `BufferStore`
    /// before dispatching async work — without the sync write, the
    /// next tick's query would see the path as absent and re-trigger.
    /// For `Reread`: the buffer is already materialised, so we
    /// dispatch the read but leave `BufferStore` untouched.
    pub fn execute<'a, I>(&self, actions: I, store: &mut BufferStore)
    where
        I: IntoIterator<Item = &'a LoadAction>,
    {
        for action in actions {
            match action {
                LoadAction::Load(path) => {
                    store.loaded.insert(path.clone(), LoadState::Pending);
                    self.trace.file_load_start(path);
                    let _ = self.tx_cmd.send(ReadCmd::Read(path.clone()));
                }
                LoadAction::Reread(path) => {
                    self.trace.file_reread_start(path);
                    let _ = self.tx_cmd.send(ReadCmd::Reread(path.clone()));
                }
            }
        }
    }
}

// ── FileWriteDriver — saves ─────────────────────────────────────────────

/// Action produced by the runtime's save query, consumed by
/// [`FileWriteDriver::execute`].
///
/// `Save` writes the buffer back to the path it was loaded from (the
/// normal `ctrl+x ctrl+s` flow). `SaveAs` writes the *active buffer's
/// content* to a **different** path, creating a new file on disk —
/// the active tab itself stays pinned to `from`. Matches legacy
/// `DocStoreOut::SaveAs` semantics.
#[derive(Clone, Debug, PartialEq)]
pub enum SaveAction {
    Save {
        path: CanonPath,
        rope: Arc<Rope>,
        version: BufferVersion,
    },
    SaveAs {
        /// The path of the buffer being saved (tab stays here). The
        /// completion trace / alert shows `from` for
        /// `WorkspaceClearUndo` because that's the buffer whose undo
        /// history logically gets re-baselined.
        from: CanonPath,
        /// Where the bytes are written. A fresh file is created on
        /// disk at `to`; the existing tab at `from` is untouched.
        to: CanonPath,
        rope: Arc<Rope>,
        version: BufferVersion,
    },
}

/// Command from the sync save driver to its async worker.
#[derive(Clone, Debug)]
pub enum WriteCmd {
    Write {
        path: CanonPath,
        rope: Arc<Rope>,
        version: BufferVersion,
    },
    WriteAs {
        from: CanonPath,
        to: CanonPath,
        rope: Arc<Rope>,
        version: BufferVersion,
    },
}

/// Completion posted by the async write worker.
///
/// `result: Ok(Arc<Rope>)` echoes the rope that was persisted so the
/// runtime can install it as the new `BufferStore` baseline without
/// re-reading from disk. For `SaveAs`, `path` is the **target** (the
/// new file on disk); `from` is the source buffer that initiated the
/// save (tab + undo bookkeeping key).
#[derive(Debug)]
pub struct WriteDone {
    pub path: CanonPath,
    pub version: BufferVersion,
    pub result: Result<Arc<Rope>, String>,
    /// `Some(original_path)` for `SaveAs` completions; `None` for
    /// plain `Save`. The runtime uses this to clear the source
    /// buffer's undo history and emit the matching `WorkspaceClearUndo`
    /// trace.
    pub from: Option<CanonPath>,
}

/// Main-loop-facing half of the save driver.
///
/// Unlike [`FileReadDriver`], `FileWriteDriver` owns no source: the
/// intent-write side effects (clearing `pending_saves`, bumping
/// `saved_version`, installing the saved rope into `BufferStore`)
/// live in the runtime. Keeping the driver stateless also keeps it
/// ignorant of sibling state crates — a strict-isolation win.
pub struct FileWriteDriver {
    tx_cmd: Sender<WriteCmd>,
    rx_done: Receiver<WriteDone>,
    trace: Arc<dyn Trace>,
}

impl FileWriteDriver {
    pub fn new(
        tx_cmd: Sender<WriteCmd>,
        rx_done: Receiver<WriteDone>,
        trace: Arc<dyn Trace>,
    ) -> Self {
        Self {
            tx_cmd,
            rx_done,
            trace,
        }
    }

    /// Drain write completions. `Vec::new()` on idle — no alloc.
    pub fn process(&self) -> Vec<WriteDone> {
        let mut out: Vec<WriteDone> = Vec::new();
        while let Ok(done) = self.rx_done.try_recv() {
            let trace_result: Result<(), String> = match &done.result {
                Ok(_) => Ok(()),
                Err(msg) => Err(msg.clone()),
            };
            match &done.from {
                None => self.trace.file_save_done(&done.path, done.version, &trace_result),
                Some(from) => self
                    .trace
                    .file_save_as_done(from, &done.path, &trace_result),
            }
            out.push(done);
        }
        out
    }

    /// Act on `SaveAction`s. Forwards a `WriteCmd` to the async
    /// worker for each action; does not touch any source state
    /// (that's the runtime's job — see the M4 design doc).
    pub fn execute<'a, I>(&self, actions: I)
    where
        I: IntoIterator<Item = &'a SaveAction>,
    {
        for action in actions {
            match action {
                SaveAction::Save {
                    path,
                    rope,
                    version,
                } => {
                    self.trace.file_save_start(path, *version);
                    let _ = self.tx_cmd.send(WriteCmd::Write {
                        path: path.clone(),
                        rope: rope.clone(),
                        version: *version,
                    });
                }
                SaveAction::SaveAs {
                    from,
                    to,
                    rope,
                    version,
                } => {
                    self.trace.file_save_as_start(from, to);
                    let _ = self.tx_cmd.send(WriteCmd::WriteAs {
                        from: from.clone(),
                        to: to.clone(),
                        rope: rope.clone(),
                        version: *version,
                    });
                }
            }
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
            ReadCmd::Reread(_) => panic!("expected initial Read, got Reread"),
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
                kind: ReadKind::Initial,
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
                kind: ReadKind::Initial,
                result: Err("No such file".into()),
            })
            .expect("send ReadDone");

        driver.process(&mut store);
        match store.loaded.get(&path) {
            Some(LoadState::Error(m)) => assert_eq!(m.as_str(), "No such file"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // ── FileWriteDriver ─────────────────────────────────────────────────

    #[test]
    fn write_execute_sends_write_cmd() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<WriteCmd>();
        let (_tx_done, rx_done) = mpsc::channel::<WriteDone>();
        let driver = FileWriteDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let path = canon("doc.txt");
        let rope = Arc::new(Rope::from_str("payload"));
        let action = SaveAction::Save {
            path: path.clone(),
            rope: rope.clone(),
            version: BufferVersion(7),
        };

        driver.execute([&action]);

        match rx_cmd.try_recv().expect("expected a WriteCmd") {
            WriteCmd::Write {
                path: p,
                rope: r,
                version,
            } => {
                assert_eq!(p, path);
                assert!(Arc::ptr_eq(&r, &rope));
                assert_eq!(version, BufferVersion(7));
            }
            WriteCmd::WriteAs { .. } => panic!("unexpected WriteAs"),
        }
    }

    #[test]
    fn write_process_surfaces_completion() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<WriteCmd>();
        let (tx_done, rx_done) = mpsc::channel::<WriteDone>();
        let driver = FileWriteDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let path = canon("doc.txt");
        let rope = Arc::new(Rope::from_str("payload"));

        tx_done
            .send(WriteDone {
                path: path.clone(),
                version: BufferVersion(3),
                result: Ok(rope.clone()),
                from: None,
            })
            .expect("send WriteDone");

        let mut completions = driver.process();
        assert_eq!(completions.len(), 1);
        let done = completions.pop().unwrap();
        assert_eq!(done.path, path);
        assert_eq!(done.version, BufferVersion(3));
        match done.result {
            Ok(r) => assert!(Arc::ptr_eq(&r, &rope)),
            Err(_) => panic!("expected Ok"),
        }
    }

    #[test]
    fn write_process_surfaces_error() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<WriteCmd>();
        let (tx_done, rx_done) = mpsc::channel::<WriteDone>();
        let driver = FileWriteDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        tx_done
            .send(WriteDone {
                path: canon("ro.txt"),
                version: BufferVersion(1),
                result: Err("Permission denied".into()),
                from: None,
            })
            .unwrap();

        let completions = driver.process();
        assert_eq!(completions.len(), 1);
        match &completions[0].result {
            Err(msg) => assert_eq!(msg, "Permission denied"),
            Ok(_) => panic!("expected Err"),
        }
    }

    #[test]
    fn execute_save_as_emits_write_as_cmd() {
        let (tx_cmd, rx_cmd) = mpsc::channel::<WriteCmd>();
        let (_tx_done, rx_done) = mpsc::channel::<WriteDone>();
        let driver = FileWriteDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let from = canon("/tmp/orig.txt");
        let to = canon("/tmp/copy.txt");
        let rope = Arc::new(Rope::from_str("payload"));
        driver.execute(std::iter::once(&SaveAction::SaveAs {
            from: from.clone(),
            to: to.clone(),
            rope,
            version: BufferVersion(7),
        }));
        match rx_cmd.try_recv() {
            Ok(WriteCmd::WriteAs { from: f, to: t, version, .. }) => {
                assert_eq!(f, from);
                assert_eq!(t, to);
                assert_eq!(version, BufferVersion(7));
            }
            other => panic!("expected WriteAs, got {other:?}"),
        }
    }

    #[test]
    fn process_save_as_preserves_from_on_completion() {
        let (tx_cmd, _rx_cmd) = mpsc::channel::<WriteCmd>();
        let (tx_done, rx_done) = mpsc::channel::<WriteDone>();
        let driver = FileWriteDriver::new(tx_cmd, rx_done, Arc::new(NoopTrace));

        let from = canon("/tmp/orig.txt");
        let to = canon("/tmp/copy.txt");
        tx_done
            .send(WriteDone {
                path: to.clone(),
                version: BufferVersion(1),
                result: Ok(Arc::new(Rope::from_str(""))),
                from: Some(from.clone()),
            })
            .unwrap();

        let completions = driver.process();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].path, to);
        assert_eq!(completions[0].from.as_ref(), Some(&from));
    }
}
