use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Doc, UndoEntry, next_change_seq};
use led_state::{AppState, SaveState};
use led_workspace::{SyncResultKind, WorkspaceIn};

use super::Mut;

/// Derive sync mutations from workspace events + latest state.
///
/// Resolves buffer lookups, deserializes undo entries, and applies doc
/// edits in the combinator chain — the reducer only assigns fields.
pub fn sync_of(workspace_in: &Stream<WorkspaceIn>, state: &Stream<Arc<AppState>>) -> Stream<Mut> {
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
            let buf = state
                .buffers
                .values()
                .find(|b| b.path.as_ref() == Some(&file_path))?;
            let mut buf = (**buf).clone();
            buf.last_seen_seq = 0;
            buf.chain_id = None;
            buf.persisted_undo_len = buf.doc.undo_history_len();
            buf.change_seq = next_change_seq();
            if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                buf.doc = buf.doc.mark_saved();
            }
            Some(Mut::BufferUpdate(buf.id, buf))
        }

        SyncResultKind::ReplayEntries {
            file_path,
            entries,
            new_last_seen_seq,
        } => {
            let buf = state
                .buffers
                .values()
                .find(|b| b.path.as_ref() == Some(&file_path))?;
            // Guard: skip duplicate application
            if new_last_seen_seq <= buf.last_seen_seq {
                return None;
            }
            let doc = apply_remote_entries(&buf.doc, &entries);
            let mut buf = (**buf).clone();
            buf.doc = doc;
            buf.last_seen_seq = new_last_seen_seq;
            buf.persisted_undo_len = buf.doc.undo_history_len();
            buf.content_hash = buf.doc.content_hash();
            buf.change_seq = next_change_seq();
            Some(Mut::BufferUpdate(buf.id, buf))
        }

        SyncResultKind::ReloadAndReplay {
            file_path,
            new_chain_id,
            content_hash,
            entries,
            new_last_seen_seq,
        } => {
            let buf = state
                .buffers
                .values()
                .find(|b| b.path.as_ref() == Some(&file_path))?;
            // Safety check: skip if buffer is dirty and content_hash
            // mismatches — prevents clobbering local unsaved edits.
            if buf.doc.dirty() && buf.content_hash != content_hash {
                log::info!(
                    "sync: skipping ReloadAndReplay for dirty buffer {}, content hash mismatch",
                    file_path.display()
                );
                return None;
            }
            if buf.content_hash != content_hash {
                log::info!(
                    "sync: content hash mismatch for {}, expecting docstore reload",
                    file_path.display()
                );
            }
            // Guard: skip duplicate application
            if new_last_seen_seq <= buf.last_seen_seq {
                return None;
            }
            let doc = apply_remote_entries(&buf.doc, &entries);
            let mut buf = (**buf).clone();
            buf.doc = doc;
            buf.chain_id = Some(new_chain_id);
            buf.last_seen_seq = new_last_seen_seq;
            buf.persisted_undo_len = buf.doc.undo_history_len();
            buf.content_hash = buf.doc.content_hash();
            buf.change_seq = next_change_seq();
            Some(Mut::BufferUpdate(buf.id, buf))
        }
    }
}

fn apply_remote_entries(doc: &Arc<dyn Doc>, entries: &[Vec<u8>]) -> Arc<dyn Doc> {
    let mut doc = doc.close_undo_group();
    for entry_data in entries {
        let Ok(entry) = rmp_serde::from_slice::<UndoEntry>(entry_data) else {
            continue;
        };
        doc = doc.apply_remote_entry(&entry);
    }
    doc
}
