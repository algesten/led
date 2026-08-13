//! Cross-source queries for the Claude driver.
//!
//! Bridges four sources — ChatSessions + ChatPrefs (state-chat,
//! user decisions), ChatLifecycle + ChatTranscripts (driver-claude,
//! external fact), and ChatStore (driver-session, external fact) —
//! into the diff actions consumed by `ClaudeDriver::execute` and the
//! rope-splice + persistence actions applied in the runtime's
//! claude_phase.
//!
//! These are plain functions, not `#[drv::memo]`s. Memoising them
//! requires the source types to implement `drv::ToStatic`, which
//! cascades into adding `drv::Input` derives across the driver
//! sources (`LifecycleState`, `ChatRow`, `SessionOverrides`, …)
//! and switching every field to `imbl::HashMap` / `imbl::Vector`.
//! That's a substantial refactor across four crates and is left as
//! follow-up — these queries are cheap (the working set is one
//! subprocess per open chat tab, typically 0-5) so the per-tick
//! cost is negligible.
//!
//! Per EXAMPLE-ARCH §Cross-driver composition, driver-claude never
//! imports driver-session; the runtime is the only place that sees
//! both.

use std::collections::HashSet;

use led_core::{CanonPath, SessionUuid};
use led_driver_claude_core::{
    ChatLifecycle, ChatTranscripts, ClaudeAction, LifecycleState, SpawnMode, TimelineEvent,
};
use led_driver_session_core::{
    ChatMessageKind, ChatMessageRow, ChatRole, ChatRow, ChatStatus, ChatStore, SessionCmd,
};
use led_state_chat::{ChatPrefs, ChatSessions};
use led_state_tabs::Tab;

/// "What `ClaudeAction`s should the driver run this tick?"
///
/// Diff between desired (open tabs that bind to a chat session) and
/// actual (`ChatLifecycle`). Three classes:
///
/// - **Spawn**: chat tab open, lifecycle missing or `Exited`.
///   `Fresh` when the session isn't in `ChatStore.rows`; `Resume`
///   when it is. Spawning/Running/NotFound treated as "don't fire".
/// - **UserMessage**: session Spawning/Running AND has a pending
///   send — one per session per tick (CLI stream-json input is one
///   user message per turn). Runtime pops the queue after the
///   action is computed.
/// - **Shutdown**: lifecycle Running/Spawning but tab no longer
///   open — clean despawn.
///
/// `NotFound` sessions are NEVER auto-respawned; the user has to
/// take an explicit action.
pub fn subprocess_action(
    tabs: &imbl::Vector<Tab>,
    sessions: &ChatSessions,
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

    // Desired set: every UUID bound to a currently-open tab.
    let mut desired: HashSet<SessionUuid> = HashSet::new();
    for tab in tabs.iter() {
        if let Some(state) = sessions.get(&tab.path) {
            desired.insert(state.session.clone());
        }
    }

    // 1) Spawn / re-spawn for desired sessions that aren't running.
    //    Only fire if the user has actually queued a message — the
    //    initial submission is what triggers the subprocess (otherwise
    //    a freshly-restored chat with empty history would spin up a
    //    process with nothing to say).
    for tab in tabs.iter() {
        let Some(s) = sessions.get(&tab.path) else {
            continue;
        };
        let uuid = &s.session;
        let life_state = lifecycle.per_session.get(uuid);
        let needs_spawn = matches!(life_state, None | Some(LifecycleState::Exited(_)));
        if !needs_spawn {
            continue;
        }
        if !prefs.has_pending(uuid) {
            continue;
        }
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
        let effort = prefs.effort_for(uuid);
        let permission_mode = prefs.permission_mode_for(uuid);
        actions.push(ClaudeAction::Spawn {
            uuid: uuid.clone(),
            mode,
            effort,
            permission_mode,
        });
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

    // 3) Drain pending sends for Spawning/Running sessions.
    //
    //    The CLI doesn't emit `Init` until it reads the first
    //    stdin line — so "Spawning" is the earliest we can write,
    //    and gating on Running would deadlock (Init waits for a
    //    message, the memo waits for Init). The manager-thread's
    //    writer is a buffered std::mpsc channel; messages queue
    //    against the spawning child until its stdin is ready.
    for tab in tabs.iter() {
        let Some(s) = sessions.get(&tab.path) else {
            continue;
        };
        let uuid = &s.session;
        if !matches!(
            lifecycle.per_session.get(uuid),
            Some(LifecycleState::Spawning | LifecycleState::Running)
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

/// "What SQLite writes does the chat persistence layer need to catch
/// up on?"
///
/// Diffs `ChatTranscripts` (live event log) against `ChatStore`
/// (persisted SQLite mirror) and emits one `SessionCmd` per pending
/// change.
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

        // 2) Append every timeline event past the persisted high-
        // water mark. The live transcript is append-only since
        // process start; the optimistic apply runs in lockstep so
        // events[k] corresponds to seq=k+1. Skip already-persisted
        // ones to avoid re-emitting after an optimistic apply on
        // the prior tick.
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

/// Minimum user/assistant rounds before we ask the model for a short
/// label.
pub const AUTO_LABEL_MIN_ROUNDS: usize = 2;

/// "Which session needs an auto-label query fired NOW?"
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

/// Bridge from lifecycle-failure-states (driver-claude) to persisted
/// status (driver-session).
pub fn orphan_status_actions(
    lifecycle: &ChatLifecycle,
    store: &ChatStore,
) -> Vec<SessionCmd> {
    if !store.loaded {
        return Vec::new();
    }
    let mut cmds = Vec::new();
    for (uuid, state) in lifecycle.per_session.iter() {
        if !matches!(state, LifecycleState::NotFound) {
            continue;
        }
        let Some(row) = store.rows.get(uuid) else {
            continue;
        };
        if row.status != ChatStatus::Orphaned {
            cmds.push(SessionCmd::UpdateChatStatus {
                id: uuid.clone(),
                status: ChatStatus::Orphaned,
            });
        }
    }
    cmds
}

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

// ── Chat rope splices ───────────────────────────────────────────────

/// One desired rope splice for a chat-buffer. Applied by the chat
/// phase's `execute` pass: insert `text` at `char_offset`, then set
/// `state.last_synced_event = source_event_idx + 1` so the watermark
/// always tracks the source event index (not the splice ordinal —
/// fixes the duplication bug where two splices per tick double-bumped
/// the watermark).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSplice {
    pub path: CanonPath,
    pub char_offset: usize,
    pub text: String,
    /// Index into the source `ChatTranscripts.per_session[uuid].
    /// events` vec that produced this splice. The applier sets
    /// `last_synced_event = source_event_idx + 1` so the watermark
    /// matches the source, not the splice batch ordinal.
    pub source_event_idx: usize,
}

/// "What rope splices should the chat phase apply this tick?"
///
/// Walks each registered chat session, looks at the events past its
/// `last_synced_event` watermark, and emits one [`ChatSplice`] per
/// renderable event with the cumulative char-offset for in-order
/// insertion.
pub fn pending_chat_splices(
    transcripts: &ChatTranscripts,
    sessions: &ChatSessions,
) -> Vec<ChatSplice> {
    let mut out: Vec<ChatSplice> = Vec::new();
    for (path, state) in sessions.by_path.iter() {
        let Some(timeline) = transcripts.per_session.get(&state.session) else {
            continue;
        };
        let mut cumulative_offset = state.response_anchor;
        for (idx, event) in timeline.events.iter().enumerate().skip(state.last_synced_event) {
            let Some(text) = render_event_for_rope(event) else {
                continue;
            };
            let inserted = text.chars().count();
            out.push(ChatSplice {
                path: path.clone(),
                char_offset: cumulative_offset,
                text,
                source_event_idx: idx,
            });
            cumulative_offset = cumulative_offset.saturating_add(inserted);
        }
    }
    out
}

/// Render a single timeline event to the text body that should be
/// spliced into the chat buffer. Returns `None` for events that
/// don't produce rope content (e.g. `UserSent` — the user typed
/// those into the rope directly; `TurnComplete` — a metadata event).
fn render_event_for_rope(event: &TimelineEvent) -> Option<String> {
    match event {
        TimelineEvent::AssistantText { text } => Some(format!("{text}\n\n")),
        TimelineEvent::AssistantToolUse { name, .. } => Some(format!("[tool: {name}]\n\n")),
        TimelineEvent::ToolResult { .. } => Some("[tool result]\n\n".to_string()),
        TimelineEvent::TurnError { errors } => {
            let first = errors.first().cloned().unwrap_or_default();
            Some(format!("[error: {first}]\n\n"))
        }
        TimelineEvent::UserSent { .. } | TimelineEvent::TurnComplete { .. } => None,
    }
}

// ── Picker / tab view / context % ───────────────────────────────────

/// One row of the "Find chat:" picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    pub session: SessionUuid,
    pub short_label: Option<String>,
    pub long_summary: Option<String>,
    pub age_seconds: i64,
    pub orphaned: bool,
}

/// "Sorted picker list for the active workspace's chats."
pub fn chat_picker_items(store: &ChatStore, now_unix: i64) -> Vec<PickerItem> {
    if !store.loaded {
        return Vec::new();
    }
    let mut items: Vec<PickerItem> = store
        .rows
        .values()
        .map(|row| PickerItem {
            session: row.id.clone(),
            short_label: row.short_label.clone(),
            long_summary: row.long_summary.clone(),
            age_seconds: (now_unix - row.last_active_at).max(0),
            orphaned: row.status == ChatStatus::Orphaned,
        })
        .collect();
    items.sort_by_key(|i| i.age_seconds);
    items
}

/// Render-model for one chat tab — what the scrollback area should
/// display.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatViewModel {
    pub session: SessionUuid,
    pub short_label: Option<String>,
    pub model: Option<String>,
    pub messages: Vec<ChatViewMessage>,
    pub context: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatViewMessage {
    pub role: ChatRole,
    pub kind: ChatMessageKind,
    pub body_json: String,
}

/// "Render model for the focused chat tab."
pub fn chat_tab_view(
    transcripts: &ChatTranscripts,
    store: &ChatStore,
    session: &SessionUuid,
) -> Option<ChatViewModel> {
    let row = store.rows.get(session);
    let timeline = transcripts.per_session.get(session);
    if row.is_none() && timeline.is_none() {
        return None;
    }
    let short_label = row.and_then(|r| r.short_label.clone());
    let model = row
        .and_then(|r| r.model.clone())
        .or_else(|| timeline.and_then(|t| t.model.clone()));

    let mut messages: Vec<ChatViewMessage> = Vec::new();
    // Persisted prefix.
    if let Some(stored) = store.messages.get(session) {
        for m in stored.iter() {
            messages.push(ChatViewMessage {
                role: m.role,
                kind: m.kind,
                body_json: m.body_json.clone(),
            });
        }
    }
    // Live tail beyond max_seq.
    if let Some(t) = timeline {
        let already = store.max_seq(session) as usize;
        for ev in t.events.iter().skip(already) {
            let (role, kind, body_json) = encode_timeline_event(ev);
            messages.push(ChatViewMessage {
                role,
                kind,
                body_json,
            });
        }
    }

    let context = context_pct_internal(transcripts, store, session);
    Some(ChatViewModel {
        session: session.clone(),
        short_label,
        model,
        messages,
        context,
    })
}

/// "Context-window fill for the focused chat as a 0-100 percent, if
/// both used and window are known."
pub fn context_pct(
    transcripts: &ChatTranscripts,
    store: &ChatStore,
    session: &SessionUuid,
) -> Option<u8> {
    let (used, window) = context_pct_internal(transcripts, store, session)?;
    if window == 0 {
        return None;
    }
    let pct = ((used as u64 * 100) / window as u64).min(100) as u8;
    Some(pct)
}

fn context_pct_internal(
    transcripts: &ChatTranscripts,
    store: &ChatStore,
    session: &SessionUuid,
) -> Option<(u32, u32)> {
    let timeline = transcripts.per_session.get(session)?;
    let usage = timeline.latest_usage?;
    let window = timeline.context_window.or_else(|| {
        store
            .rows
            .get(session)
            .and_then(|r| r.model.as_deref())
            .map(default_context_window)
    })?;
    Some((usage.total_prompt(), window))
}

fn default_context_window(model: &str) -> u32 {
    if model.contains("[1m]") {
        1_000_000
    } else {
        200_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imbl::vector;
    use led_core::{Effort, PermissionMode, UserPath};
    use led_driver_claude_core::ExitInfo;
    use led_driver_session_core::ChatRow;
    use led_state_chat::SessionOverrides;
    use led_state_tabs::Tab;

    fn u(s: &str) -> SessionUuid {
        SessionUuid::new(s)
    }

    fn p(s: &str) -> CanonPath {
        UserPath::new(s).canonicalize()
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

    fn tab_at(path: &str, id: u64) -> Tab {
        Tab {
            id: led_state_tabs::TabId(id),
            path: p(path),
            ..Default::default()
        }
    }

    fn sessions_with(entries: &[(&str, &str)]) -> ChatSessions {
        let mut s = ChatSessions::default();
        for (path, uuid) in entries {
            s.insert(p(path), u(uuid));
        }
        s
    }

    #[test]
    fn no_action_until_store_loaded() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let actions = subprocess_action(
            &tabs,
            &sessions,
            &ChatLifecycle::default(),
            &ChatPrefs::default(),
            &ChatStore::default(),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn spawn_only_fires_when_pending_send_is_queued() {
        // Without a pending send, we don't spawn the subprocess.
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let actions = subprocess_action(
            &tabs,
            &sessions,
            &ChatLifecycle::default(),
            &ChatPrefs::default(),
            &ready_store(vec![]),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn open_tab_with_no_lifecycle_emits_spawn_fresh_when_pending_and_not_in_store() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hi".into());
        let actions = subprocess_action(
            &tabs,
            &sessions,
            &ChatLifecycle::default(),
            &prefs,
            &ready_store(vec![]),
        );
        assert!(matches!(
            actions.first(),
            Some(ClaudeAction::Spawn { mode: SpawnMode::Fresh, .. })
        ));
    }

    #[test]
    fn open_tab_with_existing_store_row_emits_spawn_resume() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hi".into());
        let actions = subprocess_action(
            &tabs,
            &sessions,
            &ChatLifecycle::default(),
            &prefs,
            &ready_store(vec![row("a")]),
        );
        assert!(matches!(
            actions.first(),
            Some(ClaudeAction::Spawn { mode: SpawnMode::Resume, .. })
        ));
    }

    #[test]
    fn orphaned_store_row_does_not_spawn() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hi".into());
        let mut r = row("a");
        r.status = ChatStatus::Orphaned;
        let actions = subprocess_action(
            &tabs,
            &sessions,
            &ChatLifecycle::default(),
            &prefs,
            &ready_store(vec![r]),
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn spawning_session_does_not_re_spawn() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hi".into());
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Spawning);
        let actions = subprocess_action(&tabs, &sessions, &life, &prefs, &ready_store(vec![]));
        // Spawning gate: no Spawn re-fire, but the UserMessage is now
        // legal because Spawning is an accepted send state.
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], ClaudeAction::UserMessage { .. }));
    }

    #[test]
    fn not_found_session_does_not_respawn() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hi".into());
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::NotFound);
        let actions = subprocess_action(&tabs, &sessions, &life, &prefs, &ready_store(vec![]));
        assert!(actions.is_empty());
    }

    #[test]
    fn exited_session_respawns_when_pending() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hi".into());
        let mut life = ChatLifecycle::default();
        life.per_session
            .insert(u("a"), LifecycleState::Exited(ExitInfo::default()));
        let actions = subprocess_action(&tabs, &sessions, &life, &prefs, &ready_store(vec![]));
        assert!(actions.iter().any(|a| matches!(a, ClaudeAction::Spawn { .. })));
    }

    #[test]
    fn closed_tab_with_running_lifecycle_emits_shutdown() {
        let tabs: imbl::Vector<Tab> = imbl::Vector::new();
        let sessions = ChatSessions::default();
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Running);
        let actions = subprocess_action(
            &tabs,
            &sessions,
            &life,
            &ChatPrefs::default(),
            &ready_store(vec![]),
        );
        assert!(matches!(
            actions.first(),
            Some(ClaudeAction::Shutdown { uuid }) if uuid == &u("a")
        ));
    }

    #[test]
    fn running_session_with_pending_send_emits_user_message() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Running);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hello".into());

        let actions = subprocess_action(&tabs, &sessions, &life, &prefs, &ready_store(vec![row("a")]));
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
    fn pending_send_on_spawning_session_emits_user_message() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Spawning);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hello".into());

        let actions = subprocess_action(&tabs, &sessions, &life, &prefs, &ready_store(vec![row("a")]));
        assert!(matches!(
            actions.as_slice(),
            [ClaudeAction::UserMessage { uuid, text }]
                if uuid == &u("a") && text == "hello"
        ));
    }

    #[test]
    fn pending_send_on_exiting_session_is_not_sent() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::Exiting);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hello".into());

        let actions = subprocess_action(&tabs, &sessions, &life, &prefs, &ready_store(vec![row("a")]));
        assert!(actions.is_empty());
    }

    #[test]
    fn overrides_flow_into_spawn_action() {
        let tabs = vector![tab_at("/c/a.chat", 1)];
        let sessions = sessions_with(&[("/c/a.chat", "a")]);
        let mut prefs = ChatPrefs::default();
        prefs.queue_send(u("a"), "hi".into());
        prefs.overrides.insert(
            u("a"),
            SessionOverrides {
                effort: Some(Effort::Low),
                permission_mode: Some(PermissionMode::Plan),
            },
        );
        let actions = subprocess_action(
            &tabs,
            &sessions,
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
        assert!(matches!(&cmds[0], SessionCmd::InsertChatRow { row } if row.id == u("a")));
        assert!(matches!(&cmds[1], SessionCmd::AppendChatMessage { message }
                              if message.seq == 1 && message.session == u("a")));
        assert_eq!(cmds.len(), 2);
    }

    // ── pending_chat_splices ─────────────────────────────────────

    #[test]
    fn chat_splice_skips_user_and_complete_events_renders_assistant() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![
                TimelineEvent::UserSent { text: "q".into() }, // skipped
                TimelineEvent::AssistantText { text: "hi".into() }, // renders
                TimelineEvent::TurnComplete {
                    usage: led_driver_claude_core::Usage::default(),
                    cost_usd: 0.0,
                    num_turns: 1,
                }, // skipped
                TimelineEvent::AssistantText { text: "again".into() }, // renders
            ]),
        );
        let mut sessions = ChatSessions::default();
        sessions.insert(p("/c/a.chat"), u("a"));
        // Park anchor at 0 for a deterministic test.
        let splices = pending_chat_splices(&ts, &sessions);
        assert_eq!(splices.len(), 2);
        assert_eq!(splices[0].source_event_idx, 1);
        assert_eq!(splices[1].source_event_idx, 3);
        // Second splice's char_offset = first.text.chars().count().
        let first_len = splices[0].text.chars().count();
        assert_eq!(splices[1].char_offset, first_len);
    }

    #[test]
    fn chat_splice_honours_last_synced_event_watermark() {
        let mut ts = ChatTranscripts::default();
        ts.per_session.insert(
            u("a"),
            timeline_with(vec![
                TimelineEvent::AssistantText { text: "old".into() },
                TimelineEvent::AssistantText { text: "new".into() },
            ]),
        );
        let mut sessions = ChatSessions::default();
        sessions.insert(p("/c/a.chat"), u("a"));
        // Pretend the first event is already synced.
        sessions.get_mut(&p("/c/a.chat")).unwrap().last_synced_event = 1;

        let splices = pending_chat_splices(&ts, &sessions);
        assert_eq!(splices.len(), 1);
        assert_eq!(splices[0].source_event_idx, 1);
        assert!(splices[0].text.starts_with("new"));
    }

    // ── orphan + auto-label ─────────────────────────────────────

    #[test]
    fn orphan_actions_emit_update_when_notfound_but_active() {
        let mut life = ChatLifecycle::default();
        life.per_session.insert(u("a"), LifecycleState::NotFound);
        let cmds = orphan_status_actions(&life, &ready_store(vec![row("a")]));
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            &cmds[0],
            SessionCmd::UpdateChatStatus { id, status: ChatStatus::Orphaned } if id == &u("a")
        ));
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
}
