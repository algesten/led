use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{UndoEntry, apply_op_to_doc};
use led_state::{AppState, BufferState};
use led_workspace::{SyncResultKind, WorkspaceIn};

use super::Mut;

/// Derive sync mutations from workspace events + latest state.
///
/// Resolves buffer lookups, deserializes undo entries, and applies doc
/// edits in the combinator chain — the reducer only assigns fields.
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
            log::trace!(
                "sync: ExternalSave for {}, hash={:#x}",
                file_path.display(),
                buf.content_hash().0
            );
            let mut buf = (**buf).clone();
            buf.mark_externally_saved();
            Some(Mut::BufferUpdate(file_path, buf))
        }

        SyncResultKind::ReplayEntries {
            file_path,
            entries,
            new_last_seen_seq,
        } => {
            let buf = state.buffers.get(&file_path)?;
            log::trace!(
                "sync: ReplayEntries for {}, n_entries={}, hash_before={:#x}, last_seen={} new_seen={}",
                file_path.display(),
                entries.len(),
                buf.content_hash().0,
                buf.last_seen_seq(),
                new_last_seen_seq,
            );
            // Guard: skip duplicate application
            if new_last_seen_seq <= buf.last_seen_seq() {
                log::trace!("sync: skipping ReplayEntries, already seen");
                return None;
            }
            let mut buf = (**buf).clone();
            apply_remote_entries(&mut buf, &entries);
            log::trace!(
                "sync: ReplayEntries applied, hash_after={:#x}",
                buf.content_hash().0
            );
            buf.apply_sync_replay(new_last_seen_seq);
            Some(Mut::BufferUpdate(file_path, buf))
        }

        SyncResultKind::ReloadAndReplay {
            file_path,
            new_chain_id,
            content_hash,
            entries,
            new_last_seen_seq,
        } => {
            let buf = state.buffers.get(&file_path)?;
            // Safety check: skip if buffer is dirty and content_hash
            // mismatches — prevents clobbering local unsaved edits.
            if buf.is_dirty() && buf.content_hash() != content_hash {
                log::info!(
                    "sync: skipping ReloadAndReplay for dirty buffer {}, content hash mismatch",
                    file_path.display()
                );
                return None;
            }
            if buf.content_hash() != content_hash {
                log::info!(
                    "sync: content hash mismatch for {}, expecting docstore reload",
                    file_path.display()
                );
            }
            // Guard: skip duplicate application
            if new_last_seen_seq <= buf.last_seen_seq() {
                return None;
            }
            let mut buf = (**buf).clone();
            apply_remote_entries(&mut buf, &entries);
            buf.apply_sync_reload(new_chain_id, new_last_seen_seq);
            Some(Mut::BufferUpdate(file_path, buf))
        }
    }
}

/// Apply remote undo entries to a buffer: mutates both the doc (content)
/// and the undo history (on BufferState).
fn apply_remote_entries(buf: &mut BufferState, entries: &[UndoEntry]) {
    buf.close_undo_group();
    for entry in entries {
        let doc = apply_op_to_doc(buf.doc(), &entry.op);
        buf.apply_remote_entry(doc, entry.clone());
    }
}
