use led_core::SavedVersion;
use led_state_file_search::{FileSearchSelection, FileSearchState};
use led_state_tabs::Tabs;

use crate::query::{
    replace_all_plan, EditedBuffersInput, FileSearchQueryInput, FileSearchReplaceInput,
    TabsActiveInput,
};

/// `Alt+Enter` — project-wide replace-all.
///
/// Two paths, applied together:
///
/// 1. **In-memory.** For every currently-loaded buffer
///    (`edits.buffers`), run `regex.replace_all` against its rope.
///    Changed buffers get a fresh version via `shared::bump` so
///    `dirty()` flips — the session view becomes the source of
///    truth until the user saves. Per-file replacement counts are
///    stashed in `edits.pending_replace_in_memory` for the alert.
///
/// 2. **On-disk.** Dispatch pushes a `PendingReplaceAll` onto
///    `edits.pending_replace_all` with the set of loaded paths as
///    `skip_paths`. The main loop drains that queue and ships a
///    `FileSearchReplaceCmd` to `driver-file-search`, which walks
///    the workspace independently and rewrites the remaining files.
///
/// `fs_root` is the workspace root (dispatch's caller reads it off
/// `FsTree`). Missing root → the driver walk is skipped, in-memory
/// pass still runs.
/// `CursorRight` on a selected hit (replace_mode on) — if the hit
/// is still pending, apply the replacement and mark the row
/// replaced. Rows stay visible in the tree either way, so
/// Left-arrow on a specific replaced row can undo just that one
/// without disturbing others. Advances selection to the next
/// pending hit when one's available (wraps to the first pending).
/// Advance selection to the next pending hit after the current
/// index, wrapping to the start. No-op (selection stays) if every
/// hit has already been replaced — user can Left to undo where
/// they are, or Down to move within the fully-replaced set.
pub(super) fn advance_to_next_pending(state: &mut FileSearchState) {
    let FileSearchSelection::Result(idx) = state.selection else {
        return;
    };
    let n = state.flat_hits.len();
    if n == 0 {
        return;
    }
    // Look forward from idx+1, wrap to 0, back to idx.
    for step in 1..=n {
        let candidate = (idx + step) % n;
        if state
            .hit_replacements
            .get(candidate)
            .and_then(|e| e.as_ref())
            .is_none()
        {
            state.selection = FileSearchSelection::Result(candidate);
            return;
        }
    }
    // All replaced — stay put.
}

pub(super) fn ensure_replacements_len(state: &mut FileSearchState) {
    if state.hit_replacements.len() != state.flat_hits.len() {
        state.hit_replacements = vec![None; state.flat_hits.len()];
    }
}

pub(super) fn apply_replace_all(
    state: &led_state_file_search::FileSearchState,
    tabs: &Tabs,
    edits: &mut led_state_buffer_edits::BufferEdits,
    fs_root: Option<&led_core::CanonPath>,
) {
    if state.query.text.is_empty() {
        return;
    }

    // Tabs split into owned (non-preview) vs preview inside the
    // memo. Owned buffers land dirty; preview buffers stay clean
    // (the driver writes them on disk and our in-memory rope
    // mirrors the same regex result). Unloaded files are
    // driver-only — the plan's skip_paths intentionally omits
    // them so the driver walk processes them.
    let plan = replace_all_plan(
        FileSearchQueryInput::new(state),
        FileSearchReplaceInput::new(state),
        TabsActiveInput::new(tabs),
        EditedBuffersInput::new(edits),
    );
    if plan.in_memory.is_empty() && plan.skip_paths.is_empty() {
        // Empty plan also covers "compile failed / empty query"
        // — short-circuit before queuing a `PendingReplaceAll`
        // the driver would just no-op on.
        return;
    }

    for entry in plan.in_memory.iter() {
        let Some(eb) = edits.buffers.get_mut(&entry.path) else {
            continue;
        };
        // bump() takes ownership; clone the Arc'd rope's inner.
        super::super::shared::bump(eb, (*entry.new_rope).clone());
        if entry.preview {
            // Preview stays clean — saved_version tracks the
            // disk state which the driver is about to write.
            eb.saved_version = SavedVersion(eb.version.0);
            eb.disk_content_hash =
                led_core::EphemeralContentHash::of_rope(&eb.draft).persist();
        }
        edits
            .pending_replace_in_memory
            .push(led_state_buffer_edits::InMemoryReplace {
                path: entry.path.clone(),
                count: entry.count,
            });
    }

    if let Some(root) = fs_root {
        edits
            .pending_replace_all
            .push(led_state_buffer_edits::PendingReplaceAll {
                root: root.clone(),
                query: state.query.text.clone(),
                replacement: state.replace.text.clone(),
                case_sensitive: state.case_sensitive,
                use_regex: state.use_regex,
                skip_paths: plan.skip_paths.clone(),
            });
    }
}
