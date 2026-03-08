use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use led_core::lsp_types::{
    DiagnosticSeverity as EditorSeverity, EditorCodeAction, EditorDiagnostic, EditorInlayHint,
    EditorPosition, EditorRange, EditorTextEdit,
};
use led_core::{Action, Component, Context, DrawContext, Effect, Event, LspStatus, PanelClaim, Waker};

use lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionResponse, DocumentFormattingParams,
    FormattingOptions, GotoDefinitionParams, GotoDefinitionResponse, InlayHint, InlayHintLabel,
    InlayHintParams, InitializeParams, InitializeResult, InitializedParams, Location,
    NumberOrString, Position, PublishDiagnosticsParams, Range, RenameParams,
    ServerCapabilities, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    TextEdit, Uri, WorkspaceEdit,
};

use ratatui::Frame;
use ratatui::layout::Rect;
use serde_json::Value;

// ---------------------------------------------------------------------------
// URI helpers
// ---------------------------------------------------------------------------

fn uri_from_path(path: &Path) -> Option<Uri> {
    let s = format!("file://{}", path.to_str()?);
    s.parse().ok()
}

fn path_from_uri(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

// ---------------------------------------------------------------------------
// UTF-16 column conversion
// ---------------------------------------------------------------------------

fn utf16_col_to_char_col(line: &str, utf16_col: u32) -> usize {
    let mut utf16_offset = 0u32;
    for (i, ch) in line.chars().enumerate() {
        if utf16_offset >= utf16_col {
            return i;
        }
        utf16_offset += ch.len_utf16() as u32;
    }
    line.chars().count()
}

fn char_col_to_utf16_col(line: &str, char_col: usize) -> u32 {
    let mut utf16_offset = 0u32;
    for (i, ch) in line.chars().enumerate() {
        if i >= char_col {
            break;
        }
        utf16_offset += ch.len_utf16() as u32;
    }
    utf16_offset
}

fn lsp_pos(row: usize, col: usize, lines: &[String]) -> Position {
    let utf16_col = if row < lines.len() {
        char_col_to_utf16_col(&lines[row], col)
    } else {
        col as u32
    };
    Position::new(row as u32, utf16_col)
}

fn from_lsp_pos(pos: &Position, lines: &[String]) -> EditorPosition {
    let row = pos.line as usize;
    let col = if row < lines.len() {
        utf16_col_to_char_col(&lines[row], pos.character)
    } else {
        pos.character as usize
    };
    EditorPosition { row, col }
}

fn read_file_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().map(|l| l.to_string()).collect(),
        Err(_) => vec![],
    }
}

// ---------------------------------------------------------------------------
// Server registry
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ServerConfig {
    language_id: &'static str,
    command: &'static str,
    args: &'static [&'static str],
    extensions: &'static [&'static str],
}

struct LspRegistry {
    configs: Vec<ServerConfig>,
}

impl LspRegistry {
    fn new() -> Self {
        Self {
            configs: vec![
                ServerConfig {
                    language_id: "rust",
                    command: "rust-analyzer",
                    args: &[],
                    extensions: &["rs"],
                },
                ServerConfig {
                    language_id: "typescript",
                    command: "typescript-language-server",
                    args: &["--stdio"],
                    extensions: &["ts", "tsx", "js", "jsx"],
                },
                ServerConfig {
                    language_id: "python",
                    command: "pyright-langserver",
                    args: &["--stdio"],
                    extensions: &["py"],
                },
                ServerConfig {
                    language_id: "c",
                    command: "clangd",
                    args: &[],
                    extensions: &["c", "h", "cpp", "hpp", "cc", "cxx"],
                },
                ServerConfig {
                    language_id: "swift",
                    command: "sourcekit-lsp",
                    args: &[],
                    extensions: &["swift"],
                },
            ],
        }
    }

    fn config_for_extension(&self, ext: &str) -> Option<&ServerConfig> {
        self.configs
            .iter()
            .find(|c| c.extensions.contains(&ext))
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC transport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RequestId {
    Int(i32),
    Str(String),
}

impl RequestId {
    fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(|n| RequestId::Int(n as i32)),
            Value::String(s) => Some(RequestId::Str(s.clone())),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct LspError {
    message: String,
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LSP error: {}", self.message)
    }
}

struct LspNotification {
    method: String,
    params: Value,
}

type ResponseHandlers =
    Arc<Mutex<HashMap<RequestId, tokio::sync::oneshot::Sender<Result<Value, LspError>>>>>;

fn spawn_writer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    mut stdin: tokio::process::ChildStdin,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(body) = rx.recv().await {
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            if stdin.write_all(header.as_bytes()).await.is_err() {
                break;
            }
            if stdin.write_all(body.as_bytes()).await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    });
}

fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    response_handlers: ResponseHandlers,
    notification_tx: tokio::sync::mpsc::UnboundedSender<LspNotification>,
    writer_tx: tokio::sync::mpsc::UnboundedSender<String>,
    waker: Option<Waker>,
) {
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
        let mut reader = BufReader::new(stdout);
        let mut header_buf = String::new();

        loop {
            // Read headers
            log::debug!("LSP reader: waiting for next message...");
            let mut content_length: Option<usize> = None;
            loop {
                header_buf.clear();
                match reader.read_line(&mut header_buf).await {
                    Ok(0) => {
                        log::info!("LSP reader: EOF");
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::info!("LSP reader: read error: {}", e);
                        return;
                    }
                }
                let line = header_buf.trim();
                if line.is_empty() {
                    break;
                }
                if let Some(val) = line.strip_prefix("Content-Length: ") {
                    content_length = val.parse().ok();
                }
            }

            let Some(len) = content_length else {
                continue;
            };

            // Read body
            let mut body = vec![0u8; len];
            if let Err(e) = reader.read_exact(&mut body).await {
                log::info!("LSP reader: body read error: {}", e);
                return;
            }

            let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                log::info!("LSP reader: JSON parse error");
                continue;
            };

            // Log raw message shape for debugging
            let msg_method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("-");
            let msg_id = msg.get("id").map(|v| v.to_string()).unwrap_or("-".into());
            log::debug!("LSP raw: id={} method={} len={}", msg_id, msg_method, len);

            // id: null counts as absent (some servers include it in notifications)
            let has_id = msg.get("id").is_some_and(|v| !v.is_null());
            let has_method = msg.get("method").is_some();

            if has_id && has_method {
                // Server request — auto-reply
                let method = msg["method"].as_str().unwrap_or("");
                log::info!("LSP <- server request: {}", method);
                // Forward registerCapability as a notification for LspManager to handle
                if method == "client/registerCapability" {
                    let params = msg.get("params").cloned().unwrap_or(Value::Null);
                    let _ = notification_tx.send(LspNotification {
                        method: method.to_string(),
                        params,
                    });
                    if let Some(ref w) = waker {
                        w();
                    }
                }

                if let Some(id) = msg.get("id") {
                    // workspace/configuration expects an array of config objects
                    let result = if method == "workspace/configuration" {
                        let items = msg
                            .get("params")
                            .and_then(|p| p.get("items"))
                            .and_then(|i| i.as_array())
                            .map(|arr| arr.len())
                            .unwrap_or(1);
                        Value::Array(vec![Value::Object(serde_json::Map::new()); items])
                    } else {
                        Value::Null
                    };
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    });
                    log::debug!("LSP -> auto-reply: id={} result={}", id, result);
                    let _ = writer_tx.send(reply.to_string());
                }
            } else if has_id && !has_method {
                // Response to our request
                if let Some(id) = msg.get("id").and_then(RequestId::from_value) {
                    let mut handlers = response_handlers.lock().unwrap();
                    if let Some(sender) = handlers.remove(&id) {
                        if let Some(error) = msg.get("error") {
                            let message = error
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error")
                                .to_string();
                            log::info!("LSP <- response error id={:?}: {}", id, message);
                            let _ = sender.send(Err(LspError { message }));
                        } else {
                            log::info!("LSP <- response id={:?}", id);
                            let result = msg.get("result").cloned().unwrap_or(Value::Null);
                            let _ = sender.send(Ok(result));
                        }
                    }
                }
            } else if has_method {
                // Server notification
                let method = msg["method"].as_str().unwrap_or("").to_string();
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let _ = notification_tx.send(LspNotification { method, params });
                if let Some(ref w) = waker {
                    w();
                }
            } else {
                log::info!("LSP reader: unclassified message: {}", msg);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// LanguageServer
// ---------------------------------------------------------------------------

struct LanguageServer {
    name: String,
    next_id: AtomicI32,
    outbound_tx: tokio::sync::mpsc::UnboundedSender<String>,
    response_handlers: ResponseHandlers,
    capabilities: Mutex<Option<ServerCapabilities>>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl LanguageServer {
    async fn start(
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
                        code_action_literal_support: Some(
                            lsp_types::CodeActionLiteralSupport {
                                code_action_kind: lsp_types::CodeActionKindLiteralSupport {
                                    value_set: vec![
                                        lsp_types::CodeActionKind::QUICKFIX.as_str().to_string(),
                                        lsp_types::CodeActionKind::REFACTOR.as_str().to_string(),
                                        lsp_types::CodeActionKind::SOURCE.as_str().to_string(),
                                    ],
                                },
                            },
                        ),
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

    async fn request<P: serde::Serialize, R: serde::de::DeserializeOwned>(
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

        self.outbound_tx.send(msg.to_string()).map_err(|_| LspError {
            message: "Server connection closed".to_string(),
        })?;

        let result = rx.await.map_err(|_| LspError {
            message: "Response channel closed".to_string(),
        })??;

        serde_json::from_value(result).map_err(|e| LspError {
            message: format!("Failed to deserialize response: {}", e),
        })
    }

    fn notify<P: serde::Serialize>(&self, method: &str, params: &P) {
        log::info!("LSP -> notify: {}", method);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": serde_json::to_value(params).unwrap_or(Value::Null),
        });
        let _ = self.outbound_tx.send(msg.to_string());
    }

    async fn shutdown(&self) {
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
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    child.wait(),
                )
                .await;
                let _ = child.kill().await;
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Internal event types
// ---------------------------------------------------------------------------

enum LspManagerEvent {
    ServerStarted {
        language_id: String,
        server: Arc<LanguageServer>,
    },
    ServerError {
        error: String,
    },
    Notification(LspNotification),
    RequestResult(RequestResult),
    FileChanged(PathBuf),
}

enum RequestResult {
    GotoDefinition {
        locations: Vec<(PathBuf, usize, usize)>,
        origin_path: PathBuf,
        origin_row: usize,
        origin_col: usize,
    },
    Format {
        path: PathBuf,
        edits: Vec<EditorTextEdit>,
    },
    Rename {
        primary_path: PathBuf,
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    CodeActions {
        path: PathBuf,
        actions: Vec<EditorCodeAction>,
        raw: Vec<CodeActionOrCommand>,
    },
    CodeActionResolved {
        primary_path: PathBuf,
        file_edits: Vec<(PathBuf, Vec<EditorTextEdit>)>,
    },
    InlayHints {
        path: PathBuf,
        hints: Vec<EditorInlayHint>,
    },
    Error {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Progress tracking
// ---------------------------------------------------------------------------

struct ProgressState {
    title: String,
    message: Option<String>,
    #[allow(dead_code)]
    percentage: Option<u32>,
}

// ---------------------------------------------------------------------------
// Type conversion
// ---------------------------------------------------------------------------

fn lsp_edit_to_editor(te: &TextEdit, lines: &[String]) -> EditorTextEdit {
    let start = from_lsp_pos(&te.range.start, lines);
    let end = from_lsp_pos(&te.range.end, lines);
    EditorTextEdit {
        range: EditorRange { start, end },
        new_text: te.new_text.clone(),
    }
}

fn workspace_edit_to_file_edits(edit: &WorkspaceEdit) -> Vec<(PathBuf, Vec<EditorTextEdit>)> {
    let mut result: HashMap<PathBuf, Vec<EditorTextEdit>> = HashMap::new();

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            if let Some(path) = path_from_uri(uri) {
                let lines = read_file_lines(&path);
                let editor_edits: Vec<EditorTextEdit> =
                    edits.iter().map(|e| lsp_edit_to_editor(e, &lines)).collect();
                result.entry(path).or_default().extend(editor_edits);
            }
        }
    }

    if let Some(document_changes) = &edit.document_changes {
        use lsp_types::DocumentChanges;
        match document_changes {
            DocumentChanges::Edits(edits) => {
                for tde in edits {
                    if let Some(path) = path_from_uri(&tde.text_document.uri) {
                        let lines = read_file_lines(&path);
                        let editor_edits: Vec<EditorTextEdit> = tde
                            .edits
                            .iter()
                            .filter_map(|e| match e {
                                lsp_types::OneOf::Left(te) => {
                                    Some(lsp_edit_to_editor(te, &lines))
                                }
                                lsp_types::OneOf::Right(ate) => {
                                    Some(lsp_edit_to_editor(&ate.text_edit, &lines))
                                }
                            })
                            .collect();
                        result.entry(path).or_default().extend(editor_edits);
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(tde) = op {
                        if let Some(path) = path_from_uri(&tde.text_document.uri) {
                            let lines = read_file_lines(&path);
                            let editor_edits: Vec<EditorTextEdit> = tde
                                .edits
                                .iter()
                                .filter_map(|e| match e {
                                    lsp_types::OneOf::Left(te) => {
                                        Some(lsp_edit_to_editor(te, &lines))
                                    }
                                    lsp_types::OneOf::Right(ate) => {
                                        Some(lsp_edit_to_editor(&ate.text_edit, &lines))
                                    }
                                })
                                .collect();
                            result.entry(path).or_default().extend(editor_edits);
                        }
                    }
                }
            }
        }
    }

    result.into_iter().collect()
}

fn apply_edits_to_disk(path: &Path, edits: &[EditorTextEdit]) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }

    // Sort edits in reverse order so we apply from bottom to top
    let mut sorted_edits: Vec<&EditorTextEdit> = edits.iter().collect();
    sorted_edits.sort_by(|a, b| {
        let row_cmp = b.range.start.row.cmp(&a.range.start.row);
        if row_cmp == std::cmp::Ordering::Equal {
            b.range.start.col.cmp(&a.range.start.col)
        } else {
            row_cmp
        }
    });

    for edit in sorted_edits {
        let start_row = edit.range.start.row.min(lines.len());
        let start_col = edit.range.start.col;
        let end_row = edit.range.end.row.min(lines.len());
        let end_col = edit.range.end.col;

        // Build the new content
        let prefix = if start_row < lines.len() {
            lines[start_row]
                .chars()
                .take(start_col)
                .collect::<String>()
        } else {
            String::new()
        };
        let suffix = if end_row < lines.len() {
            lines[end_row].chars().skip(end_col).collect::<String>()
        } else {
            String::new()
        };

        let new_text = format!("{}{}{}", prefix, edit.new_text, suffix);
        let new_lines: Vec<String> = new_text.lines().map(|l| l.to_string()).collect();

        // Replace the range
        let remove_end = (end_row + 1).min(lines.len());
        lines.splice(start_row..remove_end, new_lines);
    }

    let result = lines.join("\n");
    // Preserve trailing newline if original had one
    let output = if content.ends_with('\n') && !result.ends_with('\n') {
        format!("{}\n", result)
    } else {
        result
    };
    let _ = std::fs::write(path, output);
}

fn language_id_for_extension(ext: &str) -> &'static str {
    match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "cxx" => "cpp",
        "swift" => "swift",
        _ => "plaintext",
    }
}

// ---------------------------------------------------------------------------
// LspManager
// ---------------------------------------------------------------------------

pub struct LspManager {
    registry: LspRegistry,
    servers: HashMap<String, Arc<LanguageServer>>,
    root: PathBuf,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<LspManagerEvent>,
    event_tx: tokio::sync::mpsc::UnboundedSender<LspManagerEvent>,
    waker: Option<Waker>,
    pending_starts: HashSet<String>,
    opened_docs: HashSet<PathBuf>,
    /// Paths that got TabActivated before the server was ready
    pending_opens: HashSet<PathBuf>,
    pending_code_actions: HashMap<PathBuf, Vec<CodeActionOrCommand>>,
    progress_tokens: HashMap<String, ProgressState>,
    quiescent: bool,
    _file_watcher: Option<notify::RecommendedWatcher>,
    file_watcher_globs: Option<globset::GlobSet>,
}

impl LspManager {
    pub fn new(root: PathBuf, waker: Option<Waker>) -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            registry: LspRegistry::new(),
            servers: HashMap::new(),
            root,
            event_rx,
            event_tx,
            waker,
            pending_starts: HashSet::new(),
            opened_docs: HashSet::new(),
            pending_opens: HashSet::new(),
            pending_code_actions: HashMap::new(),
            progress_tokens: HashMap::new(),
            quiescent: true,
            _file_watcher: None,
            file_watcher_globs: None,
        }
    }

    fn is_busy(&self) -> bool {
        !self.quiescent || !self.progress_tokens.is_empty()
    }

    fn lsp_status_effect(&self) -> Effect {
        let server_name = self
            .servers
            .values()
            .next()
            .map(|s| s.name.clone())
            .unwrap_or_default();

        let detail = if !self.progress_tokens.is_empty() {
            self.progress_tokens.values().next().map(|p| {
                if let Some(ref msg) = p.message {
                    format!("{} {}", p.title, msg)
                } else {
                    p.title.clone()
                }
            })
        } else {
            None
        };

        Effect::SetLspStatus(LspStatus {
            server_name,
            busy: self.is_busy(),
            detail,
        })
    }

    // -- File watching -------------------------------------------------------

    fn handle_register_capability(&mut self, params: &Value) {
        let Some(registrations) = params.get("registrations").and_then(|r| r.as_array()) else {
            return;
        };

        for reg in registrations {
            let method = reg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if method != "workspace/didChangeWatchedFiles" {
                continue;
            }

            let Some(watchers) = reg
                .get("registerOptions")
                .and_then(|o| o.get("watchers"))
                .and_then(|w| w.as_array())
            else {
                continue;
            };

            let mut builder = globset::GlobSetBuilder::new();
            for w in watchers {
                let Some(pattern) = w.get("globPattern").and_then(|g| g.as_str()) else {
                    continue;
                };
                if let Ok(glob) = globset::Glob::new(pattern) {
                    builder.add(glob);
                }
            }

            let Ok(glob_set) = builder.build() else {
                continue;
            };

            log::info!(
                "LSP file watcher: {} patterns registered",
                glob_set.len()
            );

            self.file_watcher_globs = Some(glob_set);
            self.start_file_watcher();
        }
    }

    fn start_file_watcher(&mut self) {
        use notify::{RecursiveMode, Watcher};

        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();
        let globs = match self.file_watcher_globs.as_ref() {
            Some(g) => g.clone(),
            None => return,
        };
        let root = self.root.clone();

        let watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            let Ok(ev) = res else { return };
            match ev.kind {
                notify::EventKind::Create(_)
                | notify::EventKind::Modify(_)
                | notify::EventKind::Remove(_) => {}
                _ => return,
            }
            for path in ev.paths {
                if globs.is_match(&path) {
                    let _ = event_tx.send(LspManagerEvent::FileChanged(path));
                    if let Some(ref w) = waker {
                        w();
                    }
                }
            }
        });

        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(&root, RecursiveMode::Recursive) {
                    log::info!("LSP file watcher: failed to watch {}: {}", root.display(), e);
                    return;
                }
                log::info!("LSP file watcher: watching {}", root.display());
                self._file_watcher = Some(w);
            }
            Err(e) => {
                log::info!("LSP file watcher: failed to create: {}", e);
            }
        }
    }

    fn send_file_changed(&self, path: &Path) {
        // Send to all servers — the server will filter by relevance
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let change_type = if path.exists() {
            lsp_types::FileChangeType::CHANGED
        } else {
            lsp_types::FileChangeType::DELETED
        };

        for server in self.servers.values() {
            server.notify(
                "workspace/didChangeWatchedFiles",
                &lsp_types::DidChangeWatchedFilesParams {
                    changes: vec![lsp_types::FileEvent {
                        uri: uri.clone(),
                        typ: change_type,
                    }],
                },
            );
        }
    }

    // -- Server lifecycle ---------------------------------------------------

    fn ensure_server_for_path(&mut self, path: &Path) {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        let Some(config) = self.registry.config_for_extension(ext).cloned() else {
            return;
        };

        let lang_id = config.language_id.to_string();
        if self.servers.contains_key(&lang_id) || self.pending_starts.contains(&lang_id) {
            return;
        }

        log::info!("LSP starting server for language: {}", lang_id);
        self.pending_starts.insert(lang_id.clone());

        let root = self.root.clone();
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        // Create a notification channel that feeds into our event channel
        let (notif_tx, mut notif_rx) = tokio::sync::mpsc::unbounded_channel::<LspNotification>();
        let event_tx2 = event_tx.clone();
        let waker2 = waker.clone();
        tokio::spawn(async move {
            while let Some(notif) = notif_rx.recv().await {
                let _ = event_tx2.send(LspManagerEvent::Notification(notif));
                if let Some(ref w) = waker2 {
                    w();
                }
            }
        });

        tokio::spawn(async move {
            match LanguageServer::start(&config, &root, notif_tx, waker.clone()).await {
                Ok(server) => {
                    let _ = event_tx.send(LspManagerEvent::ServerStarted {
                        language_id: lang_id,
                        server,
                    });
                    if let Some(ref w) = waker {
                        w();
                    }
                }
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::ServerError {
                        error: e.message,
                    });
                    if let Some(ref w) = waker {
                        w();
                    }
                }
            }
        });
    }

    fn server_for_path(&self, path: &Path) -> Option<Arc<LanguageServer>> {
        let ext = path.extension().and_then(|e| e.to_str())?;
        let config = self.registry.config_for_extension(ext)?;
        self.servers.get(config.language_id).cloned()
    }

    // -- Document sync ------------------------------------------------------

    fn send_did_open(&mut self, path: &Path) {
        if self.opened_docs.contains(path) {
            return;
        }
        let Some(server) = self.server_for_path(path) else {
            // Server not ready yet — remember so we can open when it starts
            log::info!("LSP didOpen deferred (server not ready): {}", path.display());
            self.pending_opens.insert(path.to_path_buf());
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let lang_id = language_id_for_extension(ext);
        let text = std::fs::read_to_string(path).unwrap_or_default();

        server.notify(
            "textDocument/didOpen",
            &lsp_types::DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: lang_id.to_string(),
                    version: 0,
                    text,
                },
            },
        );
        self.opened_docs.insert(path.to_path_buf());
    }

    fn send_did_save(&self, path: &Path) {
        let Some(server) = self.server_for_path(path) else {
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        let text = std::fs::read_to_string(path).unwrap_or_default();
        server.notify(
            "textDocument/didSave",
            &lsp_types::DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
                text: Some(text),
            },
        );
    }

    fn send_did_close(&mut self, path: &Path) {
        if !self.opened_docs.remove(path) {
            return;
        }
        let Some(server) = self.server_for_path(path) else {
            return;
        };
        let Some(uri) = uri_from_path(path) else {
            return;
        };
        server.notify(
            "textDocument/didClose",
            &lsp_types::DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri },
            },
        );
    }

    // -- Feature methods (spawn tokio tasks) --------------------------------

    fn spawn_goto_definition(&self, path: PathBuf, row: usize, col: usize) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: lsp_pos(row, col, &lines),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let result: Result<Option<GotoDefinitionResponse>, _> =
                server.request("textDocument/definition", &params).await;

            let locations = match result {
                Ok(Some(resp)) => definition_response_to_locations(resp),
                _ => vec![],
            };

            let _ = event_tx.send(LspManagerEvent::RequestResult(
                RequestResult::GotoDefinition {
                    locations,
                    origin_path: path,
                    origin_row: row,
                    origin_col: col,
                },
            ));
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    fn spawn_inlay_hints(&self, path: PathBuf, start_row: usize, end_row: usize) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = InlayHintParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: lsp_pos(start_row, 0, &lines),
                    end: lsp_pos(end_row, 0, &lines),
                },
                work_done_progress_params: Default::default(),
            };

            let result: Result<Option<Vec<InlayHint>>, _> =
                server.request("textDocument/inlayHint", &params).await;

            let hints = match result {
                Ok(Some(hints)) => hints
                    .iter()
                    .map(|h| {
                        let pos = from_lsp_pos(&h.position, &lines);
                        let label = match &h.label {
                            InlayHintLabel::String(s) => s.clone(),
                            InlayHintLabel::LabelParts(parts) => {
                                parts.iter().map(|p| p.value.as_str()).collect()
                            }
                        };
                        EditorInlayHint {
                            position: pos,
                            label,
                        }
                    })
                    .collect(),
                _ => vec![],
            };

            let _ = event_tx.send(LspManagerEvent::RequestResult(
                RequestResult::InlayHints { path, hints },
            ));
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    fn spawn_rename(&self, path: PathBuf, row: usize, col: usize, new_name: String) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = RenameParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: lsp_pos(row, col, &lines),
                },
                new_name,
                work_done_progress_params: Default::default(),
            };

            let result: Result<Option<WorkspaceEdit>, _> =
                server.request("textDocument/rename", &params).await;

            match result {
                Ok(Some(edit)) => {
                    let file_edits = workspace_edit_to_file_edits(&edit);
                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::Rename {
                            primary_path: path,
                            file_edits,
                        },
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::Error {
                            message: e.message,
                        },
                    ));
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    fn spawn_code_action(
        &self,
        path: PathBuf,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range: Range {
                    start: lsp_pos(start_row, start_col, &lines),
                    end: lsp_pos(end_row, end_col, &lines),
                },
                context: lsp_types::CodeActionContext {
                    diagnostics: vec![],
                    only: None,
                    trigger_kind: Some(lsp_types::CodeActionTriggerKind::INVOKED),
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let result: Result<Option<CodeActionResponse>, _> =
                server.request("textDocument/codeAction", &params).await;

            match result {
                Ok(Some(actions)) => {
                    let editor_actions: Vec<EditorCodeAction> = actions
                        .iter()
                        .enumerate()
                        .map(|(i, a)| {
                            let title = match a {
                                CodeActionOrCommand::CodeAction(ca) => ca.title.clone(),
                                CodeActionOrCommand::Command(cmd) => cmd.title.clone(),
                            };
                            EditorCodeAction { title, index: i }
                        })
                        .collect();

                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::CodeActions {
                            path,
                            actions: editor_actions,
                            raw: actions,
                        },
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::Error {
                            message: e.message,
                        },
                    ));
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    fn spawn_code_action_resolve(&self, path: PathBuf, index: usize) {
        let raw_actions = match self.pending_code_actions.get(&path) {
            Some(actions) => actions.clone(),
            None => return,
        };

        let Some(action) = raw_actions.get(index).cloned() else {
            return;
        };

        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            match action {
                CodeActionOrCommand::CodeAction(mut ca) => {
                    // If the action already has an edit, use it directly
                    if ca.edit.is_none() {
                        // Try to resolve
                        match server
                            .request::<_, lsp_types::CodeAction>("codeAction/resolve", &ca)
                            .await
                        {
                            Ok(resolved) => ca = resolved,
                            Err(e) => {
                                let _ = event_tx.send(LspManagerEvent::RequestResult(
                                    RequestResult::Error {
                                        message: e.message,
                                    },
                                ));
                                if let Some(ref w) = waker {
                                    w();
                                }
                                return;
                            }
                        }
                    }

                    if let Some(edit) = ca.edit {
                        let file_edits = workspace_edit_to_file_edits(&edit);
                        let _ = event_tx.send(LspManagerEvent::RequestResult(
                            RequestResult::CodeActionResolved {
                                primary_path: path,
                                file_edits,
                            },
                        ));
                    }
                }
                CodeActionOrCommand::Command(_cmd) => {
                    // Command execution not supported yet
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    fn spawn_format(&self, path: PathBuf) {
        let Some(server) = self.server_for_path(&path) else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let waker = self.waker.clone();

        tokio::spawn(async move {
            let lines = read_file_lines(&path);
            let uri = match uri_from_path(&path) {
                Some(u) => u,
                None => return,
            };
            let params = DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                options: FormattingOptions {
                    tab_size: 4,
                    insert_spaces: true,
                    ..Default::default()
                },
                work_done_progress_params: Default::default(),
            };

            let result: Result<Option<Vec<TextEdit>>, _> =
                server.request("textDocument/formatting", &params).await;

            match result {
                Ok(Some(edits)) => {
                    let editor_edits: Vec<EditorTextEdit> =
                        edits.iter().map(|e| lsp_edit_to_editor(e, &lines)).collect();
                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::Format {
                            path,
                            edits: editor_edits,
                        },
                    ));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = event_tx.send(LspManagerEvent::RequestResult(
                        RequestResult::Error {
                            message: e.message,
                        },
                    ));
                }
            }
            if let Some(ref w) = waker {
                w();
            }
        });
    }

    // -- Notification handling ----------------------------------------------

    fn handle_notification(&mut self, notif: LspNotification) -> Vec<Effect> {
        let mut effects = Vec::new();

        match notif.method.as_str() {
            "textDocument/publishDiagnostics" => {
                if let Ok(params) =
                    serde_json::from_value::<PublishDiagnosticsParams>(notif.params)
                {
                    if let Some(path) = path_from_uri(&params.uri) {
                        log::info!(
                            "LSP diagnostics: {} count={}",
                            path.display(),
                            params.diagnostics.len()
                        );
                        let lines = read_file_lines(&path);
                        let diagnostics: Vec<EditorDiagnostic> = params
                            .diagnostics
                            .iter()
                            .map(|d| {
                                let severity = match d.severity {
                                    Some(lsp_types::DiagnosticSeverity::ERROR) => {
                                        EditorSeverity::Error
                                    }
                                    Some(lsp_types::DiagnosticSeverity::WARNING) => {
                                        EditorSeverity::Warning
                                    }
                                    Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                                        EditorSeverity::Info
                                    }
                                    Some(lsp_types::DiagnosticSeverity::HINT) => {
                                        EditorSeverity::Hint
                                    }
                                    _ => EditorSeverity::Error,
                                };
                                let start = from_lsp_pos(&d.range.start, &lines);
                                let end = from_lsp_pos(&d.range.end, &lines);
                                EditorDiagnostic {
                                    range: EditorRange { start, end },
                                    severity,
                                    message: d.message.clone(),
                                }
                            })
                            .collect();
                        effects.push(Effect::Emit(Event::SetDiagnostics {
                            path,
                            diagnostics,
                        }));
                    }
                }
            }
            "$/progress" => {
                match serde_json::from_value::<lsp_types::ProgressParams>(notif.params) {
                    Ok(params) => {
                        let token = match &params.token {
                            NumberOrString::Number(n) => n.to_string(),
                            NumberOrString::String(s) => s.clone(),
                        };

                        match params.value {
                            lsp_types::ProgressParamsValue::WorkDone(wd) => match wd {
                                lsp_types::WorkDoneProgress::Begin(ref begin) => {
                                    log::info!(
                                        "LSP progress begin: token={} title={:?} message={:?}",
                                        token, begin.title, begin.message
                                    );
                                    self.progress_tokens.insert(
                                        token,
                                        ProgressState {
                                            title: begin.title.clone(),
                                            message: begin.message.clone(),
                                            percentage: begin.percentage,
                                        },
                                    );
                                    effects.push(self.lsp_status_effect());
                                }
                                lsp_types::WorkDoneProgress::Report(ref report) => {
                                    log::info!(
                                        "LSP progress report: token={} message={:?} pct={:?}",
                                        token, report.message, report.percentage
                                    );
                                    // Treat 100% as implicit End — rust-analyzer
                                    // delays the real End notification.
                                    if report.percentage == Some(100) {
                                        log::info!("LSP progress 100%, auto-ending: token={}", token);
                                        self.progress_tokens.remove(&token);
                                    } else if let Some(state) = self.progress_tokens.get_mut(&token) {
                                        if report.message.is_some() {
                                            state.message = report.message.clone();
                                        }
                                        if report.percentage.is_some() {
                                            state.percentage = report.percentage;
                                        }
                                    }
                                    effects.push(self.lsp_status_effect());
                                }
                                lsp_types::WorkDoneProgress::End(_) => {
                                    log::info!(
                                        "LSP progress end: token={} (remaining={})",
                                        token,
                                        self.progress_tokens.len().saturating_sub(1)
                                    );
                                    self.progress_tokens.remove(&token);
                                    effects.push(self.lsp_status_effect());
                                }
                            },
                        }
                    }
                    Err(e) => {
                        log::info!("LSP $/progress deserialize error: {}", e);
                    }
                }
            }
            "experimental/serverStatus" => {
                let quiescent = notif.params.get("quiescent").and_then(|v| v.as_bool());
                let message = notif.params.get("message").and_then(|v| v.as_str());
                log::info!(
                    "LSP serverStatus: quiescent={:?} message={:?}",
                    quiescent, message
                );
                if let Some(q) = quiescent {
                    self.quiescent = q;
                    effects.push(self.lsp_status_effect());
                }
            }
            "client/registerCapability" => {
                self.handle_register_capability(&notif.params);
            }
            _ => {
                log::info!("LSP unhandled notification: {}", notif.method);
            }
        }

        effects
    }

    fn handle_request_result(&mut self, result: RequestResult) -> Vec<Effect> {
        let mut effects = Vec::new();

        match result {
            RequestResult::GotoDefinition {
                locations,
                origin_path,
                origin_row,
                origin_col,
            } => {
                if let Some((path, row, col)) = locations.into_iter().next() {
                    // Record jump at origin before navigating
                    effects.push(Effect::Emit(Event::RecordJump {
                        path: origin_path,
                        row: origin_row,
                        col: origin_col,
                        scroll_offset: 0,
                    }));
                    effects.push(Effect::Emit(Event::OpenDefinition(path.clone())));
                    effects.push(Effect::Emit(Event::GoToPosition {
                        path,
                        row,
                        col,
                        scroll_offset: None,
                    }));
                }
            }
            RequestResult::Format { path, edits } => {
                if !edits.is_empty() {
                    effects.push(Effect::Emit(Event::ApplyEdits { path, edits }));
                }
            }
            RequestResult::Rename {
                primary_path,
                file_edits,
            } => {
                for (path, edits) in &file_edits {
                    if *path == primary_path {
                        effects.push(Effect::Emit(Event::ApplyEdits {
                            path: path.clone(),
                            edits: edits.clone(),
                        }));
                    } else {
                        // Apply to disk for non-open files
                        apply_edits_to_disk(path, edits);
                    }
                }
            }
            RequestResult::CodeActions { path, actions, raw } => {
                if !actions.is_empty() {
                    self.pending_code_actions.insert(path.clone(), raw);
                    effects.push(Effect::Emit(Event::ShowCodeActions { path, actions }));
                }
            }
            RequestResult::CodeActionResolved {
                primary_path,
                file_edits,
            } => {
                for (path, edits) in &file_edits {
                    if *path == primary_path {
                        effects.push(Effect::Emit(Event::ApplyEdits {
                            path: path.clone(),
                            edits: edits.clone(),
                        }));
                    } else {
                        apply_edits_to_disk(path, edits);
                    }
                }
            }
            RequestResult::InlayHints { path, hints } => {
                effects.push(Effect::Emit(Event::SetInlayHints { path, hints }));
            }
            RequestResult::Error { message } => {
                effects.push(Effect::SetMessage(format!("LSP: {}", message)));
            }
        }

        effects
    }
}

fn definition_response_to_locations(resp: GotoDefinitionResponse) -> Vec<(PathBuf, usize, usize)> {
    match resp {
        GotoDefinitionResponse::Scalar(loc) => {
            location_to_tuple(&loc).into_iter().collect()
        }
        GotoDefinitionResponse::Array(locs) => {
            locs.iter().filter_map(location_to_tuple).collect()
        }
        GotoDefinitionResponse::Link(links) => links
            .iter()
            .filter_map(|link| {
                let path = path_from_uri(&link.target_uri)?;
                let lines = read_file_lines(&path);
                let pos = from_lsp_pos(&link.target_selection_range.start, &lines);
                Some((path, pos.row, pos.col))
            })
            .collect(),
    }
}

fn location_to_tuple(loc: &Location) -> Option<(PathBuf, usize, usize)> {
    let path = path_from_uri(&loc.uri)?;
    let lines = read_file_lines(&path);
    let pos = from_lsp_pos(&loc.range.start, &lines);
    Some((path, pos.row, pos.col))
}

// ---------------------------------------------------------------------------
// Component impl
// ---------------------------------------------------------------------------

impl Component for LspManager {
    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, action: Action, _ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::Tick => {
                let mut effects = Vec::new();
                while let Ok(event) = self.event_rx.try_recv() {
                    match event {
                        LspManagerEvent::ServerStarted {
                            language_id,
                            server,
                        } => {
                            log::info!("LSP server started: {} ({})", server.name, language_id);
                            self.pending_starts.remove(&language_id);
                            self.servers.insert(language_id, server.clone());
                            // Assume busy until server reports quiescent
                            self.quiescent = false;
                            effects.push(Effect::SetLspStatus(LspStatus {
                                server_name: server.name.clone(),
                                busy: true,
                                detail: None,
                            }));
                            // Send didOpen for any docs that were waiting for this server
                            let pending: Vec<PathBuf> =
                                self.pending_opens.iter().cloned().collect();
                            for path in pending {
                                if self.server_for_path(&path).is_some() {
                                    self.pending_opens.remove(&path);
                                    self.send_did_open(&path);
                                }
                            }
                        }
                        LspManagerEvent::ServerError { error } => {
                            log::error!("LSP server error: {}", error);
                            effects.push(Effect::SetMessage(format!("LSP: {}", error)));
                        }
                        LspManagerEvent::Notification(notif) => {
                            effects.extend(self.handle_notification(notif));
                        }
                        LspManagerEvent::RequestResult(result) => {
                            effects.extend(self.handle_request_result(result));
                        }
                        LspManagerEvent::FileChanged(path) => {
                            self.send_file_changed(&path);
                        }
                    }
                }
                effects
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::TabActivated { path: Some(path) } => {
                self.ensure_server_for_path(path);
                self.send_did_open(path);
            }
            Event::FileSaved(path) => {
                self.send_did_save(path);
            }
            Event::BufferClosed(path) => {
                self.send_did_close(path);
            }
            Event::LspGotoDefinition { path, row, col } => {
                self.spawn_goto_definition(path.clone(), *row, *col);
            }
            Event::LspInlayHints {
                path,
                start_row,
                end_row,
            } => {
                self.spawn_inlay_hints(path.clone(), *start_row, *end_row);
            }
            Event::LspRename {
                path,
                row,
                col,
                new_name,
            } => {
                self.spawn_rename(path.clone(), *row, *col, new_name.clone());
            }
            Event::LspCodeAction {
                path,
                start_row,
                start_col,
                end_row,
                end_col,
            } => {
                self.spawn_code_action(
                    path.clone(),
                    *start_row,
                    *start_col,
                    *end_row,
                    *end_col,
                );
            }
            Event::LspCodeActionResolve { path, index } => {
                self.spawn_code_action_resolve(path.clone(), *index);
            }
            Event::LspFormat { path } => {
                self.spawn_format(path.clone());
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, _f: &mut Frame, _a: Rect, _ctx: &mut DrawContext) {}
}

// ---------------------------------------------------------------------------
// Drop — shutdown all servers
// ---------------------------------------------------------------------------

impl Drop for LspManager {
    fn drop(&mut self) {
        let servers: Vec<Arc<LanguageServer>> = self.servers.values().cloned().collect();
        if !servers.is_empty() {
            tokio::spawn(async move {
                for server in servers {
                    server.shutdown().await;
                }
            });
        }
    }
}
