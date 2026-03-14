use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Alert, BufferId};
use led_state::{AppState, BufferState};
use led_storage::StorageIn;

use super::Mut;

/// Derive buffer state from storage events + latest state.
pub fn buffers_of(
    storage: &Stream<Result<StorageIn, Alert>>,
    state: &Stream<Arc<AppState>>,
) -> Stream<Mut> {
    storage.sample_combine(state).filter_map(move |(result, state)| {
        match result {
            Ok(StorageIn::Opened(path, doc)) => {
                let id = BufferId(state.next_buffer_id);
                let tab_order = state.buffers.len();
                let buf = BufferState {
                    id,
                    doc: Box::new(doc),
                    path: Some(path),
                    cursor_row: 0,
                    cursor_col: 0,
                    scroll_row: 0,
                    tab_order,
                };
                Some(Mut::BufferOpen(buf, state.next_buffer_id + 1))
            }
            Ok(StorageIn::Saved(_)) => None,
            Ok(StorageIn::Changed(_)) => None,
            Ok(StorageIn::Removed(_)) => None,
            Err(a) => Some(Mut::alert(a)),
        }
    }).stream()
}
