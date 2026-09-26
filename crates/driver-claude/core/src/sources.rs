//! External-fact sources owned by the Claude driver.
//!
//! Two sources, both keyed by [`SessionUuid`]:
//!
//! - [`ChatLifecycle`] — per-session subprocess state (Spawning,
//!   Running, Exiting, Exited, NotFound). The runtime's
//!   `subprocess_action` memo diffs desired (open chat tabs)
//!   against this to emit Spawn / UserMessage / Cancel / Shutdown.
//!   The execute pattern writes intent into this source *before*
//!   dispatching the command so the next iteration's diff returns
//!   Noop (no re-fire).
//!
//! - [`ChatTranscripts`] — per-session live event timeline plus
//!   the latest observed usage / model / context window. The
//!   chat-tab render memo joins this with the persisted
//!   `ChatStore` from `driver-session`; `pending_persist_writes`
//!   diffs the live tail against `ChatStore.messages` to emit
//!   SQL INSERTs.
//!
//! Per EXAMPLE-ARCH §Sources — driver-owned, never mutated by
//! user-decision flow. Per
//! [[feedback_no_driver_types_in_appstate]] these structs stay
//! out of AppState; the runtime declares its own `#[drv::input]`
//! projections in `runtime/src/query/inputs.rs`.

use imbl::HashMap;

use led_core::SessionUuid;

use crate::parser::Usage;

// ── Lifecycle ────────────────────────────────────────────────────────

/// Per-session subprocess state.
///
/// Absence from `per_session` means "led has no record of this
/// session being spawned this process lifetime" — the source is in
/// memory only; persisted session existence lives on
/// `ChatStore.rows` (driver-session) and is the
/// `desired_subprocesses` memo's input, not this struct's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatLifecycle {
    pub per_session: HashMap<SessionUuid, LifecycleState>,
}

/// Subprocess state machine. The states are the strict minimum the
/// `subprocess_action` memo needs to decide whether to spawn /
/// despawn / re-fire.
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleState {
    /// `Spawn` cmd dispatched; awaiting first `Parsed::Init` event.
    /// Treated as "in flight" by the action memo (same as Running
    /// from the diff's POV — neither triggers another Spawn).
    Spawning,
    /// `Parsed::Init` received. Free to receive `UserMessage` cmds.
    Running,
    /// `Cancel` or `Shutdown` cmd dispatched; awaiting `Exited`.
    Exiting,
    /// Process is gone. `subprocess_action` will respawn on next
    /// user-message intent (rather than auto-restarting) so a
    /// crash loop doesn't burn quota.
    Exited(crate::abi::ExitInfo),
    /// `--resume <uuid>` returned "No conversation found ...". Do
    /// not respawn — the SQLite row is now orphaned and led's UI
    /// should offer to delete or fork. Per
    /// [[feedback_driver_failure_state]] this is explicit state,
    /// not a silent absence.
    NotFound,
}

impl LifecycleState {
    /// True if a spawn is pending or active — the action memo
    /// must not emit another `Spawn` for sessions in this state.
    pub fn in_flight(&self) -> bool {
        matches!(self, LifecycleState::Spawning | LifecycleState::Running)
    }

    /// True if the subprocess is ready to accept `UserMessage`
    /// cmds.
    pub fn accepting_messages(&self) -> bool {
        matches!(self, LifecycleState::Running)
    }
}

// ── Transcripts ──────────────────────────────────────────────────────

/// Per-session in-memory event log + the rolling
/// usage/model/context-window snapshot.
///
/// The runtime's persistence memo diffs this against `ChatStore`
/// (driver-session) and emits INSERT cmds for entries not yet
/// stored; led's display reads from `ChatStore.messages`, NOT from
/// this struct, so a led restart with no live driver still
/// renders the tab correctly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatTranscripts {
    pub per_session: HashMap<SessionUuid, SessionTimeline>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionTimeline {
    /// Append-only event log for this process lifetime. Cleared on
    /// session despawn — replayed from `ChatStore` if the tab
    /// reopens.
    pub events: Vec<TimelineEvent>,
    /// Latest assistant-event `usage` seen. Drives the context-%
    /// memo without needing to walk `events`.
    pub latest_usage: Option<Usage>,
    /// Model name from the most recent `Init` event (e.g.
    /// `"claude-opus-4-7[1m]"`).
    pub model: Option<String>,
    /// `modelUsage[model].contextWindow` from the most recent
    /// `Success` event. Populated lazily — first turn's worth.
    pub context_window: Option<u32>,
}

/// One entry in [`SessionTimeline::events`].
///
/// Mirrors the same shapes the parser emits, plus `UserSent` for
/// echoing the user's own input into the timeline (the CLI does
/// not replay user messages on stdout unless
/// `--replay-user-messages` is set, so the driver records them
/// at the moment we ship them).
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineEvent {
    UserSent {
        text: String,
    },
    AssistantText {
        text: String,
    },
    AssistantToolUse {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    TurnComplete {
        usage: Usage,
        cost_usd: f64,
        num_turns: u32,
    },
    TurnError {
        errors: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::ExitInfo;

    #[test]
    fn lifecycle_in_flight_includes_spawning_and_running() {
        assert!(LifecycleState::Spawning.in_flight());
        assert!(LifecycleState::Running.in_flight());
        assert!(!LifecycleState::Exiting.in_flight());
        assert!(!LifecycleState::Exited(ExitInfo::default()).in_flight());
        assert!(!LifecycleState::NotFound.in_flight());
    }

    #[test]
    fn lifecycle_accepts_messages_only_when_running() {
        assert!(!LifecycleState::Spawning.accepting_messages());
        assert!(LifecycleState::Running.accepting_messages());
        assert!(!LifecycleState::Exiting.accepting_messages());
        assert!(!LifecycleState::Exited(ExitInfo::default()).accepting_messages());
        assert!(!LifecycleState::NotFound.accepting_messages());
    }

    #[test]
    fn sources_default_to_empty() {
        let life = ChatLifecycle::default();
        assert!(life.per_session.is_empty());
        let ts = ChatTranscripts::default();
        assert!(ts.per_session.is_empty());
    }

    #[test]
    fn session_timeline_default_has_no_usage_or_model() {
        let t = SessionTimeline::default();
        assert!(t.events.is_empty());
        assert!(t.latest_usage.is_none());
        assert!(t.model.is_none());
        assert!(t.context_window.is_none());
    }
}
