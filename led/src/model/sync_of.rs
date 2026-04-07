use std::rc::Rc;

use led_core::rx::Stream;
use led_state::AppState;
use led_workspace::{SyncResultKind, WorkspaceIn};

use super::Mut;

/// Derive sync mutations from workspace events + latest state.
///
/// Looks up the buffer, hands the work off to `BufferState`, wraps the
/// result as a `Mut`. All validation and apply logic lives on the
/// buffer.
pub fn sync_of(workspace_in: &Stream<WorkspaceIn>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    workspace_in
        .sample_combine(state)
        .filter_map(|(ev, s)| match ev {
            WorkspaceIn::SyncResult { result } => resolve_sync(result, &s),
            _ => None,
        })
        .stream()
}

fn resolve_sync(result: SyncResultKind, state: &AppState) -> Option<Mut> {
    match result {
        SyncResultKind::NoChange { .. } => None,

        SyncResultKind::ExternalSave { file_path } => {
            let buf = state.buffers.get(&file_path)?;
            let mut buf = (**buf).clone();
            buf.mark_externally_saved();
            Some(Mut::BufferUpdate(file_path, buf))
        }

        SyncResultKind::SyncEntries {
            file_path,
            chain_id,
            content_hash,
            entries,
            new_last_seen_seq,
        } => {
            let buf = state.buffers.get(&file_path)?;
            let mut buf = (**buf).clone();
            if !buf.try_apply_sync(chain_id, content_hash, &entries, new_last_seen_seq) {
                return None;
            }
            Some(Mut::BufferUpdate(file_path, buf))
        }
    }
}
