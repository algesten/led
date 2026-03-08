use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};

use crate::server::LspError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Int(i32),
    Str(String),
}

impl RequestId {
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(|i| RequestId::Int(i as i32)),
            Value::String(s) => Some(RequestId::Str(s.clone())),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            RequestId::Int(i) => Value::Number((*i).into()),
            RequestId::Str(s) => Value::String(s.clone()),
        }
    }
}

pub struct LspNotification {
    pub method: String,
    pub params: Value,
}

pub type ResponseHandlers =
    Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<Value, LspError>>>>>;

/// Spawn writer task: reads JSON strings from channel, writes Content-Length framed messages to stdin.
pub fn spawn_writer(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let header = format!("Content-Length: {}\r\n\r\n", msg.len());
            if stdin.write_all(header.as_bytes()).await.is_err() {
                break;
            }
            if stdin.write_all(msg.as_bytes()).await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    });
}

/// Spawn reader task: reads Content-Length framed messages from stdout, dispatches responses and notifications.
pub fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    response_handlers: ResponseHandlers,
    notification_tx: mpsc::UnboundedSender<LspNotification>,
    outbound_tx: mpsc::UnboundedSender<String>,
    waker: Option<led_core::Waker>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);

        loop {
            // Read headers
            let content_length = match read_content_length(&mut reader).await {
                Some(len) => len,
                None => break, // EOF or error
            };

            // Read body
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).await.is_err() {
                break;
            }

            let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
                log::debug!("LSP: failed to parse JSON message ({content_length} bytes)");
                continue;
            };

            if msg.get("id").is_some() && msg.get("method").is_some() {
                // Server → client request: respond with null result for now
                let id = msg["id"].clone();
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null,
                });
                let _ = outbound_tx.send(response.to_string());
            } else if msg.get("id").is_some() {
                // Response to our request
                let Some(id) = RequestId::from_value(&msg["id"]) else {
                    continue;
                };
                let handler = response_handlers.lock().unwrap().remove(&id);
                if let Some(tx) = handler {
                    let result = if let Some(error) = msg.get("error") {
                        Err(LspError {
                            code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(0) as i32,
                            message: error
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error")
                                .to_string(),
                        })
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(result);
                }
            } else if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                // Notification from server
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let _ = notification_tx.send(LspNotification {
                    method: method.to_string(),
                    params,
                });
                if let Some(ref w) = waker {
                    w();
                }
            }
        }
    });
}

async fn read_content_length<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Option<usize> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.ok()?;
        if n == 0 {
            return None; // EOF
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            // End of headers
            return content_length;
        }

        if let Some(value) = trimmed.strip_prefix("Content-Length: ") {
            content_length = value.parse().ok();
        }
    }
}
