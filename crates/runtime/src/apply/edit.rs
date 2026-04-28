//! Edit-state helpers extracted from `lib.rs`.
//!
//! Verbatim moves of: `seed_edit_from_load`,
//! `auto_advance_arrow_follow`, `distance_from_save_for`.
//! Visibility bumped to `pub(crate)` so the main loop can keep
//! calling them.

use std::sync::Arc;

use led_state_buffer_edits::{BufferEdits, EditedBuffer};

use crate::dispatch;

/// Distance (in finalised groups) between the current head and
/// the most recent save-point marker. Used by legacy's `buffer_
/// undo_state.distance_from_save` for on-restore conflict
/// detection. We compute it on demand from `past`; legacy tracks
/// it incrementally on the doc, but the values agree at flush
/// time so the on-disk row is identical.
pub(crate) fn distance_from_save_for(eb: &EditedBuffer) -> i32 {
    let past = eb.history.past_groups();
    let last_save_idx = past
        .iter()
        .rposition(|g| g.save_point_hash.is_some());
    let after = match last_save_idx {
        Some(idx) => &past[idx + 1..],
        None => past,
    };
    after.iter().filter(|g| !g.ops.is_empty()).count() as i32
}

pub(crate) fn seed_edit_from_load(
    edits: &mut BufferEdits,
    path: led_core::CanonPath,
    rope: Arc<ropey::Rope>,
) -> bool {
    use imbl::hashmap::Entry;
    let seq_gen = edits.seq_gen.clone();
    match edits.buffers.entry(path) {
        Entry::Vacant(v) => {
            v.insert(EditedBuffer::fresh_with_seq_gen(rope, seq_gen));
            true
        }
        Entry::Occupied(_) => false,
    }
}

/// When a fresh find-file listing arrives AND the overlay is in
/// arrow-follow mode (user engaged arrow-nav, then descended via
/// Enter) AND nothing is currently selected, auto-select entry 0.
///
/// Mirrors what `move_selection` would do: rewrites `input` to
/// `dir_prefix(base_input) + entry.name`, keeps `show_side` up, and
/// creates a preview tab for file entries (capturing `tabs.active`
/// into `previous_tab` on the first preview). This lets the user
/// drill through directories by repeatedly pressing Enter without
/// needing to Down again after every listing arrives.
pub(crate) fn auto_advance_arrow_follow(
    ff: &mut led_state_find_file::FindFileState,
    tabs: &mut led_state_tabs::Tabs,
) {
    if !ff.arrow_follow || ff.completions.is_empty() || ff.selected.is_some() {
        return;
    }
    ff.selected = Some(0);
    ff.show_side = true;
    let base = led_state_find_file::dir_prefix(&ff.base_input).to_string();
    let entry = &ff.completions[0];
    let mut new_input = base;
    new_input.push_str(&entry.name);
    ff.input.set(new_input);
    if !entry.is_dir {
        if ff.previous_tab.is_none() {
            ff.previous_tab = tabs.active;
        }
        let path = entry.full.clone();
        dispatch::open_or_focus_tab(tabs, &path, /* promote= */ false);
    }
}
