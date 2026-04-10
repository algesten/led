use std::rc::Rc;

use led_core::Action;
use led_core::rx::Stream;
use led_state::AppState;

use super::edit;
use super::mov;
use super::{
    Mut, active_buf, has_any_input_modal, has_blocking_overlay, has_input_modal,
    is_indent_in_flight, is_word_boundary,
};

/// Editing streams: insert, delete, undo/redo, set mark, sort imports.
pub fn editing_of(
    raw_actions: &Stream<Action>,
    actions_with_state: &Stream<(Action, Rc<AppState>)>,
    state: &Stream<Rc<AppState>>,
) -> Stream<Mut> {
    let insert_char_parent_s = actions_with_state
        .filter_map(|(a, s)| match a {
            Action::InsertChar(ch) => Some((ch, s)),
            _ => None,
        })
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && !has_any_input_modal(s)
                && !is_indent_in_flight(s)
                && !s.confirm_kill
        })
        .stream();

    let insert_char_buf_s = insert_char_parent_s
        .map(|(ch, s)| {
            let close = active_buf(&s).map_or(false, |b| {
                b.last_edit_kind() != Some(led_state::EditKind::Insert)
                    || (ch.is_whitespace() && is_word_boundary(b))
            });
            (ch, close, s)
        })
        .filter_map(|(ch, close, s)| Some((ch, close, s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(ch, close, dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((ch, close, dims, path, buf))
        })
        .map(|(ch, close, dims, path, mut buf)| {
            buf.clear_mark();
            if close {
                buf.close_undo_group();
            }
            let (r, c, _) = edit::insert_char(&mut buf, ch);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(0));
            buf.set_cursor(
                led_core::Row(r),
                led_core::Col(c),
                led_core::Col(mov::reset_affinity(&buf, &dims)),
            );
            if buf.reindent_chars().contains(&ch) {
                buf.request_indent(Some(led_core::Row(r)), false);
            }
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let insert_char_complete_s = insert_char_parent_s
        .filter(|(ch, _)| ch.is_alphanumeric() || *ch == '_')
        .sample_combine(state)
        .filter(|(_, s)| s.lsp.completion.is_none())
        .filter(|(_, s)| active_buf(s).map_or(false, |b| !b.completion_triggers().is_empty()))
        .map(|_| Mut::LspRequestPending(Some(led_state::LspRequest::Complete)))
        .stream();

    let insert_newline_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::InsertNewline))
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && !has_any_input_modal(s)
                && !is_indent_in_flight(s)
                && !s.confirm_kill
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            buf.clear_mark();
            buf.close_group_on_move();
            let (r, c, a) = edit::insert_newline(&mut buf);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            buf.close_group_on_move();
            buf.request_indent(Some(led_core::Row(r)), false);
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let insert_tab_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::InsertTab))
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && !has_any_input_modal(s)
                && !is_indent_in_flight(s)
                && !s.confirm_kill
        })
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            buf.clear_mark();
            buf.close_group_on_move();
            buf.request_indent(Some(buf.cursor_row()), true);
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let delete_backward_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::DeleteBackward))
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && !has_any_input_modal(s)
                && !is_indent_in_flight(s)
                && !s.confirm_kill
        })
        .filter_map(|(_, s)| {
            let close = active_buf(&s).map_or(false, |b| {
                b.last_edit_kind() != Some(led_state::EditKind::Delete)
            });
            Some((close, s.dims?, s.active_tab.clone()?, s))
        })
        .filter_map(|(close, dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((close, dims, path, buf))
        })
        .map(|(close, dims, path, mut buf)| {
            buf.clear_mark();
            if close {
                buf.close_undo_group();
            }
            if let Some((r, c, _)) = edit::delete_backward(&mut buf) {
                buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(0));
                buf.set_cursor(
                    led_core::Row(r),
                    led_core::Col(c),
                    led_core::Col(mov::reset_affinity(&buf, &dims)),
                );
            }
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let delete_forward_s = actions_with_state
        .filter(|(a, _)| matches!(a, Action::DeleteForward))
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && !has_any_input_modal(s)
                && !is_indent_in_flight(s)
                && !s.confirm_kill
        })
        .filter_map(|(_, s)| {
            let close = active_buf(&s).map_or(false, |b| {
                b.last_edit_kind() != Some(led_state::EditKind::Delete)
            });
            Some((close, s.dims?, s.active_tab.clone()?, s))
        })
        .filter_map(|(close, dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((close, dims, path, buf))
        })
        .map(|(close, dims, path, mut buf)| {
            buf.clear_mark();
            if close {
                buf.close_undo_group();
            }
            if let Some((r, c, _)) = edit::delete_forward(&mut buf) {
                buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(0));
                buf.set_cursor(
                    led_core::Row(r),
                    led_core::Col(c),
                    led_core::Col(mov::reset_affinity(&buf, &dims)),
                );
            }
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let undo_s = raw_actions
        .filter(|a| matches!(a, Action::Undo))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            buf.close_group_on_move();
            if let Some(cursor) = buf.undo() {
                let row = buf.doc().char_to_line(cursor);
                let col = cursor.0 - buf.doc().line_to_char(row).0;
                buf.set_cursor(row, led_core::Col(col), led_core::Col(0));
                buf.set_cursor(
                    row,
                    led_core::Col(col),
                    led_core::Col(mov::reset_affinity(&buf, &dims)),
                );
            }
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let redo_s = raw_actions
        .filter(|a| matches!(a, Action::Redo))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            buf.close_group_on_move();
            if let Some(cursor) = buf.redo() {
                let row = buf.doc().char_to_line(cursor);
                let col = cursor.0 - buf.doc().line_to_char(row).0;
                buf.set_cursor(row, led_core::Col(col), led_core::Col(0));
                buf.set_cursor(
                    row,
                    led_core::Col(col),
                    led_core::Col(mov::reset_affinity(&buf, &dims)),
                );
            }
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.update_matching_bracket();
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    // SetMark: set mark on active buffer + show alert
    let set_mark_buf_s = raw_actions
        .filter(|a| matches!(a, Action::SetMark))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .filter_map(|(_, s)| Some((s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((dims, path, buf))
        })
        .map(|(dims, path, mut buf)| {
            buf.set_mark();
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            buf.touch();
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    let set_mark_alert_s = raw_actions
        .filter(|a| matches!(a, Action::SetMark))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s) && !has_input_modal(s))
        .map(|_| Mut::Alert {
            info: Some("Mark set".into()),
        })
        .stream();

    // SortImports: compute sorted text -> BufferUpdate + Alert
    let sort_imports_parent_s = raw_actions
        .filter(|a| matches!(a, Action::SortImports))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .stream();

    let sort_imports_buf_s = sort_imports_parent_s
        .filter_map(|(_, s)| {
            let path = s.active_tab.clone()?;
            let buf = s.buffers.get(&path)?;
            let file_path = buf.path()?;
            let ss = led_syntax::SyntaxState::from_path_and_doc(file_path.as_path(), &**buf.doc())?;
            let import_items = ss.imports(&**buf.doc());
            let (start_byte, end_byte, replacement) =
                led_syntax::import::sort_imports_text(&**buf.doc(), &import_items)?;
            let start_char = led_core::CharOffset(buf.doc().byte_to_char(start_byte));
            let end_char = led_core::CharOffset(buf.doc().byte_to_char(end_byte));
            let edit_row = buf.doc().char_to_line(start_char);
            let mut buf = (**buf).clone();
            buf.close_group_on_move();
            buf.edit_at(edit_row, |doc| {
                let d = doc.remove(start_char, end_char);
                let d = d.insert(start_char, &replacement);
                let old_text = doc.slice(start_char, end_char);
                let ops = vec![led_core::EditOp {
                    offset: start_char,
                    old_text,
                    new_text: replacement.clone(),
                }];
                (d, ops, ())
            });
            buf.touch();
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    let sort_imports_alert_s = sort_imports_parent_s
        .map(|(_, s)| {
            let sorted = s
                .active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .and_then(|buf| buf.path())
                .and_then(|fp| {
                    led_syntax::SyntaxState::from_path_and_doc(
                        fp.as_path(),
                        &**s.buffers.get(s.active_tab.as_ref()?).unwrap().doc(),
                    )
                })
                .map(|ss| {
                    let buf = s.buffers.get(s.active_tab.as_ref().unwrap()).unwrap();
                    let items = ss.imports(&**buf.doc());
                    led_syntax::import::sort_imports_text(&**buf.doc(), &items).is_some()
                })
                .unwrap_or(false);
            if sorted {
                Mut::Alert {
                    info: Some("Imports sorted".into()),
                }
            } else {
                Mut::Alert {
                    info: Some("Imports already sorted".into()),
                }
            }
        })
        .stream();

    let merged: Stream<Mut> = Stream::new();
    insert_char_buf_s.forward(&merged);
    insert_char_complete_s.forward(&merged);
    insert_newline_s.forward(&merged);
    insert_tab_s.forward(&merged);
    delete_backward_s.forward(&merged);
    delete_forward_s.forward(&merged);
    undo_s.forward(&merged);
    redo_s.forward(&merged);
    set_mark_buf_s.forward(&merged);
    set_mark_alert_s.forward(&merged);
    sort_imports_buf_s.forward(&merged);
    sort_imports_alert_s.forward(&merged);
    merged
}
