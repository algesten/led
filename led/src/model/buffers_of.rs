use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Alert, BufferId, Doc, DocId};
use led_docstore::DocStoreIn;
use led_state::{AppState, BufferState, SaveState};

use super::Mut;

/// Derive buffer state from docstore events + latest state.
pub fn buffers_of(
    docstore: &Stream<Result<DocStoreIn, Alert>>,
    state: &Stream<Arc<AppState>>,
) -> Stream<Mut> {
    docstore
        .sample_combine(state)
        .filter_map(move |(result, state)| match result {
            Ok(DocStoreIn::Opened { id, path, doc }) => {
                // Duplicate detection: activate existing tab if same path is already open
                if let Some(existing) = state
                    .buffers
                    .values()
                    .find(|b| b.path.as_ref() == Some(&path))
                {
                    return Some(Mut::ActivateBuffer(existing.id));
                }

                let buf_id = BufferId(state.next_buffer_id);

                // Apply restored session positions + undo if available
                let sp = state.session_positions.get(&path);
                let (cursor_row, cursor_col, scroll_row, scroll_sub_line, tab_order) = match sp {
                    Some(sp) => (
                        sp.cursor_row.min(doc.line_count().saturating_sub(1)),
                        sp.cursor_col,
                        sp.scroll_row,
                        sp.scroll_sub_line,
                        sp.tab_order,
                    ),
                    None => (0, 0, 0, 0, state.buffers.len()),
                };

                // Restore undo history if content hash matches
                let undo_data = sp.and_then(|sp| sp.undo.as_ref());
                let (doc, chain_id, persisted_undo_len, last_seen_seq, distance_from_save) =
                    match undo_data {
                        Some(undo) if undo.content_hash == doc.content_hash() => {
                            let restored = apply_undo_entries(&doc, &undo.entries);
                            (
                                restored,
                                Some(undo.chain_id.clone()),
                                undo.entries.len(),
                                undo.last_seen_seq,
                                undo.distance_from_save,
                            )
                        }
                        _ => (doc, None, 0, 0, 0),
                    };

                let is_session_restore = sp.is_some();
                let notify_hash = led_workspace::path_hash(&path);
                let content_hash = doc.content_hash();

                let save_state = if distance_from_save != 0 {
                    SaveState::Modified
                } else {
                    SaveState::Clean
                };

                let buf = BufferState {
                    id: buf_id,
                    doc_id: id,
                    doc,
                    path: Some(path),
                    cursor_row,
                    cursor_col,
                    cursor_col_affinity: cursor_col,
                    scroll_row,
                    scroll_sub_line,
                    tab_order,
                    last_edit_kind: None,
                    save_state,
                    persisted_undo_len,
                    chain_id,
                    last_seen_seq,
                    content_hash,
                };
                Some(Mut::BufferOpen {
                    buf,
                    next_id: state.next_buffer_id + 1,
                    activate: !is_session_restore
                        || state.session_active_tab_order == Some(tab_order),
                    notify_hash,
                    session_restore_done: is_session_restore && state.session_positions.len() == 1,
                })
            }
            Ok(DocStoreIn::Saved { id, doc }) => {
                let buf = find_buf_by_doc_id(&state, id)?;
                let undo_clear_path = if buf.save_state == SaveState::Saving {
                    buf.path.clone()
                } else {
                    None
                };
                let mut buf = buf.clone();
                buf.doc = doc;
                buf.save_state = SaveState::Clean;
                buf.persisted_undo_len = 0;
                buf.chain_id = None;
                buf.last_seen_seq = 0;
                buf.content_hash = buf.doc.content_hash();
                Some(Mut::BufferSaved {
                    id: buf.id,
                    buf,
                    undo_clear_path,
                })
            }
            Ok(DocStoreIn::ExternalChange { id, doc }) => {
                let buf = find_buf_by_doc_id(&state, id)?;
                let mut buf = buf.clone();
                buf.doc = doc;
                // Clamp cursor to new document bounds
                buf.cursor_row = buf.cursor_row.min(buf.doc.line_count().saturating_sub(1));
                buf.cursor_col = buf.cursor_col.min(buf.doc.line_len(buf.cursor_row));
                buf.cursor_col_affinity = buf.cursor_col;
                buf.last_edit_kind = None;
                Some(Mut::BufferUpdate(buf.id, buf))
            }
            Ok(DocStoreIn::ExternalRemove { .. }) => None,
            Err(a) => Some(Mut::alert(a)),
        })
        .stream()
}

fn find_buf_by_doc_id<'a>(state: &'a AppState, doc_id: DocId) -> Option<&'a BufferState> {
    state.buffers.values().find(|b| b.doc_id == doc_id)
}

/// Apply persisted undo entries to a doc, restoring edit history.
fn apply_undo_entries(doc: &Arc<dyn Doc>, entries: &[Vec<u8>]) -> Arc<dyn Doc> {
    let mut doc = doc.close_undo_group();
    for entry_data in entries {
        let Ok(group) = rmp_serde::from_slice::<led_core::UndoGroup>(entry_data) else {
            continue;
        };
        for op in &group.ops {
            if !op.old_text.is_empty() {
                let end = op.offset + op.old_text.chars().count();
                doc = doc.remove(op.offset, end);
            }
            if !op.new_text.is_empty() {
                doc = doc.insert(op.offset, &op.new_text);
            }
        }
        doc = doc.close_undo_group();
    }
    doc
}
