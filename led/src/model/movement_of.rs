use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Action, PanelSlot};
use led_state::AppState;

use super::mov;
use super::{Mut, has_any_input_modal, has_blocking_overlay, has_input_modal};

/// Editor movement streams: cursor movement + scroll + bracket matching.
pub fn movement_of(
    raw_actions: &Stream<Action>,
    actions_with_state: &Stream<(Action, Rc<AppState>)>,
    state: &Stream<Rc<AppState>>,
) -> Stream<Mut> {
    let line_start_s = raw_actions
        .filter(|a| matches!(a, Action::LineStart))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .filter(|(_, s)| s.focus == PanelSlot::Main)
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, _) = mov::line_start(&buf);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(0));
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(c),
                led_core::Col(mov::reset_affinity(&buf, &dims)),
            );
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let line_end_s = raw_actions
        .filter(|a| matches!(a, Action::LineEnd))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .filter(|(_, s)| s.focus == PanelSlot::Main)
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, _) = mov::line_end(&buf);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(0));
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(c),
                led_core::Col(mov::reset_affinity(&buf, &dims)),
            );
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let match_bracket_s = raw_actions
        .filter(|a| matches!(a, Action::MatchBracket))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .filter(|(_, _, buf)| buf.matching_bracket().is_some())
        .map(|(dims, path, mut buf)| {
            let (row, col) = buf.matching_bracket().unwrap();
            buf.set_cursor(row, col, led_core::Col(0));
            buf.set_cursor(row, col, led_core::Col(mov::reset_affinity(&buf, &dims)));
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let move_up_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::MoveUp))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, a) = mov::move_up(&buf, &dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let move_down_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::MoveDown))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, a) = mov::move_down(&buf, &dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let move_left_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::MoveLeft))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, _) = mov::move_left(&buf);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(0));
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(c),
                led_core::Col(mov::reset_affinity(&buf, &dims)),
            );
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let move_right_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::MoveRight))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, _) = mov::move_right(&buf);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(0));
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(c),
                led_core::Col(mov::reset_affinity(&buf, &dims)),
            );
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let page_up_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::PageUp))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, a) = mov::page_up(&buf, &dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let page_down_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::PageDown))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, a) = mov::page_down(&buf, &dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let file_start_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::FileStart))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, _) = mov::file_start();
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(c),
                led_core::Col(mov::reset_affinity(&buf, &dims)),
            );
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let file_end_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::FileEnd))
        .filter(|(_, s)| {
            !has_blocking_overlay(s) && !has_any_input_modal(s) && s.focus == PanelSlot::Main
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            let (r, c, _) = mov::file_end(&**buf.doc());
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(c),
                led_core::Col(mov::reset_affinity(&buf, &dims)),
            );
            buf.close_group_on_move();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let merged: Stream<Mut> = Stream::new();
    move_up_s.forward(&merged);
    move_down_s.forward(&merged);
    move_left_s.forward(&merged);
    move_right_s.forward(&merged);
    page_up_s.forward(&merged);
    page_down_s.forward(&merged);
    file_start_s.forward(&merged);
    file_end_s.forward(&merged);
    line_start_s.forward(&merged);
    line_end_s.forward(&merged);
    match_bracket_s.forward(&merged);
    merged
}
