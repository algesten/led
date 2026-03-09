use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use led_core::Waker;
use lsp_types::{InitializeParams, InitializeResult, InitializedParams, ServerCapabilities};
use serde_json::Value;

use crate::registry::ServerConfig;
use crate::transport::{
    LspError, LspNotification, RequestId, ResponseHandlers, spawn_reader, spawn_writer,
};
use crate::util::uri_from_path;

pub(crate) struct LanguageServer {
    pub(crate) name: String,
    next_id: AtomicI32,
    pub(crate) outbound_tx: tokio::sync::mpsc::UnboundedSender<String>,
    response_handlers: ResponseHandlers,
    pub(crate) capabilities: Mutex<Option<ServerCapabilities>>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl LanguageServer {
    pub(crate) async fn start(
        config: &ServerConfig,
        root: &Path,
        notification_tx: tokio::sync::mpsc::UnboundedSender<LspNotification>,
        waker: Option<Waker>,
    ) -> Result<Arc<Self>, LspError> {
        use tokio::process::Command;

        let mut child = Command::new(config.command)
            .args(config.args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LspError {
                message: format!("Failed to start {}: {}", config.command, e),
            })?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        // Log stderr in background
        let stderr = child.stderr.take().unwrap();
        let server_name = config.command.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::info!("LSP stderr [{}]: {}", server_name, line);
            }
        });

        let (writer_tx, writer_rx) = tokio::sync::mpsc::unbounded_channel();
        let response_handlers: ResponseHandlers = Arc::new(Mutex::new(HashMap::new()));

        spawn_writer(writer_rx, stdin);
        spawn_reader(
            stdout,
            response_handlers.clone(),
            notification_tx,
            writer_tx.clone(),
            waker,
        );

        let server = Arc::new(Self {
            name: config.command.to_string(),
            next_id: AtomicI32::new(1),
            outbound_tx: writer_tx,
            response_handlers,
            capabilities: Mutex::new(None),
            child: Mutex::new(Some(child)),
        });

        // Send initialize
        let root_uri = uri_from_path(root);
        #[allow(deprecated)]
        let init_params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root_uri.clone(),
            root_path: None,
            initialization_options: None,
            capabilities: lsp_types::ClientCapabilities {
                text_document: Some(lsp_types::TextDocumentClientCapabilities {
                    synchronization: Some(lsp_types::TextDocumentSyncClientCapabilities {
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        did_save: Some(true),
                    }),
                    definition: Some(lsp_types::GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
                    }),
                    rename: Some(lsp_types::RenameClientCapabilities {
                        dynamic_registration: Some(false),
                        prepare_support: Some(false),
                        ..Default::default()
                    }),
                    code_action: Some(lsp_types::CodeActionClientCapabilities {
                        dynamic_registration: Some(false),
                        code_action_literal_support: Some(lsp_types::CodeActionLiteralSupport {
                            code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                                value_set: vec![
                                    lsp_types::CodeActionKind::QUICKFIX.as_str().to_string(),
                                    lsp_types::CodeActionKind::REFACTOR.as_str().to_string(),
                                    lsp_types::CodeActionKind::SOURCE.as_str().to_string(),
                                ],
                            },
                        }),
                        resolve_support: Some(lsp_types::CodeActionCapabilityResolveSupport {
                            properties: vec!["edit".to_string()],
                        }),
                        ..Default::default()
                    }),
                    formatting: Some(lsp_types::DocumentFormattingClientCapabilities {
                        dynamic_registration: Some(false),
                    }),
                    publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                        related_information: Some(false),
                        tag_support: None,
                        version_support: Some(false),
                        code_description_support: Some(false),
                        data_support: Some(false),
                    }),
                    inlay_hint: Some(lsp_types::InlayHintClientCapabilities {
                        dynamic_registration: Some(false),
                        resolve_support: None,
                    }),
                    ..Default::default()
                }),
                window: Some(lsp_types::WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                workspace: Some(lsp_types::WorkspaceClientCapabilities {
                    did_change_watched_files: Some(
                        lsp_types::DidChangeWatchedFilesClientCapabilities {
                            dynamic_registration: Some(true),
                            relative_pattern_support: Some(false),
                        },
                    ),
                    workspace_edit: Some(lsp_types::WorkspaceEditClientCapabilities {
                        document_changes: Some(true),
                        ..Default::default()
                    }),
                    configuration: Some(true),
                    ..Default::default()
                }),
                experimental: Some(serde_json::json!({
                    "serverStatusNotification": true,
                })),
                ..Default::default()
            },
            trace: None,
            workspace_folders: root_uri.map(|uri| {
                vec![lsp_types::WorkspaceFolder {
                    uri,
                    name: root
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                }]
            }),
            client_info: Some(lsp_types::ClientInfo {
                name: "led".to_string(),
                version: Some("0.1.0".to_string()),
            }),
            locale: None,
            work_done_progress_params: Default::default(),
        };

        let result: InitializeResult = server.request("initialize", &init_params).await?;
        log::debug!("LSP server capabilities: {:?}", result.capabilities);
        *server.capabilities.lock().unwrap() = Some(result.capabilities);

        // Send initialized notification
        server.notify("initialized", &InitializedParams {});

        // Push empty config — rust-analyzer waits for this before indexing
        server.notify(
            "workspace/didChangeConfiguration",
            &lsp_types::DidChangeConfigurationParams {
                settings: Value::Object(serde_json::Map::new()),
            },
        );

        Ok(server)
    }

    pub(crate) async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, LspError> {
        log::info!("LSP -> request: {}", method);
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req_id = RequestId::Int(id);

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": serde_json::to_value(params).unwrap_or(Value::Null),
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut handlers = self.response_handlers.lock().unwrap();
            handlers.insert(req_id, tx);
        }

        self.outbound_tx
            .send(msg.to_string())
            .map_err(|_| LspError {
                message: "Server connection closed".to_string(),
            })?;

        let result = rx.await.map_err(|_| LspError {
            message: "Response channel closed".to_string(),
        })??;

        serde_json::from_value(result.clone()).map_err(|e| {
            log::error!(
                "Failed to deserialize LSP response for {}: {} -- raw: {}",
                method,
                e,
                &result.to_string()[..result.to_string().len().min(500)],
            );
            LspError {
                message: format!("Failed to deserialize response: {}", e),
            }
        })
    }

    pub(crate) fn notify<P: serde::Serialize>(&self, method: &str, params: &P) {
        log::info!("LSP -> notify: {}", method);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": serde_json::to_value(params).unwrap_or(Value::Null),
        });
        let _ = self.outbound_tx.send(msg.to_string());
    }

    pub(crate) async fn shutdown(&self) {
        // Send shutdown request
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "shutdown",
            "params": null,
        });
        let _ = self.outbound_tx.send(msg.to_string());

        // Give server a moment, then send exit
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let exit = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null,
        });
        let _ = self.outbound_tx.send(exit.to_string());

        // Wait for child to exit
        if let Some(mut child) = self.child.lock().unwrap().take() {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
                let _ = child.kill().await;
            });
        }
    }
}
