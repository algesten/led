use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use led_core::Waker;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RequestId {
    Int(i32),
    Str(String),
}

impl RequestId {
    pub(crate) fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(|n| RequestId::Int(n as i32)),
            Value::String(s) => Some(RequestId::Str(s.clone())),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LspError {
    pub(crate) message: String,
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LSP error: {}", self.message)
    }
}

pub(crate) struct LspNotification {
    pub(crate) method: String,
    pub(crate) params: Value,
}

pub(crate) type ResponseHandlers =
    Arc<Mutex<HashMap<RequestId, tokio::sync::oneshot::Sender<Result<Value, LspError>>>>>;

pub(crate) fn spawn_writer(
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

pub(crate) fn spawn_reader(
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
                log::debug!("LSP <- server request: {}", method);
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
                            log::debug!("LSP <- response id={:?}", id);
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
