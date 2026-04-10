use std::rc::Rc;

use led_core::Action;
use led_core::rx::Stream;
use led_state::AppState;

use super::edit;
use super::mov;
use super::{Mut, has_any_input_modal, has_blocking_overlay, is_indent_in_flight};

/// Kill ring streams: kill line and kill region.
pub fn kill_of(actions_with_state: &Stream<(Action, Rc<AppState>)>) -> Stream<Mut> {
    // ── Kill line ──

    let kill_line_parent_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::KillLine))
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && !has_any_input_modal(s)
                && !is_indent_in_flight(s)
                && !s.confirm_kill
        })
        .stream();

    let kill_line_buf_s = kill_line_parent_s
        .filter_map(|(_, s)| {
            let dims = s.dims?;
            let path = s.active_tab.clone()?;
            let mut buf = (**s.buffers.get(&path)?).clone();
            buf.close_group_on_move();
            let killed = edit::kill_line(&mut buf);
            if let Some((_, r, c, a)) = &killed {
                buf.set_cursor(led_core::Row(*r), led_core::Col(*c), led_core::Col(*a));
            }
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.touch();
            Some((path, buf, killed.map(|(k, _, _, _)| k)))
        })
        .map(|(path, buf, _)| Mut::BufferUpdate(path, buf))
        .stream();

    let kill_line_ring_s = kill_line_parent_s
        .filter_map(|(_, s)| {
            let path = s.active_tab.as_ref()?;
            let mut buf = (**s.buffers.get(path)?).clone();
            buf.close_group_on_move();
            let (killed, _, _, _) = edit::kill_line(&mut buf)?;
            Some(Mut::KillRingAccumulate(killed))
        })
        .stream();

    // ── Kill region ──

    let kill_region_parent_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::KillRegion))
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && !has_any_input_modal(s)
                && !is_indent_in_flight(s)
                && !s.confirm_kill
        })
        .stream();

    let kill_region_buf_s = kill_region_parent_s
        .filter_map(|(_, s)| {
            let dims = s.dims?;
            let path = s.active_tab.clone()?;
            let mut buf = (**s.buffers.get(&path)?).clone();
            buf.close_group_on_move();
            if let Some((_, r, c, a)) = edit::kill_region(&mut buf) {
                buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
                buf.clear_mark();
            } else {
                buf.clear_mark();
            }
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.touch();
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    let kill_region_ring_s = kill_region_parent_s
        .filter_map(|(_, s)| {
            let path = s.active_tab.as_ref()?;
            let mut buf = (**s.buffers.get(path)?).clone();
            buf.close_group_on_move();
            let (killed, _, _, _) = edit::kill_region(&mut buf)?;
            Some(Mut::KillRingSet(killed))
        })
        .stream();

    let kill_region_no_region_s = kill_region_parent_s
        .filter(|(_, s)| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .map_or(true, |b| b.mark().is_none())
        })
        .map(|_| Mut::Alert {
            info: Some("No region".into()),
        })
        .stream();

    let merged: Stream<Mut> = Stream::new();
    kill_line_buf_s.forward(&merged);
    kill_line_ring_s.forward(&merged);
    kill_region_buf_s.forward(&merged);
    kill_region_ring_s.forward(&merged);
    kill_region_no_region_s.forward(&merged);
    merged
}
