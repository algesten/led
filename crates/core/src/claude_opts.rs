//! Per-session options the user can override for a Claude chat.
//!
//! Lives in `led-core` (not `driver-claude/core`) so the
//! user-decision source (`state-chat`) can carry per-session
//! overrides without depending on the driver crate — the
//! state-tier-can-only-depend-on-shared-primitives rule from
//! EXAMPLE-ARCH §Crate layout.
//!
//! `as_flag()` returns the exact string the `claude` CLI's
//! `--effort` / `--permission-mode` flags accept; the driver's
//! native worker uses this directly when building argv.

use serde::{Deserialize, Serialize};

/// `--effort <level>`. Default `XHigh` per project preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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
    pub fn as_flag(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// Parse from the same string `as_flag` produces. Used by
    /// config + SQLite-persisted overrides.
    pub fn from_flag(s: &str) -> Option<Self> {
        Some(match s {
            "low" => Effort::Low,
            "medium" => Effort::Medium,
            "high" => Effort::High,
            "xhigh" => Effort::XHigh,
            "max" => Effort::Max,
            _ => return None,
        })
    }
}

/// `--permission-mode <mode>`. Default `Auto` (the classifier-
/// driven mode the CLI's `auto-mode` subcommand inspects).
///
/// Note the on-wire form is camelCase even though the flag
/// itself is kebab-case — `--permission-mode acceptEdits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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

    pub fn from_flag(s: &str) -> Option<Self> {
        Some(match s {
            "acceptEdits" => PermissionMode::AcceptEdits,
            "auto" => PermissionMode::Auto,
            "bypassPermissions" => PermissionMode::BypassPermissions,
            "default" => PermissionMode::Default,
            "dontAsk" => PermissionMode::DontAsk,
            "plan" => PermissionMode::Plan,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_round_trips_through_flag() {
        for e in [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ] {
            assert_eq!(Effort::from_flag(e.as_flag()), Some(e));
        }
        assert_eq!(Effort::from_flag("nonsense"), None);
    }

    #[test]
    fn permission_mode_round_trips_through_flag() {
        for p in [
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::BypassPermissions,
            PermissionMode::Default,
            PermissionMode::DontAsk,
            PermissionMode::Plan,
        ] {
            assert_eq!(PermissionMode::from_flag(p.as_flag()), Some(p));
        }
        assert_eq!(PermissionMode::from_flag("nonsense"), None);
    }

    #[test]
    fn defaults_match_project_preference() {
        assert_eq!(Effort::default(), Effort::XHigh);
        assert_eq!(PermissionMode::default(), PermissionMode::Auto);
    }
}
