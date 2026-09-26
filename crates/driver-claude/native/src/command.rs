//! Pure helpers: CLI argv construction and NDJSON message encoding.
//!
//! Split out so they're trivially unit-testable without spawning a
//! subprocess. The manager thread calls these to translate ABI
//! types into the shapes the `claude` CLI expects.

use led_core::SessionUuid;
use led_driver_claude_core::{Effort, PermissionMode, SpawnMode};

/// Build the argv for `claude -p` based on the ABI Spawn cmd.
///
/// `bin` is the binary name or absolute path — typically `"claude"`
/// (resolved against `PATH`), overridable via the `LED_CLAUDE_BIN`
/// env var (see [`crate::claude_bin`]).
pub fn build_argv(
    bin: &str,
    uuid: &SessionUuid,
    mode: SpawnMode,
    effort: Effort,
    permission_mode: PermissionMode,
) -> Vec<String> {
    let mut argv = vec![
        bin.to_string(),
        "-p".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
    ];
    match mode {
        SpawnMode::Fresh => {
            argv.push("--session-id".into());
            argv.push(uuid.as_str().to_string());
        }
        SpawnMode::Resume => {
            // `--resume <uuid>` resumes the existing CLI-side
            // transcript at that UUID; we don't pair it with
            // `--session-id` (the CLI loads the id from the
            // existing transcript).
            argv.push("--resume".into());
            argv.push(uuid.as_str().to_string());
        }
    }
    argv.push("--effort".into());
    argv.push(effort.as_flag().to_string());
    argv.push("--permission-mode".into());
    argv.push(permission_mode.as_flag().to_string());
    argv
}

/// Serialise a user message as one NDJSON line (terminator
/// included). The CLI's stream-json input grammar accepts
/// `{"type":"user","message":{"role":"user","content": <text>}}`
/// per line.
pub fn encode_user_message(text: &str) -> String {
    let v = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": text }
    });
    let mut s = v.to_string();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u() -> SessionUuid {
        SessionUuid::new("11111111-2222-3333-4444-555555555555")
    }

    #[test]
    fn fresh_spawn_uses_session_id() {
        let argv = build_argv("claude", &u(), SpawnMode::Fresh, Effort::XHigh, PermissionMode::Auto);
        let cmd_line = argv.join(" ");
        assert!(cmd_line.contains("--session-id 11111111-2222-3333-4444-555555555555"));
        assert!(!cmd_line.contains("--resume"));
        assert!(cmd_line.contains("--effort xhigh"));
        assert!(cmd_line.contains("--permission-mode auto"));
        assert!(cmd_line.contains("--input-format stream-json"));
        assert!(cmd_line.contains("--output-format stream-json"));
        assert!(cmd_line.contains("--verbose"));
    }

    #[test]
    fn resume_spawn_uses_resume_flag_only() {
        let argv = build_argv("claude", &u(), SpawnMode::Resume, Effort::Low, PermissionMode::AcceptEdits);
        let cmd_line = argv.join(" ");
        assert!(cmd_line.contains("--resume 11111111-2222-3333-4444-555555555555"));
        assert!(!cmd_line.contains("--session-id"));
        assert!(cmd_line.contains("--effort low"));
        // PermissionMode camelCase is preserved on the wire even
        // though the flag itself is kebab-case.
        assert!(cmd_line.contains("--permission-mode acceptEdits"));
    }

    #[test]
    fn argv_first_arg_is_the_bin() {
        let argv = build_argv("/opt/bin/claude", &u(), SpawnMode::Fresh, Effort::XHigh, PermissionMode::Auto);
        assert_eq!(argv[0], "/opt/bin/claude");
        assert_eq!(argv[1], "-p");
    }

    #[test]
    fn encode_user_message_is_one_ndjson_line() {
        let s = encode_user_message("hi there");
        assert!(s.ends_with('\n'));
        // Exactly one newline (the terminator).
        assert_eq!(s.matches('\n').count(), 1);
        // Round-trips through serde_json.
        let v: serde_json::Value = serde_json::from_str(s.trim_end()).expect("parses");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hi there");
    }

    #[test]
    fn encode_user_message_handles_special_chars() {
        // Newlines, quotes, unicode all need to round-trip.
        let text = "line one\nline two \"quoted\" — emoji 🚀";
        let s = encode_user_message(text);
        let v: serde_json::Value = serde_json::from_str(s.trim_end()).expect("parses");
        assert_eq!(v["message"]["content"], text);
    }
}
