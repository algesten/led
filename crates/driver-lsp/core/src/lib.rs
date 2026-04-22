//! Sync core of the LSP driver.
//!
//! Diagnostics for M16; completions / hover / code-actions land
//! in later milestones. The wire ABI (`LspCmd` / `LspEvent`) and
//! the `DiagnosticSource` state machine both live here so the
//! native driver and the runtime share the same vocabulary and
//! the state machine is testable without tokio.
//!
//! # Lifecycle sketch
//!
//! 1. Runtime emits `LspCmd::Init { root }` on workspace startup.
//! 2. For every open buffer the runtime emits `BufferOpened`
//!    (language pre-resolved from the `PathChain`) and, on edit,
//!    `BufferChanged` carrying the latest rope + monotonic
//!    `version`.
//! 3. `RequestDiagnostics` fires from the runtime whenever the
//!    buffer version or the saved version changes — the state
//!    machine coalesces repeated fires into at most one
//!    propagation window at a time.
//! 4. Completions arrive as `LspEvent::Diagnostics { path,
//!    diagnostics, version }`; the runtime accepts only if the
//!    version is still reachable (fast path) or rebaseable
//!    (replay path — stage 3).

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use led_core::CanonPath;
use led_state_diagnostics::{BufferVersion, Diagnostic};
use ropey::Rope;

pub mod diag_source;

pub use diag_source::{DiagMode, DiagPushResult, DiagnosticSource};

// ── ABI ─────────────────────────────────────────────────────────

/// Runtime → driver commands. `Clone` because the shared-memory
/// transport passes them by value through an mpsc channel.
///
/// `RequestDiagnostics` takes no per-path payload: the driver
/// iterates every currently-opened buffer, snapshots its version,
/// and decides (based on the per-server capability) whether to
/// pull or forward cached pushes. Matches legacy's global-window
/// semantics at `crates/lsp/src/manager.rs:1955`.
#[derive(Debug, Clone)]
pub enum LspCmd {
    /// One-time initialisation handshake. The root is the
    /// workspace path sent as the LSP `rootUri`.
    Init { root: CanonPath },
    /// Graceful shutdown. The driver tears down every spawned
    /// server, waits for their `shutdown` replies, then closes.
    Shutdown,
    /// A buffer has been opened (or re-opened after a language
    /// change). `language` is pre-resolved by the runtime's
    /// `Language::from_chain`; `None` means "no language server
    /// applies" and the driver ignores the buffer for LSP
    /// purposes. `version` is `eb.version` at open time.
    BufferOpened {
        path: CanonPath,
        language: Option<led_state_syntax::Language>,
        rope: Arc<Rope>,
        version: BufferVersion,
    },
    /// The rope changed. `is_save` is `true` when this change is
    /// the moment-of-save (dispatched by the save-handler after
    /// the writer confirms) — the driver uses it to emit
    /// `textDocument/didSave` in addition to the usual
    /// `didChange`.
    BufferChanged {
        path: CanonPath,
        rope: Arc<Rope>,
        version: BufferVersion,
        is_save: bool,
    },
    /// Buffer killed. The driver emits `textDocument/didClose`
    /// and drops any cached push diagnostics for the path.
    BufferClosed { path: CanonPath },
    /// Open a propagation window. Per `DiagnosticSource` this
    /// either (push mode) forwards the current push cache, or
    /// (pull mode) freezes the command queue, snapshots every
    /// opened buffer's version, and issues
    /// `textDocument/diagnostic` for each one.
    RequestDiagnostics,
}

/// Driver → runtime events. The runtime folds these into its
/// atoms.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// Diagnostics for one path, stamped with the buffer version
    /// they were pulled against. The runtime runs
    /// `offer_diagnostics` (stage 3) to decide: accept as-is,
    /// replay through the edit log, or drop silently.
    Diagnostics {
        path: CanonPath,
        version: BufferVersion,
        diagnostics: Vec<Diagnostic>,
    },
    /// First `quiescent=true` emitted by a server that supports
    /// `experimental/serverStatus`. One-shot per server — the
    /// runtime unblocks its init-deferred `RequestDiagnostics` on
    /// this event. See `DiagnosticSource::on_quiescence`.
    Ready { server: String },
    /// Progress breadcrumb for the status bar. `busy` is `false`
    /// when the server is idle; `detail` is the human-readable
    /// message the server reports (e.g. "indexing crates").
    Progress {
        server: String,
        busy: bool,
        detail: Option<String>,
    },
    /// Non-fatal server error. The runtime surfaces this as a
    /// warn alert keyed by `server`.
    Error { server: String, message: String },
}

// ── Trace ──────────────────────────────────────────────────────

/// Narrow trace hook used by the native driver. The runtime's
/// unified `Trace` delegates through an adapter, matching every
/// other driver's pattern.
pub trait Trace: Send + Sync {
    fn lsp_server_started(&self, server: &str);
    fn lsp_request_diagnostics(&self);
    fn lsp_diagnostics_done(&self, path: &CanonPath, n: usize, version: BufferVersion);
    fn lsp_mode_fallback(&self);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn lsp_server_started(&self, _: &str) {}
    fn lsp_request_diagnostics(&self) {}
    fn lsp_diagnostics_done(&self, _: &CanonPath, _: usize, _: BufferVersion) {}
    fn lsp_mode_fallback(&self) {}
}

// ── Driver handle ──────────────────────────────────────────────

/// Main-loop-facing half of the driver. Owns the `Sender` for
/// commands and the `Receiver` for events. Constructed by the
/// native `spawn()` alongside an opaque lifetime marker.
pub struct LspDriver {
    tx: Sender<LspCmd>,
    rx: Receiver<LspEvent>,
    trace: Arc<dyn Trace>,
}

impl LspDriver {
    pub fn new(tx: Sender<LspCmd>, rx: Receiver<LspEvent>, trace: Arc<dyn Trace>) -> Self {
        Self { tx, rx, trace }
    }

    /// Ship a batch of commands. The worker coalesces / reorders
    /// internally (e.g. a `RequestDiagnostics` arriving while a
    /// pull window is frozen queues until the window closes).
    pub fn execute<'a>(&self, cmds: impl IntoIterator<Item = &'a LspCmd>) {
        for cmd in cmds {
            if matches!(cmd, LspCmd::RequestDiagnostics) {
                self.trace.lsp_request_diagnostics();
            }
            let _ = self.tx.send(cmd.clone());
        }
    }

    /// Drain completions. Caller version-gates them via
    /// `offer_diagnostics`.
    pub fn process(&self) -> Vec<LspEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            if let LspEvent::Diagnostics {
                path,
                diagnostics,
                version,
            } = &ev
            {
                self.trace
                    .lsp_diagnostics_done(path, diagnostics.len(), *version);
            }
            out.push(ev);
        }
        out
    }
}
