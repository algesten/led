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

mod lifecycle;
mod notifications;
mod parse;
mod progress;
mod requests;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use led_core::{BufferVersion, CanonPath, LspRequestSeq, Notifier, PersistedContentHash, ServerId};
use led_driver_lsp_core::{
    DiagnosticSource, LspCmd, LspEvent, Trace,
    diag_source::DiagPushResult,
};
use led_state_syntax::Language;
use ropey::Rope;
use serde_json::Value;

use crate::classify::{Incoming, RequestId};
use crate::registry::LspRegistry;
use crate::subprocess::{Server, ServerIncoming};

/// Unified event the manager thread drains out of one central
/// channel. Cmd + server-message get merged here so the manager
/// blocks on a single `recv()` instead of juggling multiple.
pub(super) enum ManagerEvent {
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
pub(super) struct PendingOpen {
    pub(super) path: CanonPath,
    pub(super) rope: Arc<Rope>,
    pub(super) hash: PersistedContentHash,
}

/// A pending JSON-RPC request. Keyed by request-id on the
/// server entry's `pending_requests` map so a response can find
/// its context.
pub(super) enum PendingRequest {
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
        seq: LspRequestSeq,
        line: u32,
    },
    /// Waiting on a `completionItem/resolve` response. `seq`
    /// echoes `LspCmd::ResolveCompletion.seq` so the runtime
    /// can ignore resolves from a stale session.
    ResolveCompletion { path: CanonPath, seq: LspRequestSeq },
    /// Waiting on a `textDocument/definition` response. `seq`
    /// echoes the runtime's originating
    /// `LspCmd::RequestGotoDefinition.seq`.
    GotoDefinition { seq: LspRequestSeq },
    /// Waiting on a `textDocument/rename` response. `seq`
    /// echoes `LspCmd::RequestRename.seq` so the runtime can
    /// drop stale replies (e.g. after an abort).
    Rename { seq: LspRequestSeq },
    /// Waiting on a `textDocument/codeAction` response. `seq`
    /// echoes `LspCmd::RequestCodeAction.seq`; `path` is
    /// carried through because the `LspEvent::CodeActions`
    /// surface echoes it back.
    CodeAction { seq: LspRequestSeq, path: CanonPath },
    /// Waiting on a `codeAction/resolve` response initiated by
    /// a picker commit. The raw pre-resolve action is stashed
    /// here so if resolve succeeds without `edit`, we fall
    /// back to the raw edit (if any).
    ResolveCodeAction {
        seq: LspRequestSeq,
        raw: Value,
    },
    /// Waiting on `textDocument/inlayHint`. `path` + `version`
    /// echo back in the `LspEvent::InlayHints` emission so the
    /// runtime can version-gate the cache.
    InlayHints {
        path: CanonPath,
        version: BufferVersion,
    },
    /// Waiting on `textDocument/formatting`. `seq` echoes
    /// `LspCmd::RequestFormat.seq`; `path` is forwarded so the
    /// format edit flattens into a `FileEdit` targeting the
    /// right buffer even though the LSP response doesn't echo
    /// the uri.
    Format { seq: LspRequestSeq, path: CanonPath },
}

pub(super) struct ServerEntry {
    pub(super) language: Language,
    pub(super) server: Server,
    pub(super) diag: DiagnosticSource,
    pub(super) pending_requests: HashMap<i64, PendingRequest>,
    pub(super) queued_opens: Vec<PendingOpen>,
    /// Set once the initialize response has been handled AND the
    /// `initialized` notification sent.
    pub(super) initialized: bool,
    /// Per-doc LSP `textDocument.version` counter (1-based,
    /// monotonically increments each didChange). Separate from
    /// the content-hash tracking because the LSP spec demands a
    /// monotonic counter on the wire.
    pub(super) doc_versions: HashMap<CanonPath, i32>,
    /// Current content hash for each opened doc — the anchor we
    /// snapshot into diagnostic windows and stamp outgoing pulls
    /// with. Updated on every `BufferOpened` / `BufferChanged`.
    pub(super) buffer_hashes: HashMap<CanonPath, PersistedContentHash>,
    /// Was a RequestDiagnostics received before the server was
    /// ready? Replayed on first quiescence. Matches legacy's
    /// `init_delayed_request` semantics.
    pub(super) deferred_init_request: bool,
    /// Previous rope snapshot per path — compared against the
    /// new rope on `BufferChanged` to compute an incremental
    /// `textDocument/didChange`. When absent (first change
    /// post-open) we fall back to full-text. Small Arc clone, so
    /// the cache cost is a pointer per path.
    pub(super) last_rope_sent: HashMap<CanonPath, Arc<Rope>>,
    /// Server-advertised completion support. `completion_provider`
    /// gates `textDocument/completion`; `completion_trigger_chars`
    /// informs the runtime which input chars should kick a fresh
    /// request (the driver forwards them as-is — the runtime
    /// decides per-keystroke). `completion_resolve_provider` is
    /// future-proofing: controls whether `completionItem/resolve`
    /// round-trips on commit.
    pub(super) completion_provider: bool,
    pub(super) completion_trigger_chars: Vec<char>,
    pub(super) completion_resolve_provider: bool,
    /// Cache of raw `CodeAction`/`Command` items returned by
    /// the last `textDocument/codeAction` request, keyed by
    /// the `action_id` strings we surfaced on the
    /// corresponding `CodeActionSummary`. The runtime's
    /// `SelectCodeAction` echoes the chosen id back so we can
    /// look up the opaque LSP object and issue
    /// `codeAction/resolve` (if needed) or apply its `edit`
    /// field directly.
    pub(super) code_action_cache: HashMap<Arc<str>, Value>,
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

pub(super) struct Manager {
    pub(super) registry: LspRegistry,
    pub(super) servers: HashMap<Language, ServerEntry>,
    pub(super) next_request_id: i64,
    pub(super) incoming_tx: Sender<ServerIncoming>,
    pub(super) event_rx: Receiver<ManagerEvent>,
    pub(super) lsp_event_tx: Sender<LspEvent>,
    pub(super) notify: Notifier,
    pub(super) trace: Arc<dyn Trace>,
    pub(super) workspace_root: Option<CanonPath>,
    /// Cmds stashed here while any server's diag window is
    /// frozen. Drained FIFO once every server unfreezes.
    pub(super) deferred_cmds: VecDeque<LspCmd>,
    /// Languages whose LSP binary isn't installed, or whose
    /// spawn failed unrecoverably. Prevents re-trying the
    /// spawn on every `BufferOpened` for that language — once
    /// we've decided "this language has no server", stay
    /// decided for the session.
    pub(super) skipped_languages: std::collections::HashSet<Language>,

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
    pub(super) progress_tokens: HashMap<String, ProgressInfo>,
    /// Per-server quiescence state. Absent = default-idle
    /// (matches legacy's `unwrap_or(&true)`). Present = the last
    /// value the server reported.
    pub(super) quiescent: HashMap<Language, bool>,
    /// Last instant `send_progress_throttled` fired a
    /// `LspEvent::Progress`. 200ms minimum gap between sends,
    /// EXCEPT busy→idle transitions always fire (so the UI
    /// never gets stuck showing a spinner after the server
    /// went idle).
    pub(super) last_progress_sent_at: Option<Instant>,
    /// Last `busy` value we actually emitted. Used by the
    /// throttle's "busy→idle always fires" exception.
    pub(super) last_progress_busy: bool,
}

/// One open `$/progress` token's current title/message pair.
/// Built from `begin`; updated by `report`; removed on `end`.
#[derive(Debug, Clone, Default)]
pub(super) struct ProgressInfo {
    pub(super) title: Option<String>,
    pub(super) message: Option<String>,
}

/// Friendly server name for trace lines. `Server.name` is the
/// `command` field from the registry — `rust-analyzer` /
/// `taplo` for real binaries, but the goldens harness overrides
/// it with the absolute path to `fake-lsp`. Trim to the
/// basename so traces read `server=fake-lsp` regardless of
/// where the binary lives on disk.
pub(super) fn short_server_name(name: &str) -> &str {
    std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
}

/// Convenience: build a [`ServerId`] from `Server.name` by
/// trimming with [`short_server_name`]. Used everywhere the
/// driver crosses an event boundary (trace, `LspEvent`).
pub(super) fn short_server_id(name: &str) -> ServerId {
    ServerId::new(short_server_name(name))
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

    pub(super) fn any_frozen(&self) -> bool {
        self.servers.values().any(|e| e.diag.is_frozen())
    }

    // ── Progress aggregation ─────────────────────────────────

    /// True if any server has reported `quiescent=false` OR any
    /// `$/progress` token is open. Matches legacy exactly —
    /// either source independently keeps the spinner running.
    pub(super) fn is_busy(&self) -> bool {
        self.progress_tokens_busy() || self.any_non_quiescent()
    }

    pub(super) fn progress_tokens_busy(&self) -> bool {
        !self.progress_tokens.is_empty()
    }

    pub(super) fn any_non_quiescent(&self) -> bool {
        self.quiescent.values().any(|q| !q)
    }

    /// Detail string for `LspEvent::Progress.detail`. Source is
    /// exclusively `$/progress` — the first open token's
    /// `"{title} {message}"`, or just `"{title}"` if no message.
    /// Matches legacy `progress_lsp_in` (manager.rs:1689-1709).
    pub(super) fn progress_detail(&self) -> Option<String> {
        let info = self.progress_tokens.values().next()?;
        match (info.title.as_deref(), info.message.as_deref()) {
            (Some(t), Some(m)) if !m.is_empty() => Some(format!("{t} {m}")),
            (Some(t), _) => Some(t.to_string()),
            (None, Some(m)) if !m.is_empty() => Some(m.to_string()),
            _ => None,
        }
    }

    /// First registered server's id — used as the `server`
    /// field on emitted `LspEvent::Progress`. Matches legacy's
    /// "show whichever server got started first" behaviour.
    /// `None` when no server has spawned yet (the caller skips
    /// the emission in that case). Carries the registry-spawn
    /// `name` (full binary path) so the runtime's status-bar
    /// rendering matches what the user typed at the command
    /// line / shell-substituted PATH lookup.
    pub(super) fn first_server_name(&self) -> Option<ServerId> {
        self.servers
            .values()
            .next()
            .map(|e| ServerId::new(e.server.name.clone()))
    }

    fn earliest_deadline(&self) -> Option<Instant> {
        self.servers
            .values()
            .filter_map(|e| e.diag.deadline())
            .min()
    }

    pub(super) fn try_drain_deferred(&mut self) {
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

    pub(super) fn fresh_id(&mut self) -> i64 {
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
            LspCmd::DidChangeWatchedFiles { server, changes } => {
                self.did_change_watched_files(&server, &changes);
            }
        }
    }

    /// Send `workspace/didChangeWatchedFiles` to the server whose
    /// short name matches `server`. Silently no-ops when no
    /// matching server exists (the server may have crashed
    /// between the registration and the runtime's fan-out tick).
    fn did_change_watched_files(
        &mut self,
        server: &ServerId,
        changes: &[led_driver_lsp_core::FileEvent],
    ) {
        if changes.is_empty() {
            return;
        }
        let language = self.servers.iter().find_map(|(l, e)| {
            (short_server_name(&e.server.name) == server.as_str()).then_some(*l)
        });
        let Some(language) = language else { return };
        let entry = self.servers.get_mut(&language).expect("just found");
        let body = crate::protocol::build_did_change_watched_files_notification(changes);
        let _ = entry.server.send_body(&body);
        self.trace.lsp_send_notification(
            server,
            "workspace/didChangeWatchedFiles",
            None,
            None,
        );
    }

    /// Look up which server handles `path`. Matches legacy
    /// `server_for_path` — a path is associated with whichever
    /// language the runtime opened it under.
    pub(super) fn language_for_path(&self, path: &CanonPath) -> Option<Language> {
        self.servers.iter().find_map(|(lang, entry)| {
            entry.doc_versions.contains_key(path).then_some(*lang)
        })
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
                id,
                auto_reply,
                forward_as_notification,
                method,
                params,
            } => {
                let id_int = match &id {
                    RequestId::Int(n) => *n,
                    RequestId::Str(_) => -1,
                };
                if let Some(entry) = self.servers.get(&language) {
                    let server_name = short_server_id(&entry.server.name);
                    self.trace.lsp_recv_request(&server_name, &method, id_int);
                }
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
                short_server_id(&entry.server.name),
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

    pub(super) fn dispatch_push_result(
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

#[cfg(test)]
mod tests {
    use super::notifications::{
        DIDCHANGE_INCREMENTAL_MAX_CHARS, char_delta_bounds, char_idx_to_line_utf16,
        incremental_content_change,
    };
    use super::parse::{
        parse_definition_location, parse_diagnostic_entry, parse_diagnostic_result,
        parse_workspace_edit,
    };
    use super::*;
    use led_state_diagnostics::DiagnosticSeverity;
    use serde_json::json;

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
