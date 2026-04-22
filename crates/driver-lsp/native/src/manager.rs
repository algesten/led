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

use led_core::{CanonPath, Notifier};
use led_driver_lsp_core::{
    DiagnosticSource, LspCmd, LspEvent, Trace,
    diag_source::{DiagMode, DiagPushResult},
};
use led_state_diagnostics::{BufferVersion, Diagnostic, DiagnosticSeverity};
use led_state_syntax::Language;
use ropey::Rope;
use serde_json::{Value, json};

use crate::classify::{Incoming, RequestId};
use crate::protocol::{
    InitializeCapabilities, build_initialize_request,
    build_initialized_notification, language_id, parse_initialize_response,
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
    version: BufferVersion,
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
    /// our `BufferVersion` because LSP demands its own.
    doc_versions: HashMap<CanonPath, i32>,
    /// Current `BufferVersion` for each opened doc — the version
    /// we stamp outgoing pulls with. Updated on every
    /// `BufferOpened` / `BufferChanged`.
    buffer_versions: HashMap<CanonPath, BufferVersion>,
    /// Was a RequestDiagnostics received before the server was
    /// ready? Replayed on first quiescence. Matches legacy's
    /// `init_delayed_request` semantics.
    deferred_init_request: bool,
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
                version,
            } => {
                let Some(lang) = language else { return };
                self.ensure_server_spawned(lang);
                self.open_buffer(lang, path, rope, version);
            }
            LspCmd::BufferChanged {
                path,
                rope,
                version,
                is_save,
            } => {
                self.buffer_changed(&path, &rope, version, is_save);
            }
            LspCmd::BufferClosed { path } => {
                self.buffer_closed(&path);
            }
            LspCmd::RequestDiagnostics => {
                self.request_diagnostics();
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
        let name = format!("{:?}", language);
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

        let id = self.fresh_id();
        let root = self
            .workspace_root
            .clone()
            .unwrap_or_else(CanonPath::default);
        let body = build_initialize_request(id, &root);
        let _ = server.send_body(&body);

        let mut entry = ServerEntry {
            language,
            server,
            diag: DiagnosticSource::new(),
            pending_requests: HashMap::new(),
            queued_opens: Vec::new(),
            initialized: false,
            doc_versions: HashMap::new(),
            buffer_versions: HashMap::new(),
            deferred_init_request: false,
        };
        entry.pending_requests.insert(id, PendingRequest::Initialize);
        self.servers.insert(language, entry);
    }

    fn open_buffer(
        &mut self,
        language: Language,
        path: CanonPath,
        rope: Arc<Rope>,
        version: BufferVersion,
    ) {
        let Some(entry) = self.servers.get_mut(&language) else {
            return;
        };
        entry.buffer_versions.insert(path.clone(), version);
        if entry.initialized {
            send_did_open(entry, &path, &rope);
        } else {
            entry.queued_opens.push(PendingOpen {
                path,
                rope,
                version,
            });
        }
    }

    fn buffer_changed(
        &mut self,
        path: &CanonPath,
        rope: &Arc<Rope>,
        version: BufferVersion,
        is_save: bool,
    ) {
        // Find the server that has this path open.
        let language = self.servers.iter().find_map(|(l, e)| {
            e.doc_versions.contains_key(path).then_some(*l)
        });
        let Some(language) = language else { return };
        let entry = self.servers.get_mut(&language).expect("just found");

        // Freeze discipline: the rope moved, so any open window
        // that snapshotted this path is now stale. Close it so a
        // later RequestDiagnostics opens a fresh window at the
        // new version.
        if entry.diag.should_close_window(path, version) {
            entry.diag.close_window();
        }

        entry.buffer_versions.insert(path.clone(), version);
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
        let body = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {
                    "uri": uri_from_path(path),
                    "version": lsp_version,
                },
                // Full-sync: the whole rope as one change. Legacy
                // also uses full-sync here (incremental adds
                // complexity for minimal win on small files).
                "contentChanges": [
                    { "text": rope.to_string() }
                ],
            },
        });
        let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());

        if is_save {
            let save_body = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didSave",
                "params": {
                    "textDocument": { "uri": uri_from_path(path) },
                },
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&save_body).unwrap());
        }
    }

    fn buffer_closed(&mut self, path: &CanonPath) {
        let language = self.servers.iter().find_map(|(l, e)| {
            e.doc_versions.contains_key(path).then_some(*l)
        });
        let Some(language) = language else { return };
        let entry = self.servers.get_mut(&language).expect("just found");

        let body = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {
                "textDocument": { "uri": uri_from_path(path) },
            },
        });
        let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
        entry.doc_versions.remove(path);
        entry.buffer_versions.remove(path);
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
                let snap = entry.buffer_versions.clone();
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
        snapshot: HashMap<CanonPath, BufferVersion>,
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
        for (path, diags, version) in cache {
            let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                path: path.clone(),
                version,
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
                version,
            );
        }
        self.notify.notify();

        // Issue pulls.
        for path in pulls {
            let id = self.fresh_id();
            let entry = self.servers.get_mut(&lang).unwrap();
            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/diagnostic",
                "params": {
                    "textDocument": { "uri": uri_from_path(&path) },
                },
            });
            let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
            entry.pending_requests.insert(
                id,
                PendingRequest::PullDiagnostic { path: path.clone() },
            );
        }
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
            let shutdown_body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "shutdown",
                "params": Value::Null,
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&shutdown_body).unwrap());
            entry.pending_requests.insert(id, PendingRequest::Shutdown);
            let exit_body = json!({
                "jsonrpc": "2.0",
                "method": "exit",
                "params": Value::Null,
            });
            let _ = entry
                .server
                .send_body(&serde_json::to_vec(&exit_body).unwrap());
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
        let pending = {
            let entry = self.servers.get_mut(&language).unwrap();
            entry.pending_requests.remove(&n)
        };
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
                if caps.has_quiescence {
                    entry.diag.set_has_quiescence(true);
                }
                let _ = entry.server.send_body(&build_initialized_notification());
                entry.initialized = true;
                let queued = std::mem::take(&mut entry.queued_opens);
                for open in queued {
                    send_did_open(entry, &open.path, &open.rope);
                    entry.buffer_versions.insert(open.path.clone(), open.version);
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
        if let Some((path, diagnostics, version)) = forward {
            self.trace
                .lsp_diagnostics_done(&path, diagnostics.len(), version);
            let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                path,
                version,
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
                    entry.diag.on_push(path.clone(), diags)
                };
                self.dispatch_push_result(language, path, result);
            }
            "experimental/serverStatus" => {
                // rust-analyzer's custom status extension.
                // `quiescent=false` = server is working (indexing,
                // cachePriming, type-checking, …). `quiescent=true`
                // = idle. `message` carries a human-readable tail.
                //
                // Fire a `Progress` event for BOTH states so the
                // status-bar spinner animates throughout the
                // busy phase. The quiescent-true case additionally
                // runs `on_quiescence` to release any deferred
                // init RequestDiagnostics.
                let quiescent = params
                    .get("quiescent")
                    .and_then(|q| q.as_bool())
                    .unwrap_or(false);
                let message = params
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                let server_name = self.servers[&language].server.name.clone();
                let _ = self.lsp_event_tx.send(LspEvent::Progress {
                    server: server_name.clone(),
                    busy: !quiescent,
                    detail: message,
                });
                self.notify.notify();
                if quiescent {
                    let _ = self
                        .lsp_event_tx
                        .send(LspEvent::Ready { server: server_name });
                    self.notify.notify();
                    let reissue = {
                        let entry = self.servers.get_mut(&language).unwrap();
                        let reissue = entry.diag.on_quiescence();
                        entry.deferred_init_request = false;
                        reissue
                    };
                    if reissue {
                        self.request_diagnostics();
                    }
                }
            }
            "$/progress" => {
                // Progress token stream — legacy surfaces as
                // `LspEvent::Progress { busy, detail }`. For M16
                // we keep this as a best-effort pass-through.
                let title = params
                    .pointer("/value/title")
                    .or_else(|| params.pointer("/value/message"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let kind = params
                    .pointer("/value/kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                let busy = matches!(kind, "begin" | "report");
                let server_name = self.servers[&language].server.name.clone();
                let _ = self.lsp_event_tx.send(LspEvent::Progress {
                    server: server_name,
                    busy,
                    detail: title,
                });
                self.notify.notify();
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
            DiagPushResult::Forward(p, diags, version) => {
                self.trace.lsp_diagnostics_done(&p, diags.len(), version);
                let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                    path: p,
                    version,
                    diagnostics: diags,
                });
                self.notify.notify();
            }
            DiagPushResult::ForwardClearing(p) => {
                let version = self.servers[&language]
                    .buffer_versions
                    .get(&p)
                    .copied()
                    .unwrap_or_default();
                let _ = self.lsp_event_tx.send(LspEvent::Diagnostics {
                    path: p,
                    version,
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

fn send_did_open(entry: &mut ServerEntry, path: &CanonPath, rope: &Arc<Rope>) {
    let body = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri_from_path(path),
                "languageId": language_id(entry.language),
                "version": 1,
                "text": rope.to_string(),
            },
        },
    });
    entry.doc_versions.insert(path.clone(), 1);
    let _ = entry.server.send_body(&serde_json::to_vec(&body).unwrap());
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
}
