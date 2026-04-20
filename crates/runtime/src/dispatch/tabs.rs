//! Tab cycling + kill-buffer (M1, M6, M9).

use led_state_alerts::AlertState;
use led_state_buffer_edits::BufferEdits;
use led_state_tabs::{Tab, TabId, Tabs};

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

/// Close the active tab. If the buffer is dirty, raise a confirm-kill
/// prompt (user must press `y`/`Y` to proceed); otherwise force-kill
/// immediately.
pub(super) fn kill_active(tabs: &mut Tabs, edits: &mut BufferEdits, alerts: &mut AlertState) {
    let Some(id) = tabs.active else {
        return;
    };
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let dirty = edits
        .buffers
        .get(&tabs.open[idx].path)
        .map(|eb| eb.dirty())
        .unwrap_or(false);
    if dirty {
        alerts.confirm_kill = Some(id);
        return;
    }
    force_kill(tabs, edits, id);
}

/// Unconditionally remove the tab with the given id. Drops its
/// pending-save entry and buffer edits. After removal, activates the
/// neighbour tab or `None` if this was the last.
pub(super) fn force_kill(tabs: &mut Tabs, edits: &mut BufferEdits, id: TabId) {
    let Some(idx) = tabs.open.iter().position(|t| t.id == id) else {
        return;
    };
    let path = tabs.open[idx].path.clone();
    tabs.open.remove(idx);
    edits.buffers.remove(&path);
    edits.pending_saves.remove(&path);
    if tabs.open.is_empty() {
        tabs.active = None;
    } else if tabs.active == Some(id) {
        let next = idx.min(tabs.open.len() - 1);
        tabs.active = Some(tabs.open[next].id);
    }
}
