//! Manager thread: owns per-language server state, drives the
//! initialize handshake, routes `LspCmd` → server notifications,
//! routes server responses / notifications → `LspEvent`.
//!
//! # Channel topology
//!
//! ```text
//!     runtime ─── LspCmd ─────┐
//!                             ▼
//!                     cmd adapter thread
//!                             │
//!                  ManagerEvent::Cmd(...)
//!                             │
//!                             ▼
//!   ┌─────────── central mpsc (event_rx) ────────────┐
//!   │                                                │
//!   ▼                                                │
//! manager thread                                     │
//!   │                                                │
//!   ├── send_body(frame) ──▶ writer thread ──▶ stdin │
//!   │                                                │
//!   │                        stdout ──▶ reader thread│
//!   │                                                │
//!   │                   ManagerEvent::ServerMessage ─┘
//!   ▼
//! lsp_event_tx ─── LspEvent ──▶ runtime
//! ```
//!
//! std::thread + std::sync::mpsc throughout — same shape as every
//! other `*-native` crate in the rewrite. No tokio.
//!
//! # Freeze discipline
//!
//! When any server's `DiagnosticSource` is frozen (pull-mode
//! window active), incoming `Cmd` events are stashed in a local
//! `deferred_cmds` deque instead of dispatched. Server messages
//! continue to process normally — the freeze only pauses the
//! runtime-driven side of the pipe. Once every server's freeze
//! lifts (either all pulls responded or the 5-second deadline
//! expired), the deque drains in order and cmd dispatch resumes.
//!
//! The main recv loop uses `recv_timeout(earliest_deadline)` so
//! deadlines are honoured even with zero server traffic.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use led_core::{CanonPath, Notifier, PersistedContentHash};
use led_driver_lsp_core::{
    DiagnosticSource, LspCmd, LspEvent, Trace,
    diag_source::{DiagMode, DiagPushResult},
};
use led_state_diagnostics::{Diagnostic, DiagnosticSeverity};
use led_state_syntax::Language;
use ropey::Rope;
use serde_json::{Value, json};

use crate::classify::{Incoming, RequestId};
use crate::protocol::{
    InitializeCapabilities, build_initialize_request,
    build_did_change_configuration_notification, build_initialized_notification, language_id,
    parse_completion_response, parse_initialize_response, parse_resolve_additional_edits,
    path_from_uri, uri_from_path,
};
use crate::registry::LspRegistry;
use crate::subprocess::{Server, ServerIncoming};

/// Unified event the manager thread drains out of one central
/// channel. Cmd + server-message get merged here so the manager
/// blocks on a single `recv()` instead of juggling multiple.
enum ManagerEvent {
    /// Forwarded from the runtime.
    Cmd(LspCmd),
    /// Forwarded from a server's reader thread.
    ServerMessage(ServerIncoming),
    /// `cmd_rx` dropped — the runtime is shutting down.
    CmdChannelClosed,
}

/// A queued `BufferOpened` waiting for the server's initialize
/// response to come back before we can fire the `didOpen`
/// notification.
struct PendingOpen {
    path: CanonPath,
    rope: Arc<Rope>,
    hash: PersistedContentHash,
}

/// A pending JSON-RPC request. Keyed by request-id on the
/// server entry's `pending_requests` map so a response can find
/// its context.
enum PendingRequest {
    /// Waiting on the initialize response.
    Initialize,
    /// Waiting on a `textDocument/diagnostic` pull response for
    /// `path`. `DiagnosticSource::on_pull_response` consumes the
    /// path from its own `pending_pulls` set.
    PullDiagnostic { path: CanonPath },
    /// Waiting on `shutdown` before we send `exit`.
    Shutdown,
    /// Waiting on a `textDocument/completion` response. `seq`
    /// echoes `LspCmd::RequestCompletion.seq` back to the runtime
    /// in the resulting `LspEvent::Completion` so the runtime
    /// can drop stale items. `line` is the cursor line at
    /// request time (carried through because the LSP response
    /// doesn't echo it and we need it for prefix extraction).
    Completion {
        path: CanonPath,
        seq: u64,
        line: u32,
    },
    /// Waiting on a `completionItem/resolve` response. `seq`
    /// echoes `LspCmd::ResolveCompletion.seq` so the runtime
    /// can ignore resolves from a stale session.
    ResolveCompletion { path: CanonPath, seq: u64 },
    /// Waiting on a `textDocument/definition` response. `seq`
    /// echoes the runtime's originating
    /// `LspCmd::RequestGotoDefinition.seq`.
    GotoDefinition { seq: u64 },
    /// Waiting on a `textDocument/rename` response. `seq`
    /// echoes `LspCmd::RequestRename.seq` so the runtime can
    /// drop stale replies (e.g. after an abort).
    Rename { seq: u64 },
    /// Waiting on a `textDocument/codeAction` response. `seq`
    /// echoes `LspCmd::RequestCodeAction.seq`; `path` is
    /// carried through because the `LspEvent::CodeActions`
    /// surface echoes it back.
    CodeAction { seq: u64, path: CanonPath },
    /// Waiting on a `codeAction/resolve` response initiated by
    /// a picker commit. The raw pre-resolve action is stashed
    /// here so if resolve succeeds without `edit`, we fall
    /// back to the raw edit (if any).
    ResolveCodeAction {
        seq: u64,
        raw: Value,
    },
    /// Waiting on `textDocument/inlayHint`. `path` + `version`
    /// echo back in the `LspEvent::InlayHints` emission so the
    /// runtime can version-gate the cache.
    InlayHints {
        path: CanonPath,
        version: u64,
    },
    /// Waiting on `textDocument/formatting`. `seq` echoes
    /// `LspCmd::RequestFormat.seq`; `path` is forwarded so the
    /// format edit flattens into a `FileEdit` targeting the
    /// right buffer even though the LSP response doesn't echo
    /// the uri.
    Format { seq: u64, path: CanonPath },
}

struct ServerEntry {
    language: Language,
    server: Server,
    diag: DiagnosticSource,
    pending_requests: HashMap<i64, PendingRequest>,
    queued_opens: Vec<PendingOpen>,
    /// Set once the initialize response has been handled AND the
    /// `initialized` notification sent.
    initialized: bool,
    /// Per-doc LSP `textDocument.version` counter (1-based,
    /// monotonically increments each didChange). Separate from
    /// the content-hash tracking because the LSP spec demands a
    /// monotonic counter on the wire.
    doc_versions: HashMap<CanonPath, i32>,
    /// Current content hash for each opened doc — the anchor we
    /// snapshot into diagnostic windows and stamp outgoing pulls
    /// with. Updated on every `BufferOpened` / `BufferChanged`.
    buffer_hashes: HashMap<CanonPath, PersistedContentHash>,
    /// Was a RequestDiagnostics received before the server was
    /// ready? Replayed on first quiescence. Matches legacy's
    /// `init_delayed_request` semantics.
    deferred_init_request: bool,
    /// Previous rope snapshot per path — compared against the
    /// new rope on `BufferChanged` to compute an incremental
    /// `textDocument/didChange`. When absent (first change
    /// post-open) we fall back to full-text. Small Arc clone, so
    /// the cache cost is a pointer per path.
    last_rope_sent: HashMap<CanonPath, Arc<Rope>>,
    /// Server-advertised completion support. `completion_provider`
    /// gates `textDocument/completion`; `completion_trigger_chars`
    /// informs the runtime which input chars should kick a fresh
    /// request (the driver forwards them as-is — the runtime
    /// decides per-keystroke). `completion_resolve_provider` is
    /// future-proofing: controls whether `completionItem/resolve`
    /// round-trips on commit.
    completion_provider: bool,
    completion_trigger_chars: Vec<char>,
    completion_resolve_provider: bool,
    /// Cache of raw `CodeAction`/`Command` items returned by
    /// the last `textDocument/codeAction` request, keyed by
    /// the `action_id` strings we surfaced on the
    /// corresponding `CodeActionSummary`. The runtime's
    /// `SelectCodeAction` echoes the chosen id back so we can
    /// look up the opaque LSP object and issue
    /// `codeAction/resolve` (if needed) or apply its `edit`
    /// field directly.
    code_action_cache: HashMap<Arc<str>, Value>,
}

/// Lifecycle marker.
pub struct LspNative {
    _manager_handle: Option<JoinHandle<()>>,
    _cmd_adapter_handle: Option<JoinHandle<()>>,
}

pub fn spawn(
    trace: Arc<dyn Trace>,
    notify: Notifier,
    server_override: Option<String>,
) -> (led_driver_lsp_core::LspDriver, LspNative) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<LspCmd>();
    let (event_tx_to_core, event_rx_from_core) = mpsc::channel::<LspEvent>();

    let (central_tx, central_rx) = mpsc::channel::<ManagerEvent>();
    let cmd_adapter_handle = spawn_cmd_adapter(cmd_rx, central_tx.clone());

    let (incoming_tx, incoming_rx) = mpsc::channel::<ServerIncoming>();
    spawn_incoming_forwarder(incoming_rx, central_tx.clone());

    let manager_handle = thread::Builder::new()
        .name("led-lsp-manager".into())
        .spawn({
            let trace = trace.clone();
            move || {
                let mut mgr = Manager {
                    registry: LspRegistry::new(server_override),
                    servers: HashMap::new(),
                    next_request_id: 1,
                    incoming_tx,
                    event_rx: central_rx,
                    lsp_event_tx: event_tx_to_core,
                    notify,
                    trace,
                    workspace_root: None,
                    deferred_cmds: VecDeque::new(),
                    skipped_languages: std::collections::HashSet::new(),
                    progress_tokens: HashMap::new(),
                    quiescent: HashMap::new(),
                    last_progress_sent_at: None,
                    last_progress_busy: false,
                };
                mgr.run();
            }
        })
        .expect("spawn manager thread");

    let driver = led_driver_lsp_core::LspDriver::new(cmd_tx, event_rx_from_core, trace);
    let native = LspNative {
        _manager_handle: Some(manager_handle),
        _cmd_adapter_handle: Some(cmd_adapter_handle),
    };
    (driver, native)
}

fn spawn_cmd_adapter(
    rx: Receiver<LspCmd>,
    tx: Sender<ManagerEvent>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("led-lsp-cmd-adapter".into())
        .spawn(move || {
            while let Ok(cmd) = rx.recv() {
                if tx.send(ManagerEvent::Cmd(cmd)).is_err() {
                    return;
                }
            }
            let _ = tx.send(ManagerEvent::CmdChannelClosed);
        })
        .expect("spawn cmd adapter")
}

fn spawn_incoming_forwarder(
    rx: Receiver<ServerIncoming>,
    tx: Sender<ManagerEvent>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("led-lsp-incoming-forwarder".into())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                if tx.send(ManagerEvent::ServerMessage(msg)).is_err() {
                    return;
                }
            }
        })
        .expect("spawn incoming forwarder")
}

struct Manager {
    registry: LspRegistry,
    servers: HashMap<Language, ServerEntry>,
    next_request_id: i64,
    incoming_tx: Sender<ServerIncoming>,
    event_rx: Receiver<ManagerEvent>,
    lsp_event_tx: Sender<LspEvent>,
    notify: Notifier,
    trace: Arc<dyn Trace>,
    workspace_root: Option<CanonPath>,
    /// Cmds stashed here while any server's diag window is
    /// frozen. Drained FIFO once every server unfreezes.
    deferred_cmds: VecDeque<LspCmd>,
    /// Languages whose LSP binary isn't installed, or whose
    /// spawn failed unrecoverably. Prevents re-trying the
    /// spawn on every `BufferOpened` for that language — once
    /// we've decided "this language has no server", stay
    /// decided for the session.
    skipped_languages: std::collections::HashSet<Language>,

    // ── Progress aggregation (unified source) ────────────────
    //
    // Two independent signals converge into a single
    // `LspEvent::Progress` emission:
    //
    //   - `$/progress` tokens → `progress_tokens`.
    //   - `experimental/serverStatus quiescent=bool` → `quiescent`.
    //
    // `is_busy()` returns true if ANY quiescent=false entry OR
    // ANY open progress token exists. `progress_detail()`
    // returns the first progress token's `"{title} {message}"`
    // (or just `"{title}"`, or `None`) — serverStatus `message`
    // does NOT contribute to detail, matching legacy's shape.
    /// Open `$/progress` tokens, keyed by token id.
    progress_tokens: HashMap<String, ProgressInfo>,
    /// Per-server quiescence state. Absent = default-idle
    /// (matches legacy's `unwrap_or(&true)`). Present = the last
    /// value the server reported.
    quiescent: HashMap<Language, bool>,
    /// Last instant `send_progress_throttled` fired a
    /// `LspEvent::Progress`. 200ms minimum gap between sends,
    /// EXCEPT busy→idle transitions always fire (so the UI
    /// never gets stuck showing a spinner after the server
    /// went idle).
    last_progress_sent_at: Option<Instant>,
    /// Last `busy` value we actually emitted. Used by the
    /// throttle's "busy→idle always fires" exception.
    last_progress_busy: bool,
}

/// One open `$/progress` token's current title/message pair.
/// Built from `begin`; updated by `report`; removed on `end`.
#[derive(Debug, Clone, Default)]
struct ProgressInfo {
    title: Option<String>,
    message: Option<String>,
}

/// Threshold: if the incremental `didChange` replacement would be
/// larger than this many chars, fall back to full-text. Protects
/// against pathological deltas (rebase, format-all) where the
/// incremental form is actually larger / slower than a fresh
/// sync.
const DIDCHANGE_INCREMENTAL_MAX_CHARS: usize = 4096;

/// Friendly server name for trace lines. `Server.name` is the
/// `command` field from the registry — `rust-analyzer` /
/// `taplo` for real binaries, but the goldens harness overrides
/// it with the absolute path to `fake-lsp`. Trim to the
/// basename so traces read `server=fake-lsp` regardless of
/// where the binary lives on disk.
fn short_server_name(name: &str) -> &str {
    std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

impl Manager {
    fn run(&mut self) {
        loop {
            let ev = match self.earliest_deadline() {
                Some(deadline) => {
                    let now = Instant::now();
                    let wait = deadline.saturating_duration_since(now);
                    match self.event_rx.recv_timeout(wait) {
                        Ok(ev) => ev,
                        Err(RecvTimeoutError::Timeout) => {
                            self.on_deadline_elapsed();
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match self.event_rx.recv() {
                    Ok(ev) => ev,
                    Err(_) => return,
                },
            };
            match ev {
                ManagerEvent::CmdChannelClosed => return,
                ManagerEvent::ServerMessage(msg) => self.handle_server_message(msg),
                ManagerEvent::Cmd(cmd) => {
                    if self.any_frozen() {
                        // Freeze discipline: queue + defer.
                        self.deferred_cmds.push_back(cmd);
                    } else {
                        self.handle_cmd(cmd);
                    }
                }
            }
            // If the just-handled event unfroze the manager,
            // drain any cmds that piled up during the freeze.
            self.try_drain_deferred();
        }
    }

    fn any_frozen(&self) -> bool {
        self.servers.values().any(|e| e.diag.is_frozen())
    }

    // ── Progress aggregation ─────────────────────────────────

    /// True if any server has reported `quiescent=false` OR any
    /// `$/progress` token is open. Matches legacy exactly —
    /// either source independently keeps the spinner running.
    fn is_busy(&self) -> bool {
        self.progress_tokens_busy() || self.any_non_quiescent()
    }

    fn progress_tokens_busy(&self) -> bool {
        !self.progress_tokens.is_empty()
    }

    fn any_non_quiescent(&self) -> bool {
        self.quiescent.values().any(|q| !q)
    }

    /// Detail string for `LspEvent::Progress.detail`. Source is
    /// exclusively `$/progress` — the first open token's
    /// `"{title} {message}"`, or just `"{title}"` if no message.
    /// Matches legacy `progress_lsp_in` (manager.rs:1689-1709).
    fn progress_detail(&self) -> Option<String> {
        let info = self.progress_tokens.values().next()?;
        match (info.title.as_deref(), info.message.as_deref()) {
            (Some(t), Some(m)) if !m.is_empty() => Some(format!("{t} {m}")),
            (Some(t), _) => Some(t.to_string()),
            (None, Some(m)) if !m.is_empty() => Some(m.to_string()),
            _ => None,
        }
    }

    /// First registered server's name — used as the `server`
    /// field on emitted `LspEvent::Progress`. Matches legacy's
    /// "show whichever server got started first" behaviour. An
    /// empty string is returned when no server has spawned yet
    /// (the caller skips the emission in that case).
    fn first_server_name(&self) -> Option<String> {
        self.servers.values().next().map(|e| e.server.name.clone())
    }

    /// Emit an aggregated `LspEvent::Progress` if the throttle
    /// allows it. Throttle: 200ms minimum between sends, BUT
    /// busy→idle transitions always fire (so the UI never gets
    /// stuck with a stale spinner). Called by both the
    /// `$/progress` and `experimental/serverStatus` handlers at
    /// their tail — the two sources converge here.
    ///
    /// On a busy→idle transition the server has just finished
    /// a round of analysis (cold-index, cargo check, semantic
    /// re-check after save, …). Fire a fresh
    /// `RequestDiagnostics` so the next pull picks up whatever
    /// the server just produced. This is the main mechanism by
    /// which late cargo-check warnings reach the client when the
    /// runtime gates pulls on save (no keystroke-driven pulls).
    fn send_progress_throttled(&mut self) {
        let busy = self.is_busy();
        let detail = self.progress_detail();
        let transitioning_to_idle = self.last_progress_busy && !busy;

        let now = Instant::now();
        if !transitioning_to_idle
            && let Some(last) = self.last_progress_sent_at
            && now.duration_since(last) < std::time::Duration::from_millis(200)
        {
            return;
        }

        let Some(server_name) = self.first_server_name() else {
            return;
        };

        let _ = self.lsp_event_tx.send(LspEvent::Progress {
            server: server_name,
            busy,
            detail,
        });
        self.notify.notify();
        self.last_progress_sent_at = Some(now);
        self.last_progress_busy = busy;

        // Side effect: re-pull on every busy→idle edge. Covers
        // both `$/progress end` (cargo check finishing) and
        // `experimental/serverStatus quiescent=true` (ra's
        // overall-done signal). rust-analyzer in pull-only mode
        // wouldn't otherwise emit anything when cargo finishes.
        if transitioning_to_idle {
            self.request_diagnostics();
        }
    }

    fn earliest_deadline(&self) -> Option<Instant> {
        self.servers
            .values()
            .filter_map(|e| e.diag.deadline())
            .min()
    }

    fn try_drain_deferred(&mut self) {
        if self.any_frozen() {
            return;
        }
        while let Some(cmd) = self.deferred_cmds.pop_front() {
            self.handle_cmd(cmd);
            if self.any_frozen() {
                return; // re-frozen by the drain itself — stop
            }
        }
    }

    fn on_deadline_elapsed(&mut self) {
        let now = Instant::now();
        for entry in self.servers.values_mut() {
            if entry.diag.deadline().is_some_and(|d| d <= now) {
                entry.diag.cancel_freeze();
            }
        }
        self.try_drain_deferred();
    }

    fn fresh_id(&mut self) -> i64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    // ── Cmd handling ──────────────────────────────────────

    fn handle_cmd(&mut self, cmd: LspCmd) {
        match cmd {
            LspCmd::Init { root } => {
                self.workspace_root = Some(root);
            }
            LspCmd::Shutdown => self.shutdown_all(),
            LspCmd::BufferOpened {
                path,
                language,
                rope,
                hash,
            } => {
                let Some(lang) = language else { return };
                self.ensure_server_spawned(lang);
                self.open_buffer(lang, path, rope, hash);
            }
            LspCmd::BufferChanged {
                path,
                rope,
                hash,
                is_save,
            } => {
                self.buffer_changed(&path, &rope, hash, is_save);
            }
            LspCmd::BufferClosed { path } => {
                self.buffer_closed(&path);
            }
            LspCmd::RequestDiagnostics => {
                self.request_diagnostics();
            }
            LspCmd::RequestCompletion {
                path,
                seq,
                line,
                col,
                trigger,
            } => {
                self.request_completion(path, seq, line, col, trigger);
            }
            LspCmd::ResolveCompletion { path, seq, item } => {
                self.resolve_completion(path, seq, item);
            }
            LspCmd::RequestGotoDefinition {
                path,
                seq,
                line,
                col,
            } => {
                self.request_goto_definition(path, seq, line, col);
            }
            LspCmd::RequestRename {
                path,
                seq,
                line,
                col,
                new_name,
            } => {
                self.request_rename(path, seq, line, col, new_name);
            }
            LspCmd::RequestCodeAction {
                path,
                seq,
                start_line,
                start_col,
                end_line,
                end_col,
            } => {
                self.request_code_action(
                    path, seq, start_line, start_col, end_line, end_col,
                );
            }
            LspCmd::SelectCodeAction { path, seq, action } => {
                self.select_code_action(path, seq, action);
            }
            LspCmd::RequestFormat { path, seq } => {
                self.request_format(path, seq);
            }
            LspCmd::RequestInlayHints {
                path,
                seq,
                version,
                start_line,
                end_line,
            } => {
                self.request_inlay_hints(
                    path, seq, version, start_line, end_line,
                );
            }
        }
    }

    fn ensure_server_spawned(&mut self, language: Language) {
        if self.servers.contains_key(&language) {
            return;
        }
        if self.skipped_languages.contains(&language) {
            // We've already decided this language has no server
            // available. Don't retry on every BufferOpened.
            return;
        }
        let Some(config) = self.registry.config_for(language) else {
            // No registry entry for this language — also "no
            // server", permanently. Mark so we skip future
            // spawn calls cheaply.
            self.skipped_languages.insert(language);
            return;
        };
        // Server name = the binary command (e.g. "rust-analyzer",
        // "taplo"). Matches legacy server.rs:95 so the status-bar
        // text and trace output read the same shape regardless of
        // which editor ran the workspace.
        let name = config.command.to_string();
        let args: Vec<&str> = config.args.to_vec();
        let server = match crate::subprocess::spawn(
            name.clone(),
            config.command,
            &args,
            self.incoming_tx.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                // `NotFound` = binary not in `$PATH`. Legacy
                // (registry.rs + manager.rs) treats this as a
                // silent skip: the user just doesn't have this
                // LSP installed, which is the normal case for
                // most languages. No alert, no log.
                //
                // Anything else (permission denied, malformed
                // binary, etc.) IS surfaced as a warn alert so
                // the user can act on it.
                if e.kind() != std::io::ErrorKind::NotFound {
                    let _ = self.lsp_event_tx.send(LspEvent::Error {
                        server: name,
                        message: format!("spawn failed: {e}"),
                    });
                    self.notify.notify();
                }
                self.skipped_languages.insert(language);
                return;
            }
        };
        self.trace.lsp_server_started(&server.name);
        let server_name = short_server_name(&server.name).to_string();

        let id = self.fresh_id();
        let root = self
            .workspace_root
            .clone()
            .unwrap_or_else(CanonPath::default);
        let body = build_initialize_request(id, &root);
        let _ = server.send_body(&body);
        self.trace
            .lsp_send_request(&server_name, "initialize", id, None);

        let mut entry = ServerEntry {
            language,
            server,
            diag: DiagnosticSource::new(),
            pending_requests: HashMap::new(),
            queued_opens: Vec::new(),
            initialized: false,
            doc_versions: HashMap::new(),
            buffer_hashes: HashMap::new(),
            deferred_init_request: false,
            last_rope_sent: HashMap::new(),
            // Completion caps default to "no support"; parsed
            // from the initialize response in `finish_initialize`.
            completion_provider: false,
            completion_trigger_chars: Vec::new(),
            completion_resolve_provider: false,
            code_action_cache: HashMap::new(),
        };
        entry.pending_requests.insert(id, PendingRequest::Initialize);
        self.servers.insert(language, entry);
    }

    fn open_buffer(
        &mut self,
        language: Language,
        path: CanonPath,
        rope: Arc<Rope>,
        hash: PersistedContentHash,
    ) {
        let Some(entry) = self.servers.get_mut(&language) else {
            return;
        };
        entry.buffer_hashes.insert(path.clone(), hash);
        if entry.initialized {
            send_did_open(entry, &path, &rope, self.trace.as_ref());
        } else {
            entry.queued_opens.push(PendingOpen {
                path,
                rope,
                hash,
            });
        }
    }

    fn buffer_changed(
        &mut self,
        path: &CanonPath,
        rope: &Arc<Rope>,
        hash: PersistedContentHash,
        is_save: bool,
    ) {
        // Find the server that has this path open.
        let language = self.servers.iter().find_map(|(l, e)| {
            e.doc_versions.contains_key(path).then_some(*l)
        });
        let Some(language) = language else { return };
        let entry = self.servers.get_mut(&language).expect("just found");

        // Freeze discipline: the rope moved to new content, so
        // any open window that snapshotted this path's hash is
        // now stale. Close it so a later RequestDiagnostics opens
        // a fresh window at the current hash.
        if entry.diag.should_close_window(path, hash) {
            entry.diag.close_window();
        }

        entry.buffer_hashes.insert(path.clone(), hash);
        // Legacy push_cache invalidation: the cached push is
        // pinned to an earlier content; next push (if any) will
        // supersede, but in the meantime we don't want to forward
        // stale data on the next window.
        entry.diag.invalidate_cache(path);

        let lsp_version = {
            let v = entry.doc_versions.entry(path.clone()).or_insert(0);
            *v += 1;
            *v
        };
        // Incremental if we have a previous rope for this path
        // AND the delta is small enough to justify it. Otherwise
        // full-text. Matches legacy's "single-edit Range-based,
        // else full-text" rule by outcome (ops.len() == 1 implies
        // a single contiguous LCP/LCS-trim delta).
        let incremental_change = entry
            .last_rope_sent
            .get(path)
            .and_then(|old| incremental_content_change(old, rope));
        let content_changes = match incremental_change {
            Some(change) => json!([change]),
            None => json!([{ "text": rope.to_string() }]),
        };
        let uri = uri_from_path(path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {
                    "uri": uri.clone(),
                    "version": lsp_version,
                },
                "contentChanges": content_changes,
            },
        });
        let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
        self.trace.lsp_send_notification(
            &server_name,
            "textDocument/didChange",
            Some(&uri),
            Some(lsp_version),
        );
        entry.last_rope_sent.insert(path.clone(), rope.clone());

        if is_save {
            let save_body = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didSave",
                "params": {
                    "textDocument": { "uri": uri.clone() },
                },
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&save_body).unwrap());
            self.trace.lsp_send_notification(
                &server_name,
                "textDocument/didSave",
                Some(&uri),
                None,
            );
        }
    }

    fn buffer_closed(&mut self, path: &CanonPath) {
        let language = self.servers.iter().find_map(|(l, e)| {
            e.doc_versions.contains_key(path).then_some(*l)
        });
        let Some(language) = language else { return };
        let entry = self.servers.get_mut(&language).expect("just found");

        let uri = uri_from_path(path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {
                "textDocument": { "uri": uri.clone() },
            },
        });
        let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
        self.trace.lsp_send_notification(
            &server_name,
            "textDocument/didClose",
            Some(&uri),
            None,
        );
        entry.doc_versions.remove(path);
        entry.buffer_hashes.remove(path);
        entry.last_rope_sent.remove(path);
        entry.diag.invalidate_cache(path);
    }

    fn request_diagnostics(&mut self) {
        self.trace.lsp_request_diagnostics();
        // We iterate servers; per-server: open a window with the
        // snapshot of every currently-opened buffer. Defer if the
        // server isn't ready yet (quiescence gate).
        let languages: Vec<Language> = self.servers.keys().copied().collect();
        for lang in languages {
            let (snapshot, opened, should_defer) = {
                let entry = self.servers.get_mut(&lang).unwrap();
                if !entry.initialized {
                    // Drop silently — the post-init flush doesn't
                    // auto-request diagnostics; the runtime's next
                    // trigger will.
                    continue;
                }
                if entry.diag.should_defer_request() {
                    entry.deferred_init_request = true;
                    entry.diag.defer_init_request();
                    continue;
                }
                let snap = entry.buffer_hashes.clone();
                let opened = entry.doc_versions.keys().cloned().collect();
                (snap, opened, false)
            };
            let _ = should_defer;
            self.open_diag_window(lang, snapshot, opened);
        }
    }

    fn open_diag_window(
        &mut self,
        lang: Language,
        snapshot: HashMap<CanonPath, PersistedContentHash>,
        opened: std::collections::HashSet<CanonPath>,
    ) {
        let pulls_and_cache = {
            let entry = self.servers.get_mut(&lang).unwrap();
            let pulls = entry.diag.open_window(snapshot, &opened);
            let cache = if entry.diag.mode() == DiagMode::Push {
                entry.diag.drain_cache_for_window()
            } else {
                Vec::new()
            };
            (pulls, cache)
        };
        let (pulls, cache) = pulls_and_cache;

        // Forward cached push results immediately.
        for (path, diags, hash) in cache {
            let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                path: path.clone(),
                hash,
                diagnostics: diags,
            });
            self.trace.lsp_diagnostics_done(
                &path,
                self.servers[&lang]
                    .diag
                    .mode()
                    .ne(&DiagMode::Push)
                    .then_some(0)
                    .unwrap_or(0),
                hash,
            );
        }
        self.notify.notify();

        // Issue pulls.
        for path in pulls {
            let id = self.fresh_id();
            let entry = self.servers.get_mut(&lang).unwrap();
            let uri = uri_from_path(&path);
            let server_name = short_server_name(&entry.server.name).to_string();
            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/diagnostic",
                "params": {
                    "textDocument": { "uri": uri.clone() },
                },
            });
            let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
            self.trace.lsp_send_request(
                &server_name,
                "textDocument/diagnostic",
                id,
                Some(&uri),
            );
            entry.pending_requests.insert(
                id,
                PendingRequest::PullDiagnostic { path: path.clone() },
            );
        }
    }

    /// Send `textDocument/completion` for the cursor at
    /// `(line, col)` on `path`. The runtime's `seq` is carried
    /// into the `PendingRequest` so the eventual
    /// `LspEvent::Completion` can echo it back — stale responses
    /// (seq older than the latest live request) are dropped at
    /// the ingest end. Silently no-ops when no server is attached
    /// to the path's language or the server doesn't advertise
    /// `completionProvider`.
    fn request_completion(
        &mut self,
        path: CanonPath,
        seq: u64,
        line: u32,
        col: u32,
        trigger: Option<char>,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        if !entry.completion_provider {
            return;
        }
        // triggerCharacter is only set when the char was in the
        // server-advertised list; otherwise report Invoked (2).
        // Matches legacy `spawn_completion` exactly — legacy
        // always sends Invoked with `trigger_character: None`,
        // but we honour the char when we know the server asked
        // for it so smart servers can tune the candidate set.
        let (trigger_kind, trigger_char_json) = match trigger {
            Some(c) if entry.completion_trigger_chars.contains(&c) => {
                (2u8 /* TriggerCharacter */, json!(c.to_string()))
            }
            _ => (1u8 /* Invoked */, Value::Null),
        };
        let uri = uri_from_path(&path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "position": { "line": line, "character": col },
                "context": {
                    "triggerKind": trigger_kind,
                    "triggerCharacter": trigger_char_json,
                },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize completion"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/completion",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::Completion { path, seq, line });
    }

    /// Send `completionItem/resolve` for the item the user just
    /// committed. The opaque `data` field on the original
    /// `CompletionItem` (stored as `resolve_data`) is echoed
    /// back so the server can look up whatever index it was
    /// carrying. Returns the server's `additionalTextEdits` via
    /// `LspEvent::CompletionResolved`.
    fn resolve_completion(
        &mut self,
        path: CanonPath,
        seq: u64,
        item: led_driver_lsp_core::CompletionItem,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        if !entry.completion_resolve_provider {
            return;
        }
        let mut payload = json!({
            "label": item.label.as_ref(),
        });
        if let Some(data) = item.resolve_data.as_ref() {
            // `data` is an opaque blob; we stored it as a JSON
            // string in `resolve_data`. Round-tripping through
            // serde_json::from_str restores the original shape
            // so the server sees what it sent us.
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                payload["data"] = v;
            }
        }
        if let Some(detail) = item.detail.as_ref() {
            payload["detail"] = json!(detail.as_ref());
        }
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "completionItem/resolve",
            "params": payload,
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize resolve"));
        self.trace
            .lsp_send_request(&server_name, "completionItem/resolve", id, None);
        entry
            .pending_requests
            .insert(id, PendingRequest::ResolveCompletion { path, seq });
    }

    // ── M18 stubs ─────────────────────────────────────────
    //
    // Each handler below lands as a fully-wired RPC in its own
    // stage (2..=6). For now they're no-ops so the runtime can
    // call the new `LspCmd` variants without the manager
    // panicking or falling through.

    fn request_goto_definition(
        &mut self,
        path: CanonPath,
        seq: u64,
        line: u32,
        col: u32,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_goto_none(seq);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "position": { "line": line, "character": col },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize goto-def"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/definition",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::GotoDefinition { seq });
    }

    fn emit_goto_none(&self, seq: u64) {
        let _ = self.lsp_event_tx.send(LspEvent::GotoDefinition {
            seq,
            location: None,
        });
        self.notify.notify();
    }

    fn finish_goto_definition(
        &mut self,
        seq: u64,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let location = payload.ok().and_then(parse_definition_location);
        let _ = self
            .lsp_event_tx
            .send(LspEvent::GotoDefinition { seq, location });
        self.notify.notify();
    }

    fn request_rename(
        &mut self,
        path: CanonPath,
        seq: u64,
        line: u32,
        col: u32,
        new_name: Arc<str>,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::Rename);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "position": { "line": line, "character": col },
                "newName": new_name.as_ref(),
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize rename"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/rename",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::Rename { seq });
    }

    fn emit_empty_edits(
        &self,
        seq: u64,
        origin: led_driver_lsp_core::EditsOrigin,
    ) {
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin,
            edits: Arc::new(Vec::new()),
        });
        self.notify.notify();
    }

    fn finish_rename(
        &mut self,
        seq: u64,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let edits = match payload {
            Ok(v) => parse_workspace_edit(&v),
            Err(_) => Vec::new(),
        };
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin: led_driver_lsp_core::EditsOrigin::Rename,
            edits: Arc::new(edits),
        });
        self.notify.notify();
    }

    fn request_code_action(
        &mut self,
        path: CanonPath,
        seq: u64,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_code_actions(path, seq);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        // Fresh request purges any stale cache from the
        // previous session. A picker always pairs 1:1 with a
        // most-recent request.
        entry.code_action_cache.clear();
        let uri = uri_from_path(&path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/codeAction",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "range": {
                    "start": { "line": start_line, "character": start_col },
                    "end":   { "line": end_line,   "character": end_col   },
                },
                "context": { "diagnostics": [] },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize codeAction"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/codeAction",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::CodeAction { seq, path });
    }

    fn emit_empty_code_actions(&self, path: CanonPath, seq: u64) {
        let _ = self.lsp_event_tx.send(LspEvent::CodeActions {
            path,
            seq,
            actions: Arc::new(Vec::new()),
        });
        self.notify.notify();
    }

    fn finish_code_action(
        &mut self,
        language: Language,
        path: CanonPath,
        seq: u64,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let raw_items = match payload {
            Ok(Value::Array(arr)) => arr,
            _ => Vec::new(),
        };
        let entry = self.servers.get_mut(&language).unwrap();
        entry.code_action_cache.clear();
        let mut summaries: Vec<led_driver_lsp_core::CodeActionSummary> =
            Vec::with_capacity(raw_items.len());
        for (idx, raw) in raw_items.into_iter().enumerate() {
            let Some(title) = raw
                .get("title")
                .and_then(|t| t.as_str())
                .map(|s| Arc::<str>::from(s))
            else {
                continue;
            };
            let kind = raw
                .get("kind")
                .and_then(|k| k.as_str())
                .map(|s| Arc::<str>::from(s));
            // Pure Command variants have no `edit`; CodeAction
            // objects with an `edit` present skip resolve.
            let has_edit = raw.get("edit").is_some();
            let resolve_needed = !has_edit;
            let action_id: Arc<str> = Arc::<str>::from(format!("ca-{idx}"));
            entry
                .code_action_cache
                .insert(action_id.clone(), raw);
            summaries.push(led_driver_lsp_core::CodeActionSummary {
                title,
                kind,
                resolve_needed,
                action_id,
            });
        }
        let _ = self.lsp_event_tx.send(LspEvent::CodeActions {
            path,
            seq,
            actions: Arc::new(summaries),
        });
        self.notify.notify();
    }

    fn select_code_action(
        &mut self,
        path: CanonPath,
        seq: u64,
        action: led_driver_lsp_core::CodeActionSummary,
    ) {
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::CodeAction);
            return;
        };
        let raw = match self
            .servers
            .get(&language)
            .and_then(|e| e.code_action_cache.get(&action.action_id).cloned())
        {
            Some(raw) => raw,
            None => {
                // Cache was purged between request and commit
                // (another Alt-i fired). Legacy parity: drop.
                self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::CodeAction);
                return;
            }
        };
        if !action.resolve_needed && raw.get("edit").is_some() {
            // Edits are already in hand — parse + emit directly.
            let edits = raw
                .get("edit")
                .map(parse_workspace_edit)
                .unwrap_or_default();
            let _ = self.lsp_event_tx.send(LspEvent::Edits {
                seq,
                origin: led_driver_lsp_core::EditsOrigin::CodeAction,
                edits: Arc::new(edits),
            });
            self.notify.notify();
            return;
        }
        // Otherwise issue `codeAction/resolve` for the full
        // item. rust-analyzer and typescript both lazy-resolve
        // so this is the common case.
        let id = self.fresh_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "codeAction/resolve",
            "params": raw.clone(),
        });
        let entry = self.servers.get_mut(&language).expect("server exists");
        let server_name = short_server_name(&entry.server.name).to_string();
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize resolve"));
        self.trace
            .lsp_send_request(&server_name, "codeAction/resolve", id, None);
        entry
            .pending_requests
            .insert(id, PendingRequest::ResolveCodeAction { seq, raw });
    }

    fn finish_resolve_code_action(
        &mut self,
        seq: u64,
        raw: Value,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let resolved = payload.ok().unwrap_or(raw);
        let edits = resolved
            .get("edit")
            .map(parse_workspace_edit)
            .unwrap_or_default();
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin: led_driver_lsp_core::EditsOrigin::CodeAction,
            edits: Arc::new(edits),
        });
        self.notify.notify();
    }

    fn request_format(&mut self, path: CanonPath, seq: u64) {
        let Some(language) = self.language_for_path(&path) else {
            // No LSP for this language — emit empty edits so
            // the runtime's post-format save unlocks.
            self.emit_empty_edits(seq, led_driver_lsp_core::EditsOrigin::Format);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/formatting",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "options": {
                    "tabSize": 4,
                    "insertSpaces": true,
                },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize formatting"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/formatting",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::Format { seq, path });
    }

    fn finish_format(
        &mut self,
        seq: u64,
        path: CanonPath,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let edits_vec = match payload {
            Ok(Value::Array(arr)) => parse_text_edit_list(&arr),
            _ => Vec::new(),
        };
        let file_edits = if edits_vec.is_empty() {
            Vec::new()
        } else {
            vec![led_driver_lsp_core::FileEdit {
                path,
                edits: edits_vec,
            }]
        };
        let _ = self.lsp_event_tx.send(LspEvent::Edits {
            seq,
            origin: led_driver_lsp_core::EditsOrigin::Format,
            edits: Arc::new(file_edits),
        });
        self.notify.notify();
    }

    fn request_inlay_hints(
        &mut self,
        path: CanonPath,
        seq: u64,
        version: u64,
        start_line: u32,
        end_line: u32,
    ) {
        let _ = seq; // seq is internal-only (tracing); manager re-echoes version.
        let Some(language) = self.language_for_path(&path) else {
            self.emit_empty_inlay_hints(path, version);
            return;
        };
        let id = self.fresh_id();
        let entry = self.servers.get_mut(&language).expect("just resolved");
        let uri = uri_from_path(&path);
        let server_name = short_server_name(&entry.server.name).to_string();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/inlayHint",
            "params": {
                "textDocument": { "uri": uri.clone() },
                "range": {
                    "start": { "line": start_line, "character": 0 },
                    "end":   { "line": end_line,   "character": 0 },
                },
            },
        });
        let _ = entry
            .server
            .send_body(&serde_json::to_vec(&body).expect("serialize inlayHint"));
        self.trace.lsp_send_request(
            &server_name,
            "textDocument/inlayHint",
            id,
            Some(&uri),
        );
        entry
            .pending_requests
            .insert(id, PendingRequest::InlayHints { path, version });
    }

    fn emit_empty_inlay_hints(&self, path: CanonPath, version: u64) {
        let _ = self.lsp_event_tx.send(LspEvent::InlayHints {
            path,
            version,
            hints: Arc::new(Vec::new()),
        });
        self.notify.notify();
    }

    fn finish_inlay_hints(
        &mut self,
        path: CanonPath,
        version: u64,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let hints = match payload {
            Ok(Value::Array(arr)) => parse_inlay_hints(&arr),
            _ => Vec::new(),
        };
        let _ = self.lsp_event_tx.send(LspEvent::InlayHints {
            path,
            version,
            hints: Arc::new(hints),
        });
        self.notify.notify();
    }

    /// Look up which server handles `path`. Matches legacy
    /// `server_for_path` — a path is associated with whichever
    /// language the runtime opened it under.
    fn language_for_path(&self, path: &CanonPath) -> Option<Language> {
        self.servers.iter().find_map(|(lang, entry)| {
            entry.doc_versions.contains_key(path).then_some(*lang)
        })
    }

    fn finish_completion(
        &mut self,
        path: CanonPath,
        seq: u64,
        line: u32,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let result = match payload {
            Ok(v) => v,
            Err(_) => return, // server errored; drop silently.
        };
        let parsed = parse_completion_response(&result, line);
        let _ = self.lsp_event_tx.send(LspEvent::Completion {
            path,
            seq,
            items: Arc::new(parsed.items),
            prefix_line: line,
            prefix_start_col: parsed.prefix_start_col,
        });
        self.notify.notify();
    }

    fn finish_resolve_completion(
        &mut self,
        path: CanonPath,
        seq: u64,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let result = match payload {
            Ok(v) => v,
            Err(_) => return,
        };
        let edits = parse_resolve_additional_edits(&result);
        let _ = self.lsp_event_tx.send(LspEvent::CompletionResolved {
            path,
            seq,
            additional_edits: edits,
        });
        self.notify.notify();
    }

    fn shutdown_all(&mut self) {
        // Simplified for now: send shutdown + exit, drop servers.
        // A proper implementation would await the shutdown reply
        // before sending exit; the drop semantics clean up
        // regardless.
        let languages: Vec<Language> = self.servers.keys().copied().collect();
        for lang in languages {
            let id = self.fresh_id();
            let entry = self.servers.get_mut(&lang).unwrap();
            let server_name = short_server_name(&entry.server.name).to_string();
            let shutdown_body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "shutdown",
                "params": Value::Null,
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&shutdown_body).unwrap());
            self.trace
                .lsp_send_request(&server_name, "shutdown", id, None);
            let entry = self.servers.get_mut(&lang).unwrap();
            entry.pending_requests.insert(id, PendingRequest::Shutdown);
            let exit_body = json!({
                "jsonrpc": "2.0",
                "method": "exit",
                "params": Value::Null,
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&exit_body).unwrap());
            self.trace
                .lsp_send_notification(&server_name, "exit", None, None);
        }
        self.servers.clear();
    }

    // ── Server message handling ────────────────────────────

    fn handle_server_message(&mut self, msg: ServerIncoming) {
        let language = self.servers.iter().find_map(|(l, e)| {
            if e.server.name == msg.server {
                Some(*l)
            } else {
                None
            }
        });
        let Some(language) = language else { return };
        match msg.incoming {
            Incoming::Response { id, payload } => {
                self.handle_response(language, id, payload);
            }
            Incoming::Request {
                auto_reply,
                forward_as_notification,
                method,
                params,
                ..
            } => {
                self.handle_server_request(
                    language,
                    method,
                    params,
                    auto_reply,
                    forward_as_notification,
                );
            }
            Incoming::Notification { method, params } => {
                self.handle_notification(language, method, params);
            }
        }
    }

    fn handle_response(
        &mut self,
        language: Language,
        id: RequestId,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let RequestId::Int(n) = id else { return };
        let (pending, server_name) = {
            let entry = self.servers.get_mut(&language).unwrap();
            (
                entry.pending_requests.remove(&n),
                short_server_name(&entry.server.name).to_string(),
            )
        };
        self.trace.lsp_recv_response(&server_name, n);
        let Some(pending) = pending else { return };

        match pending {
            PendingRequest::Initialize => self.finish_initialize(language, payload),
            PendingRequest::PullDiagnostic { path } => {
                self.finish_pull_diagnostic(language, path, payload);
            }
            PendingRequest::Shutdown => {
                // Drop the entry to kill the subprocess.
                self.servers.remove(&language);
            }
            PendingRequest::Completion { path, seq, line } => {
                self.finish_completion(path, seq, line, payload);
            }
            PendingRequest::ResolveCompletion { path, seq } => {
                self.finish_resolve_completion(path, seq, payload);
            }
            PendingRequest::GotoDefinition { seq } => {
                self.finish_goto_definition(seq, payload);
            }
            PendingRequest::Rename { seq } => {
                self.finish_rename(seq, payload);
            }
            PendingRequest::CodeAction { seq, path } => {
                self.finish_code_action(language, path, seq, payload);
            }
            PendingRequest::ResolveCodeAction { seq, raw } => {
                self.finish_resolve_code_action(seq, raw, payload);
            }
            PendingRequest::InlayHints { path, version } => {
                self.finish_inlay_hints(path, version, payload);
            }
            PendingRequest::Format { seq, path } => {
                self.finish_format(seq, path, payload);
            }
        }
    }

    fn finish_initialize(
        &mut self,
        language: Language,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let entry = self.servers.get_mut(&language).unwrap();
        match payload {
            Ok(result) => {
                let caps: InitializeCapabilities = parse_initialize_response(&result);
                if caps.diagnostic_provider {
                    entry.diag.set_mode(DiagMode::Pull);
                }
                entry.completion_provider = caps.completion_provider;
                entry.completion_trigger_chars = caps.completion_trigger_chars.clone();
                entry.completion_resolve_provider = caps.completion_resolve_provider;
                // Quiescence is NOT latched from the initialize
                // response. Some servers advertise
                // `serverStatusNotification` capability but never
                // emit the notification; others emit it without
                // advertising. Legacy detects at runtime on the
                // first notification — see the handler for
                // `experimental/serverStatus` below. The `caps.has_quiescence`
                // bit is retained for logs only.
                let _ = caps.has_quiescence;
                let server_name = short_server_name(&entry.server.name).to_string();
                let _ = entry.server.send_body(&build_initialized_notification());
                self.trace.lsp_send_notification(
                    &server_name,
                    "initialized",
                    None,
                    None,
                );
                // rust-analyzer waits for this before starting its cold-index
                // phase. Empty settings is the right payload — we don't override
                // any defaults. See docs/rewrite/lsp-patterns.md §2.5.
                let _ = entry
                    .server
                    .send_body(&build_did_change_configuration_notification());
                self.trace.lsp_send_notification(
                    &server_name,
                    "workspace/didChangeConfiguration",
                    None,
                    None,
                );
                entry.initialized = true;
                let queued = std::mem::take(&mut entry.queued_opens);
                for open in queued {
                    send_did_open(entry, &open.path, &open.rope, self.trace.as_ref());
                    entry.buffer_hashes.insert(open.path.clone(), open.hash);
                }
            }
            Err(err) => {
                let server_name = entry.server.name.clone();
                let _ = self.lsp_event_tx.send(LspEvent::Error {
                    server: server_name,
                    message: format!(
                        "initialize failed (code {}): {}",
                        err.code, err.message
                    ),
                });
                self.notify.notify();
            }
        }
    }

    fn finish_pull_diagnostic(
        &mut self,
        language: Language,
        path: CanonPath,
        payload: Result<Value, crate::classify::JsonRpcError>,
    ) {
        let diags = match payload {
            Ok(result) => parse_diagnostic_result(&result),
            Err(_) => Vec::new(),
        };
        let entry = self.servers.get_mut(&language).unwrap();
        let (forward, _all_done) = entry.diag.on_pull_response(path, diags);
        if let Some((path, diagnostics, hash)) = forward {
            self.trace
                .lsp_diagnostics_done(&path, diagnostics.len(), hash);
            let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                path,
                hash,
                diagnostics,
            });
            self.notify.notify();
        }
    }

    fn handle_server_request(
        &mut self,
        language: Language,
        _method: String,
        _params: Value,
        auto_reply: Value,
        _forward_as_notification: bool,
    ) {
        let Some(entry) = self.servers.get_mut(&language) else {
            return;
        };
        let body = serde_json::to_vec(&auto_reply).expect("auto-reply is valid");
        let _ = entry.server.send_body(&body);
    }

    fn handle_notification(
        &mut self,
        language: Language,
        method: String,
        params: Value,
    ) {
        if let Some(entry) = self.servers.get(&language) {
            let server_name = short_server_name(&entry.server.name).to_string();
            self.trace.lsp_recv_notification(&server_name, &method);
        }
        match method.as_str() {
            "textDocument/publishDiagnostics" => {
                let Some(path) = params
                    .get("uri")
                    .and_then(|u| u.as_str())
                    .and_then(path_from_uri)
                    .map(|pb| led_core::UserPath::new(pb).canonicalize())
                else {
                    return;
                };
                let diags = params
                    .get("diagnostics")
                    .and_then(|d| d.as_array())
                    .map(|arr| arr.iter().filter_map(parse_diagnostic_entry).collect())
                    .unwrap_or_default();
                let result = {
                    let entry = self.servers.get_mut(&language).unwrap();
                    // Stamp the push with the buffer's CURRENT
                    // content hash — the hash we believe matches
                    // the bytes rust-analyzer just analysed. That
                    // lets the runtime's replay pipeline map the
                    // diagnostic through any edits the user has
                    // since landed instead of pinning it to
                    // whichever hash a future drain happens to see.
                    let current_hash = entry
                        .buffer_hashes
                        .get(&path)
                        .copied()
                        .unwrap_or_default();
                    entry.diag.on_push(path.clone(), diags, current_hash)
                };
                self.dispatch_push_result(language, path, result);
            }
            "experimental/serverStatus" => {
                // rust-analyzer's custom status extension.
                // `quiescent=false` = server is working (indexing,
                // cachePriming, type-checking, …). `quiescent=true`
                // = idle. `message` carries a human-readable tail
                // that we deliberately discard — detail is owned
                // by `$/progress` exclusively, matching legacy
                // `progress_lsp_in` (manager.rs:1689-1709).
                //
                // Quiescence detection is runtime-first: the very
                // arrival of a `serverStatus` notification proves
                // the server supports the extension, regardless of
                // what its initialize capabilities advertised. On
                // first arrival we latch `has_quiescence = true`
                // (which also flips `lsp_ready = false` — the
                // server is NOT ready until it emits
                // `quiescent=true`).
                let quiescent = params
                    .get("quiescent")
                    .and_then(|q| q.as_bool())
                    .unwrap_or(false);
                let server_name = self.servers[&language].server.name.clone();
                // `was_busy` reads the PREVIOUS quiescent value —
                // absent entry means default-idle (matches
                // legacy's `unwrap_or(&true)` → `!true = false`).
                let was_busy = !*self.quiescent.get(&language).unwrap_or(&true);
                {
                    let entry = self.servers.get_mut(&language).unwrap();
                    if !entry.diag.has_quiescence() {
                        entry.diag.set_has_quiescence(true);
                    }
                }
                self.quiescent.insert(language, quiescent);
                if quiescent {
                    let _ = self
                        .lsp_event_tx
                        .send(LspEvent::Ready { server: server_name });
                    self.notify.notify();
                    // Consume the deferred-init flag so
                    // `should_defer_request` stops blocking
                    // future requests. The re-pull trigger on
                    // busy→idle lives in `send_progress_throttled`
                    // below, covering both this path and
                    // `$/progress end` with one source of truth.
                    if was_busy {
                        let entry = self.servers.get_mut(&language).unwrap();
                        entry.diag.on_quiescence();
                        entry.deferred_init_request = false;
                    }
                }
                // Unified progress emission — both sources
                // converge through `send_progress_throttled`.
                self.send_progress_throttled();
            }
            "$/progress" => {
                // Progress token lifecycle: `begin` inserts a new
                // token with title+message; `report` updates; `end`
                // removes. `report` with `percentage=100` is
                // promoted to `end` (matches legacy's
                // `classify_progress` at manager.rs:2042-2052).
                let _ = language;
                let token = params
                    .get("token")
                    .map(|t| match t {
                        Value::String(s) => s.clone(),
                        Value::Number(n) => n.to_string(),
                        _ => String::new(),
                    })
                    .unwrap_or_default();
                if token.is_empty() {
                    return;
                }
                let kind = params
                    .pointer("/value/kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                let title = params
                    .pointer("/value/title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let message = params
                    .pointer("/value/message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                let percentage = params
                    .pointer("/value/percentage")
                    .and_then(|p| p.as_u64());
                let effective_kind = if kind == "report" && percentage == Some(100) {
                    "end"
                } else {
                    kind
                };
                match effective_kind {
                    "begin" => {
                        self.progress_tokens
                            .insert(token, ProgressInfo { title, message });
                    }
                    "report" => {
                        let entry = self.progress_tokens.entry(token).or_default();
                        if title.is_some() {
                            entry.title = title;
                        }
                        if message.is_some() {
                            entry.message = message;
                        }
                    }
                    "end" => {
                        self.progress_tokens.remove(&token);
                    }
                    _ => {}
                }
                self.send_progress_throttled();
            }
            "window/logMessage" | "window/showMessage" | "client/registerCapability" => {
                // Ignored for now.
            }
            _ => {}
        }
    }

    fn dispatch_push_result(
        &mut self,
        language: Language,
        path: CanonPath,
        result: DiagPushResult,
    ) {
        match result {
            DiagPushResult::Forward(p, diags, hash) => {
                self.trace.lsp_diagnostics_done(&p, diags.len(), hash);
                let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                    path: p,
                    hash,
                    diagnostics: diags,
                });
                self.notify.notify();
            }
            DiagPushResult::ForwardClearing(p) => {
                // Clearing push (empty list) outside a window.
                // Legacy forwards with the current buffer hash —
                // clearing is always safe; the runtime's
                // hash-match / replay gate applies either way.
                let hash = self.servers[&language]
                    .buffer_hashes
                    .get(&p)
                    .copied()
                    .unwrap_or_default();
                let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                    path: p,
                    hash,
                    diagnostics: Vec::new(),
                });
                self.notify.notify();
            }
            DiagPushResult::RestartWindow => {
                self.trace.lsp_mode_fallback();
                // DiagnosticSource already closed the window in
                // the pull→push fallback. Re-issue a synthetic
                // RequestDiagnostics so push-mode windows drain
                // the cache (now containing the offending push).
                self.request_diagnostics();
                let _ = path;
            }
            DiagPushResult::Ignore => {}
        }
    }
}

fn send_did_open(
    entry: &mut ServerEntry,
    path: &CanonPath,
    rope: &Arc<Rope>,
    trace: &dyn Trace,
) {
    let uri = uri_from_path(path);
    let body = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri.clone(),
                "languageId": language_id(entry.language),
                "version": 1,
                "text": rope.to_string(),
            },
        },
    });
    entry.doc_versions.insert(path.clone(), 1);
    // Seed the incremental-didChange cache so the FIRST
    // didChange can go incremental instead of full-text.
    entry.last_rope_sent.insert(path.clone(), rope.clone());
    let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
    trace.lsp_send_notification(
        short_server_name(&entry.server.name),
        "textDocument/didOpen",
        Some(&uri),
        Some(1),
    );
}

/// Compute an LSP Range-based `contentChange` entry from the
/// char-delta between `old` and `new`. Returns `None` if the
/// delta is too large (>`DIDCHANGE_INCREMENTAL_MAX_CHARS` chars
/// of replacement text) — caller falls back to full-text. Also
/// returns `None` when the ropes are identical; callers treat
/// that as "nothing to send."
///
/// Positions are emitted as UTF-16 code units per LSP's default
/// encoding (`general.positionEncodings` unspecified by us →
/// server assumes UTF-16).
fn incremental_content_change(old: &Rope, new: &Rope) -> Option<Value> {
    let (prefix, old_end, new_end) = char_delta_bounds(old, new)?;
    let replacement_len = new_end.saturating_sub(prefix);
    if replacement_len > DIDCHANGE_INCREMENTAL_MAX_CHARS {
        return None;
    }
    let (start_line, start_utf16) = char_idx_to_line_utf16(old, prefix);
    let (end_line, end_utf16) = char_idx_to_line_utf16(old, old_end);
    let new_text: String = new.slice(prefix..new_end).to_string();
    Some(json!({
        "range": {
            "start": { "line": start_line, "character": start_utf16 },
            "end":   { "line": end_line,   "character": end_utf16   },
        },
        "text": new_text,
    }))
}

/// Longest-common-prefix / longest-common-suffix trim.
/// Returns `(prefix_char_idx, old_end_char_idx, new_end_char_idx)`
/// — the inclusive-start / exclusive-end range of chars that
/// actually differ between the two ropes. `None` when the ropes
/// are byte-for-byte identical.
fn char_delta_bounds(old: &Rope, new: &Rope) -> Option<(usize, usize, usize)> {
    let old_len = old.len_chars();
    let new_len = new.len_chars();
    if old_len == new_len {
        // Cheap escape: identical ropes → no delta.
        let old_cmp = old.slice(..).bytes().eq(new.slice(..).bytes());
        if old_cmp {
            return None;
        }
    }
    let min_len = old_len.min(new_len);

    // Common prefix via paired char iteration.
    let mut prefix = 0usize;
    let mut o_it = old.chars();
    let mut n_it = new.chars();
    while prefix < min_len {
        match (o_it.next(), n_it.next()) {
            (Some(o), Some(n)) if o == n => prefix += 1,
            _ => break,
        }
    }

    // Common suffix (indexed from the end, stopping at `prefix`).
    let max_suffix = min_len - prefix;
    let mut suffix = 0usize;
    while suffix < max_suffix {
        let o_idx = old_len - 1 - suffix;
        let n_idx = new_len - 1 - suffix;
        if old.char(o_idx) != new.char(n_idx) {
            break;
        }
        suffix += 1;
    }
    Some((prefix, old_len - suffix, new_len - suffix))
}

/// Convert a char index into `(line, utf16_col)` — UTF-16 code
/// units relative to the start of the containing line. LSP
/// `Position` uses this encoding by default.
fn char_idx_to_line_utf16(rope: &Rope, char_idx: usize) -> (usize, usize) {
    let line = rope.char_to_line(char_idx);
    let line_start = rope.line_to_char(line);
    let utf16_at_line_start = rope.char_to_utf16_cu(line_start);
    let utf16_at_char = rope.char_to_utf16_cu(char_idx);
    (line, utf16_at_char - utf16_at_line_start)
}

/// Parse an `InlayHint[]` response into the compact wire
/// shape led's painter consumes. Hints without a recognisable
/// `position` + `label` are dropped. `label` may be either a
/// bare string or an array of `InlayHintLabelPart` objects;
/// we concatenate the parts' `value` fields.
fn parse_inlay_hints(
    items: &[Value],
) -> Vec<led_driver_lsp_core::InlayHint> {
    items
        .iter()
        .filter_map(|v| {
            let pos = v.get("position")?;
            let line = pos.get("line")?.as_u64()? as u32;
            let col = pos.get("character")?.as_u64()? as u32;
            let label_value = v.get("label")?;
            let label = match label_value {
                Value::String(s) => s.clone(),
                Value::Array(parts) => {
                    let mut acc = String::new();
                    for p in parts {
                        if let Some(s) = p.get("value").and_then(|vv| vv.as_str()) {
                            acc.push_str(s);
                        }
                    }
                    acc
                }
                _ => return None,
            };
            let padding_left = v
                .get("paddingLeft")
                .and_then(|p| p.as_bool())
                .unwrap_or(false);
            let padding_right = v
                .get("paddingRight")
                .and_then(|p| p.as_bool())
                .unwrap_or(false);
            Some(led_driver_lsp_core::InlayHint {
                line,
                col,
                label: Arc::<str>::from(label),
                padding_left,
                padding_right,
            })
        })
        .collect()
}

/// Parse a `WorkspaceEdit` response from `textDocument/rename`
/// or a resolved code action into a flat `Vec<FileEdit>`. LSP
/// has two shapes:
///
/// - `changes`: `{ uri: [TextEdit] }` — the legacy form.
/// - `documentChanges`: `[{ textDocument: {uri,version},
///   edits: [TextEdit] }, ...]` — the versioned form.
///
/// We flatten either shape into one `FileEdit` per distinct
/// uri. Unknown shapes (pure-null, pure-errors) return an
/// empty vec which the runtime treats as "no-op rename" —
/// still surfaces the alert and dismisses.
fn parse_workspace_edit(
    result: &Value,
) -> Vec<led_driver_lsp_core::FileEdit> {
    let mut out: Vec<led_driver_lsp_core::FileEdit> = Vec::new();
    if let Some(changes) = result.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits_json) in changes {
            let Some(path) =
                path_from_uri(uri).map(|p| led_core::UserPath::new(p).canonicalize())
            else {
                continue;
            };
            let Some(arr) = edits_json.as_array() else { continue };
            let edits = parse_text_edit_list(arr);
            if !edits.is_empty() {
                out.push(led_driver_lsp_core::FileEdit { path, edits });
            }
        }
    }
    if let Some(doc_changes) =
        result.get("documentChanges").and_then(|d| d.as_array())
    {
        for change in doc_changes {
            let Some(uri) = change
                .pointer("/textDocument/uri")
                .and_then(|v| v.as_str())
                .and_then(path_from_uri)
                .map(|p| led_core::UserPath::new(p).canonicalize())
            else {
                continue;
            };
            let Some(arr) = change.get("edits").and_then(|e| e.as_array()) else {
                continue;
            };
            let edits = parse_text_edit_list(arr);
            if !edits.is_empty() {
                out.push(led_driver_lsp_core::FileEdit { path: uri, edits });
            }
        }
    }
    out
}

fn parse_text_edit_list(arr: &[Value]) -> Vec<led_driver_lsp_core::TextEditOp> {
    arr.iter().filter_map(parse_text_edit).collect()
}

fn parse_text_edit(v: &Value) -> Option<led_driver_lsp_core::TextEditOp> {
    let range = v.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let start_line = start.get("line")?.as_u64()? as u32;
    let start_col = start.get("character")?.as_u64()? as u32;
    let end_line = end.get("line")?.as_u64()? as u32;
    let end_col = end.get("character")?.as_u64()? as u32;
    let new_text = v.get("newText").and_then(|t| t.as_str()).unwrap_or("");
    Some(led_driver_lsp_core::TextEditOp {
        start_line,
        start_col,
        end_line,
        end_col,
        new_text: Arc::<str>::from(new_text),
    })
}

/// Parse a `textDocument/definition` response. The LSP shape
/// is one of: `null`, a single `Location`, an array of
/// `Location`, or an array of `LocationLink`. We only use the
/// first entry and flatten to [`led_driver_lsp_core::Location`].
///
/// Returns `None` when the server has no answer, or when every
/// entry is malformed.
fn parse_definition_location(
    result: Value,
) -> Option<led_driver_lsp_core::Location> {
    let entry = match result {
        Value::Null => return None,
        Value::Array(arr) => arr.into_iter().next()?,
        v @ Value::Object(_) => v,
        _ => return None,
    };
    // `LocationLink` uses `targetUri` + `targetSelectionRange`;
    // `Location` uses `uri` + `range`. Try both.
    let uri = entry
        .get("uri")
        .or_else(|| entry.get("targetUri"))
        .and_then(|u| u.as_str())?;
    let range = entry
        .get("range")
        .or_else(|| entry.get("targetSelectionRange"))
        .or_else(|| entry.get("targetRange"))?;
    let start = range.get("start")?;
    let line = start.get("line").and_then(|v| v.as_u64())? as u32;
    let col = start.get("character").and_then(|v| v.as_u64())? as u32;
    let path = path_from_uri(uri)?;
    Some(led_driver_lsp_core::Location {
        path: led_core::UserPath::new(path).canonicalize(),
        line,
        col,
    })
}

/// Parse a `textDocument/diagnostic` pull-response body. LSP
/// documents two shapes: Full (report) and Unchanged. We only
/// care about Full here; Unchanged yields an empty list.
fn parse_diagnostic_result(result: &Value) -> Vec<Diagnostic> {
    let kind = result.get("kind").and_then(|k| k.as_str()).unwrap_or("full");
    if kind != "full" {
        return Vec::new();
    }
    result
        .get("items")
        .and_then(|i| i.as_array())
        .map(|arr| arr.iter().filter_map(parse_diagnostic_entry).collect())
        .unwrap_or_default()
}

/// Parse one LSP `Diagnostic` object into our
/// [`led_state_diagnostics::Diagnostic`]. Positions are
/// forwarded verbatim (LSP uses UTF-16 by default — we'll convert
/// in stage 5 when we have the rope snapshot at accept time). For
/// now, interpret positions as char offsets; incorrect for
/// non-ASCII but not worse than legacy's first cut.
fn parse_diagnostic_entry(entry: &Value) -> Option<Diagnostic> {
    let range = entry.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let start_line = start.get("line")?.as_u64()? as usize;
    let start_col = start.get("character")?.as_u64()? as usize;
    let end_line = end.get("line")?.as_u64()? as usize;
    let end_col = end.get("character")?.as_u64()? as usize;
    let severity = match entry.get("severity").and_then(|s| s.as_u64()) {
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        Some(3) => DiagnosticSeverity::Info,
        Some(4) => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Error,
    };
    let message = entry
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let source = entry
        .get("source")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let code = entry.get("code").and_then(|c| {
        c.as_str().map(|s| s.to_string()).or_else(|| {
            c.as_i64().map(|n| n.to_string())
        })
    });
    Some(Diagnostic {
        start_line,
        start_col,
        end_line,
        end_col,
        severity,
        message,
        source,
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_adapter_forwards_in_order() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<LspCmd>();
        let (central_tx, central_rx) = mpsc::channel::<ManagerEvent>();
        let h = spawn_cmd_adapter(cmd_rx, central_tx);

        cmd_tx.send(LspCmd::RequestDiagnostics).unwrap();
        cmd_tx
            .send(LspCmd::BufferClosed {
                path: CanonPath::default(),
            })
            .unwrap();
        drop(cmd_tx);

        let got1 = central_rx.recv().unwrap();
        let got2 = central_rx.recv().unwrap();
        let got3 = central_rx.recv().unwrap();
        h.join().unwrap();

        assert!(matches!(got1, ManagerEvent::Cmd(LspCmd::RequestDiagnostics)));
        assert!(matches!(
            got2,
            ManagerEvent::Cmd(LspCmd::BufferClosed { .. })
        ));
        assert!(matches!(got3, ManagerEvent::CmdChannelClosed));
    }

    #[test]
    fn parse_diagnostic_entry_reads_range_and_severity() {
        use serde_json::json;
        let v = json!({
            "range": {
                "start": {"line": 3, "character": 5},
                "end":   {"line": 3, "character": 12},
            },
            "severity": 2,
            "message": "unused variable",
            "source": "rustc",
            "code": "unused_variables",
        });
        let d = parse_diagnostic_entry(&v).unwrap();
        assert_eq!(d.start_line, 3);
        assert_eq!(d.start_col, 5);
        assert_eq!(d.end_line, 3);
        assert_eq!(d.end_col, 12);
        assert_eq!(d.severity, DiagnosticSeverity::Warning);
        assert_eq!(d.message, "unused variable");
        assert_eq!(d.source.as_deref(), Some("rustc"));
        assert_eq!(d.code.as_deref(), Some("unused_variables"));
    }

    #[test]
    fn parse_diagnostic_entry_missing_optionals_tolerated() {
        use serde_json::json;
        let v = json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end":   {"line": 0, "character": 1},
            },
            "message": "x",
        });
        let d = parse_diagnostic_entry(&v).unwrap();
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert_eq!(d.source, None);
        assert_eq!(d.code, None);
    }

    #[test]
    fn parse_diagnostic_result_picks_items_from_full_report() {
        use serde_json::json;
        let v = json!({
            "kind": "full",
            "items": [
                {
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end":   {"line": 0, "character": 1},
                    },
                    "severity": 1,
                    "message": "oops",
                },
                {
                    "range": {
                        "start": {"line": 4, "character": 2},
                        "end":   {"line": 4, "character": 7},
                    },
                    "severity": 2,
                    "message": "hmm",
                },
            ],
        });
        let got = parse_diagnostic_result(&v);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].severity, DiagnosticSeverity::Error);
        assert_eq!(got[1].severity, DiagnosticSeverity::Warning);
    }

    #[test]
    fn parse_diagnostic_result_empty_for_unchanged_report() {
        use serde_json::json;
        let v = json!({"kind": "unchanged", "resultId": "abc"});
        assert!(parse_diagnostic_result(&v).is_empty());
    }

    #[test]
    fn parse_diagnostic_entry_integer_code_stringifies() {
        use serde_json::json;
        let v = json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end":   {"line": 0, "character": 1},
            },
            "message": "",
            "code": 277,
        });
        assert_eq!(parse_diagnostic_entry(&v).unwrap().code.as_deref(), Some("277"));
    }

    // ── Incremental didChange diff ──────────────────────────

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn char_delta_bounds_identical_ropes_returns_none() {
        assert!(char_delta_bounds(&rope("abc"), &rope("abc")).is_none());
    }

    #[test]
    fn char_delta_bounds_insert_in_middle() {
        let old = rope("hello world");
        let new = rope("hello big world");
        let (prefix, old_end, new_end) =
            char_delta_bounds(&old, &new).expect("differ");
        assert_eq!(prefix, 6, "prefix stops before the first differing char");
        assert_eq!(old_end, 6, "old end == prefix (pure insert)");
        assert_eq!(new_end, 10, "new end covers 'big '");
    }

    #[test]
    fn char_delta_bounds_delete_in_middle() {
        let old = rope("hello big world");
        let new = rope("hello world");
        let (prefix, old_end, new_end) =
            char_delta_bounds(&old, &new).expect("differ");
        assert_eq!(prefix, 6);
        assert_eq!(old_end, 10);
        assert_eq!(new_end, 6);
    }

    #[test]
    fn char_delta_bounds_append_at_eof() {
        let old = rope("abc");
        let new = rope("abcdef");
        let (prefix, old_end, new_end) =
            char_delta_bounds(&old, &new).expect("differ");
        assert_eq!(prefix, 3);
        assert_eq!(old_end, 3);
        assert_eq!(new_end, 6);
    }

    #[test]
    fn char_delta_bounds_prepend_at_start() {
        let old = rope("world");
        let new = rope("hello world");
        let (prefix, old_end, new_end) =
            char_delta_bounds(&old, &new).expect("differ");
        assert_eq!(prefix, 0);
        assert_eq!(old_end, 0);
        assert_eq!(new_end, 6);
    }

    #[test]
    fn incremental_content_change_emits_range_for_small_edit() {
        let old = rope("line1\nline2\n");
        let new = rope("line1 extra\nline2\n");
        let change = incremental_content_change(&old, &new).expect("incremental");
        let range = &change["range"];
        assert_eq!(range["start"]["line"], 0);
        assert_eq!(range["start"]["character"], 5);
        assert_eq!(range["end"]["line"], 0);
        assert_eq!(range["end"]["character"], 5);
        assert_eq!(change["text"], " extra");
    }

    #[test]
    fn incremental_content_change_emits_multiline_range_for_newline_delete() {
        // Delete the line break between lines 1 and 2.
        let old = rope("abc\ndef\n");
        let new = rope("abcdef\n");
        let change = incremental_content_change(&old, &new).expect("incremental");
        let range = &change["range"];
        assert_eq!(range["start"]["line"], 0);
        assert_eq!(range["start"]["character"], 3);
        assert_eq!(range["end"]["line"], 1);
        assert_eq!(range["end"]["character"], 0);
        assert_eq!(change["text"], "");
    }

    #[test]
    fn incremental_content_change_falls_back_for_oversized_delta() {
        let old = rope("");
        let new_text: String = "x".repeat(DIDCHANGE_INCREMENTAL_MAX_CHARS + 1);
        let new = rope(&new_text);
        assert!(
            incremental_content_change(&old, &new).is_none(),
            "giant replacement must fall back to full-text"
        );
    }

    #[test]
    fn char_idx_to_line_utf16_counts_surrogate_pairs() {
        // "🦀" is U+1F980 — outside the BMP, so it's 2 UTF-16
        // code units but 1 char. An LSP position after the crab
        // must report character=2, not 1.
        let r = rope("🦀x");
        assert_eq!(char_idx_to_line_utf16(&r, 0), (0, 0));
        assert_eq!(char_idx_to_line_utf16(&r, 1), (0, 2));
        assert_eq!(char_idx_to_line_utf16(&r, 2), (0, 3));
    }

    // ── M18 goto-definition ───────────────────────────────

    #[test]
    fn parse_definition_null_returns_none() {
        assert!(parse_definition_location(json!(null)).is_none());
    }

    #[test]
    fn parse_definition_single_location_picks_start_position() {
        let v = json!({
            "uri": "file:///tmp/main.rs",
            "range": {
                "start": { "line": 4, "character": 7 },
                "end":   { "line": 4, "character": 12 },
            }
        });
        let loc = parse_definition_location(v).expect("location");
        assert_eq!(loc.line, 4);
        assert_eq!(loc.col, 7);
    }

    #[test]
    fn parse_definition_array_takes_first_entry() {
        let v = json!([
            {
                "uri": "file:///tmp/main.rs",
                "range": {
                    "start": { "line": 0, "character": 3 },
                    "end":   { "line": 0, "character": 9 },
                }
            },
            {
                "uri": "file:///tmp/other.rs",
                "range": {
                    "start": { "line": 99, "character": 0 },
                    "end":   { "line": 99, "character": 1 },
                }
            },
        ]);
        let loc = parse_definition_location(v).expect("first");
        assert_eq!(loc.line, 0);
        assert_eq!(loc.col, 3);
    }

    #[test]
    fn parse_definition_location_link_uses_target_fields() {
        let v = json!({
            "targetUri": "file:///tmp/x.rs",
            "targetSelectionRange": {
                "start": { "line": 12, "character": 4 },
                "end":   { "line": 12, "character": 10 },
            }
        });
        let loc = parse_definition_location(v).expect("locationlink");
        assert_eq!(loc.line, 12);
        assert_eq!(loc.col, 4);
    }

    // ── M18 rename WorkspaceEdit parsing ──────────────────

    #[test]
    fn parse_workspace_edit_changes_shape_flattens_by_uri() {
        let v = json!({
            "changes": {
                "file:///tmp/a.rs": [
                    { "range": { "start": { "line": 1, "character": 4 },
                                 "end":   { "line": 1, "character": 7 } },
                      "newText": "bar" },
                    { "range": { "start": { "line": 3, "character": 12 },
                                 "end":   { "line": 3, "character": 15 } },
                      "newText": "bar" },
                ],
                "file:///tmp/b.rs": [
                    { "range": { "start": { "line": 0, "character": 0 },
                                 "end":   { "line": 0, "character": 3 } },
                      "newText": "bar" },
                ],
            }
        });
        let out = parse_workspace_edit(&v);
        assert_eq!(out.len(), 2);
        // Order across URIs is unspecified; sort for assertion.
        let mut edits_by_name: Vec<_> = out
            .iter()
            .map(|fe| (fe.path.display().to_string(), fe.edits.len()))
            .collect();
        edits_by_name.sort();
        assert!(edits_by_name.iter().any(|(n, k)| n.ends_with("a.rs") && *k == 2));
        assert!(edits_by_name.iter().any(|(n, k)| n.ends_with("b.rs") && *k == 1));
    }

    #[test]
    fn parse_workspace_edit_document_changes_shape() {
        let v = json!({
            "documentChanges": [{
                "textDocument": { "uri": "file:///tmp/a.rs", "version": 3 },
                "edits": [
                    { "range": { "start": { "line": 0, "character": 0 },
                                 "end":   { "line": 0, "character": 3 } },
                      "newText": "bar" },
                ]
            }]
        });
        let out = parse_workspace_edit(&v);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].edits.len(), 1);
        assert_eq!(out[0].edits[0].new_text.as_ref(), "bar");
    }

    #[test]
    fn parse_workspace_edit_unknown_shape_is_empty() {
        assert!(parse_workspace_edit(&json!({})).is_empty());
        assert!(parse_workspace_edit(&Value::Null).is_empty());
    }
}
