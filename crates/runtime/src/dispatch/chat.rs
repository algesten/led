//! Chat-tab dispatch.
//!
//! Chats now live as regular `EditedBuffer`s with a synthetic
//! `~/.cache/led/chats/<uuid>.chat` path. Editing flows through the
//! normal dispatch primitives (cursor, mark, insert, delete) — this
//! module owns only the chat-specific gestures:
//!
//! - [`new_chat`] mints a session UUID, materialises an empty
//!   buffer at a chat path, opens it as a `Tab`, and registers
//!   the chat metadata in `ChatSessions`.
//! - [`submit`] takes the text from the active chat buffer's
//!   `submit_offset` onward as the user's next turn, queues it
//!   via `ChatPrefs`, and parks the response anchor at the new
//!   end-of-rope so an in-flight reply splices in at the right spot
//!   even if the user keeps typing past it.

use std::path::PathBuf;
use std::sync::Arc;

use led_core::{CanonPath, Effort, PermissionMode, SessionUuid, UserPath};
use led_state_alerts::AlertState;
use led_state_buffer_edits::{BufferEdits, EditedBuffer, Persisted};
use led_state_chat::{ChatPrefs, ChatSessions, SessionOverrides};
use led_state_tabs::{Tab, Tabs};
use ropey::Rope;

use crate::Clock;
use crate::INFO_TTL;

use super::shared::next_tab_id;

/// Mint a session UUID, open a fresh chat buffer as a regular
/// `Tab`. The buffer's path is synthetic — the runtime treats chat
/// paths as never-touch-disk and the persistence flows through
/// the existing `ChatStore` SQLite tables.
///
/// `ChatSessions` records the binding between the synthetic path
/// and the session UUID so subsequent submit / response logic can
/// recover one from the other.
pub(super) fn new_chat(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    sessions: &mut ChatSessions,
    prefs: &mut ChatPrefs,
    alerts: &mut AlertState,
    clock: &Clock,
) {
    let uuid = mint_session_uuid(clock);
    let path = make_chat_path(&uuid);

    // Honour env-var overrides for effort / permission_mode so
    // users can iterate on responsiveness without touching code.
    let effort = std::env::var("LED_CHAT_EFFORT")
        .ok()
        .and_then(|s| parse_effort(&s));
    let permission_mode = std::env::var("LED_CHAT_PERMISSION")
        .ok()
        .and_then(|s| parse_permission_mode(&s));
    if effort.is_some() || permission_mode.is_some() {
        prefs.overrides.insert(
            uuid.clone(),
            SessionOverrides {
                effort,
                permission_mode,
            },
        );
    }

    // Seed an empty `EditedBuffer` so the body renderer has a
    // draft to display immediately. The file-read driver will
    // also pick up the on-disk file we touched in `make_chat_path`
    // and emit a `LoadDone`, but `seed_edit_from_load` is
    // vacant-only — it skips our pre-seeded entry. EXAMPLE-ARCH
    // §"Shadow sources": draft is the user-side mirror, persisted
    // is the disk anchor; both start at the same empty rope.
    let empty = Arc::new(Rope::new());
    edits
        .buffers
        .insert(path.clone(), EditedBuffer::fresh(Persisted(empty)));

    sessions.insert(path.clone(), uuid.clone());

    // Open the tab and focus it. Use `next_tab_id` so dispatch
    // doesn't need to thread `TabIdGen` through every caller —
    // ids stay dense by scanning the existing max.
    let id = next_tab_id(tabs);
    tabs.open.push_back(Tab {
        id,
        path: path.clone(),
        ..Default::default()
    });
    tabs.active = Some(id);

    let short = uuid.as_str().chars().take(8).collect::<String>();
    let eff_label = effort.unwrap_or_default().as_flag().to_string();
    alerts.set_info(
        format!("New chat #{short} (effort={eff_label})"),
        clock.now,
        INFO_TTL,
    );
}

/// `Alt+Enter` handler.
///
/// 1. Lifts `rope[submit_offset..end]` into a pending send.
/// 2. Ensures the rope ends with a newline so the assistant's
///    reply splices onto its own line. If the user's last char
///    isn't `\n`, append `"\n\n"` (one to terminate the user's
///    line + one blank-line separator). If the rope already
///    ends with one or more `\n`s, leave the whitespace alone —
///    multi-newline preservation per the design brief.
/// 3. Parks `submit_offset` and `response_anchor` at the new
///    end-of-rope so the response inserts after the trailing
///    blank line; the cursor follows.
///
/// No-op if the active tab isn't a chat or the pending-text slice
/// is whitespace-only.
pub(super) fn submit(
    tabs: &mut Tabs,
    edits: &mut BufferEdits,
    sessions: &mut ChatSessions,
    prefs: &mut ChatPrefs,
) {
    let Some(active_id) = tabs.active else { return };
    let Some(active_idx) = tabs.open.iter().position(|t| t.id == active_id) else {
        return;
    };
    let path = tabs.open[active_idx].path.clone();
    let Some(state) = sessions.get_mut(&path) else {
        return;
    };
    let Some(eb) = edits.buffers.get_mut(&path) else {
        return;
    };

    let rope_len_before = eb.draft.len_chars();
    if state.submit_offset > rope_len_before {
        state.submit_offset = rope_len_before;
    }
    if state.submit_offset == rope_len_before {
        return;
    }
    let slice: String = eb
        .draft
        .slice(state.submit_offset..rope_len_before)
        .chars()
        .collect();
    if slice.trim().is_empty() {
        return;
    }
    let from = state.submit_offset;
    let to_before_pad = rope_len_before;

    // Ensure at least one *blank line* between the user's text
    // and the cursor / insertion point. Count trailing `\n`s; if
    // < 2, top up to 2 (additional newlines beyond two are
    // preserved as the user typed them).
    //
    // EXAMPLE-ARCH §"The execute pattern" — mutate the source
    // synchronously here so the next render shows the padded
    // rope before the response arrives.
    let trailing_nl = count_trailing_newlines(&eb.draft, rope_len_before);
    let pad = 2usize.saturating_sub(trailing_nl);
    let final_end = if pad > 0 {
        let padding = "\n".repeat(pad);
        let mut new_rope = (*eb.draft).clone();
        new_rope.insert(rope_len_before, &padding);
        eb.set_draft(Arc::new(new_rope));
        eb.version.0 = eb.version.0.saturating_add(1);
        rope_len_before + pad
    } else {
        rope_len_before
    };

    // Record the user range BEFORE the pad — the assistant's
    // reply will splice past `final_end`, so user_range covers
    // the just-typed slice and nothing more.
    state.user_ranges.push((from, to_before_pad));
    state.submit_offset = final_end;
    state.response_anchor = final_end;

    // Move the cursor to end-of-rope so the response inserts at
    // the cursor and the cursor naturally advances past the
    // spliced text (ropey shifts positions at-or-after the insert
    // point forward by the inserted length).
    let tab = &mut tabs.open[active_idx];
    let new_line = eb.draft.char_to_line(final_end);
    let line_start = eb.draft.line_to_char(new_line);
    let col = final_end - line_start;
    tab.cursor = led_state_tabs::Cursor {
        line: new_line,
        col,
        preferred_col: col,
    };

    prefs.queue_send(state.session.clone(), slice);
}

/// Build the synthetic chat-buffer path for `uuid`. Lives under
/// `$LED_CHAT_DIR` if set, else `~/.cache/led/chats/`. The
/// directory is created on first use so canonicalisation
/// succeeds; the file itself is touched empty so `BufferStore`
/// can register it without the file-read driver bouncing.
fn make_chat_path(uuid: &SessionUuid) -> CanonPath {
    let dir = std::env::var("LED_CHAT_DIR")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            dirs::cache_dir().map(|c| c.join("led").join("chats"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp/led-chats"));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("{}.chat", uuid.as_str()));
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&file);
    UserPath::new(file).canonicalize()
}

/// Count how many `\n` chars sit at the end of the rope. Walks
/// backwards from `end` until a non-newline char (or position 0)
/// is hit. Cheap — at most ~3 reads in practice.
fn count_trailing_newlines(rope: &ropey::Rope, end: usize) -> usize {
    let mut n = 0;
    let mut i = end;
    while i > 0 {
        if rope.char(i - 1) == '\n' {
            n += 1;
            i -= 1;
        } else {
            break;
        }
    }
    n
}

fn parse_effort(s: &str) -> Option<Effort> {
    match s.to_ascii_lowercase().as_str() {
        "low" => Some(Effort::Low),
        "medium" | "med" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" | "max" => Some(Effort::XHigh),
        _ => None,
    }
}

fn parse_permission_mode(s: &str) -> Option<PermissionMode> {
    match s {
        "auto" => Some(PermissionMode::Auto),
        "acceptEdits" | "accept_edits" => Some(PermissionMode::AcceptEdits),
        "plan" => Some(PermissionMode::Plan),
        "bypassPermissions" | "bypass" => Some(PermissionMode::BypassPermissions),
        _ => None,
    }
}

/// Mint a UUIDv4-shaped session id from `Clock::wall_now`.
///
/// The `claude` CLI rejects `--session-id` payloads that aren't
/// proper UUIDs. Derive 128 bits from the wall clock, splat them
/// into the canonical 8-4-4-4-12 layout, then set the
/// version/variant nibbles. No `uuid` crate dependency for this
/// one mint site.
fn mint_session_uuid(clock: &Clock) -> SessionUuid {
    let nanos = clock
        .wall_now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let lo = (nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15)) as u64;
    let hi = (nanos.wrapping_mul(0xBF58_476D_1CE4_E5B9)) as u64;
    let g1 = ((hi >> 32) & 0xFFFF_FFFF) as u32;
    let g2 = ((hi >> 16) & 0xFFFF) as u16;
    let mut g3 = (hi & 0xFFFF) as u16;
    let mut g4 = ((lo >> 48) & 0xFFFF) as u16;
    let g5 = lo & 0x0000_FFFF_FFFF_FFFF;
    g3 = (g3 & 0x0FFF) | 0x4000;
    g4 = (g4 & 0x3FFF) | 0x8000;
    SessionUuid::new(format!(
        "{g1:08x}-{g2:04x}-{g3:04x}-{g4:04x}-{g5:012x}"
    ))
}
