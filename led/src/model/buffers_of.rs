use std::rc::Rc;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Alert, BufferId, Doc, DocId};
use led_docstore::DocStoreIn;
use led_state::{AppState, BufferState, SaveState};

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
                    let content_hash = doc.content_hash();
                    let notify_hash = led_workspace::path_hash(&path);

                    let buf = BufferState {
                        id: BufferId(state.next_buffer_id),
                        doc_id: id,
                        doc,
                        path: Some(path),
                        cursor_row: row,
                        cursor_col: col,
                        cursor_col_affinity: col,
                        scroll_row: row.saturating_sub(buffer_height / 2),
                        scroll_sub_line: 0,
                        tab_order,
                        mark: None,
                        last_edit_kind: None,
                        save_state: SaveState::Clean,
                        persisted_undo_len: 0,
                        chain_id: None,
                        last_seen_seq: 0,
                        content_hash,
                        change_seq: 0,
                        isearch: None,
                        last_search: None,
                        syntax_highlights: Rc::new(Vec::new()),
                        bracket_pairs: Rc::new(Vec::new()),
                        matching_bracket: None,
                        pending_indent_row: None,
                        pending_tab_fallback: false,
                        reindent_chars: Arc::from([]),
                        completion_triggers: Vec::new(),
                        is_preview: true,
                    };
                    let remove_old_id = state.preview.buffer;
                    let remove_old_hash = remove_old_id.and_then(|pid| {
                        state
                            .notify_hash_to_buffer
                            .iter()
                            .find(|(_, v)| **v == pid)
                            .map(|(k, _)| k.clone())
                    });
                    let pre_preview_buffer = if state.preview.pre_preview_buffer.is_some() {
                        state.preview.pre_preview_buffer
                    } else {
                        state.active_buffer
                    };

                    return Some(Mut::PreviewOpen {
                        buf,
                        next_id: state.next_buffer_id + 1,
                        notify_hash,
                        remove_old_id,
                        remove_old_hash,
                        pre_preview_buffer,
                    });
                }

                // Duplicate detection: activate existing tab if same path is already open
                if let Some(existing) = state
                    .buffers
                    .values()
                    .find(|b| b.path.as_ref() == Some(&path))
                {
                    return Some(Mut::ActivateBuffer(existing.id));
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

                let buf_id = BufferId(state.next_buffer_id);

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
                let (doc, chain_id, persisted_undo_len, last_seen_seq, distance_from_save) =
                    match undo_data {
                        Some(undo) if undo.content_hash == doc.content_hash() => {
                            let restored = apply_undo_entries(&doc, &undo.entries);
                            // Override distance_from_save to match persisted value,
                            // since replay accumulates directions which may differ
                            // from the distance at the time of the last flush.
                            let restored =
                                restored.with_distance_from_save(undo.distance_from_save);
                            (
                                restored,
                                Some(undo.chain_id.clone()),
                                undo.entries.len(),
                                undo.last_seen_seq,
                                undo.distance_from_save,
                            )
                        }
                        _ => (doc, None, 0, 0, 0),
                    };

                let is_session_restore = sp.is_some();
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

                let buf = BufferState {
                    id: buf_id,
                    doc_id: id,
                    doc,
                    path: Some(path),
                    cursor_row,
                    cursor_col,
                    cursor_col_affinity: cursor_col,
                    scroll_row,
                    scroll_sub_line,
                    tab_order,
                    mark: None,
                    last_edit_kind: None,
                    save_state,
                    persisted_undo_len,
                    chain_id,
                    last_seen_seq,
                    content_hash,
                    change_seq: 0,
                    isearch: None,
                    last_search: None,
                    syntax_highlights: Rc::new(Vec::new()),
                    bracket_pairs: Rc::new(Vec::new()),
                    matching_bracket: None,
                    pending_indent_row: None,
                    pending_tab_fallback: false,
                    reindent_chars: Arc::from([]),
                    completion_triggers: Vec::new(),
                    is_preview: false,
                };
                let activate =
                    !is_session_restore || state.session.active_tab_order == Some(tab_order);
                Some(Mut::BufferOpen {
                    buf,
                    next_id: state.next_buffer_id + 1,
                    activate,
                    notify_hash,
                    session_restore_done: is_session_restore && state.session.positions.len() == 1,
                    clear_pending_jump,
                })
            }
            Ok(DocStoreIn::Saved { id, doc }) => {
                let buf = find_buf_by_doc_id(&state, id)?;
                let undo_clear_path = if buf.save_state == SaveState::Saving {
                    buf.path.clone()
                } else {
                    None
                };
                let mut buf = (**buf).clone();
                buf.doc = doc;
                buf.save_state = SaveState::Clean;
                buf.persisted_undo_len = buf.doc.undo_history_len();
                buf.chain_id = None;
                buf.last_seen_seq = 0;
                buf.content_hash = buf.doc.content_hash();
                Some(Mut::BufferSaved {
                    id: buf.id,
                    buf,
                    undo_clear_path,
                })
            }
            Ok(DocStoreIn::SavedAs { id, path, doc }) => {
                let buf = find_buf_by_doc_id(&state, id)?;
                let undo_clear_path = if buf.save_state == SaveState::Saving {
                    buf.path.clone()
                } else {
                    None
                };
                let mut buf = (**buf).clone();
                buf.path = Some(path.clone());
                buf.doc = doc;
                buf.save_state = SaveState::Clean;
                buf.persisted_undo_len = buf.doc.undo_history_len();
                buf.chain_id = None;
                buf.last_seen_seq = 0;
                buf.content_hash = buf.doc.content_hash();
                Some(Mut::BufferSavedAs {
                    id: buf.id,
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
                let buf = find_buf_by_doc_id(&state, id).or_else(|| {
                    state
                        .buffers
                        .values()
                        .find(|b| b.path.as_ref() == Some(&path))
                });
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
                let incoming_hash = doc.content_hash();
                if incoming_hash == buf.content_hash {
                    log::trace!(
                        "ExternalChange: content_hash unchanged ({incoming_hash:#x}), skipping"
                    );
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        let mut buf = (**buf).clone();
                        buf.doc = buf.doc.mark_saved();
                        return Some(Mut::BufferUpdate(buf.id, buf));
                    }
                    return None;
                }
                if buf.doc.dirty() {
                    log::trace!("ExternalChange: buffer is dirty, skipping");
                    return None;
                }
                log::trace!(
                    "ExternalChange: applying, hash {:#x} -> {incoming_hash:#x}",
                    buf.content_hash
                );
                let mut buf = (**buf).clone();
                buf.doc = doc;
                buf.content_hash = incoming_hash;
                buf.change_seq = led_core::next_change_seq();
                // Clamp cursor to new document bounds
                buf.cursor_row = buf.cursor_row.min(buf.doc.line_count().saturating_sub(1));
                buf.cursor_col = buf.cursor_col.min(buf.doc.line_len(buf.cursor_row));
                buf.cursor_col_affinity = buf.cursor_col;
                buf.last_edit_kind = None;
                Some(Mut::BufferUpdate(buf.id, buf))
            }
            Ok(DocStoreIn::ExternalRemove { .. }) => None,
            Ok(DocStoreIn::OpenFailed { path }) => Some(Mut::SessionOpenFailed { path }),
            Err(a) => Some(Mut::alert(a)),
        })
        .stream()
}

fn find_buf_by_doc_id<'a>(state: &'a AppState, doc_id: DocId) -> Option<&'a Rc<BufferState>> {
    state.buffers.values().find(|b| b.doc_id == doc_id)
}

/// Apply persisted undo entries to a doc, restoring edit history.
fn apply_undo_entries(doc: &Arc<dyn Doc>, entries: &[Vec<u8>]) -> Arc<dyn Doc> {
    let mut doc = doc.close_undo_group();
    for entry_data in entries {
        let Ok(entry) = rmp_serde::from_slice::<led_core::UndoEntry>(entry_data) else {
            continue;
        };
        doc = doc.apply_remote_entry(&entry);
    }
    doc
}
