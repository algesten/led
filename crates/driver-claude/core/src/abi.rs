//! ABI types crossing the mpsc between the sync driver and the
//! native subprocess worker.
//!
//! Sync direction:
//!
//! - Runtime → driver: `ClaudeAction` (consumed by
//!   `ClaudeDriver::execute`, added in a later stage).
//! - Driver → native worker: [`ClaudeCmd`].
//!
//! Async direction:
//!
//! - Native worker → driver: [`ClaudeEvent`]. The sync driver's
//!   `process` step drains this into [`super::ChatLifecycle`] and
//!   [`super::ChatTranscripts`].
//!
//! `SessionUuid` tags every cmd and event so the worker can fan
//! out across multiple parallel subprocesses (one per chat tab).

use led_core::SessionUuid;
use serde::{Deserialize, Serialize};

use crate::parser::ParsedStdout;

/// `--effort <level>` flag. Default is `XHigh` per project
/// preference; users override per session via `state-chat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    #[default]
    XHigh,
    Max,
}

impl Effort {
    /// The exact string the CLI's `--effort` flag accepts.
    pub fn as_flag(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// `--permission-mode <mode>` flag. Default is `Auto` (the
/// classifier-driven mode the CLI's `auto-mode` subcommand inspects).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    AcceptEdits,
    #[default]
    Auto,
    BypassPermissions,
    Default,
    DontAsk,
    Plan,
}

impl PermissionMode {
    /// The exact string the CLI's `--permission-mode` flag accepts
    /// (camelCase on the wire — note `acceptEdits` not `accept-edits`).
    pub fn as_flag(self) -> &'static str {
        match self {
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Auto => "auto",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::Default => "default",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Plan => "plan",
        }
    }
}

/// How the worker should bring up the subprocess.
///
/// Stored on `ClaudeCmd::Spawn` rather than two separate cmds so
/// the worker handles spawn lifecycle uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// `claude -p ... --session-id <uuid>` — new session, our UUID.
    Fresh,
    /// `claude -p ... --resume <uuid>` — continue an existing one.
    /// The same UUID we minted before; the CLI's storage decides
    /// whether it's still loadable (otherwise we get back
    /// [`ParsedStdout::SessionNotFound`]).
    Resume,
}

/// Command from the sync driver to the native worker.
#[derive(Debug, Clone)]
pub enum ClaudeCmd {
    /// Spawn a subprocess for `uuid`. Worker:
    ///
    /// 1. `claude -p --input-format stream-json --output-format
    ///    stream-json --verbose --session-id|--resume <uuid>
    ///    --effort <eff> --permission-mode <mode>`.
    /// 2. Spin reader + writer threads.
    /// 3. Emit `ClaudeEvent::Parsed` per stdout line, `Stderr`
    ///    per stderr line, `Exited` on process exit.
    Spawn {
        uuid: SessionUuid,
        mode: SpawnMode,
        effort: Effort,
        permission_mode: PermissionMode,
    },
    /// Send one user message on the subprocess's stdin. Worker
    /// serialises `{"type":"user","message":{"role":"user",
    /// "content": text}}\n` and flushes.
    UserMessage { uuid: SessionUuid, text: String },
    /// SIGINT the subprocess (`claude -p` has no documented
    /// in-protocol cancel). Worker still emits `Exited` when the
    /// process terminates so the lifecycle source progresses.
    Cancel { uuid: SessionUuid },
    /// Close stdin and wait for the process to drain. Polite
    /// shutdown path used when a chat tab closes.
    Shutdown { uuid: SessionUuid },
}

/// Process exit detail. Either `code` or `signal` is set on every
/// real exit; both `None` means "the worker noticed the child died
/// but couldn't read either" — treat as crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitInfo {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Event from the native worker to the sync driver.
#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    /// One stdout line, already parsed by [`crate::parse_line`].
    /// `uuid` is the SessionUuid we *spawned* with — the CLI may
    /// echo a different one inside the JSON (notably on
    /// [`ParsedStdout::SessionNotFound`], where it mints a fresh
    /// id for the error event itself).
    Parsed {
        uuid: SessionUuid,
        parsed: ParsedStdout,
    },
    /// One stderr line. Surface in trace; the user only sees it
    /// if something went wrong.
    Stderr { uuid: SessionUuid, line: String },
    /// Subprocess exited.
    Exited { uuid: SessionUuid, exit: ExitInfo },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_flag_strings_match_cli() {
        // Verbatim choices from `claude --help`:
        // "low|medium|high|xhigh|max".
        for (e, s) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::XHigh, "xhigh"),
            (Effort::Max, "max"),
        ] {
            assert_eq!(e.as_flag(), s);
        }
    }

    #[test]
    fn permission_mode_flag_strings_match_cli() {
        // Verbatim choices from `claude --help`:
        // "acceptEdits|auto|bypassPermissions|default|dontAsk|plan".
        for (p, s) in [
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::Auto, "auto"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
            (PermissionMode::Default, "default"),
            (PermissionMode::DontAsk, "dontAsk"),
            (PermissionMode::Plan, "plan"),
        ] {
            assert_eq!(p.as_flag(), s);
        }
    }

    #[test]
    fn defaults_match_project_preference() {
        assert_eq!(Effort::default(), Effort::XHigh);
        assert_eq!(PermissionMode::default(), PermissionMode::Auto);
    }

    #[test]
    fn exit_info_ok_only_for_zero_code() {
        assert!(ExitInfo { code: Some(0), signal: None }.ok());
        assert!(!ExitInfo { code: Some(1), signal: None }.ok());
        assert!(!ExitInfo { code: None, signal: Some(15) }.ok());
        assert!(!ExitInfo::default().ok());
    }
}
