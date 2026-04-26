//! Classify a decoded JSON-RPC 2.0 message into the four cases
//! the LSP transport cares about: response-to-our-request,
//! server-initiated request (which we must auto-reply to),
//! server notification, or malformed.
//!
//! Kept pure (no tokio, no channels) so the branching — including
//! the method-specific auto-reply shape — is exhaustively
//! unit-testable with fixture JSON strings. The transport layer
//! calls [`classify`] on each parsed frame and acts on the
//! returned `Incoming`.
//!
//! # Why server-initiated requests matter
//!
//! Servers routinely fire requests AT the client (`workspace/
//! configuration`, `client/registerCapability`, `window/
//! workDoneProgress/create`, …). If we don't reply promptly with
//! *something* they stall. Most get a `null` result; a few need a
//! method-specific shape — e.g. `workspace/configuration` wants an
//! array of empty config objects, one per requested item. Getting
//! this wrong hangs rust-analyzer before it ever emits a
//! diagnostic, so the classifier pre-computes the auto-reply
//! body alongside the request classification.

use serde_json::{Map, Value};

/// JSON-RPC 2.0 id — per spec, integer OR string OR null. Null-id
/// messages never land in any id map; we drop them upstream.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

impl RequestId {
    /// Extract from a JSON value. `null` / unsupported shapes
    /// return `None`.
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Number(n) => n.as_i64().map(Self::Int),
            Value::String(s) => Some(Self::Str(s.clone())),
            _ => None,
        }
    }

    /// Serialise back for inclusion in auto-replies.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Int(n) => Value::Number((*n).into()),
            Self::Str(s) => Value::String(s.clone()),
        }
    }
}

/// Error payload from a response. `code` is the JSON-RPC error
/// code; `message` is server-supplied free text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

/// Classification of one decoded JSON-RPC frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// Reply to a request we sent. `id` correlates against
    /// whatever the transport stashed when it issued the request.
    Response {
        id: RequestId,
        payload: Result<Value, JsonRpcError>,
    },
    /// Server initiated a request at us. The transport MUST write
    /// `auto_reply` back (it already carries the correct
    /// JSON-RPC envelope shape). `forward_as_notification` is
    /// `true` when the manager also needs to see the original
    /// request as a notification — currently only
    /// `client/registerCapability`, which carries trigger-char
    /// updates the manager reconciles against its capability map.
    Request {
        id: RequestId,
        method: String,
        params: Value,
        auto_reply: Value,
        forward_as_notification: bool,
    },
    /// Server-pushed notification. No reply expected.
    Notification { method: String, params: Value },
}

/// Classify a decoded JSON-RPC message body. `None` means the
/// frame is malformed at the level of "has neither `method` nor
/// `id`" — the caller should log and skip. Parser-level JSON
/// errors (invalid UTF-8 etc.) are caught earlier in
/// [`crate::framing`] and never reach here.
pub fn classify(body: &[u8]) -> Option<Incoming> {
    let v: Value = serde_json::from_slice(body).ok()?;

    // `id: null` counts as absent — some servers put it on
    // notifications (spec-violation, but tolerated).
    let id = v.get("id").and_then(RequestId::from_value);
    let method = v.get("method").and_then(|m| m.as_str());

    match (id, method) {
        (Some(id), Some(method)) => {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            Some(classify_server_request(id, method.to_string(), params))
        }
        (Some(id), None) => {
            // Response: either result or error, never both (spec).
            let payload = if let Some(err) = v.get("error") {
                Err(JsonRpcError {
                    code: err
                        .get("code")
                        .and_then(|c| c.as_i64())
                        .unwrap_or(-1),
                    message: err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error")
                        .to_string(),
                })
            } else {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            Some(Incoming::Response { id, payload })
        }
        (None, Some(method)) => {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            Some(Incoming::Notification {
                method: method.to_string(),
                params,
            })
        }
        (None, None) => None,
    }
}

/// Build the auto-reply body + decide whether to forward, for
/// known server-initiated request methods.
fn classify_server_request(id: RequestId, method: String, params: Value) -> Incoming {
    let (auto_reply_result, forward_as_notification) = match method.as_str() {
        // Server asks "what's my config for these N scopes?". Per
        // LSP spec we reply with an array of N config objects,
        // one per requested item. Returning an array of empty
        // objects is a valid "no custom config" answer that every
        // server handles.
        "workspace/configuration" => {
            let n = params
                .get("items")
                .and_then(|i| i.as_array())
                .map(|arr| arr.len())
                .unwrap_or(1);
            let arr = Value::Array(vec![Value::Object(Map::new()); n]);
            (arr, false)
        }
        // rust-analyzer uses this to tell us dynamic trigger chars
        // for completion. The manager needs to see it as a
        // notification to reconcile; we auto-reply with null to
        // ack the request.
        "client/registerCapability" => (Value::Null, true),
        // Unrecognised — reply null. Servers tolerate this for
        // most optional requests.
        _ => (Value::Null, false),
    };
    let auto_reply = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id.to_json(),
        "result": auto_reply_result,
    });
    Incoming::Request {
        id,
        method,
        params,
        auto_reply,
        forward_as_notification,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify_str(s: &str) -> Option<Incoming> {
        classify(s.as_bytes())
    }

    // ── Response path ───────────────────────────────────────

    #[test]
    fn response_with_result_is_ok() {
        let msg = classify_str(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#).unwrap();
        match msg {
            Incoming::Response { id, payload } => {
                assert_eq!(id, RequestId::Int(7));
                assert_eq!(payload.unwrap(), json!({"ok": true}));
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn response_with_missing_result_defaults_to_null() {
        let msg = classify_str(r#"{"jsonrpc":"2.0","id":7}"#).unwrap();
        if let Incoming::Response { payload, .. } = msg {
            assert_eq!(payload.unwrap(), Value::Null);
        } else {
            panic!();
        }
    }

    #[test]
    fn response_with_error_preserves_code_and_message() {
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","id":"abc","error":{"code":-32601,"message":"method not found"}}"#,
        )
        .unwrap();
        match msg {
            Incoming::Response {
                id,
                payload: Err(err),
            } => {
                assert_eq!(id, RequestId::Str("abc".into()));
                assert_eq!(err.code, -32601);
                assert_eq!(err.message, "method not found");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn response_with_error_missing_fields_uses_defaults() {
        let msg = classify_str(r#"{"jsonrpc":"2.0","id":1,"error":{}}"#).unwrap();
        if let Incoming::Response {
            payload: Err(err), ..
        } = msg
        {
            assert_eq!(err.code, -1);
            assert_eq!(err.message, "unknown error");
        } else {
            panic!();
        }
    }

    // ── Server-request path ────────────────────────────────

    #[test]
    fn workspace_configuration_auto_replies_with_array_of_empty_objects() {
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","id":3,"method":"workspace/configuration","params":{"items":[{"section":"rust-analyzer"},{"section":"foo"},{"section":"bar"}]}}"#,
        )
        .unwrap();
        match msg {
            Incoming::Request {
                id,
                method,
                auto_reply,
                forward_as_notification,
                ..
            } => {
                assert_eq!(id, RequestId::Int(3));
                assert_eq!(method, "workspace/configuration");
                assert!(!forward_as_notification);
                let result = auto_reply.get("result").unwrap();
                assert_eq!(result.as_array().unwrap().len(), 3);
                for obj in result.as_array().unwrap() {
                    assert_eq!(obj, &json!({}));
                }
                assert_eq!(auto_reply.get("id"), Some(&json!(3)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn workspace_configuration_with_no_items_replies_single_empty() {
        // Rare but spec-legal: servers sometimes request "the
        // global config" with no items[]. Reply with one empty.
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","id":1,"method":"workspace/configuration","params":{}}"#,
        )
        .unwrap();
        if let Incoming::Request { auto_reply, .. } = msg {
            assert_eq!(
                auto_reply.get("result").unwrap().as_array().unwrap().len(),
                1
            );
        } else {
            panic!();
        }
    }

    #[test]
    fn register_capability_forwards_as_notification_and_acks_null() {
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","id":"x","method":"client/registerCapability","params":{"registrations":[]}}"#,
        )
        .unwrap();
        match msg {
            Incoming::Request {
                method,
                auto_reply,
                forward_as_notification,
                params,
                ..
            } => {
                assert_eq!(method, "client/registerCapability");
                assert!(forward_as_notification);
                assert_eq!(auto_reply.get("result"), Some(&Value::Null));
                assert_eq!(params, json!({"registrations": []}));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn unknown_server_request_gets_null_auto_reply() {
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","id":42,"method":"window/workDoneProgress/create","params":{"token":"t"}}"#,
        )
        .unwrap();
        if let Incoming::Request {
            auto_reply,
            forward_as_notification,
            ..
        } = msg
        {
            assert!(!forward_as_notification);
            assert_eq!(auto_reply.get("result"), Some(&Value::Null));
        } else {
            panic!();
        }
    }

    #[test]
    fn auto_reply_uses_exact_request_id_type() {
        // String IDs must stay strings; integer IDs must stay
        // integers. Servers correlate by id; a reply with the
        // wrong type is worse than no reply.
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","id":"call-7","method":"window/workDoneProgress/create"}"#,
        )
        .unwrap();
        if let Incoming::Request { auto_reply, .. } = msg {
            assert_eq!(auto_reply.get("id"), Some(&json!("call-7")));
        } else {
            panic!();
        }
    }

    // ── Notification path ──────────────────────────────────

    #[test]
    fn notification_has_method_no_id() {
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///a"}}"#,
        )
        .unwrap();
        match msg {
            Incoming::Notification { method, params } => {
                assert_eq!(method, "textDocument/publishDiagnostics");
                assert_eq!(params, json!({"uri":"file:///a"}));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn notification_with_null_id_treated_as_notification() {
        // Spec-violation tolerated: some servers send `id: null`
        // on notifications. Classify as notification, not an
        // unrecognised shape.
        let msg = classify_str(
            r#"{"jsonrpc":"2.0","id":null,"method":"$/progress","params":{}}"#,
        )
        .unwrap();
        assert!(matches!(msg, Incoming::Notification { .. }));
    }

    #[test]
    fn notification_with_missing_params_defaults_to_null() {
        let msg = classify_str(r#"{"jsonrpc":"2.0","method":"$/logTrace"}"#).unwrap();
        if let Incoming::Notification { params, .. } = msg {
            assert_eq!(params, Value::Null);
        } else {
            panic!();
        }
    }

    // ── Malformed ──────────────────────────────────────────

    #[test]
    fn no_id_no_method_classifies_as_none() {
        assert!(classify_str(r#"{"jsonrpc":"2.0"}"#).is_none());
    }

    #[test]
    fn non_json_body_classifies_as_none() {
        assert!(classify_str("garbage{").is_none());
    }
}
