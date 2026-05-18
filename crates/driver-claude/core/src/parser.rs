//! Stream-json parser for the `claude -p` CLI.
//!
//! One line in, one [`ParsedStdout`] out (or [`None`] for event
//! types led ignores — `rate_limit_event` is currently the only
//! event we recognise but don't translate beyond a thin
//! pass-through; new event types added by the CLI are dropped).
//!
//! Two design rules:
//!
//! 1. **Tolerant of new fields.** The CLI's `system/init` payload
//!    keeps growing (plugins, agents, skills, mcp_servers, …). All
//!    structs here allow extra fields; only the fields led reads
//!    are listed.
//! 2. **Strict on shape.** A line that looks like an event we care
//!    about but fails to deserialise returns [`None`] rather than
//!    panicking — the native worker still drains stderr so the user
//!    sees malformed lines via the trace, but the in-process pump
//!    survives.
//!
//! Per EXAMPLE-ARCH §Drivers — this is the portable sync core,
//! fully unit-testable without spawning a subprocess.

use std::collections::HashMap;

use serde::Deserialize;

/// Token usage reported on each assistant message and the final
/// `result` event.
///
/// The "context fill" percentage led surfaces is
/// `total_prompt() / model_context_window`; the model window
/// arrives on the `result.modelUsage.<model>.contextWindow` field
/// (see [`ModelUsage`]). No hard-coded window table required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u32,
    pub cache_creation_input_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub output_tokens: u32,
}

impl Usage {
    /// Total prompt size — input plus both cache buckets. This is
    /// the number that goes into context-fill (the model sees all
    /// three as part of the prompt regardless of cost bucket).
    pub fn total_prompt(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }
}

/// Per-model breakdown from `result.modelUsage[<model-id>]`.
///
/// Fields are camelCase on the wire because this sub-object is
/// emitted by the API-side accounting layer, not the rest of the
/// CLI's snake_case payload. The CLI tells us the
/// `contextWindow` directly — led doesn't keep a hard-coded
/// `model_id → window` table.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub cache_creation_input_tokens: u32,
    #[serde(rename = "costUSD")]
    pub cost_usd: f64,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

/// One stream-json event reduced to the fields led actually reads.
///
/// `session_id` is kept as `String` here because the parser does
/// not depend on `led-core`; the runtime wraps it in `SessionUuid`
/// at the driver/runtime boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedStdout {
    /// First event of every spawn — `{"type":"system","subtype":"init",…}`.
    Init {
        session_id: String,
        model: String,
        cwd: String,
        tools: Vec<String>,
        permission_mode: String,
    },
    /// `{"type":"assistant", message.content[i].type="text"}`. The
    /// CLI emits one assistant event per content block; multi-block
    /// messages arrive as a sequence.
    AssistantText {
        session_id: String,
        text: String,
        usage: Option<Usage>,
    },
    /// `{"type":"assistant", message.content[i].type="tool_use"}`.
    AssistantToolUse {
        session_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
        usage: Option<Usage>,
    },
    /// `{"type":"user", message.content[i].type="tool_result"}`.
    /// `content` can be either a string or an array of content
    /// blocks; led keeps the raw shape so renderers can pick
    /// what to do.
    ToolResult {
        session_id: String,
        tool_use_id: String,
        content: serde_json::Value,
    },
    /// Final event of a successful turn —
    /// `{"type":"result","subtype":"success",…}`.
    Success {
        session_id: String,
        result_text: String,
        usage: Usage,
        total_cost_usd: f64,
        duration_ms: u64,
        num_turns: u32,
        model_usage: HashMap<String, ModelUsage>,
    },
    /// `--resume <uuid>` was passed for a UUID the CLI's storage
    /// does not contain. Surfaces as
    /// `{"type":"result","subtype":"error_during_execution",
    ///   errors:["No conversation found with session ID: …"]}`.
    /// `emitted_session_id` is the *new* UUID the CLI minted for
    /// the error event — not the one we asked it to resume.
    SessionNotFound {
        emitted_session_id: String,
        errors: Vec<String>,
    },
    /// Other `error_during_execution` results (API failures,
    /// permission denials with no other surface, etc.).
    Error {
        session_id: String,
        errors: Vec<String>,
        duration_ms: u64,
    },
    /// `{"type":"rate_limit_event",…}`. Pass-through only; the
    /// runtime surfaces it in the status line if it wants.
    RateLimit {
        session_id: String,
        status: String,
        resets_at: Option<u64>,
    },
}

/// Parse a single line of stream-json. Trailing newlines are
/// tolerated. Returns `None` for unknown event types, malformed
/// JSON, or shapes that don't fit any variant.
pub fn parse_line(s: &str) -> Option<ParsedStdout> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = value.as_object()?;
    let ty = obj.get("type")?.as_str()?;
    match ty {
        "system" => parse_system(obj),
        "assistant" => parse_assistant(obj),
        "user" => parse_user(obj),
        "result" => parse_result(obj),
        "rate_limit_event" => parse_rate_limit(obj),
        _ => None,
    }
}

// ── per-type helpers ─────────────────────────────────────────────────

fn parse_system(obj: &serde_json::Map<String, serde_json::Value>) -> Option<ParsedStdout> {
    if obj.get("subtype")?.as_str()? != "init" {
        return None;
    }
    let session_id = obj.get("session_id")?.as_str()?.to_string();
    let model = obj.get("model")?.as_str()?.to_string();
    let cwd = obj.get("cwd")?.as_str()?.to_string();
    let tools = obj
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // `permissionMode` is camelCase on the init payload (CLI quirk;
    // the flag itself is `--permission-mode`).
    let permission_mode = obj
        .get("permissionMode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(ParsedStdout::Init {
        session_id,
        model,
        cwd,
        tools,
        permission_mode,
    })
}

fn parse_assistant(obj: &serde_json::Map<String, serde_json::Value>) -> Option<ParsedStdout> {
    let session_id = obj.get("session_id")?.as_str()?.to_string();
    let message = obj.get("message")?.as_object()?;
    let content = message.get("content")?.as_array()?;
    let block = content.first()?.as_object()?;
    let block_ty = block.get("type")?.as_str()?;
    let usage = message
        .get("usage")
        .and_then(|v| serde_json::from_value::<Usage>(v.clone()).ok());
    match block_ty {
        "text" => {
            let text = block.get("text")?.as_str()?.to_string();
            Some(ParsedStdout::AssistantText {
                session_id,
                text,
                usage,
            })
        }
        "tool_use" => {
            let tool_use_id = block.get("id")?.as_str()?.to_string();
            let name = block.get("name")?.as_str()?.to_string();
            let input = block.get("input").cloned().unwrap_or(serde_json::Value::Null);
            Some(ParsedStdout::AssistantToolUse {
                session_id,
                tool_use_id,
                name,
                input,
                usage,
            })
        }
        _ => None,
    }
}

fn parse_user(obj: &serde_json::Map<String, serde_json::Value>) -> Option<ParsedStdout> {
    let session_id = obj.get("session_id")?.as_str()?.to_string();
    let message = obj.get("message")?.as_object()?;
    let content = message.get("content")?.as_array()?;
    let block = content.first()?.as_object()?;
    if block.get("type")?.as_str()? != "tool_result" {
        return None;
    }
    let tool_use_id = block.get("tool_use_id")?.as_str()?.to_string();
    let content_val = block.get("content").cloned().unwrap_or(serde_json::Value::Null);
    Some(ParsedStdout::ToolResult {
        session_id,
        tool_use_id,
        content: content_val,
    })
}

fn parse_result(obj: &serde_json::Map<String, serde_json::Value>) -> Option<ParsedStdout> {
    let subtype = obj.get("subtype")?.as_str()?;
    let session_id = obj
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let duration_ms = obj.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    match subtype {
        "success" => {
            let result_text = obj
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let usage = obj
                .get("usage")
                .and_then(|v| serde_json::from_value::<Usage>(v.clone()).ok())
                .unwrap_or_default();
            let total_cost_usd = obj
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let num_turns = obj
                .get("num_turns")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let model_usage = obj
                .get("modelUsage")
                .and_then(|v| serde_json::from_value::<HashMap<String, ModelUsage>>(v.clone()).ok())
                .unwrap_or_default();
            Some(ParsedStdout::Success {
                session_id,
                result_text,
                usage,
                total_cost_usd,
                duration_ms,
                num_turns,
                model_usage,
            })
        }
        "error_during_execution" => {
            let errors: Vec<String> = obj
                .get("errors")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // SessionNotFound is the one error shape the driver
            // needs to react to differently (mark the session
            // gone, don't retry). Everything else is a generic
            // turn-level error.
            if errors
                .iter()
                .any(|e| e.starts_with("No conversation found with session ID"))
            {
                Some(ParsedStdout::SessionNotFound {
                    emitted_session_id: session_id,
                    errors,
                })
            } else {
                Some(ParsedStdout::Error {
                    session_id,
                    errors,
                    duration_ms,
                })
            }
        }
        _ => None,
    }
}

fn parse_rate_limit(obj: &serde_json::Map<String, serde_json::Value>) -> Option<ParsedStdout> {
    let session_id = obj.get("session_id")?.as_str()?.to_string();
    let info = obj.get("rate_limit_info")?.as_object()?;
    let status = info.get("status")?.as_str()?.to_string();
    let resets_at = info
        .get("resetsAt")
        .and_then(|v| v.as_u64());
    Some(ParsedStdout::RateLimit {
        session_id,
        status,
        resets_at,
    })
}

// ── tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from a real `claude -p --input-format stream-json
    // --output-format stream-json --verbose --effort low
    // --permission-mode auto` invocation with input "Reply with
    // exactly the word HI and nothing else.". Trimmed to the
    // fields the parser inspects; extra fields elided to keep the
    // test source small. Real fixtures (with every CLI field
    // intact) live in `tests/fixtures/`.

    const REAL_INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/Users/martin/dev/led","session_id":"c553ff7f-0832-45a2-87bd-a13131e83cf2","tools":["Task","Bash","Read"],"mcp_servers":[],"model":"claude-opus-4-7[1m]","permissionMode":"auto","slash_commands":[],"apiKeySource":"none","claude_code_version":"2.1.143","uuid":"2ae3a267-21f5-4ec4-ae35-860ef111d8a0"}"#;

    const REAL_ASSISTANT_TEXT: &str = r#"{"type":"assistant","message":{"model":"claude-opus-4-7","id":"msg_017uQ9dd4t2ecasWvnMHx3Xn","type":"message","role":"assistant","content":[{"type":"text","text":"HI"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":6,"cache_creation_input_tokens":9791,"cache_read_input_tokens":18383,"output_tokens":1,"service_tier":"standard"}},"parent_tool_use_id":null,"session_id":"c553ff7f-0832-45a2-87bd-a13131e83cf2","uuid":"3a2ebe06-46f8-460a-ad60-6795e432b923"}"#;

    const REAL_RATE_LIMIT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1779143400,"rateLimitType":"five_hour","overageStatus":"rejected"},"uuid":"1a7d8a04-e406-4a5c-9793-19d08269cc73","session_id":"c553ff7f-0832-45a2-87bd-a13131e83cf2"}"#;

    const REAL_SUCCESS: &str = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1936,"duration_api_ms":3837,"num_turns":1,"result":"HI","stop_reason":"end_turn","session_id":"c553ff7f-0832-45a2-87bd-a13131e83cf2","total_cost_usd":0.07108225,"usage":{"input_tokens":6,"cache_creation_input_tokens":9791,"cache_read_input_tokens":18383,"output_tokens":6,"service_tier":"standard"},"modelUsage":{"claude-opus-4-7[1m]":{"inputTokens":6,"outputTokens":6,"cacheReadInputTokens":18383,"cacheCreationInputTokens":9791,"costUSD":0.07056525,"contextWindow":1000000,"maxOutputTokens":64000}}}"#;

    // Captured from `... --resume 00000000-0000-0000-0000-000000000000`.
    const REAL_SESSION_NOT_FOUND: &str = r#"{"type":"result","subtype":"error_during_execution","duration_ms":0,"duration_api_ms":0,"is_error":true,"num_turns":0,"stop_reason":null,"session_id":"6672498a-55e2-4d2b-ae63-0ddd5a2e1fbb","total_cost_usd":0,"usage":{"input_tokens":0,"output_tokens":0},"errors":["No conversation found with session ID: 00000000-0000-0000-0000-000000000000"]}"#;

    // Synthesised from the documented protocol shape — kept here
    // rather than captured because every real tool_use turn costs
    // money. The driver doesn't care about *which* tool; only that
    // the shape round-trips.
    const SYNTH_ASSISTANT_TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01","name":"Bash","input":{"command":"ls src/"}}],"usage":{"input_tokens":10,"output_tokens":5}},"session_id":"abc-123"}"#;

    const SYNTH_TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"main.rs\nlib.rs"}]},"session_id":"abc-123"}"#;

    #[test]
    fn parses_init_with_minimal_fields() {
        let got = parse_line(REAL_INIT).expect("init parses");
        match got {
            ParsedStdout::Init {
                session_id,
                model,
                cwd,
                tools,
                permission_mode,
            } => {
                assert_eq!(session_id, "c553ff7f-0832-45a2-87bd-a13131e83cf2");
                assert_eq!(model, "claude-opus-4-7[1m]");
                assert_eq!(cwd, "/Users/martin/dev/led");
                assert_eq!(tools, vec!["Task", "Bash", "Read"]);
                assert_eq!(permission_mode, "auto");
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn parses_assistant_text_with_usage() {
        let got = parse_line(REAL_ASSISTANT_TEXT).expect("assistant/text parses");
        match got {
            ParsedStdout::AssistantText {
                session_id,
                text,
                usage,
            } => {
                assert_eq!(session_id, "c553ff7f-0832-45a2-87bd-a13131e83cf2");
                assert_eq!(text, "HI");
                let u = usage.expect("usage on real assistant");
                assert_eq!(u.input_tokens, 6);
                assert_eq!(u.cache_creation_input_tokens, 9791);
                assert_eq!(u.cache_read_input_tokens, 18383);
                assert_eq!(u.output_tokens, 1);
                assert_eq!(u.total_prompt(), 6 + 9791 + 18383);
            }
            other => panic!("expected AssistantText, got {other:?}"),
        }
    }

    #[test]
    fn parses_assistant_tool_use() {
        let got = parse_line(SYNTH_ASSISTANT_TOOL_USE).expect("tool_use parses");
        match got {
            ParsedStdout::AssistantToolUse {
                session_id,
                tool_use_id,
                name,
                input,
                usage,
            } => {
                assert_eq!(session_id, "abc-123");
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(name, "Bash");
                assert_eq!(input["command"], "ls src/");
                assert!(usage.is_some());
            }
            other => panic!("expected AssistantToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parses_tool_result() {
        let got = parse_line(SYNTH_TOOL_RESULT).expect("tool_result parses");
        match got {
            ParsedStdout::ToolResult {
                session_id,
                tool_use_id,
                content,
            } => {
                assert_eq!(session_id, "abc-123");
                assert_eq!(tool_use_id, "toolu_01");
                assert_eq!(content, serde_json::json!("main.rs\nlib.rs"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn parses_success_with_model_usage_and_context_window() {
        let got = parse_line(REAL_SUCCESS).expect("success parses");
        match got {
            ParsedStdout::Success {
                session_id,
                result_text,
                usage,
                total_cost_usd,
                duration_ms,
                num_turns,
                model_usage,
            } => {
                assert_eq!(session_id, "c553ff7f-0832-45a2-87bd-a13131e83cf2");
                assert_eq!(result_text, "HI");
                assert_eq!(usage.input_tokens, 6);
                assert_eq!(usage.output_tokens, 6);
                assert!((total_cost_usd - 0.07108225).abs() < 1e-9);
                assert_eq!(duration_ms, 1936);
                assert_eq!(num_turns, 1);
                let mu = model_usage
                    .get("claude-opus-4-7[1m]")
                    .expect("modelUsage for spawned model");
                assert_eq!(mu.context_window, 1_000_000);
                assert_eq!(mu.max_output_tokens, 64_000);
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn parses_session_not_found_distinct_from_generic_error() {
        let got = parse_line(REAL_SESSION_NOT_FOUND).expect("session-not-found parses");
        match got {
            ParsedStdout::SessionNotFound {
                emitted_session_id,
                errors,
            } => {
                // The session_id on the event is a *new* UUID the
                // CLI minted for the error itself, NOT the UUID
                // that was passed via --resume.
                assert_eq!(emitted_session_id, "6672498a-55e2-4d2b-ae63-0ddd5a2e1fbb");
                assert_eq!(errors.len(), 1);
                assert!(errors[0].starts_with("No conversation found with session ID"));
                assert!(errors[0].contains("00000000-0000-0000-0000-000000000000"));
            }
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn parses_generic_error_during_execution() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":12,"session_id":"abc","errors":["upstream API returned 500"]}"#;
        let got = parse_line(line).expect("error parses");
        match got {
            ParsedStdout::Error {
                session_id,
                errors,
                duration_ms,
            } => {
                assert_eq!(session_id, "abc");
                assert_eq!(errors, vec!["upstream API returned 500"]);
                assert_eq!(duration_ms, 12);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parses_rate_limit_event() {
        let got = parse_line(REAL_RATE_LIMIT).expect("rate_limit parses");
        match got {
            ParsedStdout::RateLimit {
                session_id,
                status,
                resets_at,
            } => {
                assert_eq!(session_id, "c553ff7f-0832-45a2-87bd-a13131e83cf2");
                assert_eq!(status, "allowed");
                assert_eq!(resets_at, Some(1_779_143_400));
            }
            other => panic!("expected RateLimit, got {other:?}"),
        }
    }

    #[test]
    fn returns_none_for_unknown_event_type() {
        let line = r#"{"type":"something_new","payload":42}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn returns_none_for_malformed_json() {
        assert!(parse_line("not json").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("[]").is_none()); // top-level array, not object
    }

    #[test]
    fn tolerates_trailing_newline() {
        let line = format!("{REAL_INIT}\n");
        assert!(matches!(parse_line(&line), Some(ParsedStdout::Init { .. })));
    }

    #[test]
    fn tolerates_unknown_fields_on_init() {
        // The real CLI's init has 20+ fields; the parser only reads
        // five. Adding new ones must not break parsing.
        let line = r#"{"type":"system","subtype":"init","session_id":"x","model":"m","cwd":"/","tools":[],"permissionMode":"auto","newly_added_field":"surprise","another":42}"#;
        assert!(matches!(parse_line(line), Some(ParsedStdout::Init { .. })));
    }

    #[test]
    fn assistant_text_without_usage_is_ok() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"session_id":"x"}"#;
        match parse_line(line).expect("parses") {
            ParsedStdout::AssistantText { usage, .. } => assert!(usage.is_none()),
            other => panic!("expected AssistantText, got {other:?}"),
        }
    }
}
