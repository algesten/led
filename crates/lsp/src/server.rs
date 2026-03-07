use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use lsp_types::*;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::registry::ServerConfig;
use crate::transport::{self, LspNotification, RequestId, ResponseHandlers};

#[derive(Debug, Clone)]
pub struct LspError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LSP error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for LspError {}

pub struct LanguageServer {
    pub name: String,
    next_id: AtomicI32,
    outbound_tx: mpsc::UnboundedSender<String>,
    response_handlers: ResponseHandlers,
    #[allow(dead_code)]
    capabilities: Option<ServerCapabilities>,
    child: Option<tokio::process::Child>,
}

impl LanguageServer {
    pub async fn start(
        config: &ServerConfig,
        root_uri: Uri,
        notification_tx: mpsc::UnboundedSender<LspNotification>,
        waker: Option<led_core::Waker>,
    ) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn {}: {}", config.command, e))?;

        let stdin = child.stdin.take().ok_or("No stdin")?;
        let stdout = child.stdout.take().ok_or("No stdout")?;

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<String>();
        let response_handlers: ResponseHandlers = Arc::new(Mutex::new(HashMap::new()));

        transport::spawn_writer(stdin, outbound_rx);
        transport::spawn_reader(
            stdout,
            response_handlers.clone(),
            notification_tx,
            outbound_tx.clone(),
            waker,
        );

        let mut server = Self {
            name: config.command.clone(),
            next_id: AtomicI32::new(1),
            outbound_tx,
            response_handlers,
            capabilities: None,
            child: Some(child),
        };

        // Initialize handshake
        #[allow(deprecated)]
        let init_params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    synchronization: Some(TextDocumentSyncClientCapabilities {
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        did_save: Some(true),
                    }),
                    definition: Some(GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(false),
                    }),
                    rename: Some(RenameClientCapabilities {
                        dynamic_registration: Some(false),
                        prepare_support: Some(true),
                        ..Default::default()
                    }),
                    code_action: Some(CodeActionClientCapabilities {
                        dynamic_registration: Some(false),
                        ..Default::default()
                    }),
                    formatting: Some(DocumentFormattingClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    inlay_hint: Some(InlayHintClientCapabilities {
                        dynamic_registration: Some(false),
                        ..Default::default()
                    }),
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                        related_information: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let result = server
            .request::<lsp_types::request::Initialize>(init_params)
            .await
            .map_err(|e| format!("Initialize failed: {e}"))?;

        server.capabilities = Some(result.capabilities);

        // Send initialized notification
        server.notify::<lsp_types::notification::Initialized>(InitializedParams {});

        Ok(server)
    }

    pub async fn request<R: lsp_types::request::Request>(
        &self,
        params: R::Params,
    ) -> Result<R::Result, LspError>
    where
        R::Params: serde::Serialize,
        R::Result: serde::de::DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req_id = RequestId::Int(id);

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req_id.to_value(),
            "method": R::METHOD,
            "params": serde_json::to_value(&params).unwrap_or(Value::Null),
        });

        let (tx, rx) = oneshot::channel();
        self.response_handlers.lock().unwrap().insert(req_id, tx);

        self.outbound_tx
            .send(msg.to_string())
            .map_err(|_| LspError {
                code: -1,
                message: "Channel closed".into(),
            })?;

        let result = rx.await.map_err(|_| LspError {
            code: -1,
            message: "Response channel dropped".into(),
        })??;

        serde_json::from_value(result).map_err(|e| LspError {
            code: -1,
            message: format!("Failed to deserialize response: {e}"),
        })
    }

    pub fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params)
    where
        N::Params: serde::Serialize,
    {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": N::METHOD,
            "params": serde_json::to_value(&params).unwrap_or(Value::Null),
        });
        let _ = self.outbound_tx.send(msg.to_string());
    }

    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }

    pub async fn shutdown(mut self) {
        // Send shutdown request (ignore errors — server may already be gone)
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "shutdown",
            "params": null,
        });
        let _ = self.outbound_tx.send(msg.to_string());

        // Send exit notification
        let exit = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
        });
        let _ = self.outbound_tx.send(exit.to_string());

        // Wait briefly for the child to exit
        if let Some(ref mut child) = self.child {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        }
    }
}
