use std::rc::Rc;

use led_core::Action;
use led_core::rx::Stream;
use led_state::AppState;

use super::{Mut, has_blocking_overlay};

/// Jump list streams: jump back and jump forward.
pub fn jump_of(raw_actions: &Stream<Action>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    // ── Jump back ──

    let jump_back_parent_s = raw_actions
        .filter(|a| matches!(a, Action::JumpBack))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .filter(|(_, s)| s.jump.index > 0)
        .stream();

    // Save current position when jumping back from present
    let jump_back_save_s = jump_back_parent_s
        .filter(|(_, s)| s.jump.index == s.jump.entries.len())
        .filter_map(|(_, s)| {
            let active = s.active_tab.as_ref()?;
            let buf = s.buffers.get(active)?;
            let p = buf.path()?;
            Some(Mut::JumpRecord(led_state::JumpPosition {
                path: p.clone(),
                row: buf.cursor_row(),
                col: buf.cursor_col(),
                scroll_offset: buf.scroll_row(),
            }))
        })
        .stream();

    let jump_back_index_s = jump_back_parent_s
        .map(|(_, s)| Mut::SetJumpIndex(s.jump.index - 1))
        .stream();

    // Navigate to target position (existing buffer -> BufferUpdate, or open new)
    let jump_back_nav_existing_s = jump_back_parent_s
        .filter_map(|(_, s)| {
            let pos = s.jump.entries.get(s.jump.index - 1)?;
            let buf = s.buffers.get(&pos.path)?;
            if !buf.is_materialized() {
                return None;
            }
            let buf = (**buf).clone();
            let r = led_core::Row((*pos.row).min(buf.doc().line_count().saturating_sub(1)));
            buf.set_cursor(r, pos.col, pos.col);
            buf.set_scroll(pos.scroll_offset, buf.scroll_sub_line());
            Some((pos.path.clone(), buf))
        })
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    let jump_back_activate_s = jump_back_parent_s
        .filter_map(|(_, s)| s.jump.entries.get(s.jump.index - 1).cloned())
        .map(|pos| Mut::ActivateBuffer(pos.path))
        .stream();

    let jump_back_open_s = jump_back_parent_s
        .filter_map(|(_, s)| {
            let pos = s.jump.entries.get(s.jump.index - 1)?;
            if s.buffers
                .get(&pos.path)
                .is_some_and(|b| b.is_materialized())
            {
                return None;
            }
            Some(pos.clone())
        })
        .map(|pos| Mut::RequestOpen(pos.path))
        .stream();

    let jump_back_cursor_s = jump_back_parent_s
        .filter_map(|(_, s)| {
            let pos = s.jump.entries.get(s.jump.index - 1)?;
            if s.buffers
                .get(&pos.path)
                .is_some_and(|b| b.is_materialized())
            {
                return None;
            }
            Some(pos.clone())
        })
        .map(|pos| Mut::SetTabPendingCursor {
            path: pos.path,
            row: pos.row,
            col: pos.col,
            scroll_row: pos.scroll_offset,
        })
        .stream();

    // ── Jump forward ──

    let jump_fwd_parent_s = raw_actions
        .filter(|a| matches!(a, Action::JumpForward))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .filter(|(_, s)| s.jump.index + 1 < s.jump.entries.len())
        .stream();

    let jump_fwd_index_s = jump_fwd_parent_s
        .map(|(_, s)| Mut::SetJumpIndex(s.jump.index + 1))
        .stream();

    let jump_fwd_nav_existing_s = jump_fwd_parent_s
        .filter_map(|(_, s)| {
            let pos = s.jump.entries.get(s.jump.index + 1)?;
            let buf = s.buffers.get(&pos.path)?;
            if !buf.is_materialized() {
                return None;
            }
            let buf = (**buf).clone();
            let r = led_core::Row((*pos.row).min(buf.doc().line_count().saturating_sub(1)));
            buf.set_cursor(r, pos.col, pos.col);
            buf.set_scroll(pos.scroll_offset, buf.scroll_sub_line());
            Some((pos.path.clone(), buf))
        })
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    let jump_fwd_activate_s = jump_fwd_parent_s
        .filter_map(|(_, s)| s.jump.entries.get(s.jump.index + 1).cloned())
        .map(|pos| Mut::ActivateBuffer(pos.path))
        .stream();

    let jump_fwd_open_s = jump_fwd_parent_s
        .filter_map(|(_, s)| {
            let pos = s.jump.entries.get(s.jump.index + 1)?;
            if s.buffers
                .get(&pos.path)
                .is_some_and(|b| b.is_materialized())
            {
                return None;
            }
            Some(pos.clone())
        })
        .map(|pos| Mut::RequestOpen(pos.path))
        .stream();

    let jump_fwd_cursor_s = jump_fwd_parent_s
        .filter_map(|(_, s)| {
            let pos = s.jump.entries.get(s.jump.index + 1)?;
            if s.buffers
                .get(&pos.path)
                .is_some_and(|b| b.is_materialized())
            {
                return None;
            }
            Some(pos.clone())
        })
        .map(|pos| Mut::SetTabPendingCursor {
            path: pos.path,
            row: pos.row,
            col: pos.col,
            scroll_row: pos.scroll_offset,
        })
        .stream();

    let merged: Stream<Mut> = Stream::new();
    jump_back_save_s.forward(&merged);
    jump_back_index_s.forward(&merged);
    jump_back_nav_existing_s.forward(&merged);
    jump_back_activate_s.forward(&merged);
    jump_back_open_s.forward(&merged);
    jump_back_cursor_s.forward(&merged);
    jump_fwd_index_s.forward(&merged);
    jump_fwd_nav_existing_s.forward(&merged);
    jump_fwd_activate_s.forward(&merged);
    jump_fwd_open_s.forward(&merged);
    jump_fwd_cursor_s.forward(&merged);
    merged
}
