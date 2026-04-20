//! Tab cycling + kill-buffer (M1, M6).

use led_state_buffer_edits::BufferEdits;
use led_state_tabs::{Tab, Tabs};

pub(super) fn cycle_active(tabs: &mut Tabs, delta: isize) {
    if tabs.open.is_empty() {
        return;
    }
    let n = tabs.open.len() as isize;
    let cur_idx = tabs
        .active
        .and_then(|id| tabs.open.iter().position(|t: &Tab| t.id == id))
        .unwrap_or(0) as isize;
    let next_idx = (cur_idx + delta).rem_euclid(n) as usize;
    tabs.active = Some(tabs.open[next_idx].id);
}

/// Close the active tab. M6 no-ops if the buffer is dirty (no
/// confirm-kill prompt yet — M9 adds that). After a successful kill,
/// activate the neighbour tab or `None` if this was the last.
pub(super) fn kill_active(tabs: &mut Tabs, edits: &mut BufferEdits) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    // Guard against losing unsaved work. M9 swaps this for a
    // confirm-kill prompt.
    if let Some(eb) = edits.buffers.get(&tabs.open[idx].path)
        && eb.dirty()
    {
        return;
    }
    let path = tabs.open[idx].path.clone();
    tabs.open.remove(idx);
    edits.buffers.remove(&path);
    edits.pending_saves.remove(&path);
    if tabs.open.is_empty() {
        tabs.active = None;
    } else {
        let next = idx.min(tabs.open.len() - 1);
        tabs.active = Some(tabs.open[next].id);
    }
}
