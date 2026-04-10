use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Action, CanonPath};
use led_state::{AppState, BufferState, JumpPosition};

use super::{Mut, search};

/// True if isearch is active and this action would be consumed by isearch.
pub fn is_consumed_by_isearch(a: &Action, s: &AppState) -> bool {
    is_in_isearch(s) && isearch_consumes(a)
}

pub fn is_in_isearch_pub(s: &AppState) -> bool {
    is_in_isearch(s)
}

fn is_in_isearch(s: &AppState) -> bool {
    s.active_tab
        .as_ref()
        .and_then(|p| s.buffers.get(p))
        .map_or(false, |b| b.isearch.is_some())
}

fn isearch_consumes(a: &Action) -> bool {
    matches!(
        a,
        Action::InsertChar(_) | Action::DeleteBackward | Action::Abort | Action::InsertNewline
    )
    // Note: InBufferSearch is NOT included — it's handled by handle_action
    // (start_search when not in isearch, search_next when in isearch).
}

fn with_isearch_buf(
    s: &AppState,
    f: impl FnOnce(&mut BufferState),
) -> Option<(CanonPath, BufferState)> {
    let path = s.active_tab.clone()?;
    let mut buf = (**s.buffers.get(&path)?).clone();
    f(&mut buf);
    Some((path, buf))
}

/// Isearch modal handling. Returns a Mut stream for isearch actions.
///
/// When isearch is active:
/// - Consumed actions (InsertChar, DeleteBackward, etc.) → isearch Muts
/// - Non-consumed, non-ignored actions → SearchAccept Mut (clears isearch)
/// - Ignored actions (Resize, Quit, Suspend) → nothing
///
/// The caller must ensure that consumed actions are NOT also handled by
/// their normal streams (use `isearch_consumes` in the action's guard).
pub fn isearch_of(
    with_state: &Stream<(Action, Rc<AppState>)>,
    _state: &Stream<Rc<AppState>>,
    _is_migrated: fn(&Action) -> bool,
) -> Stream<Mut> {
    // InsertChar during isearch → append to query
    let insert_char_s = with_state
        .filter_map(|(a, s)| match a {
            Action::InsertChar(ch) if is_in_isearch(&s) => Some((ch, s)),
            _ => None,
        })
        .filter_map(|(ch, s)| {
            with_isearch_buf(&s, |buf| {
                buf.isearch.as_mut().unwrap().query.push(ch);
                search::update_search(buf);
            })
        })
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    // DeleteBackward during isearch → pop from query
    let delete_s = with_state
        .filter(|(a, s)| matches!(a, Action::DeleteBackward) && is_in_isearch(&s))
        .filter_map(|(_, s)| {
            with_isearch_buf(&s, |buf| {
                let empty = {
                    let is = buf.isearch.as_mut().unwrap();
                    is.query.pop();
                    is.query.is_empty()
                };
                if empty {
                    let origin = buf.isearch.as_ref().unwrap().origin;
                    buf.set_cursor(origin.0, origin.1, origin.1);
                    let is = buf.isearch.as_mut().unwrap();
                    is.matches.clear();
                    is.match_idx = None;
                    is.failed = false;
                } else {
                    search::update_search(buf);
                }
            })
        })
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    // InBufferSearch stays in handle_action (start_search or search_next
    // depending on isearch state). Cannot be in isearch_of due to
    // sample_combine timing with handle_action's state mutations.

    // Abort during isearch → cancel (restore cursor to origin)
    let cancel_s = with_state
        .filter(|(a, s)| matches!(a, Action::Abort) && is_in_isearch(&s))
        .filter_map(|(_, s)| {
            with_isearch_buf(&s, |buf| {
                search::search_cancel(buf);
            })
        })
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    // InsertNewline during isearch → record jump + accept
    let accept_jump_s = with_state
        .filter(|(a, s)| matches!(a, Action::InsertNewline) && is_in_isearch(&s))
        .filter_map(|(_, s)| {
            let path = s.active_tab.as_ref()?;
            let buf = s.buffers.get(path)?;
            let is = buf.isearch.as_ref()?;
            let cursor_moved = buf.cursor_row() != is.origin.0 || buf.cursor_col() != is.origin.1;
            if !cursor_moved {
                return None;
            }
            Some(Mut::JumpRecord(JumpPosition {
                path: buf.path()?.clone(),
                row: is.origin.0,
                col: is.origin.1,
                scroll_offset: is.origin_scroll,
            }))
        })
        .stream();

    let accept_s = with_state
        .filter(|(a, s)| matches!(a, Action::InsertNewline) && is_in_isearch(&s))
        .filter_map(|(_, s)| {
            with_isearch_buf(&s, |buf| {
                search::search_accept(buf);
            })
        })
        .map(|(path, buf)| Mut::BufferUpdate(path, buf))
        .stream();

    // Non-consumed, non-ignored actions during isearch → accept search
    // (The action itself is still handled normally by its own stream.)
    // Accept isearch for non-consumed, non-ignored actions.
    // Excludes InBufferSearch (handled by handle_action: start_search or search_next).
    let accept_on_pass_s = with_state
        .filter(|(a, s)| {
            is_in_isearch(&s)
                && !isearch_consumes(a)
                && !matches!(
                    a,
                    Action::Resize(..) | Action::Quit | Action::Suspend | Action::InBufferSearch
                )
        })
        .map(|(_, s)| Mut::SearchAccept(s.active_tab.clone().unwrap()))
        .stream();

    let muts: Stream<Mut> = Stream::new();
    insert_char_s.forward(&muts);
    delete_s.forward(&muts);
    cancel_s.forward(&muts);
    accept_jump_s.forward(&muts);
    accept_s.forward(&muts);
    accept_on_pass_s.forward(&muts);
    muts
}
