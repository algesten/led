use std::sync::Arc;

use led_core::{AStream, Alert, BufferId, StreamOpsExt, TextDoc};
use led_state::{AppState, BufferState};
use led_storage::StorageIn;
use tokio_stream::StreamExt;

use crate::model::Mut;

pub fn buffers_of(
    storage: impl AStream<Result<StorageIn, Alert>>,
    state: impl AStream<Arc<AppState>>,
) -> impl AStream<Mut> {
    storage
        .sample_combine(state)
        .filter_map(|(result, state)| match result {
            Ok(StorageIn::Opened(path, reader)) => match TextDoc::from_reader(reader) {
                Ok(doc) => {
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
                Err(e) => Some(Mut::Warn(Some(format!(
                    "Failed to read {}: {e}",
                    path.display()
                )))),
            },
            Ok(StorageIn::Saved(_)) => None,
            Ok(StorageIn::Changed(_)) => None,
            Ok(StorageIn::Removed(_)) => None,
            Err(Alert::Info(msg)) => Some(Mut::Info(Some(msg))),
            Err(Alert::Warn(msg)) => Some(Mut::Warn(Some(msg))),
        })
}
