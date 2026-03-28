use led_core::Action;
use led_state::{AppState, JumpPosition};

use super::super::{jump, search};
use super::helpers::with_buf;

/// Handle action while in incremental search mode.
/// Returns true if the action was consumed (don't fall through to normal handling).
pub(super) fn handle_isearch_action(state: &mut AppState, action: &Action) -> bool {
    match action {
        Action::InsertChar(c) => {
            with_buf(state, |buf, _dims| {
                buf.isearch.as_mut().unwrap().query.push(*c);
                search::update_search(buf);
            });
            true
        }
        Action::DeleteBackward => {
            with_buf(state, |buf, _dims| {
                let empty = {
                    let is = buf.isearch.as_mut().unwrap();
                    is.query.pop();
                    is.query.is_empty()
                };
                if empty {
                    let is = buf.isearch.as_ref().unwrap();
                    buf.cursor_row = is.origin.0;
                    buf.cursor_col = is.origin.1;
                    let is = buf.isearch.as_mut().unwrap();
                    is.matches.clear();
                    is.match_idx = None;
                    is.failed = false;
                } else {
                    search::update_search(buf);
                }
            });
            true
        }
        Action::InBufferSearch => {
            with_buf(state, |buf, _dims| {
                search::search_next(buf);
            });
            true
        }
        Action::Abort => {
            with_buf(state, |buf, _dims| {
                search::search_cancel(buf);
            });
            true
        }
        Action::InsertNewline => {
            // Record jump from search origin before accepting
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buffers.get(&id) {
                    if let (Some(is), Some(path)) = (&buf.isearch, &buf.path) {
                        let cursor_moved =
                            buf.cursor_row != is.origin.0 || buf.cursor_col != is.origin.1;
                        if cursor_moved {
                            let pos = JumpPosition {
                                path: path.clone(),
                                row: is.origin.0,
                                col: is.origin.1,
                                scroll_offset: is.origin_scroll,
                            };
                            jump::record_jump(state, pos);
                        }
                    }
                }
            }
            with_buf(state, |buf, _dims| {
                search::search_accept(buf);
            });
            true
        }
        // Movement keys: accept search, then fall through to normal handling
        Action::MoveUp
        | Action::MoveDown
        | Action::MoveLeft
        | Action::MoveRight
        | Action::LineStart
        | Action::LineEnd
        | Action::PageUp
        | Action::PageDown
        | Action::FileStart
        | Action::FileEnd => {
            with_buf(state, |buf, _dims| {
                search::search_accept(buf);
            });
            false
        }
        // Pass through without exiting search
        Action::Resize(..) | Action::Quit | Action::Suspend => false,
        // Everything else: accept search and fall through
        _ => {
            with_buf(state, |buf, _dims| {
                search::search_accept(buf);
            });
            false
        }
    }
}
