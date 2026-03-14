use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Alert, BufferId};
use led_docstore::DocStoreIn;
use led_state::{AppState, BufferState};

use super::Mut;

/// Derive buffer state from docstore events + latest state.
pub fn buffers_of(
    docstore: &Stream<Result<DocStoreIn, Alert>>,
    state: &Stream<Arc<AppState>>,
) -> Stream<Mut> {
    docstore
        .sample_combine(state)
        .filter_map(move |(result, state)| match result {
            Ok(DocStoreIn::Opened { path, doc, .. }) => {
                let id = BufferId(state.next_buffer_id);
                let tab_order = state.buffers.len();
                let buf = BufferState {
                    id,
                    doc,
                    path: Some(path),
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    tab_order,
                };
                Some(Mut::BufferOpen(buf, state.next_buffer_id + 1))
            }
            Ok(DocStoreIn::Saved { .. }) => None,
            Ok(DocStoreIn::ExternalChange { .. }) => None,
            Ok(DocStoreIn::ExternalRemove { .. }) => None,
            Err(a) => Some(Mut::alert(a)),
        })
        .stream()
}
