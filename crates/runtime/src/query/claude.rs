//! Cross-source queries for the Claude driver.
//!
//! Bridges four sources — ChatTabs + ChatPrefs (state-chat, user
//! decisions), ChatLifecycle (driver-claude, external fact), and
//! ChatStore (driver-session, external fact) — into the diff
//! actions consumed by `ClaudeDriver::execute`.
//!
//! These are plain functions, not `#[drv::memo]`s. Memoising them
//! requires the source types to implement `drv::ToStatic`, which
//! cascades into adding `drv::Input` derives across the driver
//! sources (`LifecycleState`, `ChatRow`, `SessionOverrides`, …)
//! and switching every field to `imbl::HashMap` / `imbl::Vector`.
//! That's a substantial refactor across four crates and is left
//! as follow-up — these queries are cheap (the working set is one
//! subprocess per open chat tab, typically 0-5) so the
//! per-tick cost is negligible.
//!
//! Per EXAMPLE-ARCH §Cross-driver composition, driver-claude
//! never imports driver-session; the runtime is the only place
//! that sees both.

use led_core::SessionUuid;
use led_driver_claude_core::{
    ChatLifecycle, ChatTranscripts, ClaudeAction, LifecycleState, SpawnMode, TimelineEvent,
};
use led_driver_session_core::{
    ChatMessageKind, ChatMessageRow, ChatRole, ChatRow, ChatStatus, ChatStore, SessionCmd,
};
use led_state_chat::{ChatPrefs, ChatTabs};
use std::collections::HashSet;

/// "Which sessions should have a live subprocess right now?"
///
/// Simple projection of [`ChatTabs::open`] into a set — every
/// open chat tab wants a subprocess.
pub fn desired_subprocesses(tabs: &ChatTabs) -> HashSet<SessionUuid> {
    tabs.open.iter().cloned().collect()
}

/// "What `ClaudeAction`s should the driver run this tick?"
///
/// Diff between desired (open tabs) and actual (`ChatLifecycle`).
/// Three classes:
///
/// - **Spawn**: tab open, lifecycle missing or `Exited`. `Fresh`
///   when the session isn't in `ChatStore.rows`; `Resume` when
///   it is. Spawning/Running/NotFound treated as "don't fire".
/// - **UserMessage**: session Running AND has a pending send —
///   one per session per tick (CLI stream-json input is one
///   user message per turn). Runtime pops the queue after the
///   action is computed.
/// - **Shutdown**: lifecycle Running/Spawning but tab no longer
///   open — clean despawn.
///
/// `NotFound` sessions are NEVER auto-respawned; the user has
/// to take an explicit action.
pub fn subprocess_action(
    tabs: &ChatTabs,
    lifecycle: &ChatLifecycle,
    prefs: &ChatPrefs,
    store: &ChatStore,
) -> Vec<ClaudeAction> {
    let mut actions: Vec<ClaudeAction> = Vec::new();

    // Don't act until the SQLite bulk-load is done — otherwise a
    // restored-tab scenario would Spawn { Fresh } for sessions
    // whose `claude_sessions` row is about to surface as Resume.
    if !store.loaded {
        return actions;
    }

    let desired: HashSet<&SessionUuid> = tabs.open.iter().collect();

    // 1) Spawn / re-spawn for desired sessions that aren't running.
    for uuid in tabs.open.iter() {
        let state = lifecycle.per_session.get(uuid);
        let needs_spawn = match state {
            None => true,
            Some(LifecycleState::Exited(_)) => true,
            Some(_) => false,
        };
        if needs_spawn {
            let mode = if let Some(row) = store.rows.get(uuid) {
                if row.status == ChatStatus::Orphaned {
                    // Don't try to resume orphans — the CLI's
                    // transcript for this id is gone.
                    continue;
                }
                SpawnMode::Resume
            } else {
                SpawnMode::Fresh
            };
            let effort = prefs
                .overrides
                .get(uuid)
                .and_then(|o| o.effort)
                .unwrap_or_default();
            let permission_mode = prefs
                .overrides
                .get(uuid)
                .and_then(|o| o.permission_mode)
                .unwrap_or_default();
            actions.push(ClaudeAction::Spawn {
                uuid: uuid.clone(),
                mode,
                effort,
                permission_mode,
            });
        }
    }

    // 2) Shutdown for sessions whose tab closed.
    for (uuid, state) in lifecycle.per_session.iter() {
        if desired.contains(uuid) {
            continue;
        }
        if matches!(state, LifecycleState::Spawning | LifecycleState::Running) {
            actions.push(ClaudeAction::Shutdown { uuid: uuid.clone() });
        }
    }

    // 3) Drain pending sends for Running sessions.
    for uuid in tabs.open.iter() {
        if !matches!(
            lifecycle.per_session.get(uuid),
            Some(LifecycleState::Running)
        ) {
            continue;
        }
        if let Some(q) = prefs.pending_sends.get(uuid)
            && let Some(text) = q.front()
        {
            actions.push(ClaudeAction::UserMessage {
                uuid: uuid.clone(),
                text: text.clone(),
            });
        }
    }

    actions
}

// ── Persistence diff ────────────────────────────────────────────────

/// "What SQLite writes does the chat persistence layer need to
/// catch up on?"
///
/// Diffs `ChatTranscripts` (live event log) against `ChatStore`
/// (persisted SQLite mirror) and emits one `SessionCmd` per
/// pending change:
///
/// - **InsertChatRow** for sessions not yet in `ChatStore.rows`.
///   `workspace_root` + `now_unix` come from the runtime (the
///   workspace is per-led-instance; time is from `ClockInput`
///   per EXAMPLE-ARCH §Time is a source field).
/// - **AppendChatMessage** for each timeline event beyond the
///   persisted high-water mark. `seq` allocated as
///   `max_seq + 1`, monotonic per session.
/// - **UpdateChatLastActive** whenever the latest event index
///   has advanced past the persisted `last_active_at` snapshot
///   we track via the row's `last_active_at` field (cheap
///   change-detection: if there are messages to append, also
///   bump the timestamp + usage).
///
/// The reducer applies the optimistic `apply_*` helpers on
/// `ChatStore` BEFORE these cmds are dispatched, so the next
/// tick's diff is `Noop`. Same execute-pattern discipline as
/// `subprocess_action`.
pub fn pending_persist_writes(
    transcripts: &ChatTranscripts,
    store: &ChatStore,
    workspace_root: &str,
    now_unix: i64,
) -> Vec<SessionCmd> {
    let mut cmds: Vec<SessionCmd> = Vec::new();
    if !store.loaded {
        return cmds;
    }

    for (uuid, timeline) in transcripts.per_session.iter() {
        // 1) Insert the row if we've never seen this session in
        // SQLite before. Carry the model + usage snapshot so the
        // row's metadata is useful from the first turn.
        let row_known = store.rows.contains_key(uuid);
        if !row_known {
            cmds.push(SessionCmd::InsertChatRow {
                row: ChatRow {
                    id: uuid.clone(),
                    workspace_root: workspace_root.to_string(),
                    short_label: None,
                    long_summary: None,
                    model: timeline.model.clone(),
                    effort: None,
                    permission_mode: None,
                    created_at: now_unix,
                    last_active_at: now_unix,
                    last_usage_json: timeline
                        .latest_usage
                        .map(|u| serde_json::to_string(&u).unwrap_or_default()),
                    status: ChatStatus::Active,
                },
            });
        }

        // 2) Append every timeline event past the persisted
        // high-water mark. The live transcript is append-only
        // since process start; the optimistic apply runs in
        // lockstep so events[k] corresponds to seq=k+1. Skip
        // already-persisted ones to avoid re-emitting after an
        // optimistic apply on the prior tick.
        let already_persisted = store.max_seq(uuid) as usize;
        for (idx, event) in timeline.events.iter().enumerate().skip(already_persisted) {
            let seq = idx as u64 + 1;
            let (role, kind, body_json) = encode_timeline_event(event);
            cmds.push(SessionCmd::AppendChatMessage {
                message: ChatMessageRow {
                    session: uuid.clone(),
                    seq,
                    role,
                    kind,
                    body_json,
                    usage_json: timeline
                        .latest_usage
                        .map(|u| serde_json::to_string(&u).unwrap_or_default()),
                    created_at: now_unix,
                },
            });
        }

        // 3) Bump last_active_at when there's been activity past
        // what's persisted. Skip if the row was just inserted —
        // InsertChatRow already carries last_active_at.
        if row_known
            && let Some(row) = store.rows.get(uuid)
            && row.last_active_at < now_unix
            && !timeline.events.is_empty()
        {
            cmds.push(SessionCmd::UpdateChatLastActive {
                id: uuid.clone(),
                at: now_unix,
                usage_json: timeline
                    .latest_usage
                    .map(|u| serde_json::to_string(&u).unwrap_or_default()),
            });
        }
    }

    cmds
}

fn encode_timeline_event(event: &TimelineEvent) -> (ChatRole, ChatMessageKind, String) {
    match event {
        TimelineEvent::UserSent { text } => (
            ChatRole::User,
            ChatMessageKind::Text,
            serde_json::to_string(text).unwrap_or_default(),
        ),
        TimelineEvent::AssistantText { text } => (
            ChatRole::Assistant,
            ChatMessageKind::Text,
            serde_json::to_string(text).unwrap_or_default(),
        ),
        TimelineEvent::AssistantToolUse {
            tool_use_id,
            name,
            input,
        } => (
            ChatRole::Assistant,
            ChatMessageKind::ToolUse,
            serde_json::to_string(&serde_json::json!({
                "tool_use_id": tool_use_id,
                "name": name,
                "input": input,
            }))
            .unwrap_or_default(),
        ),
        TimelineEvent::ToolResult {
            tool_use_id,
            content,
        } => (
            ChatRole::Tool,
            ChatMessageKind::ToolResult,
            serde_json::to_string(&serde_json::json!({
                "tool_use_id": tool_use_id,
                "content": content,
            }))
            .unwrap_or_default(),
        ),
        TimelineEvent::TurnComplete {
            usage,
            cost_usd,
            num_turns,
        } => (
            ChatRole::System,
            ChatMessageKind::TurnComplete,
            serde_json::to_string(&serde_json::json!({
                "usage": usage,
                "cost_usd": cost_usd,
                "num_turns": num_turns,
            }))
            .unwrap_or_default(),
        ),
        TimelineEvent::TurnError { errors } => (
            ChatRole::System,
            ChatMessageKind::TurnError,
            serde_json::to_string(&serde_json::json!({ "errors": errors }))
                .unwrap_or_default(),
        ),
    }
}

// ── Auto-label gate ─────────────────────────────────────────────────

/// Minimum user/assistant rounds before we ask the model for a
/// short label. Two rounds gives the model enough context to
/// summarise meaningfully without burning quota on every fresh
/// chat that the user might abandon.
pub const AUTO_LABEL_MIN_ROUNDS: usize = 2;

/// "Which session needs an auto-label query fired NOW?"
///
/// Returns `Some(uuid)` for the first session that:
/// - Exists in `ChatStore.rows` (already persisted, status Active)
/// - Has no `short_label` set yet
/// - Has at least [`AUTO_LABEL_MIN_ROUNDS`] complete
///   user+assistant rounds in its transcript
///
/// The runtime is responsible for issuing the synthetic
/// label-prompt UserMessage (via `ChatPrefs::queue_send`) AND
/// for tracking that one's been sent, so the next tick doesn't
/// see the same predicate as still-true. The "tracking" is
/// implicit: once the assistant responds, dispatch parses the
/// labels and writes `UpdateChatLabels`; on the following tick
/// the short_label is set and this predicate returns None.
///
/// Returns one at a time per tick — the queue handler batches
/// sends naturally; staggering avoids a thundering herd on
/// first-ever load.
pub fn needs_auto_label(
    transcripts: &ChatTranscripts,
    store: &ChatStore,
) -> Option<SessionUuid> {
    if !store.loaded {
        return None;
    }
    for (uuid, row) in store.rows.iter() {
        if row.short_label.is_some() || row.status != ChatStatus::Active {
            continue;
        }
        let Some(timeline) = transcripts.per_session.get(uuid) else {
            continue;
        };
        let rounds = count_complete_rounds(&timeline.events);
        if rounds >= AUTO_LABEL_MIN_ROUNDS {
            return Some(uuid.clone());
        }
    }
    None
}

/// Count `UserSent` → `AssistantText` pairs in order. Tool turns
/// don't count toward "rounds" — they're sub-steps of the
/// assistant's response, not separate turns.
fn count_complete_rounds(events: &[TimelineEvent]) -> usize {
    let mut rounds = 0;
    let mut saw_user = false;
    for ev in events {
        match ev {
            TimelineEvent::UserSent { .. } => saw_user = true,
            TimelineEvent::AssistantText { .. } if saw_user => {
                rounds += 1;
                saw_user = false;
            }
            _ => {}
        }
    }
    rounds
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_core::{Effort, PermissionMode};
    use led_driver_claude_core::ExitInfo;
    use led_driver_session_core::ChatRow;
    use led_state_chat::SessionOverrides;

    fn u(s: &str) -> SessionUuid {
        SessionUuid::new(s)
    }

    fn row(id: &str) -> ChatRow {
        ChatRow {
            id: u(id),
            workspace_root: "/ws".into(),
            short_label: None,
            long_summary: None,
            model: None,
            effort: None,
            permission_mode: None,
            created_at: 0,
            last_active_at: 0,
            last_usage_json: None,
            status: ChatStatus::Active,
        }
    }

    fn ready_store(rows: Vec<ChatRow>) -> ChatStore {
        let mut s = ChatStore::default();
        for r in rows {
            s.rows.insert(r.id.clone(), r);
        }
        s.loaded = true;
        s
    }

    #[test]
    fn no_action_until_store_loaded() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let actions = subprocess_action(
            &tabs,
            &ChatLifecycle::default(),
            &ChatPrefs::default(),
            &ChatStore::default(),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn open_tab_with_no_lifecycle_emits_spawn_fresh_when_not_in_store() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let actions = subprocess_action(
            &tabs,
            &ChatLifecycle::default(),
            &ChatPrefs::default(),
            &ready_store(vec![]),
        );
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ClaudeAction::Spawn { uuid, mode, .. } => {
                assert_eq!(uuid, &u("a"));
                assert!(matches!(mode, SpawnMode::Fresh));
            }
            other => panic!("expected Spawn Fresh, got {other:?}"),
        }
    }

    #[test]
    fn open_tab_with_existing_store_row_emits_spawn_resume() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let actions = subprocess_action(
            &tabs,
            &ChatLifecycle::default(),
            &ChatPrefs::default(),
            &ready_store(vec![row("a")]),
        );
        assert!(matches!(
            &actions[0],
            ClaudeAction::Spawn { mode: SpawnMode::Resume, .. }
        ));
    }

    #[test]
    fn orphaned_store_row_does_not_spawn() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let mut r = row("a");
        r.status = ChatStatus::Orphaned;
        let actions = subprocess_action(
            &tabs,
            &ChatLifecycle::default(),
            &ChatPrefs::default(),
            &ready_store(vec![r]),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn spawning_session_does_not_re_spawn() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Spawning);
        let actions = subprocess_action(&tabs, &life, &ChatPrefs::default(), &ready_store(vec![]));
        assert!(actions.is_empty());
    }

    #[test]
    fn not_found_session_does_not_respawn() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::NotFound);
        let actions = subprocess_action(&tabs, &life, &ChatPrefs::default(), &ready_store(vec![]));
        assert!(actions.is_empty());
    }

    #[test]
    fn exited_session_respawns_on_next_tick() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let mut life = ChatLifecycle::default();
        life.per_session
            .insert(u("a"), LifecycleState::Exited(ExitInfo::default()));
        let actions = subprocess_action(&tabs, &life, &ChatPrefs::default(), &ready_store(vec![]));
        assert!(matches!(&actions[0], ClaudeAction::Spawn { .. }));
    }

    #[test]
    fn closed_tab_with_running_lifecycle_emits_shutdown() {
        let tabs = ChatTabs::default();
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Running);
        let actions = subprocess_action(&tabs, &life, &ChatPrefs::default(), &ready_store(vec![]));
        assert!(matches!(
            &actions[0],
            ClaudeAction::Shutdown { uuid } if uuid == &u("a")
        ));
    }

    #[test]
    fn running_session_with_pending_send_emits_user_message() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Running);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hello".into());

        let actions = subprocess_action(&tabs, &life, &prefs, &ready_store(vec![row("a")]));
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ClaudeAction::UserMessage { uuid, text } => {
                assert_eq!(uuid, &u("a"));
                assert_eq!(text, "hello");
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }

    #[test]
    fn pending_send_on_spawning_session_does_not_emit_user_message() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Spawning);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hello".into());

        let actions = subprocess_action(&tabs, &life, &prefs, &ready_store(vec![row("a")]));
        assert!(actions.is_empty());
    }

    #[test]
    fn overrides_flow_into_spawn_action() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        let mut prefs = ChatPrefs::default();
        prefs.overrides.insert(
            u("a"),
            SessionOverrides {
                effort: Some(Effort::Low),
                permission_mode: Some(PermissionMode::Plan),
            },
        );
        let actions = subprocess_action(
            &tabs,
            &ChatLifecycle::default(),
            &prefs,
            &ready_store(vec![]),
        );
        match &actions[0] {
            ClaudeAction::Spawn {
                effort,
                permission_mode,
                ..
            } => {
                assert_eq!(*effort, Effort::Low);
                assert_eq!(*permission_mode, PermissionMode::Plan);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn desired_subprocesses_is_set_of_open_tab_uuids() {
        let mut tabs = ChatTabs::default();
        tabs.open_or_focus(u("a"));
        tabs.open_or_focus(u("b"));
        let set = desired_subprocesses(&tabs);
        assert!(set.contains(&u("a")));
        assert!(set.contains(&u("b")));
        assert_eq!(set.len(), 2);
    }

    // ── pending_persist_writes ──────────────────────────────────────

    fn timeline_with(events: Vec<TimelineEvent>) -> led_driver_claude_core::SessionTimeline {
        led_driver_claude_core::SessionTimeline {
            events,
            ..Default::default()
        }
    }

    #[test]
    fn persist_writes_empty_when_store_not_loaded() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(u("a"), timeline_with(vec![]));
        let cmds = pending_persist_writes(&ts, &ChatStore::default(), "/ws", 100);
        assert!(cmds.is_empty());
    }

    #[test]
    fn persist_writes_insert_row_for_unknown_session() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![TimelineEvent::UserSent {
                text: "hi".into(),
            }]),
        );
        let cmds = pending_persist_writes(&ts, &ready_store(vec![]), "/ws", 100);
        // Insert + Append (and no UpdateLastActive since row is brand new).
        assert!(matches!(&cmds[0], SessionCmd::InsertChatRow { row } if row.id == u("a")));
        assert!(matches!(&cmds[1], SessionCmd::AppendChatMessage { message }
                              if message.seq == 1 && message.session == u("a")));
        assert_eq!(cmds.len(), 2);
    }

    #[test]
    fn persist_writes_append_picks_up_where_store_left_off() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![
                TimelineEvent::UserSent { text: "1".into() },
                TimelineEvent::AssistantText { text: "2".into() },
                TimelineEvent::UserSent { text: "3".into() },
            ]),
        );
        // Persisted: rows {a}, messages a/[seq=1]. So the live
        // timeline has 3 events but seq 1 is already persisted —
        // we should emit appends for seq=2 and seq=3 only.
        let mut store = ChatStore::default();
        store.rows.insert(u("a"), row("a"));
        store.messages.insert(
            u("a"),
            vec![ChatMessageRow {
                session: u("a"),
                seq: 1,
                role: ChatRole::User,
                kind: ChatMessageKind::Text,
                body_json: r#""1""#.into(),
                usage_json: None,
                created_at: 0,
            }],
        );
        store.loaded = true;

        let cmds = pending_persist_writes(&ts, &store, "/ws", 100);
        let appends: Vec<&ChatMessageRow> = cmds
            .iter()
            .filter_map(|c| match c {
                SessionCmd::AppendChatMessage { message } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(appends.len(), 2);
        assert_eq!(appends[0].seq, 2);
        assert_eq!(appends[1].seq, 3);
    }

    #[test]
    fn persist_writes_emits_update_last_active_for_known_row() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![TimelineEvent::UserSent {
                text: "hi".into(),
            }]),
        );
        // Row exists with last_active < now.
        let mut store = ChatStore::default();
        let mut r = row("a");
        r.last_active_at = 50;
        store.rows.insert(u("a"), r);
        store.loaded = true;

        let cmds = pending_persist_writes(&ts, &store, "/ws", 100);
        assert!(cmds.iter().any(|c| matches!(
            c,
            SessionCmd::UpdateChatLastActive { id, at: 100, .. } if id == &u("a")
        )));
    }

    // ── needs_auto_label ────────────────────────────────────────────

    #[test]
    fn needs_label_returns_none_if_store_not_loaded() {
        let ts = ChatTranscripts::default();
        assert!(needs_auto_label(&ts, &ChatStore::default()).is_none());
    }

    #[test]
    fn needs_label_fires_after_two_complete_rounds() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![
                TimelineEvent::UserSent { text: "1".into() },
                TimelineEvent::AssistantText { text: "1".into() },
                TimelineEvent::UserSent { text: "2".into() },
                TimelineEvent::AssistantText { text: "2".into() },
            ]),
        );
        assert_eq!(
            needs_auto_label(&ts, &ready_store(vec![row("a")])),
            Some(u("a"))
        );
    }

    #[test]
    fn needs_label_not_yet_after_one_round() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![
                TimelineEvent::UserSent { text: "1".into() },
                TimelineEvent::AssistantText { text: "1".into() },
            ]),
        );
        assert!(needs_auto_label(&ts, &ready_store(vec![row("a")])).is_none());
    }

    #[test]
    fn needs_label_skips_session_with_short_label_already_set() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![
                TimelineEvent::UserSent { text: "1".into() },
                TimelineEvent::AssistantText { text: "1".into() },
                TimelineEvent::UserSent { text: "2".into() },
                TimelineEvent::AssistantText { text: "2".into() },
            ]),
        );
        let mut r = row("a");
        r.short_label = Some("already".into());
        assert!(needs_auto_label(&ts, &ready_store(vec![r])).is_none());
    }

    #[test]
    fn needs_label_skips_orphaned() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![
                TimelineEvent::UserSent { text: "1".into() },
                TimelineEvent::AssistantText { text: "1".into() },
                TimelineEvent::UserSent { text: "2".into() },
                TimelineEvent::AssistantText { text: "2".into() },
            ]),
        );
        let mut r = row("a");
        r.status = ChatStatus::Orphaned;
        assert!(needs_auto_label(&ts, &ready_store(vec![r])).is_none());
    }
}
