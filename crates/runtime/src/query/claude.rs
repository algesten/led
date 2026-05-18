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
use led_driver_claude_core::{ChatLifecycle, ClaudeAction, LifecycleState, SpawnMode};
use led_driver_session_core::{ChatStatus, ChatStore};
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
}
