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
                let buf_id = BufferId(state.next_buffer_id);
                let tab_order = state.buffers.len();
                let buf = BufferState {
                    id: buf_id,
                    doc_id: id,
                    doc,
                    path: Some(path),
                    cursor_row: 0,
                    cursor_col: 0,
                    cursor_col_affinity: 0,
                    scroll_row: 0,
                    scroll_sub_line: 0,
                    tab_order,
                    last_edit_kind: None,
                    save_state: SaveState::Clean,
                };
                Some(Mut::BufferOpen(buf, state.next_buffer_id + 1))
            }
            Ok(DocStoreIn::Saved { id }) => {
                let buf = find_buf_by_doc_id(&state, id)?;
                let mut buf = buf.clone();
                buf.doc = buf.doc.mark_saved();
                buf.save_state = SaveState::Clean;
                Some(Mut::BufferUpdate(buf.id, buf))
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
