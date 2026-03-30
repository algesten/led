use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Alert, DocId};
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
            Ok(DocStoreIn::Opened {
                id,
                path,
                doc,
                tab_order,
            }) => {
                // Preview open: check if pending_preview matches this path
                let is_preview = (*state.preview.pending)
                    .as_ref()
                    .map_or(false, |req| req.path == path);

                if is_preview {
                    let req = (*state.preview.pending).as_ref().unwrap();
                    let row = req.row.min(doc.line_count().saturating_sub(1));
                    let col = req.col;
                    let buffer_height = state.dims.map_or(20, |d| d.buffer_height());
                    let notify_hash = led_workspace::path_hash(&path);

                    let mut buf = BufferState::new(id, doc, Some(path), Default::default());
                    buf.set_cursor(led_core::Row(row), led_core::Col(col), led_core::Col(col));
                    buf.set_scroll(
                        led_core::Row(row.saturating_sub(buffer_height / 2)),
                        led_core::SubLine(0),
                    );
                    buf.set_tab_order(led_core::TabOrder(tab_order));
                    buf.set_preview(true);
                    let remove_old_path = state.preview.buffer.clone();
                    let remove_old_hash = remove_old_path.as_ref().and_then(|pp| {
                        state
                            .notify_hash_to_buffer
                            .iter()
                            .find(|(_, v)| *v == pp)
                            .map(|(k, _)| k.clone())
                    });
                    let pre_preview_buffer = if state.preview.pre_preview_buffer.is_some() {
                        state.preview.pre_preview_buffer.clone()
                    } else {
                        state.active_buffer.clone()
                    };

                    return Some(Mut::PreviewOpen {
                        buf,
                        notify_hash,
                        remove_old_path,
                        remove_old_hash,
                        pre_preview_buffer,
                    });
                }

                // Duplicate detection: activate existing tab if same path is already open
                // (only if it's a loaded buffer, not a ghost)
                if state.buffers.get(&path).is_some_and(|b| b.is_loaded()) {
                    return Some(Mut::ActivateBuffer(path));
                }

                // Stale preview detection: when a preview buffer is active,
                // the preview_open derived stream may have in-flight opens
                // for files the user has already scrolled past.  Drop them
                // unless they were explicitly requested by a user action.
                if state.preview.buffer.is_some() {
                    let requested_by_user = (*state.pending_open).as_ref() == Some(&path)
                        || state.startup.arg_paths.contains(&path)
                        || state.session.positions.contains_key(&path);
                    if !requested_by_user {
                        return None;
                    }
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

                let save_state = if distance_from_save != 0 {
                    SaveState::Modified
                } else {
                    SaveState::Clean
                };

                // Apply pending jump position if this buffer matches
                let pending_jump = state.jump.pending_position.as_ref().and_then(|p| {
                    if Some(&p.path) == Some(&path) {
                        Some(p.clone())
                    } else {
                        None
                    }
                });
                let clear_pending_jump = pending_jump.is_some();

                let (cursor_row, cursor_col, scroll_row) = match &pending_jump {
                    Some(p) => (
                        p.row.min(doc.line_count().saturating_sub(1)),
                        p.col,
                        p.scroll_offset,
                    ),
                    None => (cursor_row, cursor_col, scroll_row),
                };

                let mut buf = BufferState::new(id, doc, Some(path.clone()), Default::default());

                // Replay persisted undo entries into the buffer
                if let Some(undo) = undo_data {
                    if undo.content_hash == *content_hash {
                        apply_undo_entries(&mut buf, &undo.entries);
                        // Override distance_from_save to match persisted value,
                        // since replay accumulates directions which may differ
                        // from the distance at the time of the last flush.
                        buf.set_undo_distance_from_save(undo.distance_from_save);
                    }
                }

                buf.set_cursor(
                    led_core::Row(cursor_row),
                    led_core::Col(cursor_col),
                    led_core::Col(cursor_col),
                );
                buf.set_scroll(
                    led_core::Row(scroll_row),
                    led_core::SubLine(scroll_sub_line),
                );
                buf.set_tab_order(led_core::TabOrder(tab_order));
                if save_state == SaveState::Modified {
                    buf.mark_modified_if_dirty();
                }
                buf.restore_session(persisted_undo_len, chain_id, last_seen_seq, content_hash);
                let activate = if is_startup_arg {
                    is_last_arg
                } else {
                    !is_session_restore || state.session.active_tab_order == Some(tab_order)
                };
                Some(Mut::BufferOpen {
                    path,
                    buf,
                    activate,
                    notify_hash,
                    session_restore_done: is_session_restore && state.session.positions.len() == 1,
                    clear_pending_jump,
                })
            }
            Ok(DocStoreIn::Saved { id, doc }) => {
                let buf = find_buf_by_doc_id(&state, id)?;
                let path = buf.path_buf().cloned()?;
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
            Ok(DocStoreIn::SavedAs { id, path, doc }) => {
                let buf = find_buf_by_doc_id(&state, id)?;
                let old_path = buf.path_buf().cloned()?;
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
            Ok(DocStoreIn::ExternalChange { id, path, doc }) => {
                // Try DocId first, fall back to path — DocId mismatch happens
                // when a file was re-opened as a duplicate (ActivateBuffer
                // instead of BufferOpen): the buffer keeps the original DocId
                // but the docstore assigned a new one for the watcher.
                let buf = find_buf_by_doc_id(&state, id).or_else(|| state.buffers.get(&path));
                let buf = match buf {
                    Some(b) => b,
                    None => {
                        log::trace!(
                            "ExternalChange: no buffer for doc_id {:?} or path {}",
                            id,
                            path.display()
                        );
                        return None;
                    }
                };
                let buf_path = buf.path_buf().cloned().unwrap_or_else(|| path.clone());
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
            Ok(DocStoreIn::ExternalRemove { .. }) => None,
            Ok(DocStoreIn::OpenFailed { path }) => Some(Mut::SessionOpenFailed { path }),
            Err(a) => Some(Mut::alert(a)),
        })
        .stream()
}

fn find_buf_by_doc_id<'a>(state: &'a AppState, doc_id: DocId) -> Option<&'a Rc<BufferState>> {
    state.buffers.values().find(|b| b.doc_id() == doc_id)
}

/// Apply persisted undo entries to a buffer, restoring edit history.
fn apply_undo_entries(buf: &mut BufferState, entries: &[Vec<u8>]) {
    buf.close_undo_group();
    for entry_data in entries {
        let Ok(entry) = rmp_serde::from_slice::<led_core::UndoEntry>(entry_data) else {
            continue;
        };
        let doc = led_core::apply_op_to_doc(buf.doc(), &entry.op);
        buf.apply_remote_entry(doc, entry);
    }
}
