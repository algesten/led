/// A fake LSP server for integration tests.
///
/// Reads `.fake-lsp.json` from the workspace root (extracted from the
/// `initialize` request's `rootUri`) and uses it to produce deterministic
/// responses. This lets integration tests exercise the full LSP pipeline
/// without requiring a real language server.
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

// ── Config ──

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// Diagnostics to publish on `textDocument/didOpen`.
    /// Key is the relative path (e.g. "src/main.rs"), value is an array of
    /// LSP Diagnostic objects (raw JSON, forwarded verbatim).
    #[serde(default)]
    diagnostics: HashMap<String, Vec<Value>>,

    /// Completion items returned for every `textDocument/completion` request.
    /// Each entry is a raw LSP CompletionItem object.
    #[serde(default)]
    completions: Vec<Value>,

    /// Code actions returned for every `textDocument/codeAction` request.
    /// Each entry is a raw LSP CodeAction object.
    #[serde(default)]
    code_actions: Vec<Value>,

    /// Formatted file contents, keyed by relative path.
    /// On `textDocument/formatting`, the server returns a single edit that
    /// replaces the entire document with this text.
    #[serde(default)]
    formatting: HashMap<String, String>,

    /// Completion trigger characters reported in server capabilities.
    #[serde(default)]
    trigger_characters: Vec<String>,
}

// ── Server state ──

struct FakeLsp {
    root_path: PathBuf,
    config: Config,
    /// URI → full text content.
    documents: HashMap<String, String>,
}

impl FakeLsp {
    fn new() -> Self {
        Self {
            root_path: PathBuf::new(),
            config: Config::default(),
            documents: HashMap::new(),
        }
    }

    // ── Message dispatch ──

    /// Handle a JSON-RPC message. Returns a response for requests, None for
    /// notifications.
    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let method = msg.get("method").and_then(|m| m.as_str())?;
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            // Requests (have id)
            "initialize" => Some(self.initialize(&params, id?)),
            "textDocument/completion" => Some(self.completion(&params, id?)),
            "textDocument/definition" => Some(self.definition(&params, id?)),
            "textDocument/rename" => Some(self.rename(&params, id?)),
            "textDocument/codeAction" => Some(self.code_action(id?)),
            "textDocument/formatting" => Some(self.formatting(&params, id?)),
            "textDocument/inlayHint" => Some(response(id?, json!([]))),
            "completionItem/resolve" => Some(response(id?, params)),
            "shutdown" => Some(response(id?, Value::Null)),

            // Notifications (no id)
            "initialized" => None,
            "textDocument/didOpen" => {
                self.did_open(&params);
                None
            }
            "textDocument/didChange" => {
                self.did_change(&params);
                None
            }
            "textDocument/didSave" => None,
            "textDocument/didClose" => {
                if let Some(uri) = text_document_uri(&params) {
                    self.documents.remove(&uri);
                }
                None
            }
            "workspace/didChangeConfiguration" => None,
            "exit" => std::process::exit(0),
            _ => {
                // Unknown request → null response; unknown notification → ignore.
                id.map(|id| response(id, Value::Null))
            }
        }
    }

    // ── Lifecycle ──

    fn initialize(&mut self, params: &Value, id: Value) -> Value {
        // Extract workspace root from rootUri.
        if let Some(root_uri) = params.get("rootUri").and_then(|v| v.as_str()) {
            self.root_path = uri_to_path(root_uri);
        }

        // Load config from workspace root.
        let config_path = self.root_path.join(".fake-lsp.json");
        if let Ok(data) = std::fs::read_to_string(&config_path)
            && let Ok(cfg) = serde_json::from_str::<Config>(&data)
        {
            self.config = cfg;
        }

        let trigger_chars: Vec<Value> = self
            .config
            .trigger_characters
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect();

        response(
            id,
            json!({
                "capabilities": {
                    "textDocumentSync": {
                        "openClose": true,
                        "change": 2,  // incremental
                        "save": { "includeText": false }
                    },
                    "completionProvider": {
                        "triggerCharacters": trigger_chars,
                        "resolveProvider": true
                    },
                    "definitionProvider": true,
                    "renameProvider": true,
                    "codeActionProvider": {
                        "resolveProvider": true
                    },
                    "documentFormattingProvider": true,
                    "inlayHintProvider": true
                },
                "serverInfo": {
                    "name": "fake-lsp",
                    "version": "0.1.0"
                }
            }),
        )
    }

    // ── Document sync ──

    fn did_open(&mut self, params: &Value) {
        let Some(td) = params.get("textDocument") else {
            return;
        };
        let uri = td.get("uri").and_then(|v| v.as_str()).unwrap_or("");
        let text = td.get("text").and_then(|v| v.as_str()).unwrap_or("");
        self.documents.insert(uri.to_string(), text.to_string());

        // Publish configured diagnostics for this file.
        let rel = self.relative_path(uri);
        if let Some(diags) = self.config.diagnostics.get(&rel) {
            let notif = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": uri,
                    "diagnostics": diags
                }
            });
            write_message(&mut io::stdout().lock(), &notif);
        }

        // Send progress begin→end so the client sees server_name and busy=false.
        self.send_progress();
    }

    fn did_change(&mut self, params: &Value) {
        let Some(uri) = text_document_uri(params) else {
            return;
        };
        let Some(changes) = params.get("contentChanges").and_then(|v| v.as_array()) else {
            return;
        };
        let Some(content) = self.documents.get_mut(&uri) else {
            return;
        };
        for change in changes {
            if let Some(range) = change.get("range") {
                // Incremental change.
                let text = change.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let start = json_pos_to_offset(content, range.get("start"));
                let end = json_pos_to_offset(content, range.get("end"));
                if start <= end && end <= content.len() {
                    content.replace_range(start..end, text);
                }
            } else if let Some(text) = change.get("text").and_then(|v| v.as_str()) {
                // Full sync.
                *content = text.to_string();
            }
        }
    }

    // ── Completion ──

    fn completion(&self, _params: &Value, id: Value) -> Value {
        response(
            id,
            json!({
                "isIncomplete": false,
                "items": self.config.completions
            }),
        )
    }

    // ── Goto definition ──

    fn definition(&self, params: &Value, id: Value) -> Value {
        let uri = text_document_uri(params).unwrap_or_default();
        let pos = params
            .get("position")
            .cloned()
            .unwrap_or(json!({"line": 0, "character": 0}));
        let content = self.documents.get(&uri).map(|s| s.as_str()).unwrap_or("");
        let word = word_at_position(content, &pos);

        if word.is_empty() {
            return response(id, Value::Null);
        }

        // Search for `fn <word>` in the document.
        for (line_idx, line) in content.lines().enumerate() {
            if let Some(col) = find_fn_definition(line, &word) {
                return response(
                    id,
                    json!({
                        "uri": uri,
                        "range": {
                            "start": { "line": line_idx, "character": col },
                            "end": { "line": line_idx, "character": col + word.len() }
                        }
                    }),
                );
            }
        }
        response(id, Value::Null)
    }

    // ── Rename ──

    fn rename(&self, params: &Value, id: Value) -> Value {
        let uri = text_document_uri(params).unwrap_or_default();
        let pos = params
            .get("position")
            .cloned()
            .unwrap_or(json!({"line": 0, "character": 0}));
        let new_name = params.get("newName").and_then(|v| v.as_str()).unwrap_or("");
        let content = self.documents.get(&uri).map(|s| s.as_str()).unwrap_or("");
        let word = word_at_position(content, &pos);

        if word.is_empty() {
            return response(id, json!({ "changes": {} }));
        }

        // Find all occurrences of the word as whole-word matches.
        let mut edits = Vec::new();
        for (line_idx, line) in content.lines().enumerate() {
            let mut search_from = 0;
            while let Some(start) = line[search_from..].find(&word) {
                let abs_start = search_from + start;
                let abs_end = abs_start + word.len();
                // Check whole-word boundaries.
                let before_ok =
                    abs_start == 0 || !is_ident_char(line.as_bytes()[abs_start - 1] as char);
                let after_ok =
                    abs_end >= line.len() || !is_ident_char(line.as_bytes()[abs_end] as char);
                if before_ok && after_ok {
                    edits.push(json!({
                        "range": {
                            "start": { "line": line_idx, "character": abs_start },
                            "end": { "line": line_idx, "character": abs_end }
                        },
                        "newText": new_name
                    }));
                }
                search_from = abs_end;
            }
        }

        response(id, json!({ "changes": { uri: edits } }))
    }

    // ── Code action ──

    fn code_action(&self, id: Value) -> Value {
        response(id, Value::Array(self.config.code_actions.clone()))
    }

    // ── Formatting ──

    fn formatting(&self, params: &Value, id: Value) -> Value {
        let uri = text_document_uri(params).unwrap_or_default();
        let rel = self.relative_path(&uri);
        let Some(formatted) = self.config.formatting.get(&rel) else {
            return response(id, json!([]));
        };
        let content = self.documents.get(&uri).map(|s| s.as_str()).unwrap_or("");
        let line_count = content.lines().count().max(1);

        // Single edit replacing the entire document.
        response(
            id,
            json!([{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": line_count, "character": 0 }
                },
                "newText": formatted
            }]),
        )
    }

    // ── Progress ──

    fn send_progress(&self) {
        let out = &mut io::stdout().lock();
        write_message(
            out,
            &json!({
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": {
                    "token": "init",
                    "value": {
                        "kind": "begin",
                        "title": "Indexing"
                    }
                }
            }),
        );
        write_message(
            out,
            &json!({
                "jsonrpc": "2.0",
                "method": "$/progress",
                "params": {
                    "token": "init",
                    "value": {
                        "kind": "end"
                    }
                }
            }),
        );
    }

    // ── Helpers ──

    /// Convert a file URI to a path relative to the workspace root.
    fn relative_path(&self, uri: &str) -> String {
        let path = uri_to_path(uri);
        path.strip_prefix(&self.root_path)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string()
    }
}

// ── JSON-RPC framing ──

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut header = String::new();
    let mut content_length: Option<usize> = None;
    loop {
        header.clear();
        if reader.read_line(&mut header).ok()? == 0 {
            return None; // EOF
        }
        let line = header.trim();
        if line.is_empty() {
            break;
        }
        if let Some(val) = line.strip_prefix("Content-Length: ") {
            content_length = val.parse().ok();
        }
    }
    let len = content_length?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message(out: &mut impl Write, msg: &Value) {
    let body = msg.to_string();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

fn response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

// ── URI / path helpers ──

fn uri_to_path(uri: &str) -> PathBuf {
    // Strip "file://" prefix.
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);
    PathBuf::from(path_str)
}

fn text_document_uri(params: &Value) -> Option<String> {
    params
        .get("textDocument")
        .and_then(|td| td.get("uri"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

// ── Text helpers ──

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Extract the identifier word at a given LSP position in the text.
fn word_at_position(content: &str, pos: &Value) -> String {
    let line_idx = pos.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let char_idx = pos.get("character").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    let Some(line) = content.lines().nth(line_idx) else {
        return String::new();
    };
    if char_idx >= line.len() {
        return String::new();
    }

    let bytes = line.as_bytes();
    let mut start = char_idx;
    let mut end = char_idx;
    while start > 0 && is_ident_char(bytes[start - 1] as char) {
        start -= 1;
    }
    while end < bytes.len() && is_ident_char(bytes[end] as char) {
        end += 1;
    }
    line[start..end].to_string()
}

/// Find `fn <word>` in a line and return the column of `<word>`.
fn find_fn_definition(line: &str, word: &str) -> Option<usize> {
    let pattern = format!("fn {}", word);
    let idx = line.find(&pattern)?;
    let col = idx + 3; // skip "fn "
    // Check that the match is a whole word.
    let end = col + word.len();
    if end < line.len() && is_ident_char(line.as_bytes()[end] as char) {
        return None;
    }
    Some(col)
}

/// Convert an LSP position `{ "line": N, "character": M }` to a byte offset.
fn json_pos_to_offset(content: &str, pos: Option<&Value>) -> usize {
    let Some(pos) = pos else { return 0 };
    let line = pos.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let character = pos.get("character").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    let mut offset = 0;
    for (i, l) in content.split('\n').enumerate() {
        if i == line {
            // character is UTF-16 offset; for ASCII this is the same as byte offset.
            return offset + character.min(l.len());
        }
        offset += l.len() + 1; // +1 for '\n'
    }
    content.len()
}

// ── Entry point ──

fn main() {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut server = FakeLsp::new();

    while let Some(msg) = read_message(&mut reader) {
        if let Some(resp) = server.handle(&msg) {
            write_message(&mut io::stdout().lock(), &resp);
        }
    }
}
