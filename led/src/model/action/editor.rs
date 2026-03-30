use led_core::Action;
use led_state::AppState;

use super::super::mov;
use super::helpers::{close_group_on_move, with_buf};

pub(super) fn handle_editor_movement(state: &mut AppState, action: &Action) {
    match action {
        Action::MoveUp => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::move_up(buf, dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            close_group_on_move(buf);
        }),
        Action::MoveDown => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::move_down(buf, dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            close_group_on_move(buf);
        }),
        Action::PageUp => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::page_up(buf, dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            close_group_on_move(buf);
        }),
        Action::PageDown => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::page_down(buf, dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            close_group_on_move(buf);
        }),
        Action::FileStart => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::file_start();
            let a = mov::reset_affinity(buf, dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            close_group_on_move(buf);
        }),
        Action::FileEnd => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::file_end(&**buf.doc());
            let a = mov::reset_affinity(buf, dims);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            close_group_on_move(buf);
        }),
        _ => {}
    }
}
