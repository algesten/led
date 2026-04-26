//! Sync core of the LSP driver.
//!
//! Diagnostics for M16, completions for M17, and the
//! goto-definition / rename / code-actions / format / inlay-hints
//! trio for M18. The wire ABI (`LspCmd` / `LspEvent`) and the
//! `DiagnosticSource` state machine both live here so the native
//! driver and the runtime share the same vocabulary and the
//! state machine is testable without tokio.
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
    /// `textDocument/definition` for the identifier at
    /// `(line, col)` on `path`. Answered by
    /// [`LspEvent::GotoDefinition`]; at most one location is
    /// forwarded back (the first LSP Location in the response).
    RequestGotoDefinition {
        path: CanonPath,
        seq: u64,
        line: u32,
        col: u32,
    },
    /// `textDocument/rename` — rename every occurrence of the
    /// symbol at `(line, col)` to `new_name`. Resulting
    /// `WorkspaceEdit` flattens to a `Vec<FileEdit>` delivered
    /// via [`LspEvent::Edits`] tagged `EditsOrigin::Rename`.
    RequestRename {
        path: CanonPath,
        seq: u64,
        line: u32,
        col: u32,
        new_name: Arc<str>,
    },
    /// `textDocument/codeAction` for the range `(start..end)`
    /// on `path`. Titles + resolve data come back as
    /// [`LspEvent::CodeActions`]; committing one subsequently
    /// fires [`LspCmd::SelectCodeAction`].
    RequestCodeAction {
        path: CanonPath,
        seq: u64,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    },
    /// Commit a code action the user picked from the picker.
    /// The summary carries whatever `resolve_data` the server
    /// originally attached so the native driver can issue a
    /// `codeAction/resolve` round-trip when `resolve_needed`
    /// is true. Resulting edits land as
    /// [`LspEvent::Edits { origin: CodeAction, .. }`].
    SelectCodeAction {
        path: CanonPath,
        seq: u64,
        action: CodeActionSummary,
    },
    /// `textDocument/formatting` for the whole file at `path`.
    /// Edits come back as
    /// [`LspEvent::Edits { origin: Format, .. }`]; an empty
    /// `edits` vector is the "no-op format / already formatted"
    /// signal that lets the dispatcher release any queued save.
    RequestFormat { path: CanonPath, seq: u64 },
    /// `textDocument/inlayHint` for the visible range.
    /// `version` is the buffer version the request was
    /// computed against — the runtime re-requests on version
    /// bump or viewport scroll. The response arrives as
    /// [`LspEvent::InlayHints`] stamped with the same version.
    RequestInlayHints {
        path: CanonPath,
        seq: u64,
        version: u64,
        start_line: u32,
        end_line: u32,
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

/// One point in a buffer. Used as the target of
/// [`LspEvent::GotoDefinition`] and inside [`TextEditOp`].
/// `line` / `col` are 0-indexed char offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: CanonPath,
    pub line: u32,
    pub col: u32,
}

/// One edit inside an LSP `WorkspaceEdit` or formatting
/// response. Ranges are `[start..end)` in char coordinates;
/// `new_text` replaces the range verbatim. Empty `new_text`
/// means "delete the range"; empty range + non-empty text
/// means "insert".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEditOp {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub new_text: Arc<str>,
}

/// A per-file bundle of edits. Results of rename, format, or
/// code-action resolve flatten to `Vec<FileEdit>`; the runtime
/// applies them buffer-by-buffer (opening a buffer if the path
/// isn't already loaded is out of scope for M18 — legacy
/// parity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: CanonPath,
    pub edits: Vec<TextEditOp>,
}

/// One LSP inlay hint — a short label the server wants the
/// editor to render as ghost text at `(line, col)`. `padding_left` /
/// `padding_right` are the spec's optional flags for controlling
/// whether the label abuts or pads from the surrounding text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub line: u32,
    pub col: u32,
    pub label: Arc<str>,
    pub padding_left: bool,
    pub padding_right: bool,
}

/// Picker-facing summary of a `CodeAction` from the server.
/// The native driver stores the server's raw item alongside so
/// selection can round-trip through `codeAction/resolve`
/// without the runtime having to understand LSP shapes.
///
/// `action_id` is an opaque string the native driver assigns
/// so [`LspCmd::SelectCodeAction`] can look the raw item back
/// up without threading `lsp_types::CodeActionOrCommand`
/// values through the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeActionSummary {
    pub title: Arc<str>,
    pub kind: Option<Arc<str>>,
    /// `true` when the action ships without an `edit` field —
    /// the native driver must issue `codeAction/resolve` on
    /// selection to obtain the edits.
    pub resolve_needed: bool,
    /// Driver-internal id. Carried through
    /// [`LspCmd::SelectCodeAction`] verbatim so the native
    /// driver can match it to its stored
    /// `lsp_types::CodeActionOrCommand`.
    pub action_id: Arc<str>,
}

/// Which RPC produced an [`LspEvent::Edits`] delivery. Lets the
/// runtime decide what post-edit bookkeeping is needed — save
/// is unlocked on `Format` only, jump record is cleared on
/// `Rename`, no-op otherwise for `CodeAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditsOrigin {
    Rename,
    CodeAction,
    Format,
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
    /// in-progress identifier starts when the server told us via
    /// a `textEdit.range`; `None` means no item carried a range,
    /// in which case the runtime backtracks through identifier
    /// characters from the cursor to find the prefix start (the
    /// driver doesn't have rope access to do this itself).
    /// `prefix_line` is the line the identifier sits on (== cursor
    /// line when the request fired).
    Completion {
        path: CanonPath,
        seq: u64,
        items: Arc<Vec<CompletionItem>>,
        prefix_line: u32,
        prefix_start_col: Option<u32>,
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
    /// Response to [`LspCmd::RequestGotoDefinition`]. `location`
    /// is `Some` when the server returned at least one
    /// Location; we forward the first entry verbatim.
    /// `None` signals "no match" so the dispatcher can surface
    /// a "no definition found" alert.
    GotoDefinition {
        seq: u64,
        location: Option<Location>,
    },
    /// Response to rename / code-action-select / format. The
    /// runtime flattens each `FileEdit` into a buffer edit (and
    /// records history) for the buffers it has open; edits for
    /// unopened paths are intentionally skipped. `origin` is
    /// opaque metadata the runtime uses to decide post-edit
    /// bookkeeping (save unlock for `Format`, jump clear for
    /// `Rename`).
    Edits {
        seq: u64,
        origin: EditsOrigin,
        edits: Arc<Vec<FileEdit>>,
    },
    /// Response to [`LspCmd::RequestCodeAction`]. Titles-only
    /// surface — the native driver keeps raw items keyed by
    /// `action_id` so selection round-trips through
    /// [`LspCmd::SelectCodeAction`] without the runtime seeing
    /// LSP shapes.
    CodeActions {
        path: CanonPath,
        seq: u64,
        actions: Arc<Vec<CodeActionSummary>>,
    },
    /// Response to [`LspCmd::RequestInlayHints`]. `version`
    /// echoes the buffer version the request was issued
    /// against so stale replies don't clobber hints painted
    /// for a newer rope.
    InlayHints {
        path: CanonPath,
        version: u64,
        hints: Arc<Vec<InlayHint>>,
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
    /// Outbound JSON-RPC request to the server. `path_uri` is the
    /// `textDocument.uri` field when the method targets a single
    /// document (definition, rename, codeAction, completion, …).
    fn lsp_send_request(
        &self,
        server: &str,
        method: &str,
        id: i64,
        path_uri: Option<&str>,
    );
    /// Outbound JSON-RPC notification. `path_uri` + `version` are
    /// `Some` for `textDocument/didOpen` / `didChange` / `didSave`
    /// / `didClose`; both `None` for workspace-wide notifications
    /// (`initialized`, `workspace/didChangeConfiguration`, `exit`).
    fn lsp_send_notification(
        &self,
        server: &str,
        method: &str,
        path_uri: Option<&str>,
        version: Option<i32>,
    );
    /// Inbound JSON-RPC response correlated by `id` to a previous
    /// `lsp_send_request`.
    fn lsp_recv_response(&self, server: &str, id: i64);
    /// Inbound JSON-RPC notification (`$/progress`,
    /// `textDocument/publishDiagnostics`, server status, …).
    fn lsp_recv_notification(&self, server: &str, method: &str);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn lsp_server_started(&self, _: &str) {}
    fn lsp_request_diagnostics(&self) {}
    fn lsp_diagnostics_done(&self, _: &CanonPath, _: usize, _: PersistedContentHash) {}
    fn lsp_mode_fallback(&self) {}
    fn lsp_send_request(&self, _: &str, _: &str, _: i64, _: Option<&str>) {}
    fn lsp_send_notification(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<i32>,
    ) {
    }
    fn lsp_recv_response(&self, _: &str, _: i64) {}
    fn lsp_recv_notification(&self, _: &str, _: &str) {}
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
