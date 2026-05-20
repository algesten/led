//! Claude driver phases — ingest, query → execute.
//!
//! Follows EXAMPLE-ARCH's three-phase loop discipline:
//!
//! - [`ingest`] writes external facts INTO sources
//!   (`ChatLifecycle`, `ChatTranscripts`) by draining the driver's
//!   event mpsc. No reads.
//! - [`execute`] reads sources via pure memo queries
//!   (`subprocess_action`, `pending_persist_writes`,
//!   `orphan_status_actions`, `pending_chat_splices`) and applies
//!   the diff actions — `ClaudeAction`s to the driver, `SessionCmd`s
//!   to the session driver, rope splices to `BufferEdits`. Every
//!   action also writes the matching intent back into a source
//!   synchronously so the next iteration's diff returns Noop (the
//!   execute pattern).

use std::sync::Arc;

use led_core::{Effort, PermissionMode, SessionUuid};
use led_driver_session_core::SessionCmd;
use led_state_chat::SessionOverrides;

use crate::Sources;
use crate::phases::TickEnv;
use crate::query::claude::{
    ChatSplice, orphan_status_actions, pending_chat_splices, pending_persist_writes,
    subprocess_action,
};

/// Drain pending `ClaudeEvent`s into the driver-owned sources.
pub(crate) fn ingest(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        chat_lifecycle,
        chat_transcripts,
        ..
    } = sources;
    env.drivers
        .claude
        .process(chat_lifecycle, chat_transcripts);
}

/// Compute every chat action for this tick from the source state,
/// then apply them. EXAMPLE-ARCH §"The execute pattern": each
/// action mutates the relevant source synchronously (lifecycle,
/// rope, ChatStore mirror) before the cmd ships, so the next
/// iteration's memo returns Noop until external completions land.
pub(crate) fn execute(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        tabs,
        edits,
        chat_lifecycle,
        chat_transcripts,
        chat_store,
        chat_sessions,
        chat_prefs,
        session_driver,
        fs,
        clock,
        ..
    } = sources;

    // ── Auto-register restored chat tabs ────────────────────────
    // `ChatSessions` is in-memory only; a `*.chat` tab restored
    // from the session DB on startup has no entry. Walk `Tabs.open`,
    // recognise chat-shaped paths (filename `<uuid>.chat`),
    // re-register them with `submit_offset` parked at the current
    // rope end so everything already in the buffer counts as
    // already-submitted history (no spurious gutter markers, no
    // re-send of past turns). Also apply env-var overrides so
    // `LED_CHAT_EFFORT=low` works on restored sessions, not just
    // freshly-created ones.
    reseed_restored_chats(tabs, edits, chat_sessions, chat_prefs);

    // ── Subprocess actions ──────────────────────────────────────
    let actions = subprocess_action(
        &tabs.open,
        chat_sessions,
        chat_lifecycle,
        chat_prefs,
        chat_store,
    );
    for action in &actions {
        if let led_driver_claude_core::ClaudeAction::UserMessage { uuid, .. } = action {
            chat_prefs.pop_pending(uuid);
        }
    }
    env.drivers
        .claude
        .execute(actions.iter(), chat_lifecycle, chat_transcripts);

    // ── Persistence writes ──────────────────────────────────────
    let workspace_root: String = fs
        .root
        .as_ref()
        .map(|c| c.display().to_string())
        .unwrap_or_default();
    let now_unix: i64 = clock
        .wall_now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let persist_cmds =
        pending_persist_writes(chat_transcripts, chat_store, &workspace_root, now_unix);
    apply_session_mirrors(chat_store, &persist_cmds);
    if !persist_cmds.is_empty() {
        env.drivers
            .session
            .execute(persist_cmds.iter(), session_driver);
    }

    let orphan_cmds = orphan_status_actions(chat_lifecycle, chat_store);
    apply_session_mirrors(chat_store, &orphan_cmds);
    if !orphan_cmds.is_empty() {
        env.drivers
            .session
            .execute(orphan_cmds.iter(), session_driver);
    }

    // ── Chat rope splices ───────────────────────────────────────
    // EXAMPLE-ARCH §"Actions as diffs": the chat buffer's content
    // is desired-state-derived from `ChatTranscripts`, the diff
    // produces splice actions, applying them advances
    // `last_synced_event` (the execute-pattern sync write that
    // closes the re-fire loop).
    let splices = pending_chat_splices(chat_transcripts, chat_sessions);
    for splice in splices {
        apply_chat_splice(edits, chat_sessions, &splice);
    }
}

/// Mirror the runtime's optimistic apply onto `ChatStore` for
/// every `SessionCmd` we're about to ship. Mirrors the discipline
/// of the file-write driver writing `LoadState::Pending` before
/// the worker call returns — the next memo round sees the row
/// already present, so the next tick's diff is Noop.
fn apply_session_mirrors(
    store: &mut led_driver_session_core::ChatStore,
    cmds: &[SessionCmd],
) {
    for cmd in cmds {
        match cmd {
            SessionCmd::InsertChatRow { row } => store.apply_insert(row.clone()),
            SessionCmd::AppendChatMessage { message } => {
                store.apply_append(message.clone())
            }
            SessionCmd::UpdateChatLabels {
                id,
                short_label,
                long_summary,
            } => store.apply_labels(id, short_label.clone(), long_summary.clone()),
            SessionCmd::UpdateChatLastActive { id, at, usage_json } => {
                store.apply_last_active(id, *at, usage_json.clone())
            }
            SessionCmd::UpdateChatStatus { id, status } => {
                store.apply_status(id, *status)
            }
            _ => {}
        }
    }
}

/// Re-create `ChatSessions` entries for any `Tabs.open` entry
/// whose path is a chat-buffer file (`<uuid>.chat` with a valid
/// UUIDv4 stem) but doesn't yet have a `ChatSessions` mapping.
///
/// Necessary because `ChatSessions` is process-local — the
/// session DB only restores `Tabs` + `EditedBuffers`, so chat
/// tabs come back without the metadata that tells the chat
/// driver "this path talks to that UUID". Detecting by filename
/// shape lets us reconstruct the mapping without persisting a
/// separate SQLite table.
///
/// `submit_offset` and `response_anchor` are parked at
/// `rope.len_chars()` so existing content reads as
/// already-submitted history.
fn reseed_restored_chats(
    tabs: &led_state_tabs::Tabs,
    edits: &led_state_buffer_edits::BufferEdits,
    sessions: &mut led_state_chat::ChatSessions,
    prefs: &mut led_state_chat::ChatPrefs,
) {
    let env_effort = std::env::var("LED_CHAT_EFFORT")
        .ok()
        .and_then(|s| parse_effort_env(&s));
    let env_perm = std::env::var("LED_CHAT_PERMISSION")
        .ok()
        .and_then(|s| parse_permission_env(&s));

    for tab in tabs.open.iter() {
        if sessions.is_chat(&tab.path) {
            continue;
        }
        let Some(uuid) = uuid_from_chat_path(&tab.path) else {
            continue;
        };
        sessions.insert(tab.path.clone(), uuid.clone());
        // Park offsets at end-of-rope so old content stays clean.
        if let (Some(state), Some(eb)) =
            (sessions.get_mut(&tab.path), edits.buffers.get(&tab.path))
        {
            let end = eb.draft.len_chars();
            state.submit_offset = end;
            state.response_anchor = end;
        }
        // Apply env-var overrides on the same uuid so subsequent
        // Spawn actions get the user's chosen effort / permission
        // mode rather than the project default of XHigh.
        if (env_effort.is_some() || env_perm.is_some())
            && !prefs.overrides.contains_key(&uuid)
        {
            prefs.overrides.insert(
                uuid,
                SessionOverrides {
                    effort: env_effort,
                    permission_mode: env_perm,
                },
            );
        }
    }
}

fn parse_effort_env(s: &str) -> Option<Effort> {
    match s.to_ascii_lowercase().as_str() {
        "low" => Some(Effort::Low),
        "medium" | "med" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" | "max" => Some(Effort::XHigh),
        _ => None,
    }
}

fn parse_permission_env(s: &str) -> Option<PermissionMode> {
    match s {
        "auto" => Some(PermissionMode::Auto),
        "acceptEdits" | "accept_edits" => Some(PermissionMode::AcceptEdits),
        "plan" => Some(PermissionMode::Plan),
        "bypassPermissions" | "bypass" => Some(PermissionMode::BypassPermissions),
        _ => None,
    }
}

/// Extract a `SessionUuid` from a chat-buffer path. Returns
/// `Some(uuid)` iff the filename matches `<uuidv4>.chat` exactly.
fn uuid_from_chat_path(path: &led_core::CanonPath) -> Option<SessionUuid> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".chat")?;
    if !looks_like_uuid_v4(stem) {
        return None;
    }
    Some(SessionUuid::new(stem))
}

/// `xxxxxxxx-xxxx-4xxx-Nxxx-xxxxxxxxxxxx` shape check (just
/// the structural form — dashes at the right positions, hex
/// elsewhere, length 36).
fn looks_like_uuid_v4(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.chars().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => c == '-',
        _ => c.is_ascii_hexdigit(),
    })
}

/// Apply one `ChatSplice` action: insert `text` at `char_offset`
/// in the rope, bump version, and advance the session's offsets
/// past the inserted region. Also adjusts any subsequent
/// `user_ranges` that sit past the splice point so colouring
/// stays anchored to the right bytes.
fn apply_chat_splice(
    edits: &mut led_state_buffer_edits::BufferEdits,
    sessions: &mut led_state_chat::ChatSessions,
    splice: &ChatSplice,
) {
    let Some(eb) = edits.buffers.get_mut(&splice.path) else {
        return;
    };
    let mut rope = (*eb.draft).clone();
    let len = rope.len_chars();
    let at = splice.char_offset.min(len);
    rope.insert(at, &splice.text);
    eb.set_draft(Arc::new(rope));
    eb.version.0 = eb.version.0.saturating_add(1);

    if let Some(state) = sessions.get_mut(&splice.path) {
        let inserted = splice.text.chars().count();
        // Anchor the watermark to the source event index — never
        // increment by splice ordinal. Two splices for the same
        // session in one tick would otherwise double-bump the
        // watermark and skip events.
        state.last_synced_event = splice.source_event_idx + 1;
        // Advance the response anchor + submit_offset past the
        // inserted text so the next splice lands after this one
        // and the next user submission appends past it.
        state.response_anchor = state.response_anchor.saturating_add(inserted);
        if state.submit_offset >= at {
            state.submit_offset = state.submit_offset.saturating_add(inserted);
        }
        // Shift user_ranges that sit past the splice point.
        for (start, end) in state.user_ranges.iter_mut() {
            if *start >= at {
                *start += inserted;
                *end += inserted;
            }
        }
    }
}
