use led_core::SavedVersion;
use led_state_file_search::{FileSearchSelection, FileSearchState};
use led_state_tabs::Tabs;

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
    let pattern = if state.use_regex {
        state.query.text.clone()
    } else {
        regex_syntax::escape(&state.query.text)
    };
    let re = match regex::RegexBuilder::new(&pattern)
        .case_insensitive(!state.case_sensitive)
        .build()
    {
        Ok(r) => r,
        Err(_) => return,
    };
    let replacement = state.replace.text.as_str();

    // Tabs split into owned (non-preview) vs preview. Both get
    // their rope updated in-memory so the view stays consistent;
    // the difference is the dirty flag and who writes disk.
    //   - Owned: in-memory + dirty, user saves explicitly.
    //   - Preview: in-memory + saved_version=version (stays
    //     clean). Added to skip_paths so the driver walk doesn't
    //     also write the file — we already applied the edit
    //     in-memory, and the driver writing on top of our edit
    //     would be a race.
    //   - Unloaded files: driver writes them (not in skip_paths).
    let owned_paths: std::collections::HashSet<led_core::CanonPath> = tabs
        .open
        .iter()
        .filter(|t| !t.preview)
        .map(|t| t.path.clone())
        .collect();
    let preview_paths: std::collections::HashSet<led_core::CanonPath> = tabs
        .open
        .iter()
        .filter(|t| t.preview)
        .map(|t| t.path.clone())
        .collect();

    let mut skip_paths: Vec<led_core::CanonPath> = Vec::new();

    // Owned buffers — in-memory + dirty.
    let mut loaded_owned: Vec<led_core::CanonPath> = edits
        .buffers
        .keys()
        .filter(|p| owned_paths.contains(p))
        .cloned()
        .collect();
    loaded_owned.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    for path in loaded_owned {
        let Some(eb) = edits.buffers.get_mut(&path) else {
            continue;
        };
        let existing = eb.rope.to_string();
        let count = re.find_iter(&existing).count();
        if count == 0 {
            skip_paths.push(path);
            continue;
        }
        let replaced = re.replace_all(&existing, replacement);
        if replaced.as_ref() != existing {
            super::super::shared::bump(eb, ropey::Rope::from_str(replaced.as_ref()));
            edits
                .pending_replace_in_memory
                .push(led_state_buffer_edits::InMemoryReplace {
                    path: path.clone(),
                    count,
                });
        }
        skip_paths.push(path);
    }

    // Preview buffers — in-memory but stays clean. Driver skips
    // them via skip_paths; we wrote the content locally and
    // pending_single_replace is not queued (the rope reflects the
    // final state directly).
    let mut loaded_preview: Vec<led_core::CanonPath> = edits
        .buffers
        .keys()
        .filter(|p| preview_paths.contains(p))
        .cloned()
        .collect();
    loaded_preview.sort_by(|a, b| a.as_path().cmp(b.as_path()));
    for path in loaded_preview {
        let Some(eb) = edits.buffers.get_mut(&path) else {
            continue;
        };
        let existing = eb.rope.to_string();
        let count = re.find_iter(&existing).count();
        if count == 0 {
            skip_paths.push(path);
            continue;
        }
        let replaced = re.replace_all(&existing, replacement);
        if replaced.as_ref() != existing {
            super::super::shared::bump(eb, ropey::Rope::from_str(replaced.as_ref()));
            // Preview stays clean — saved_version tracks the disk
            // state which the driver is about to write to match.
            eb.saved_version = SavedVersion(eb.version.0);
        eb.disk_content_hash =
            led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
            edits
                .pending_replace_in_memory
                .push(led_state_buffer_edits::InMemoryReplace {
                    path: path.clone(),
                    count,
                });
        }
        // Preview paths NOT added to skip_paths — the driver
        // writes disk, our in-memory rope mirrors the same regex
        // result. Both converge on identical content.
    }

    if let Some(root) = fs_root {
        edits.pending_replace_all.push(
            led_state_buffer_edits::PendingReplaceAll {
                root: root.clone(),
                query: state.query.text.clone(),
                replacement: replacement.to_string(),
                case_sensitive: state.case_sensitive,
                use_regex: state.use_regex,
                skip_paths,
            },
        );
    }
}
