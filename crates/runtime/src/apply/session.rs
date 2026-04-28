//! Session / persistence helpers extracted from `lib.rs`.
//!
//! Verbatim moves of: `config_dir_for_session`,
//! `build_session_data`, `apply_session_kv`, `build_session_kv`,
//! `apply_pending_undo_restore`, `new_chain_id`,
//! `disk_content_hash_for`, `apply_sync_result`,
//! `apply_remote_entries`. Visibility bumped to `pub(crate)` so the
//! main loop and ingest paths can keep calling them.

use led_core::{CanonPath, ChainId, SavedVersion, WatchSeq};
use led_driver_buffers_core::BufferStore;
use led_state_buffer_edits::{BufferEdits, EditGroup, EditedBuffer};
use led_state_session::{SessionBuffer, SessionData};
use led_state_tabs::Tabs;

use crate::UndoPersistTracker;

/// Resolve the per-user config directory the session driver
/// stores `db.sqlite` and `primary/<hash>` under. Honours
/// `XDG_CONFIG_HOME` like the keymap/theme loaders, otherwise
/// `~/.config/led/`. Returns `None` when neither is resolvable
/// (CI sandboxes, etc.) — the runtime treats that as
/// "session is a no-op", same as standalone mode.
pub(crate) fn config_dir_for_session() -> Option<CanonPath> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = std::path::PathBuf::from(xdg).join("led");
        std::fs::create_dir_all(&path).ok()?;
        return Some(led_core::UserPath::new(path).canonicalize());
    }
    let home = std::env::var_os("HOME")?;
    let path = std::path::PathBuf::from(home).join(".config").join("led");
    std::fs::create_dir_all(&path).ok()?;
    Some(led_core::UserPath::new(path).canonicalize())
}

/// Build the [`SessionData`] payload from the live atom set.
/// Mirrors legacy's session-on-quit assembly: one
/// `SessionBuffer` per non-preview tab (cursor + scroll), plus
/// the active-tab order, the side-panel toggle, and any kv pairs
/// the runtime collected (browser state, jump list, etc. — those
/// will arrive in a follow-up; the slot is here today).
///
/// The undo-flush + ClearUndo flow lives separately: legacy
/// flushes undo on a debounce timer (and on any save) via a
/// distinct WorkspaceOut::FlushUndo command; our `SaveSession`
/// is just the workspaces / buffers / kv portion.
pub(crate) fn build_session_data(
    tabs: &Tabs,
    _edits: &BufferEdits,
    _store: &BufferStore,
    browser: &led_state_browser::BrowserUi,
    jumps: &led_state_jumps::JumpListState,
) -> SessionData {
    let mut session_buffers: Vec<SessionBuffer> =
        Vec::with_capacity(tabs.open.len());
    let mut active_tab_order: usize = 0;
    for tab in tabs.open.iter() {
        if tab.preview {
            continue;
        }
        if Some(tab.id) == tabs.active {
            active_tab_order = session_buffers.len();
        }
        session_buffers.push(SessionBuffer {
            path: tab.path.clone(),
            tab_order: session_buffers.len(),
            cursor: tab.cursor,
            scroll: tab.scroll,
            undo: None,
        });
    }
    SessionData {
        active_tab_order,
        show_side_panel: browser.visible,
        buffers: session_buffers,
        kv: build_session_kv(browser, jumps),
    }
}

/// Inverse of [`build_session_kv`]: re-hydrates the browser +
/// jump-list atoms from the kv blob the driver loaded out of
/// `session_kv`. Legacy's equivalent is `model::session_of`.
/// Unknown keys are tolerated; type-mismatched values fall back
/// to defaults so a corrupted row doesn't block the restore.
pub(crate) fn apply_session_kv(
    kv: &std::collections::HashMap<String, String>,
    browser: &mut led_state_browser::BrowserUi,
    jumps: &mut led_state_jumps::JumpListState,
) {
    if let Some(sel) = kv.get("browser.selected_path") {
        browser.selected_path = Some(
            led_core::UserPath::new(std::path::PathBuf::from(sel))
                .canonicalize(),
        );
    }
    if let Some(off) = kv.get("browser.scroll_offset")
        && let Ok(n) = off.parse::<usize>()
    {
        browser.scroll_offset = n;
    }
    if let Some(dirs) = kv.get("browser.expanded_dirs") {
        browser.expanded_dirs = dirs
            .split('\n')
            .filter(|s| !s.is_empty())
            .map(|s| {
                led_core::UserPath::new(std::path::PathBuf::from(s))
                    .canonicalize()
            })
            .collect();
    }
    if let Some(json) = kv.get("jump_list.entries")
        && let Ok(entries) =
            serde_json::from_str::<std::collections::VecDeque<
                led_state_jumps::JumpPosition,
            >>(json)
    {
        jumps.entries = entries;
        if let Some(idx) = kv.get("jump_list.index")
            && let Ok(n) = idx.parse::<usize>()
        {
            jumps.index = n.min(jumps.entries.len());
        } else {
            jumps.index = jumps.entries.len();
        }
    }
}

/// Mirrors legacy's `build_session_kv` (`led/src/derived.rs`).
/// Browser selection / scroll / expanded set + jump-list entries
/// + index, encoded as plain string values so the schema row stays
///   stable across rewrite-internal type churn.
pub(crate) fn build_session_kv(
    browser: &led_state_browser::BrowserUi,
    jumps: &led_state_jumps::JumpListState,
) -> std::collections::HashMap<String, String> {
    let mut kv = std::collections::HashMap::new();
    if let Some(sel) = &browser.selected_path {
        kv.insert(
            "browser.selected_path".into(),
            sel.as_path().to_string_lossy().into_owned(),
        );
    }
    kv.insert(
        "browser.scroll_offset".into(),
        browser.scroll_offset.to_string(),
    );
    let dirs: Vec<String> = browser
        .expanded_dirs
        .iter()
        .map(|d| d.as_path().to_string_lossy().into_owned())
        .collect();
    if !dirs.is_empty() {
        kv.insert("browser.expanded_dirs".into(), dirs.join("\n"));
    }
    if let Ok(json) = serde_json::to_string(&jumps.entries) {
        kv.insert("jump_list.entries".into(), json);
        kv.insert("jump_list.index".into(), jumps.index.to_string());
    }
    kv
}

/// Apply a stashed [`UndoRestoreData`] to a now-materialised
/// buffer: replay each `EditGroup`'s ops forward onto the rope,
/// install the restored chain into `eb.history.past`, and seed
/// the per-buffer flush tracker so subsequent `FlushUndo`
/// commands resume from the restored tail.
///
/// Two callers:
/// - the load-completion ingest hook (first-time materialise
///   path; runs once per buffer per session)
/// - the `SessionEvent::Restored` arm (CLI-arg buffers that
///   loaded BEFORE Init replied — `inserted` was true on a tick
///   where `pending_undo` was still empty, so the restore data
///   has to be applied retroactively here)
///
/// Returns silently when the disk-hash gate fails (file
/// changed externally between sessions) — the chain stays in
/// `pending_undo`'s now-removed slot, effectively dropped.
pub(crate) fn apply_pending_undo_restore(
    path: &CanonPath,
    edits: &mut BufferEdits,
    session: &mut led_state_session::SessionState,
    undo_persistence: &mut imbl::HashMap<CanonPath, UndoPersistTracker>,
) {
    let Some(restore) = session.pending_undo.remove(path) else {
        return;
    };
    let Some(eb) = edits.buffers.get_mut(path) else {
        return;
    };
    if eb.disk_content_hash != restore.content_hash {
        return;
    }
    let mut new_rope = (*eb.rope).clone();
    for group in &restore.entries {
        for op in &group.ops {
            use led_state_buffer_edits::EditOp;
            match op {
                EditOp::Delete { at, text } => {
                    let len = text.chars().count();
                    let end = (*at + len).min(new_rope.len_chars());
                    if *at < new_rope.len_chars() && end > *at {
                        new_rope.remove(*at..end);
                    }
                }
                EditOp::Insert { at, text } => {
                    let pos = (*at).min(new_rope.len_chars());
                    new_rope.insert(pos, text);
                }
            }
        }
    }
    eb.rope = std::sync::Arc::new(new_rope);
    if !restore.entries.is_empty() {
        eb.version.0 = eb.version.0.saturating_add(1);
    }
    let mut history = led_state_buffer_edits::History::with_seq_gen(
        edits.seq_gen.clone(),
    );
    history.restore_past(restore.entries.clone());
    eb.history = history;
    undo_persistence.insert(
        path.clone(),
        UndoPersistTracker {
            chain_id: restore.chain_id.clone(),
            persisted_len: restore.entries.len(),
            last_seq: restore.last_seq,
        },
    );
}

/// Generate a unique `chain_id` for an undo persistence chain.
/// Mirrors legacy's `led_workspace::new_chain_id` — 64-bit hash
/// of (now, pid). Collision-safe enough for a per-buffer
/// session marker; not cryptographic.
pub(crate) fn new_chain_id() -> ChainId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = DefaultHasher::new();
    t.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    UNDO_CHAIN_NONCE
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .hash(&mut hasher);
    ChainId::new(format!("{:016x}", hasher.finish()))
}

static UNDO_CHAIN_NONCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Hash that anchors the chain to "what's on disk." Returns
/// `eb.disk_content_hash`, which is set at load completion (the
/// rope at version 0 IS the disk content) and refreshed at save
/// completion (the just-written rope is the new disk content).
/// Mirrors legacy `BufferState::content_hash` — used so the
/// next-launch restore can re-hash the disk file and refuse to
/// replay if the bytes shifted between sessions.
pub(crate) fn disk_content_hash_for(eb: &EditedBuffer) -> led_core::PersistedContentHash {
    eb.disk_content_hash
}

/// M26 — apply a `SessionEvent::SyncResult` arrival.
///
/// Three discriminants:
///
/// - `SyncEntries` with matching chain + content_hash: apply the
///   peer's `EditGroup`s to the rope. Cursor stays put on M26;
///   future polish can use `History::rebase_*` helpers.
/// - `SyncEntries` with chain or hash mismatch: queue a synthetic
///   `FileWatchEvent::Changed { kinds: MODIFIED }` into
///   `FileWatchState.recent_events` so the next-tick
///   `external_reread_targets` memo emits a `LoadAction::Reread`.
///   The reconcile branch then takes over.
/// - `ExternalSave`: same fallback — synthesize a reread.
/// - `NoChange`: drop. Includes the self-echo case (our own
///   `FlushUndo` → notify-touch → `CheckSync` round-trip).
pub(crate) fn apply_sync_result(
    kind: led_driver_session_core::SyncResultKind,
    edits: &mut BufferEdits,
    undo_persistence: &mut imbl::HashMap<CanonPath, UndoPersistTracker>,
    file_watch: &mut led_driver_file_watch_core::FileWatchState,
) {
    use led_driver_session_core::SyncResultKind;
    match kind {
        SyncResultKind::SyncEntries {
            path,
            chain_id,
            content_hash,
            entries,
            new_last_seen_seq,
        } => {
            let chain_match = undo_persistence
                .get(&path)
                .is_some_and(|t| t.chain_id == chain_id);
            let hash_match = edits
                .buffers
                .get(&path)
                .is_some_and(|eb| eb.disk_content_hash == content_hash);
            if !chain_match || !hash_match {
                synthesize_reread(file_watch, &path);
                return;
            }
            apply_remote_entries(edits, &path, &entries);
            if let Some(tracker) = undo_persistence.get_mut(&path) {
                tracker.last_seq = new_last_seen_seq;
                tracker.persisted_len = tracker.persisted_len.saturating_add(entries.len());
            }
        }
        SyncResultKind::ExternalSave { path } => {
            synthesize_reread(file_watch, &path);
        }
        SyncResultKind::NoChange { .. } => {
            // Drop. Includes the self-echo from FlushUndo →
            // notify-touch → CheckSync round-trip on a single
            // primary's own write.
        }
    }
}

/// Apply a peer's `EditGroup`s to the local rope. Each group's
/// ops execute in declaration order; deletes carry their text so
/// the local rope just removes the matching range, inserts
/// substitute the new text. After applying, push the group into
/// the local `History.past` so a local `Ctrl-/` can undo the
/// peer-applied change exactly as if we'd typed it.
pub(crate) fn apply_remote_entries(
    edits: &mut BufferEdits,
    path: &CanonPath,
    entries: &[EditGroup],
) {
    let Some(eb) = edits.buffers.get_mut(path) else {
        return;
    };
    if entries.is_empty() {
        return;
    }
    let mut new_rope = (*eb.rope).clone();
    for group in entries {
        for op in &group.ops {
            use led_state_buffer_edits::EditOp;
            match op {
                EditOp::Delete { at, text } => {
                    let len = text.chars().count();
                    let end = (*at + len).min(new_rope.len_chars());
                    if *at < new_rope.len_chars() && end > *at {
                        new_rope.remove(*at..end);
                    }
                }
                EditOp::Insert { at, text } => {
                    let pos = (*at).min(new_rope.len_chars());
                    new_rope.insert(pos, text);
                }
            }
        }
    }
    eb.rope = std::sync::Arc::new(new_rope);
    eb.disk_content_hash =
        led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
    eb.version.0 = eb.version.0.saturating_add(1);
    // Buffer stays clean: the peer's edits are now part of the
    // shared chain, and our local view matches the disk snapshot
    // the peer was writing against.
    eb.saved_version = SavedVersion(eb.version.0);
    // Stash the peer's groups in our local history so undo
    // walks them.
    eb.history.restore_past(entries.to_vec());
}

/// Synthesize a `MODIFIED` event for `path` into the file-watch
/// driver's `recent_events`. Chain/hash-mismatch SyncResults
/// fall back through this so the existing
/// `external_reread_targets` memo handles the recovery
/// uniformly.
pub(crate) fn synthesize_reread(
    file_watch: &mut led_driver_file_watch_core::FileWatchState,
    path: &CanonPath,
) {
    // Use a synthetic WatchSeq distinct from any registration —
    // memos that match against `registry` won't see it as
    // matching a per-buffer parent watch, but the
    // `external_reread_targets` memo can be written to handle
    // this synthetic id specially. For M26, allocate a sentinel
    // id derived from the path hash so the same path always
    // produces the same id (deterministic across runs of the
    // test suite).
    let id = WatchSeq(
        std::collections::hash_map::DefaultHasher::new().pipe(|mut h| {
            use std::hash::Hasher;
            path.as_path().to_string_lossy().hash_into(&mut h);
            h.finish()
        }),
    );
    file_watch.synthesize_modified(id, path.clone());
}

/// Tiny pipe helper so the synthesize_reread call can build a
/// hash inline without a let-mut sequence.
trait PipeExt: Sized {
    fn pipe<R, F: FnOnce(Self) -> R>(self, f: F) -> R {
        f(self)
    }
}
impl<T> PipeExt for T {}

trait HashIntoExt {
    fn hash_into(self, h: &mut std::collections::hash_map::DefaultHasher);
}
impl HashIntoExt for std::borrow::Cow<'_, str> {
    fn hash_into(self, h: &mut std::collections::hash_map::DefaultHasher) {
        use std::hash::Hasher;
        h.write(self.as_bytes());
    }
}
