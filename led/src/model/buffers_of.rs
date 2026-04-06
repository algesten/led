use std::rc::Rc;

use led_core::Alert;
use led_core::rx::Stream;
use led_docstore::DocStoreIn;
use led_state::{AppState, BufferState, ChangeReason, SaveState};

use super::Mut;

/// Derive buffer state from docstore events + latest state.
pub fn buffers_of(
    docstore: &Stream<Result<DocStoreIn, Alert>>,
    state: &Stream<Rc<AppState>>,
) -> Stream<Mut> {
    docstore
        .sample_combine(state)
        .filter_map(move |(result, state)| match result {
            Ok(DocStoreIn::Opened { path, doc }) => {
                // Drop if this file is no longer in any tab (e.g. stale preview open
                // for a file the user already scrolled past).
                if !state.tabs.iter().any(|t| *t.path() == path) {
                    return None;
                }

                // Drop duplicate: buffer is already materialized.
                if state
                    .buffers
                    .get(&path)
                    .is_some_and(|b| b.is_materialized())
                {
                    return None;
                }

                // Apply restored session positions + undo if available
                let sp = state.session.positions.get(&path);
                let (cursor_row, cursor_col, scroll_row, scroll_sub_line) = match sp {
                    Some(sp) => (
                        sp.cursor_row.min(doc.line_count().saturating_sub(1)),
                        sp.cursor_col,
                        sp.scroll_row,
                        sp.scroll_sub_line,
                    ),
                    None => (0, 0, 0, 0),
                };

                // Restore undo history if content hash matches
                let undo_data = sp.and_then(|sp| sp.undo.as_ref());
                let (chain_id, persisted_undo_len, last_seen_seq, distance_from_save) =
                    match undo_data {
                        Some(undo) if undo.content_hash == doc.content_hash().0 => (
                            Some(undo.chain_id.clone()),
                            undo.entries.len(),
                            undo.last_seen_seq,
                            undo.distance_from_save,
                        ),
                        _ => (None, 0, 0, 0),
                    };

                let is_session_restore = sp.is_some();
                let is_startup_arg = state.startup.arg_paths.contains(&path);
                let is_last_arg = state.startup.arg_paths.last() == Some(&path);
                let notify_hash = led_workspace::path_hash(&path);
                let content_hash = doc.content_hash();

                let undo_entries = match undo_data {
                    Some(undo) if undo.content_hash == *content_hash => undo.entries.clone(),
                    _ => Vec::new(),
                };

                let activate = if is_startup_arg {
                    is_last_arg
                } else if is_session_restore {
                    let tab_index = state.tabs.iter().position(|t| *t.path() == path);
                    tab_index.is_some() && state.session.active_tab_order == tab_index
                } else {
                    true
                };
                Some(Mut::BufferOpen {
                    path,
                    doc,
                    cursor: (cursor_row, cursor_col),
                    scroll: (scroll_row, scroll_sub_line),
                    activate,
                    notify_hash,
                    undo_entries,
                    persisted_undo_len,
                    chain_id,
                    last_seen_seq,
                    distance_from_save,
                })
            }
            Ok(DocStoreIn::Saved { path, doc }) => {
                let buf = state.buffers.get(&path)?;
                let undo_clear_path = if buf.save_state() == SaveState::Saving {
                    Some(path.clone())
                } else {
                    None
                };
                let mut buf = (**buf).clone();
                buf.save_completed(doc);
                Some(Mut::BufferSaved {
                    path,
                    buf,
                    undo_clear_path,
                })
            }
            Ok(DocStoreIn::SavedAs { path, doc }) => {
                // path is the NEW path (where the file was saved to).
                // The active buffer is the one that initiated the save-as.
                let active_path = state.active_tab.as_ref()?;
                let buf = state.buffers.get(active_path)?;
                let old_path = buf.path().cloned()?;
                let undo_clear_path = if buf.save_state() == SaveState::Saving {
                    Some(old_path.clone())
                } else {
                    None
                };
                let mut buf = (**buf).clone();
                buf.save_as_completed(doc, path.clone());
                Some(Mut::BufferSavedAs {
                    path: old_path,
                    buf,
                    new_path: path,
                    undo_clear_path,
                })
            }
            Ok(DocStoreIn::ExternalChange { path, doc }) => {
                let buf = match state.buffers.get(&path) {
                    Some(b) => b,
                    None => {
                        log::trace!("ExternalChange: no buffer for path {}", path.display());
                        return None;
                    }
                };
                let buf_path = buf.path().cloned().unwrap_or_else(|| path.clone());
                let incoming_hash = doc.content_hash();
                if incoming_hash == buf.content_hash() {
                    log::trace!(
                        "ExternalChange: content_hash unchanged ({:#x}), skipping",
                        incoming_hash.0
                    );
                    if buf.is_dirty() && buf.save_state() == SaveState::Clean {
                        let mut buf = (**buf).clone();
                        buf.mark_externally_saved();
                        return Some(Mut::BufferUpdate(
                            buf_path,
                            buf,
                            ChangeReason::ExternalFileChange,
                        ));
                    }
                    return None;
                }
                if buf.is_dirty() {
                    log::trace!("ExternalChange: buffer is dirty, skipping");
                    return None;
                }
                log::trace!(
                    "ExternalChange: applying, hash {:#x} -> {:#x}",
                    buf.content_hash().0,
                    incoming_hash.0
                );
                let mut buf = (**buf).clone();
                buf.reload_from_disk(doc);
                Some(Mut::BufferUpdate(
                    buf_path,
                    buf,
                    ChangeReason::ExternalFileChange,
                ))
            }
            Ok(DocStoreIn::Opening { .. }) => None,
            Ok(DocStoreIn::ExternalRemove { .. }) => None,
            Ok(DocStoreIn::OpenFailed { path }) => Some(Mut::SessionOpenFailed { path }),
            Err(a) => Some(Mut::alert(a)),
        })
        .stream()
}

/// Apply persisted undo entries to a buffer, restoring edit history.
pub(super) fn apply_undo_entries(buf: &mut BufferState, entries: &[Vec<u8>]) {
    buf.close_undo_group();
    for entry_data in entries {
        let Ok(entry) = rmp_serde::from_slice::<led_core::UndoEntry>(entry_data) else {
            continue;
        };
        let doc = led_core::apply_op_to_doc(buf.doc(), &entry.op);
        buf.apply_remote_entry(doc, entry);
    }
}
