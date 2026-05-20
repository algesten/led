//! Action / command memos.
//!
//! Memos that combine input projections into actionable results
//! consumed by the driver execute step: load, save, reread, sync,
//! and the static-deadline fold.

use led_core::{BufferStateSum, CanonPath};
use led_driver_buffers_core::{LoadAction, SaveAction};
use led_driver_clipboard_core::ClipboardAction;
use led_driver_file_watch_core::{ChangeKinds, FileWatchEvent};
use led_driver_session_core::SessionCmd;
use led_state_buffer_edits::Persisted;
use led_state_jumps::JumpPosition;
use std::sync::Arc;

use super::inputs::*;

// ── Memos ──────────────────────────────────────────────────────────────

/// "What files need a load started?"
///
/// Diff between the paths open in tabs and the `BufferStore` map.
/// Absent → `Load`; `Pending | Ready | Error` → skip. Once a load is
/// in flight, the `Pending` entry prevents re-triggering.
///
/// Filters before cloning so we only allocate `CanonPath`s for paths
/// that actually need loading.
#[drv::memo(single)]
pub fn file_load_action<'a, 'b>(
    store: StoreLoadedInput<'a>,
    tabs: TabsOpenInput<'b>,
) -> imbl::Vector<LoadAction> {
    tabs.open
        .iter()
        .filter(|t| !store.loaded.contains_key(&t.path))
        .map(|t| LoadAction::Load(t.path.clone()))
        .collect()
}

/// "What saves should we dispatch now?"
///
/// Diffs the user's save requests (`pending_saves`) against the
/// edited buffers, emitting one `SaveAction` per path that is both
/// requested and dirty. Runtime sync-clears `pending_saves` for the
/// emitted paths before calling `FileWriteDriver::execute` — without
/// that clear the next tick's query would emit the same saves again.
///
/// Idle: `pending_saves` is empty → returns `Vec::new()` (no alloc).
/// Σ(saved_version) across all edited buffers. The runtime
/// compares this against `lsp_requested_state_sum` to decide
/// when to fire `LspCmd::RequestDiagnostics`. Pure derivation
/// of `BufferEdits.buffers` — the source stores only the sum we
/// last emitted for.
///
/// **Only `saved_version` is summed**, not live `version`. This
/// gates diagnostic requests to save events + the first
/// `lsp_notified` tick (caught by the `Option<u64>` None → Some
/// transition in the caller). Live-edit pulls would make
/// rust-analyzer re-analyze on every keystroke, and the user
/// would see error squiggles pop in mid-typing for transient
/// parser failures that disappear once they finish the word.
/// Save-gated pulls match the "errors appear after save" UX the
/// user expects.
#[drv::memo(single)]
pub fn buffer_state_sum<'b>(buffers: EditedBuffersInput<'b>) -> BufferStateSum {
    BufferStateSum(
        buffers
            .buffers
            .values()
            .fold(0u64, |acc, eb| acc.wrapping_add(eb.saved_version.0)),
    )
}

#[drv::memo(single)]
pub fn file_save_action<'p, 'b>(
    pending: PendingSavesInput<'p>,
    buffers: EditedBuffersInput<'b>,
) -> Vec<SaveAction> {
    let mut out: Vec<SaveAction> = Vec::new();
    for path in pending.paths.iter() {
        let Some(eb) = buffers.buffers.get(path) else {
            continue;
        };
        // No dirty filter: the user explicitly asked to Save (or
        // SaveNoFormat); writing a byte-identical file is cheap
        // and matches legacy. SaveAll's gating happens in the
        // dispatch helper (`request_save_all`) which only adds
        // dirty paths to `pending_saves` in the first place.
        out.push(SaveAction::Save {
            path: path.clone(),
            content: Persisted(eb.draft.as_rope().clone()),
            version: eb.version,
        });
    }
    // SaveAs commits. Unlike Save, SaveAs doesn't require the buffer
    // to be dirty — the user may want to snapshot a pristine buffer
    // to a new path.
    for (from, to) in pending.save_as.iter() {
        let Some(eb) = buffers.buffers.get(from) else {
            continue;
        };
        out.push(SaveAction::SaveAs {
            from: from.clone(),
            to: to.clone(),
            content: Persisted(eb.draft.as_rope().clone()),
            version: eb.version,
        });
    }
    out
}

// ── File-watch derived memos ──────────────────────────────────────────

/// "Which open buffers had their on-disk content modified since
/// the last drain?"
///
/// Walks the per-tick `recent_events` queue, drops Removed-only
/// and Created-only entries (per legacy), and keeps Modified
/// events whose path matches an open buffer. The runtime
/// dispatches a `LoadAction::Reread` for each.
///
/// Idle ticks: `recent_events` empty → memo cache-hits the empty
/// vector. Non-empty events with no buffer match → empty result;
/// still cache-hits if the inputs haven't changed.
#[drv::memo(single)]
pub fn external_reread_targets<'a, 'b>(
    events: FileWatchEventsInput<'a>,
    edits: EditedBuffersInput<'b>,
) -> Arc<Vec<CanonPath>> {
    if events.recent_events.is_empty() {
        return Arc::new(Vec::new());
    }
    let mut seen: std::collections::HashSet<&CanonPath> =
        std::collections::HashSet::new();
    let mut out: Vec<CanonPath> = Vec::new();
    for (id, queue) in events.recent_events.iter() {
        if *id == crate::WATCHER_ID_NOTIFY_DIR {
            continue;
        }
        for ev in queue {
            let FileWatchEvent::Changed { path, kinds, .. } = ev else {
                continue;
            };
            // Reread fires only on MODIFIED. Skip events that
            // include REMOVED (legacy parity: external delete is
            // inert). Skip Create-only events too — those don't
            // carry "new content" semantics for an already-open
            // buffer.
            if kinds.contains_any(ChangeKinds::REMOVED) {
                continue;
            }
            if !kinds.contains_any(ChangeKinds::MODIFIED) {
                continue;
            }
            if edits.buffers.contains_key(path) && seen.insert(path) {
                out.push(path.clone());
            }
        }
    }
    Arc::new(out)
}

/// "Which open buffers' `path_hash()` map to which canonical
/// path?" Used by the cross-instance sync-check fan-out: the
/// notify-dir watcher reports basenames that are 16-char hex
/// hashes; this memo provides the reverse lookup without rebuilding
/// the index per tick.
///
/// Memoized over the buffer keyset only. The `path_hash()` String
/// allocation now amortises across every idle tick.
#[drv::memo(single)]
pub fn notify_hash_index<'b>(
    edits: EditedBuffersInput<'b>,
) -> Arc<std::collections::HashMap<String, CanonPath>> {
    let mut out: std::collections::HashMap<String, CanonPath> =
        std::collections::HashMap::with_capacity(edits.buffers.len());
    for path in edits.buffers.keys() {
        out.insert(path.path_hash(), path.clone());
    }
    Arc::new(out)
}

/// "Which open buffers were touched by another instance and need
/// a sync-check?" One `SessionCmd::CheckSync` per open buffer
/// whose hash was reported on the `<config>/notify/` dir. Returns
/// an empty vector when no notify-dir events fired this tick.
#[drv::memo(single)]
pub fn sync_check_cmds<'a, 'b, 'u>(
    events: FileWatchEventsInput<'a>,
    index: HashIndexInput<'b>,
    undo: UndoPersistenceInput<'u>,
) -> Arc<Vec<SessionCmd>> {
    let Some(queue) = events.recent_events.get(&crate::WATCHER_ID_NOTIFY_DIR)
    else {
        return Arc::new(Vec::new());
    };
    let mut seen: std::collections::HashSet<&CanonPath> =
        std::collections::HashSet::new();
    let mut cmds: Vec<SessionCmd> = Vec::new();
    for ev in queue {
        let FileWatchEvent::Changed { path, .. } = ev else {
            continue;
        };
        let basename = match path.as_path().file_name() {
            Some(s) => s.to_string_lossy(),
            None => continue,
        };
        let Some(buf_path) = index.by_path.get(basename.as_ref()) else {
            continue;
        };
        if !seen.insert(buf_path) {
            continue;
        }
        let Some(tracker) = undo.by_path.get(buf_path) else {
            continue;
        };
        cmds.push(SessionCmd::CheckSync {
            path: buf_path.clone(),
            last_seen_seq: tracker.last_seq,
            current_chain_id: tracker.chain_id.clone(),
        });
    }
    Arc::new(cmds)
}

/// "What's the next non-LSP-spinner deadline the runner should
/// wake at?"
///
/// Folds three time-bound state machines: the info-alert TTL,
/// the find-file hint TTL, and per-buffer undo-flush debounce
/// windows. The LSP-spinner 80ms wake stays in `run` because it
/// depends on `Instant::now()` (which would invalidate the memo
/// every tick); the runner takes `min(static_deadline, now+80ms)`
/// when any server is busy.
///
/// Idle ticks: empty inputs → `None` via cache-hit, the runner
/// sleeps the full 60-second default.
#[drv::memo(single)]
pub fn static_deadline<'a, 'f, 'u>(
    alert: AlertExpiryInput<'a>,
    find_file: FindFileInput<'f>,
    undo: UndoFlushDebounceInput<'u>,
) -> Option<std::time::Instant> {
    use std::time::{Duration, Instant};
    let mut soonest: Option<Instant> = None;
    let mut consider = |t: Instant| {
        soonest = Some(match soonest {
            Some(cur) if cur < t => cur,
            _ => t,
        });
    };
    if let Some(t) = *alert.info_expires_at {
        consider(t);
    }
    if let Some(ff) = find_file.overlay.as_ref()
        && let Some(t) = ff.input.hint_expires_at
    {
        consider(t);
    }
    let debounce_window = Duration::from_millis(200);
    if let Some(earliest) = undo
        .entries
        .values()
        .map(|e| e.first_seen + debounce_window)
        .min()
    {
        consider(earliest);
    }
    soonest
}

/// "What clipboard action should we fire this tick?"
///
/// Returns `None` on an idle tick (no yank pending, no write
/// queued). Returns `Some(Read)` when a yank is pending and no
/// read is in flight. Returns `Some(Write(_))` with a clone of the
/// pending text when a kill queued one. When both signals are
/// live, yank wins — matches legacy ordering.
///
/// Zero allocation on idle (returns a simple `Option`); one Arc
/// clone on the Write path, which is the same as the driver's own
/// execute.
#[drv::memo(single)]
pub fn clipboard_action<'c, 'd>(
    clip: ClipboardIntentInput<'c>,
    drv: ClipboardDriverInput<'d>,
) -> Option<ClipboardAction> {
    use led_driver_clipboard_core::{ReadState, WriteState};
    if clip.pending_yank.is_some() && matches!(drv.read, ReadState::Idle) {
        Some(ClipboardAction::Read)
    } else if let Some(text) = clip.pending_write.as_ref()
        && matches!(drv.write, WriteState::Idle)
    {
        Some(ClipboardAction::Write(text.clone()))
    } else {
        None
    }
}

/// "What completion request does the find-file overlay need fired?"
///
/// Derives a `FindFileCmd` from the overlay's `input` by splitting at
/// the last `/`: trailing-slash inputs list the dir itself with an
/// empty prefix, otherwise we list the parent with the leaf as the
/// case-insensitive prefix. `show_hidden` flips on when the prefix
/// starts with `.`.
///
/// The memo cache-keys on `FindFileInput`, so an unchanged input
/// string returns the same `Vec` and the driver sees no re-fire.
/// Activation changes `input` (None → Some(...)) — the memo
/// recomputes and emits the initial request. Input edits each
/// change the string and re-fire.
///
/// Returns an empty `Vec` when the overlay is inactive.
#[drv::memo(single)]
pub fn find_file_action<'f>(
    ff: FindFileInput<'f>,
) -> Vec<led_driver_find_file_core::FindFileCmd> {
    // Execute-pattern: dispatch pushed one `FindFileRequest` per
    // input edit into the state's queue; the memo translates each
    // domain request to the driver ABI `FindFileCmd` and ships the
    // whole queue, and the main loop drains it after execute.
    // Inactive overlay or empty queue → empty Vec (zero alloc hot
    // path). Translation is the field-rename boundary that keeps
    // `state-find-file` from storing driver-ABI types as fields.
    let Some(state) = ff.overlay.as_ref() else {
        return Vec::new();
    };
    state
        .pending_find_file_list
        .iter()
        .map(|r| led_driver_find_file_core::FindFileCmd {
            dir: r.dir.clone(),
            prefix: r.prefix.clone(),
            show_hidden: r.show_hidden,
        })
        .collect()
}

/// "If the active tab were about to lose focus (tab cycle,
/// goto-def, …), what [`JumpPosition`] should we record onto
/// the jump list?"
///
/// Pure projection of the active `Tab`'s `(path, cursor, scroll)`
/// — the same shape `nav::current_position` /
/// `apply::lsp::current_jump_position` compute non-memoized.
/// Callers (tab cycle, future goto-def) read this memo and
/// decide whether the resulting move actually warrants a push
/// (e.g. tab cycle skips when the next tab is the same as
/// the current one).
///
/// `None` when there's no active tab.
#[drv::memo(single)]
pub fn outgoing_jump_position<'t>(
    tabs: TabsActiveInput<'t>,
) -> Option<JumpPosition> {
    let id = (*tabs.active)?;
    let tab = tabs.open.iter().find(|t| t.id == id)?;
    Some(JumpPosition {
        path: tab.path.clone(),
        line: tab.cursor.line,
        col: tab.cursor.col,
        top: tab.scroll.top,
        top_sub_line: tab.scroll.top_sub_line,
    })
}
