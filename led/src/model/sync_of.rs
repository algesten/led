use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Doc, UndoGroup};
use led_state::AppState;
use led_workspace::{SyncResultKind, WorkspaceIn};

use super::Mut;

/// Derive sync mutations from workspace events + latest state.
///
/// Resolves buffer lookups, deserializes undo groups, and applies doc
/// edits in the combinator chain — the reducer only assigns fields.
pub fn sync_of(workspace_in: &Stream<WorkspaceIn>, state: &Stream<Arc<AppState>>) -> Stream<Mut> {
    workspace_in
        .sample_combine(state)
        .map(|(ev, s)| match ev {
            WorkspaceIn::SyncResult { result } => resolve_sync(result, &s),
            _ => None,
        })
        .filter(|opt| opt.is_some())
        .map(|opt| opt.unwrap())
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
            Some(Mut::SyncReset { buf_id: buf.id })
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
            let doc = apply_remote_entries(&buf.doc, &entries);
            Some(Mut::SyncApply {
                buf_id: buf.id,
                doc,
                chain_id: buf.chain_id.clone(),
                last_seen_seq: new_last_seen_seq,
            })
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
            if buf.content_hash != content_hash {
                log::info!(
                    "sync: content hash mismatch for {}, expecting docstore reload",
                    file_path.display()
                );
            }
            let doc = apply_remote_entries(&buf.doc, &entries);
            Some(Mut::SyncApply {
                buf_id: buf.id,
                doc,
                chain_id: Some(new_chain_id),
                last_seen_seq: new_last_seen_seq,
            })
        }
    }
}

fn apply_remote_entries(doc: &Arc<dyn Doc>, entries: &[Vec<u8>]) -> Arc<dyn Doc> {
    let mut doc = doc.close_undo_group();
    for entry_data in entries {
        let Ok(group) = rmp_serde::from_slice::<UndoGroup>(entry_data) else {
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
