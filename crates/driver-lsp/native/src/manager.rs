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
//! # Scope for this chunk
//!
//! - Spawn one server per language on first `BufferOpened` for
//!   that language.
//! - Drive the full initialize → initialized → didOpen sequence.
//! - Parse initialize response capabilities → feed
//!   `DiagnosticSource::set_mode` / `set_has_quiescence`.
//! - Queue `BufferOpened` requests that arrive before the
//!   handshake completes; flush them once `initialized` is sent.
//!
//! Not yet in this chunk (layer on next):
//! - BufferChanged / BufferClosed forwarding.
//! - textDocument/diagnostic pulls.
//! - publishDiagnostics push handling.
//! - Shutdown flow.
//! - Quiescence + freeze discipline in the event loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use led_core::{CanonPath, Notifier};
use led_driver_lsp_core::{
    DiagnosticSource, LspCmd, LspEvent, Trace,
    diag_source::DiagMode,
};
use led_state_diagnostics::BufferVersion;
use led_state_syntax::Language;
use ropey::Rope;
use serde_json::{Value, json};

use crate::classify::{Incoming, RequestId};
use crate::framing::encode_frame;
use crate::protocol::{
    InitializeCapabilities, build_initialize_request,
    build_initialized_notification, language_id, parse_initialize_response,
    uri_from_path,
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
/// notification. Legacy did the same: once initialize returns,
/// flush the pending opens.
struct PendingOpen {
    path: CanonPath,
    rope: Arc<Rope>,
    version: BufferVersion,
}

/// A pending response — we sent a request with id `n`, the
/// server will reply with `id: n`. Used for request/response
/// correlation.
enum PendingRequest {
    /// Initialize response. Carries the list of pending opens to
    /// flush once capabilities land.
    Initialize,
}

/// One running language server + every piece of state the
/// manager tracks about it.
struct ServerEntry {
    language: Language,
    server: Server,
    diag: DiagnosticSource,
    pending_requests: HashMap<i64, PendingRequest>,
    /// `BufferOpened` requests that arrived before the handshake
    /// finished. Flushed as `didOpen` notifications once
    /// initialize returns.
    queued_opens: Vec<PendingOpen>,
    /// Tracks whether we've already sent `initialized` — once
    /// true, `BufferOpened` goes straight through as `didOpen`.
    initialized: bool,
    /// Per-doc `textDocument.version` counter. Separate from our
    /// `BufferVersion` because LSP insists on its own monotonic
    /// counter (starts at 1 on didOpen, increments each didChange).
    doc_versions: HashMap<CanonPath, i32>,
}

/// Lifecycle marker. Drop order matters:
/// 1. Manager thread exits when `event_rx` hangs up.
/// 2. Cmd adapter exits when `cmd_rx` hangs up.
/// 3. Each `Server`'s Drop kills its subprocess + joins pumps.
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

    // Central event channel — one half fed by cmd adapter, other
    // half by reader threads. Manager drains the rx.
    let (central_tx, central_rx) = mpsc::channel::<ManagerEvent>();

    let cmd_adapter_handle = spawn_cmd_adapter(cmd_rx, central_tx.clone());

    // Reader threads speak `ServerIncoming`; manager drains
    // `ManagerEvent`. One forwarder thread bridges.
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

/// Adapter thread: read LspCmds from the runtime, wrap each as
/// `ManagerEvent::Cmd`, forward to the central channel. On
/// hangup, emit `CmdChannelClosed` so the manager knows to
/// unwind.
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

/// Bridge: reader threads emit `ServerIncoming`; this thread
/// wraps each as `ManagerEvent::ServerMessage` so the manager
/// can drain one channel. Exits when every sender is dropped
/// (i.e. all servers have shut down).
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
    /// Sender cloned into each server's reader thread. Every
    /// parsed frame ends up as a `ServerIncoming` here; the
    /// forwarder thread re-wraps into `ManagerEvent` for the
    /// central event channel.
    incoming_tx: Sender<ServerIncoming>,
    event_rx: Receiver<ManagerEvent>,
    lsp_event_tx: Sender<LspEvent>,
    notify: Notifier,
    trace: Arc<dyn Trace>,
    workspace_root: Option<CanonPath>,
}

impl Manager {
    fn run(&mut self) {
        while let Ok(ev) = self.event_rx.recv() {
            match ev {
                ManagerEvent::CmdChannelClosed => return,
                ManagerEvent::Cmd(cmd) => self.handle_cmd(cmd),
                ManagerEvent::ServerMessage(msg) => self.handle_server_message(msg),
            }
        }
    }

    fn fresh_id(&mut self) -> i64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    fn handle_cmd(&mut self, cmd: LspCmd) {
        match cmd {
            LspCmd::Init { root } => {
                self.workspace_root = Some(root);
            }
            LspCmd::Shutdown => {
                // TODO(stage 4g): graceful shutdown — send
                // `shutdown` + `exit` to every server. For now,
                // just drop our server handles which kills the
                // subprocesses.
                self.servers.clear();
            }
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
            LspCmd::BufferChanged { .. }
            | LspCmd::BufferClosed { .. }
            | LspCmd::RequestDiagnostics => {
                // TODO(stage 4g): forward as didChange / didClose
                // notifications; drive DiagnosticSource windows.
            }
        }
    }

    /// Lazy-spawn: first time a buffer of language L opens, we
    /// spawn L's server. Subsequent buffers of L reuse it.
    fn ensure_server_spawned(&mut self, language: Language) {
        if self.servers.contains_key(&language) {
            return;
        }
        let Some(config) = self.registry.config_for(language) else {
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
                let _ = self.lsp_event_tx.send(LspEvent::Error {
                    server: name,
                    message: format!("spawn failed: {e}"),
                });
                self.notify.notify();
                return;
            }
        };
        self.trace.lsp_server_started(&server.name);

        // Send initialize immediately — no BufferOpened queueing
        // happens yet because we just spawned.
        let id = self.fresh_id();
        let root = self
            .workspace_root
            .clone()
            .unwrap_or_else(|| CanonPath::default());
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
        };
        entry.pending_requests.insert(id, PendingRequest::Initialize);
        self.servers.insert(language, entry);
    }

    /// Either fire `didOpen` immediately (if the server is
    /// ready) or queue for post-initialize flush.
    fn open_buffer(
        &mut self,
        language: Language,
        path: CanonPath,
        rope: Arc<Rope>,
        _buffer_version: BufferVersion,
    ) {
        let Some(entry) = self.servers.get_mut(&language) else {
            return;
        };
        if entry.initialized {
            send_did_open(entry, &path, &rope);
        } else {
            entry.queued_opens.push(PendingOpen {
                path,
                rope,
                version: _buffer_version,
            });
        }
    }

    fn handle_server_message(&mut self, msg: ServerIncoming) {
        // Find the entry by its `name` string — the manager
        // stores servers keyed by `Language`, not name, so we
        // translate. Cheap: the server count is tiny (~1-10).
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
        let Some(entry) = self.servers.get_mut(&language) else {
            return;
        };
        let RequestId::Int(n) = id else { return };
        let Some(pending) = entry.pending_requests.remove(&n) else {
            return;
        };
        match pending {
            PendingRequest::Initialize => match payload {
                Ok(result) => {
                    let caps: InitializeCapabilities = parse_initialize_response(&result);
                    if caps.diagnostic_provider {
                        entry.diag.set_mode(DiagMode::Pull);
                    }
                    if caps.has_quiescence {
                        entry.diag.set_has_quiescence(true);
                    }
                    // initialized notification
                    let _ = entry.server.send_body(&build_initialized_notification());
                    entry.initialized = true;
                    // Flush queued opens
                    let queued = std::mem::take(&mut entry.queued_opens);
                    for open in queued {
                        send_did_open(entry, &open.path, &open.rope);
                        let _ = open.version; // stashed for stage 4g
                    }
                }
                Err(err) => {
                    let _ = self.lsp_event_tx.send(LspEvent::Error {
                        server: entry.server.name.clone(),
                        message: format!(
                            "initialize failed (code {}): {}",
                            err.code, err.message
                        ),
                    });
                    self.notify.notify();
                }
            },
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
        // Always send the auto-reply. `forward_as_notification` is
        // ignored for now — stage 4g wires trigger-char capability
        // tracking where it matters.
        let Some(entry) = self.servers.get_mut(&language) else {
            return;
        };
        let body = serde_json::to_vec(&auto_reply).expect("auto-reply is valid");
        let _ = entry.server.send_body(&body);
    }

    fn handle_notification(
        &mut self,
        _language: Language,
        _method: String,
        _params: Value,
    ) {
        // TODO(stage 4g): route publishDiagnostics → on_push,
        // experimental/serverStatus → on_quiescence, $/progress
        // → LspEvent::Progress, window/logMessage → drop.
    }
}

/// Serialise + send a `textDocument/didOpen` notification.
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

/// Handy frame-encode helper (kept here so tests can reach it
/// without pulling in the whole manager).
#[allow(dead_code)]
fn encode(body: &Value) -> Vec<u8> {
    encode_frame(&serde_json::to_vec(body).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a sequence of LspCmds through the cmd adapter thread
    /// and verify they arrive on the central channel wrapped as
    /// `ManagerEvent::Cmd` in the same order.
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
        drop(cmd_tx); // trigger hangup path

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
}
