//! Sync core of the LSP driver.
//!
//! Diagnostics for M16 and completions for M17; hover / code-
//! actions land in later milestones. The wire ABI (`LspCmd` /
//! `LspEvent`) and the `DiagnosticSource` state machine both
//! live here so the native driver and the runtime share the
//! same vocabulary and the state machine is testable without
//! tokio.
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

use led_core::{CanonPath, PersistedContentHash};
use led_state_diagnostics::Diagnostic;
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
    /// purposes. `hash` is the rope's content hash at open time
    /// — used by the diagnostic-source machinery to stamp
    /// deliveries with an anchor that's stable across undo /
    /// redo round-trips.
    BufferOpened {
        path: CanonPath,
        language: Option<led_state_syntax::Language>,
        rope: Arc<Rope>,
        hash: PersistedContentHash,
    },
    /// The rope changed. `is_save` is `true` when this change is
    /// the moment-of-save (dispatched by the save-handler after
    /// the writer confirms) — the driver uses it to emit
    /// `textDocument/didSave` in addition to the usual
    /// `didChange`. `hash` is the post-change content hash; the
    /// driver uses it to close any open diagnostic window whose
    /// snapshot no longer matches and, after saves, runs as the
    /// save-point anchor for the runtime's replay path.
    BufferChanged {
        path: CanonPath,
        rope: Arc<Rope>,
        hash: PersistedContentHash,
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
    /// Ask the server for completion items at `(line, col)` on
    /// `path`. `seq` is a monotonic sequence id the runtime
    /// allocates so the driver can drop stale responses and the
    /// runtime can ignore a completion event whose seq is older
    /// than the latest outstanding request. `trigger` is the
    /// character that caused the request (if any); the worker
    /// forwards it to the server as
    /// `CompletionContext.triggerCharacter` when the char is in
    /// the server-advertised `triggerCharacters` set, otherwise
    /// `triggerKind` is `Invoked`.
    RequestCompletion {
        path: CanonPath,
        seq: u64,
        line: u32,
        col: u32,
        trigger: Option<char>,
    },
    /// Ask the server to fill in an item's `additionalTextEdits`
    /// (and any other resolvable fields) via
    /// `completionItem/resolve`. Fired on commit when the
    /// selected item advertises `dataResolveNeeded`. `seq`
    /// identifies the commit action so the runtime can drop
    /// resolved edits that belong to a stale session.
    ResolveCompletion {
        path: CanonPath,
        seq: u64,
        item: CompletionItem,
    },
}

/// One completion candidate from the server. Trimmed to the
/// subset legacy's UI actually used: label + optional detail for
/// display, `sort_text` for tie-break ordering, `kind` carried
/// through so future milestones can style by category, and the
/// insertion payload (`text_edit` preferred, `insert_text`
/// fallback). `resolve_data` + `raw_json` carry the opaque
/// `CompletionItem.data` the server expects back on
/// `completionItem/resolve` — see legacy
/// `crates/lsp/src/manager.rs:1046-1080`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Primary display string + fuzzy-filter key.
    pub label: Arc<str>,
    /// Right-column hint (type signature, module path, …).
    pub detail: Option<Arc<str>>,
    /// LSP-advertised sort key. `None` falls back to `label`.
    pub sort_text: Option<Arc<str>>,
    /// What to insert when `text_edit` is absent. Falls back to
    /// `label` when both are missing.
    pub insert_text: Option<Arc<str>>,
    /// Preferred insertion — a (line, col_start, col_end,
    /// new_text) tuple. When present, overrides `insert_text`
    /// and gives the precise replacement range the server wants
    /// (e.g. "delete the typed prefix, insert full identifier").
    /// Ranges are 0-indexed, exclusive end, row/col in chars.
    pub text_edit: Option<CompletionTextEdit>,
    /// LSP `CompletionItemKind` as the raw u8 (1=Text, 2=Method,
    /// 3=Function, …). `None` when the server omits it. The
    /// runtime keeps this opaque for now; future milestones can
    /// use it for icon / colour.
    pub kind: Option<u8>,
    /// `true` when the server advertised
    /// `completionProvider.resolveProvider` AND this item still
    /// has unresolved fields (missing `additionalTextEdits` in
    /// the initial response). Drives whether the runtime fires
    /// `ResolveCompletion` on commit.
    pub resolve_needed: bool,
    /// Opaque server-specific identifier echoed on resolve. The
    /// native driver stores this and threads it through the
    /// resolve round-trip; the runtime never inspects it.
    pub resolve_data: Option<Arc<str>>,
}

/// Range-based insertion. `line` is the logical-line the edit
/// applies to (usually the cursor's current line); `col_start` /
/// `col_end` are char offsets within that line (exclusive end).
/// `new_text` is the literal replacement string. The runtime
/// applies this at commit time when present, overriding any
/// `insert_text` on the parent item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionTextEdit {
    pub line: u32,
    pub col_start: u32,
    pub col_end: u32,
    pub new_text: Arc<str>,
}

/// Driver → runtime events. The runtime folds these into its
/// atoms.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// Diagnostics for one path, stamped with the content hash
    /// they were pulled against. The runtime runs
    /// `offer_diagnostics` to decide: accept as-is (hash matches
    /// current), replay through the edit log since a save-point
    /// marker with a matching hash, or drop silently.
    Diagnostics {
        path: CanonPath,
        hash: PersistedContentHash,
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
    /// Completion response for a previous
    /// [`LspCmd::RequestCompletion`]. `seq` echoes the request
    /// id so the runtime can drop responses older than the
    /// latest in-flight request (typing fast races the server
    /// — stale completions would show obsolete items).
    /// `prefix_start_col` is the char col where the user's
    /// in-progress identifier starts; the runtime uses it to
    /// refilter client-side as the user keeps typing without
    /// re-hitting the server. `prefix_line` is the line the
    /// identifier sits on (== cursor line when the request
    /// fired).
    Completion {
        path: CanonPath,
        seq: u64,
        items: Arc<Vec<CompletionItem>>,
        prefix_line: u32,
        prefix_start_col: u32,
    },
    /// Response to [`LspCmd::ResolveCompletion`]. Carries the
    /// server's additional edits to apply AFTER the primary
    /// insertion landed (typically imports added at the top of
    /// the file). `seq` matches the originating resolve id.
    CompletionResolved {
        path: CanonPath,
        seq: u64,
        additional_edits: Vec<CompletionTextEdit>,
    },
}

// ── Trace ──────────────────────────────────────────────────────

/// Narrow trace hook used by the native driver. The runtime's
/// unified `Trace` delegates through an adapter, matching every
/// other driver's pattern.
pub trait Trace: Send + Sync {
    fn lsp_server_started(&self, server: &str);
    fn lsp_request_diagnostics(&self);
    fn lsp_diagnostics_done(&self, path: &CanonPath, n: usize, hash: PersistedContentHash);
    fn lsp_mode_fallback(&self);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn lsp_server_started(&self, _: &str) {}
    fn lsp_request_diagnostics(&self) {}
    fn lsp_diagnostics_done(&self, _: &CanonPath, _: usize, _: PersistedContentHash) {}
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
                hash,
            } = &ev
            {
                self.trace
                    .lsp_diagnostics_done(path, diagnostics.len(), *hash);
            }
            out.push(ev);
        }
        out
    }
}
