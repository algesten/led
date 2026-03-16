use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Alert, BufferId, DocId};
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

                // Apply restored session positions if available
                let (cursor_row, cursor_col, scroll_row, scroll_sub_line, tab_order) =
                    match state.session_positions.get(&path) {
                        Some(sp) => (
                            sp.cursor_row.min(doc.line_count().saturating_sub(1)),
                            sp.cursor_col,
                            sp.scroll_row,
                            sp.scroll_sub_line,
                            sp.tab_order,
                        ),
                        None => (0, 0, 0, 0, state.buffers.len()),
                    };

                let is_session_restore = state.session_positions.contains_key(&path);
                let notify_hash = led_workspace::path_hash(&path);

                let content_hash = doc.content_hash();
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
                    save_state: SaveState::Clean,
                    persisted_undo_len: 0,
                    chain_id: None,
                    last_seen_seq: 0,
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
