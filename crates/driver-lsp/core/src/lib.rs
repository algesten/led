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

use led_core::{
    BufferStateSum, BufferVersion, CanonPath, LspRequestSeq, PersistedContentHash, ServerId,
};
use led_state_diagnostics::Diagnostic;
use led_state_syntax::Language;
use ropey::Rope;

pub mod diag_source;
pub mod diag_states;

pub use diag_source::{DiagMode, DiagPushResult, DiagnosticSource};
pub use diag_states::{BufferDiagnostics, DiagnosticsStates, LspServerStatus, LspStatuses};

// ── Driver-owned source ─────────────────────────────────────────
//
// Per EXAMPLE-ARCH § "Stateless drivers still need an in-flight
// source": the LSP driver carries an `LspState` that records what
// has been dispatched and what's still in flight, written by
// `execute` and cleared by `process`. Memos consult it to gate
// re-firing the same command while an outstanding response is
// still pending.

/// Which RPC a still-outstanding [`LspRequestSeq`] belongs to.
/// Stored in [`LspState::in_flight`] so the matching event can
/// clear the entry on arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Diagnostics,
    Completion,
    ResolveCompletion,
    GotoDefinition,
    Rename,
    CodeAction,
    SelectCodeAction,
    Format,
    InlayHints,
}

/// Per-buffer pull-diagnostic gating state. The runtime's
/// "should we re-fire RequestDiagnostics?" memo reads this so a
/// previously-failed pull doesn't keep re-firing on every tick.
///
/// `Idle` is the safe default — memos may re-fire freely. `Pending`
/// means a pull is outstanding; the runtime should NOT re-fire
/// until the matching response (or `LspEvent::Error`) clears it.
/// `Done` means the last pull completed successfully; the runtime
/// only re-fires when the gating sum advances. `Failed` means the
/// last pull errored out — the runtime should not re-fire for this
/// path until the next save event explicitly advances the gate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PullState {
    #[default]
    Idle,
    Pending(LspRequestSeq),
    Done,
    Failed(Arc<str>),
}

/// Per-language LSP server lifecycle status, populated as the
/// driver's `Init` / `Ready` / `Progress` / `Error` events arrive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServerStatus {
    #[default]
    NotStarted,
    Initializing,
    Ready,
    Failed,
}

/// Driver-owned source for the LSP driver. Mutated by
/// [`LspDriver::execute`] (records intent) and
/// [`LspDriver::process`] (clears matching slots on each arriving
/// event). Plain struct, like every other driver source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LspState {
    /// `true` once the runtime has dispatched [`LspCmd::Init`].
    /// Driver source is the single point of truth for "has the
    /// workspace been announced to the LSP layer?" — the runtime's
    /// init-dispatch gate reads this directly rather than mirroring
    /// it on AppState.
    pub init_sent: bool,
    /// Outstanding LSP RPCs keyed by sequence id. Cleared by
    /// `process` when the matching response (or `Error`) arrives.
    /// Lets future memos gate "is this seq still in flight?"
    /// without inspecting individual `LspEvent::*` payloads.
    pub in_flight: imbl::HashMap<LspRequestSeq, RequestKind>,
    /// Per-buffer diagnostic-pull gate. The runtime's
    /// `desired_diagnostic_pull` memo (or the inline
    /// `should_request_diag` predicate today) consults this
    /// before re-firing `RequestDiagnostics` so a previously-
    /// failed pull doesn't re-fire on every tick. Populated by
    /// `execute` (Pending on emission) and `process` (Done on
    /// `LspEvent::Diagnostics`, Failed on `LspEvent::Error` that
    /// carries a path).
    pub pull_state: imbl::HashMap<CanonPath, PullState>,
    /// Per-language LSP server status — populated by
    /// `process` when `LspEvent::Ready` / `Progress` / `Error`
    /// events arrive. The `Language` keying matches the native
    /// manager's per-language `ServerEntry` map.
    pub server_status: imbl::HashMap<Language, ServerStatus>,
    /// Most recent non-fatal error message the LSP layer
    /// surfaced, mirroring [`led_driver_clipboard_core::
    /// ClipboardState::last_error`]. The runtime can use it to
    /// avoid spamming the user with a stream of identical
    /// warnings — same pattern as the clipboard driver.
    pub last_error: Option<Arc<str>>,
    /// `Some(sum)` holds Σ(version + saved_version) at the last
    /// `RequestDiagnostics` emission; `None` means we've never
    /// fired one. Driver-outbound bookkeeping: tracks a side-
    /// effect the runtime emitted via this driver, so it lives
    /// here rather than mirrored on AppState. The dispatch gate
    /// compares the current buffer-state sum against this to
    /// decide whether to re-fire.
    pub requested_state_sum: Option<BufferStateSum>,
}

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
        seq: LspRequestSeq,
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
        seq: LspRequestSeq,
        item: CompletionItem,
    },
    /// `textDocument/definition` for the identifier at
    /// `(line, col)` on `path`. Answered by
    /// [`LspEvent::GotoDefinition`]; at most one location is
    /// forwarded back (the first LSP Location in the response).
    RequestGotoDefinition {
        path: CanonPath,
        seq: LspRequestSeq,
        line: u32,
        col: u32,
    },
    /// `textDocument/rename` — rename every occurrence of the
    /// symbol at `(line, col)` to `new_name`. Resulting
    /// `WorkspaceEdit` flattens to a `Vec<FileEdit>` delivered
    /// via [`LspEvent::Edits`] tagged `EditsOrigin::Rename`.
    RequestRename {
        path: CanonPath,
        seq: LspRequestSeq,
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
        seq: LspRequestSeq,
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
        seq: LspRequestSeq,
        action: CodeActionSummary,
    },
    /// `textDocument/formatting` for the whole file at `path`.
    /// Edits come back as
    /// [`LspEvent::Edits { origin: Format, .. }`]; an empty
    /// `edits` vector is the "no-op format / already formatted"
    /// signal that lets the dispatcher release any queued save.
    RequestFormat { path: CanonPath, seq: LspRequestSeq },
    /// `textDocument/inlayHint` for the visible range.
    /// `version` is the buffer version the request was
    /// computed against — the runtime re-requests on version
    /// bump or viewport scroll. The response arrives as
    /// [`LspEvent::InlayHints`] stamped with the same version.
    RequestInlayHints {
        path: CanonPath,
        seq: LspRequestSeq,
        version: BufferVersion,
        start_line: u32,
        end_line: u32,
    },
    /// `workspace/didChangeWatchedFiles` notification — fan-out
    /// of filesystem changes the server registered globs for.
    /// `server` is the short server name (e.g. `rust-analyzer`)
    /// the runtime memo resolved when matching paths against the
    /// per-server glob set. `changes` is the LSP `FileEvent[]`
    /// payload, already filtered to the matching server's globs.
    DidChangeWatchedFiles {
        server: ServerId,
        changes: Vec<FileEvent>,
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

pub use led_abi_lsp::InlayHint;

/// One filesystem change crossing as an LSP
/// `workspace/didChangeWatchedFiles` notification entry. The
/// runtime fan-out memo emits these on a per-server basis; the
/// native worker percent-encodes `path` into a `file://` URI
/// when serialising to the wire (URI rendering is platform-
/// adjacent, lives in `driver-lsp/native`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    pub path: CanonPath,
    pub kind: FileEventKind,
}

/// LSP `FileChangeType` (1=Created, 2=Changed, 3=Deleted). The
/// numeric values are wire-stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

pub use led_abi_lsp::{CodeActionSummary, RegistrationGlob};

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
/// sources.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// Diagnostics for one path, stamped with the content hash
    /// they were pulled against. **Primary channel: pull-only.**
    /// Emitted by `textDocument/diagnostic` pull responses. The
    /// runtime runs `offer_diagnostics` to decide: accept as-is
    /// (hash matches current), replay through the edit log since
    /// a save-point marker with a matching hash, or drop silently.
    ///
    /// Pushes never arrive on this channel — see
    /// [`LspEvent::PushFallback`] for the push-fallback path.
    Diagnostics {
        path: CanonPath,
        hash: PersistedContentHash,
        diagnostics: Vec<Diagnostic>,
    },
    /// Push-fallback diagnostics for one path. Emitted by
    /// `textDocument/publishDiagnostics` pushes (either while the
    /// server is in push-only mode, or while a pull-capable server
    /// has no in-flight pull for this path). The runtime fold
    /// accepts these only when no `LspEvent::Diagnostics` has
    /// landed for the path — pull always wins. Carries the same
    /// content-hash stamping discipline as `Diagnostics` so the
    /// replay path works identically.
    ///
    /// Per audit Theme L Target C + the project rule "Pull
    /// diagnostics only": this channel exists so push events
    /// remain visible (push-only servers need it) without
    /// contaminating the primary pull channel.
    PushFallback {
        path: CanonPath,
        hash: PersistedContentHash,
        diagnostics: Vec<Diagnostic>,
    },
    /// First `quiescent=true` emitted by a server that supports
    /// `experimental/serverStatus`. One-shot per server — the
    /// runtime unblocks its init-deferred `RequestDiagnostics` on
    /// this event. See `DiagnosticSource::on_quiescence`.
    Ready { server: ServerId },
    /// Progress breadcrumb for the status bar. `busy` is `false`
    /// when the server is idle; `detail` is the human-readable
    /// message the server reports (e.g. "indexing crates").
    Progress {
        server: ServerId,
        busy: bool,
        detail: Option<String>,
    },
    /// Non-fatal server error. The runtime surfaces this as a
    /// warn alert keyed by `server`.
    Error { server: ServerId, message: String },
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
        seq: LspRequestSeq,
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
        seq: LspRequestSeq,
        additional_edits: Vec<CompletionTextEdit>,
    },
    /// Response to [`LspCmd::RequestGotoDefinition`]. `location`
    /// is `Some` when the server returned at least one
    /// Location; we forward the first entry verbatim.
    /// `None` signals "no match" so the dispatcher can surface
    /// a "no definition found" alert.
    GotoDefinition {
        seq: LspRequestSeq,
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
        seq: LspRequestSeq,
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
        seq: LspRequestSeq,
        actions: Arc<Vec<CodeActionSummary>>,
    },
    /// Response to [`LspCmd::RequestInlayHints`]. `version`
    /// echoes the buffer version the request was issued
    /// against so stale replies don't clobber hints painted
    /// for a newer rope.
    InlayHints {
        path: CanonPath,
        version: BufferVersion,
        hints: Arc<Vec<InlayHint>>,
    },
    /// Server registered a fresh `workspace/didChangeWatchedFiles`
    /// glob set via `client/registerCapability`. The runtime
    /// folds this into `LspWatchedGlobs.by_server`, replacing the
    /// list keyed by `(server, registration_id)`. Multiple
    /// registrations per server are valid (different globs for
    /// different concerns); each one carries its own id.
    WatchedFilesRegistered {
        server: ServerId,
        registration_id: String,
        globs: Arc<Vec<RegistrationGlob>>,
    },
    /// Server retracted a prior registration via
    /// `client/unregisterCapability`. Symmetric to
    /// [`LspEvent::WatchedFilesRegistered`].
    WatchedFilesUnregistered {
        server: ServerId,
        registration_id: String,
    },
    /// A `textDocument/diagnostic` pull came back as an error
    /// response (server reported a JSON-RPC error). The runtime
    /// stamps the per-path `pull_state` to `Failed(message)` so
    /// the gating memo stops re-firing the same pull. Distinct
    /// from [`LspEvent::Error`] (which is a server-scoped status
    /// surface) — `PullFailed` is per-path bookkeeping.
    PullFailed {
        path: CanonPath,
        message: Arc<str>,
    },
}

// ── Trace ──────────────────────────────────────────────────────

/// Narrow trace hook used by the native driver. The runtime's
/// unified `Trace` delegates through an adapter, matching every
/// other driver's pattern.
pub trait Trace: Send + Sync {
    fn lsp_server_started(&self, server: &ServerId);
    fn lsp_request_diagnostics(&self);
    fn lsp_diagnostics_done(&self, path: &CanonPath, n: usize, hash: PersistedContentHash);
    /// Legacy hook for pull→push mode-fallback. Per audit Theme L
    /// Target C the runtime-time downgrade was removed; this hook
    /// is no longer called from the driver but stays on the trait
    /// for shape stability.
    fn lsp_mode_fallback(&self);
    /// Outbound JSON-RPC request to the server. `path_uri` is the
    /// `textDocument.uri` field when the method targets a single
    /// document (definition, rename, codeAction, completion, …).
    fn lsp_send_request(
        &self,
        server: &ServerId,
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
        server: &ServerId,
        method: &str,
        path_uri: Option<&str>,
        version: Option<i32>,
    );
    /// Inbound JSON-RPC response correlated by `id` to a previous
    /// `lsp_send_request`.
    fn lsp_recv_response(&self, server: &ServerId, id: i64);
    /// Inbound JSON-RPC notification (`$/progress`,
    /// `textDocument/publishDiagnostics`, server status, …).
    fn lsp_recv_notification(&self, server: &ServerId, method: &str);
    /// Inbound JSON-RPC request from the server
    /// (`client/registerCapability`,
    /// `client/unregisterCapability`,
    /// `workspace/configuration`, `window/workDoneProgress/create`,
    /// …). Symmetric to `lsp_recv_notification`; the reply ships
    /// via the auto-reply path on `lsp_send_response`-equivalent
    /// machinery (we currently emit the auto-ack inline without
    /// a separate trace line).
    fn lsp_recv_request(&self, server: &ServerId, method: &str, id: i64);
}

pub struct NoopTrace;
impl Trace for NoopTrace {
    fn lsp_server_started(&self, _: &ServerId) {}
    fn lsp_request_diagnostics(&self) {}
    fn lsp_diagnostics_done(&self, _: &CanonPath, _: usize, _: PersistedContentHash) {}
    fn lsp_mode_fallback(&self) {}
    fn lsp_send_request(&self, _: &ServerId, _: &str, _: i64, _: Option<&str>) {}
    fn lsp_send_notification(
        &self,
        _: &ServerId,
        _: &str,
        _: Option<&str>,
        _: Option<i32>,
    ) {
    }
    fn lsp_recv_response(&self, _: &ServerId, _: i64) {}
    fn lsp_recv_notification(&self, _: &ServerId, _: &str) {}
    fn lsp_recv_request(&self, _: &ServerId, _: &str, _: i64) {}
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
    ///
    /// Updates the driver source synchronously per EXAMPLE-ARCH §
    /// "Stateless drivers still need an in-flight source": each
    /// seq-bearing cmd registers an `in_flight[seq] = kind` slot,
    /// `RequestDiagnostics` flips per-buffer `pull_state` Pending
    /// for every currently-Idle path so a memo that re-derives
    /// "should we re-fire?" sees the in-flight gate immediately.
    pub fn execute<'a>(
        &self,
        cmds: impl IntoIterator<Item = &'a LspCmd>,
        state: &mut LspState,
    ) {
        for cmd in cmds {
            match cmd {
                LspCmd::Init { .. } => {
                    state.init_sent = true;
                }
                LspCmd::RequestDiagnostics => {
                    self.trace.lsp_request_diagnostics();
                    // Flip every currently-Idle path to Pending
                    // (with a synthetic seq sentinel — the real
                    // per-path correlation lives in the native
                    // worker). `Failed` entries are NOT reset
                    // here: that's exactly the "stop re-firing"
                    // guarantee — only the next ingest of a
                    // Diagnostics / Error event can rotate them.
                    for (_path, value) in state.pull_state.iter_mut() {
                        if matches!(value, PullState::Idle | PullState::Done) {
                            *value = PullState::Pending(LspRequestSeq::default());
                        }
                    }
                }
                LspCmd::RequestCompletion { seq, .. } => {
                    state.in_flight.insert(*seq, RequestKind::Completion);
                }
                LspCmd::ResolveCompletion { seq, .. } => {
                    state
                        .in_flight
                        .insert(*seq, RequestKind::ResolveCompletion);
                }
                LspCmd::RequestGotoDefinition { seq, .. } => {
                    state
                        .in_flight
                        .insert(*seq, RequestKind::GotoDefinition);
                }
                LspCmd::RequestRename { seq, .. } => {
                    state.in_flight.insert(*seq, RequestKind::Rename);
                }
                LspCmd::RequestCodeAction { seq, .. } => {
                    state.in_flight.insert(*seq, RequestKind::CodeAction);
                }
                LspCmd::SelectCodeAction { seq, .. } => {
                    state
                        .in_flight
                        .insert(*seq, RequestKind::SelectCodeAction);
                }
                LspCmd::RequestFormat { seq, .. } => {
                    state.in_flight.insert(*seq, RequestKind::Format);
                }
                LspCmd::RequestInlayHints { seq, .. } => {
                    state.in_flight.insert(*seq, RequestKind::InlayHints);
                }
                LspCmd::Shutdown
                | LspCmd::BufferOpened { .. }
                | LspCmd::BufferChanged { .. }
                | LspCmd::BufferClosed { .. }
                | LspCmd::DidChangeWatchedFiles { .. } => {}
            }
            // `BufferClosed` drops any pull-state entry for the
            // path — the gate must reset when the buffer leaves
            // the LSP's purview.
            if let LspCmd::BufferClosed { path } = cmd {
                state.pull_state.remove(path);
            }
            let _ = self.tx.send(cmd.clone());
        }
    }

    /// Drain completions and reconcile the driver source. Caller
    /// version-gates the `Diagnostics` payload via
    /// `offer_diagnostics` AFTER this returns.
    pub fn process(&self, state: &mut LspState) -> Vec<LspEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            match &ev {
                LspEvent::Diagnostics {
                    path,
                    diagnostics,
                    hash,
                } => {
                    self.trace
                        .lsp_diagnostics_done(path, diagnostics.len(), *hash);
                    state.pull_state.insert(path.clone(), PullState::Done);
                }
                LspEvent::Ready { .. } => {
                    // `Ready` is server-keyed by `ServerId` (the
                    // shortened binary basename); the language ↔
                    // server mapping is owned by the native
                    // manager. Without a language we can't update
                    // the per-language slot from this event alone,
                    // so leave the `server_status` map to be
                    // populated by future events that carry a
                    // language. The `last_error` reset on `Ready`
                    // matches the "the server is healthy again"
                    // semantic.
                    state.last_error = None;
                }
                LspEvent::Progress { .. } => {
                    // Progress is a status-bar breadcrumb; same
                    // story as Ready re: language mapping.
                }
                LspEvent::Error { message, .. } => {
                    let msg: Arc<str> = Arc::from(message.as_str());
                    state.last_error = Some(msg.clone());
                    // Best-effort: every currently-Pending pull
                    // becomes Failed so the gating memo stops
                    // re-firing. The native worker doesn't yet
                    // carry per-path failure info on `Error`, so
                    // this conservatively fails every outstanding
                    // pull. A future ABI extension can target
                    // failures precisely.
                    for (_path, value) in state.pull_state.iter_mut() {
                        if matches!(value, PullState::Pending(_)) {
                            *value = PullState::Failed(msg.clone());
                        }
                    }
                }
                LspEvent::Completion { seq, .. } => {
                    state.in_flight.remove(seq);
                }
                LspEvent::CompletionResolved { seq, .. } => {
                    state.in_flight.remove(seq);
                }
                LspEvent::GotoDefinition { seq, .. } => {
                    state.in_flight.remove(seq);
                }
                LspEvent::Edits { seq, .. } => {
                    state.in_flight.remove(seq);
                }
                LspEvent::CodeActions { seq, .. } => {
                    state.in_flight.remove(seq);
                }
                LspEvent::PullFailed { path, message } => {
                    state
                        .pull_state
                        .insert(path.clone(), PullState::Failed(message.clone()));
                    state.last_error = Some(message.clone());
                }
                LspEvent::PushFallback { .. } => {
                    // Push fallback never advances `pull_state` —
                    // the primary `LspEvent::Diagnostics` channel
                    // is the only one that records pull progress.
                    // Leaving pull_state unchanged lets the runtime
                    // memo gate further pulls correctly: a Pending
                    // pull stays Pending until its own response
                    // (or `PullFailed`) lands.
                }
                LspEvent::InlayHints { .. }
                | LspEvent::WatchedFilesRegistered { .. }
                | LspEvent::WatchedFilesUnregistered { .. } => {}
            }
            out.push(ev);
        }
        out
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use led_core::UserPath;
    use std::sync::mpsc;

    fn p(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
    }

    fn driver() -> (LspDriver, mpsc::Receiver<LspCmd>, mpsc::Sender<LspEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<LspCmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<LspEvent>();
        (LspDriver::new(cmd_tx, ev_rx, Arc::new(NoopTrace)), cmd_rx, ev_tx)
    }

    #[test]
    fn execute_init_sets_init_sent() {
        let (drv, _cmd_rx, _ev_tx) = driver();
        let mut state = LspState::default();
        drv.execute(
            [&LspCmd::Init { root: p("/tmp") }],
            &mut state,
        );
        assert!(state.init_sent);
    }

    #[test]
    fn execute_seq_cmds_register_in_flight() {
        let (drv, _cmd_rx, _ev_tx) = driver();
        let mut state = LspState::default();
        let seq = LspRequestSeq(7);
        drv.execute(
            [&LspCmd::RequestGotoDefinition {
                path: p("/a.rs"),
                seq,
                line: 0,
                col: 0,
            }],
            &mut state,
        );
        assert_eq!(state.in_flight.get(&seq), Some(&RequestKind::GotoDefinition));
    }

    #[test]
    fn process_clears_in_flight_on_matching_event() {
        let (drv, _cmd_rx, ev_tx) = driver();
        let mut state = LspState::default();
        let seq = LspRequestSeq(42);
        state.in_flight.insert(seq, RequestKind::GotoDefinition);
        ev_tx
            .send(LspEvent::GotoDefinition {
                seq,
                location: None,
            })
            .unwrap();
        let _ = drv.process(&mut state);
        assert!(state.in_flight.get(&seq).is_none());
    }

    #[test]
    fn process_records_last_error() {
        let (drv, _cmd_rx, ev_tx) = driver();
        let mut state = LspState::default();
        ev_tx
            .send(LspEvent::Error {
                server: ServerId::new("rust-analyzer"),
                message: "boom".into(),
            })
            .unwrap();
        let _ = drv.process(&mut state);
        assert_eq!(state.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn process_marks_pull_state_failed_on_error() {
        let (drv, _cmd_rx, ev_tx) = driver();
        let mut state = LspState::default();
        let path = p("/a.rs");
        state
            .pull_state
            .insert(path.clone(), PullState::Pending(LspRequestSeq(1)));
        ev_tx
            .send(LspEvent::Error {
                server: ServerId::new("rust-analyzer"),
                message: "boom".into(),
            })
            .unwrap();
        let _ = drv.process(&mut state);
        match state.pull_state.get(&path) {
            Some(PullState::Failed(msg)) => assert_eq!(&**msg, "boom"),
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn execute_request_diagnostics_flips_idle_to_pending() {
        let (drv, _cmd_rx, _ev_tx) = driver();
        let mut state = LspState::default();
        let path = p("/a.rs");
        state.pull_state.insert(path.clone(), PullState::Idle);
        drv.execute([&LspCmd::RequestDiagnostics], &mut state);
        assert!(matches!(
            state.pull_state.get(&path),
            Some(PullState::Pending(_))
        ));
    }

    #[test]
    fn execute_request_diagnostics_does_not_reset_failed() {
        let (drv, _cmd_rx, _ev_tx) = driver();
        let mut state = LspState::default();
        let path = p("/a.rs");
        state
            .pull_state
            .insert(path.clone(), PullState::Failed(Arc::from("prior")));
        drv.execute([&LspCmd::RequestDiagnostics], &mut state);
        // Failed must persist — that's the "stop re-firing"
        // guarantee. Only an explicit BufferClosed (or a future
        // saved-version advance) clears Failed.
        match state.pull_state.get(&path) {
            Some(PullState::Failed(_)) => {}
            other => panic!("Failed must persist, got {:?}", other),
        }
    }

    #[test]
    fn process_diagnostics_marks_pull_state_done() {
        let (drv, _cmd_rx, ev_tx) = driver();
        let mut state = LspState::default();
        let path = p("/a.rs");
        state
            .pull_state
            .insert(path.clone(), PullState::Pending(LspRequestSeq(3)));
        ev_tx
            .send(LspEvent::Diagnostics {
                path: path.clone(),
                hash: PersistedContentHash(0),
                diagnostics: Vec::new(),
            })
            .unwrap();
        let _ = drv.process(&mut state);
        assert_eq!(state.pull_state.get(&path), Some(&PullState::Done));
    }

    #[test]
    fn execute_buffer_closed_drops_pull_state() {
        let (drv, _cmd_rx, _ev_tx) = driver();
        let mut state = LspState::default();
        let path = p("/a.rs");
        state.pull_state.insert(path.clone(), PullState::Done);
        drv.execute(
            [&LspCmd::BufferClosed { path: path.clone() }],
            &mut state,
        );
        assert!(state.pull_state.get(&path).is_none());
    }
}
