use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

mod action;
mod actions_of;
mod buffers_of;
mod edit;
pub(crate) mod file_search;
pub(crate) mod find_file;
mod jump;
mod lsp_of;
mod mov;
mod process_of;
mod search;
mod session_of;
mod sync_of;

use led_config_file::ConfigFile;
use led_core::git::FileStatus;
use led_core::keys::{Keymap, Keys};
use led_core::rx::Stream;
use led_core::theme::Theme;
use std::path::PathBuf;

use led_core::{Action, Alert, Doc, DocId, PanelSlot};
use led_state::{
    AppState, BracketPair, BufferState, ChangeReason, Dimensions, HighlightSpan,
    SessionRestorePhase,
};
use led_workspace::Workspace;

use crate::Drivers;
use crate::model::actions_of::actions_of;
use crate::model::buffers_of::buffers_of;
use crate::model::process_of::process_of;
use crate::model::sync_of::sync_of;

pub fn model(drivers: Drivers, init: AppState) -> Stream<Rc<AppState>> {
    let state: Stream<Rc<AppState>> = Stream::new();

    // ── 1. Derive from hoisted state ──

    use led_workspace::WorkspaceIn as WI;

    let workspace_misc_s = drivers
        .workspace_in
        .filter_map(|ev| match ev {
            WI::SessionSaved => Some(Mut::SessionSaved),
            WI::WatchersReady => Some(Mut::WatchersReady),
            _ => None,
        })
        .stream();

    let workspace_s = drivers
        .workspace_in
        .filter_map(|ev| match ev {
            WI::Workspace { workspace } => Some(workspace),
            _ => None,
        })
        .sample_combine(&state)
        .map(|(workspace, s)| {
            let mut dirs = vec![workspace.root.clone()];
            dirs.extend(s.browser.expanded_dirs.iter().cloned());
            Mut::Workspace {
                workspace,
                initial_dirs: dirs,
            }
        })
        .stream();

    let workspace_changed_s = drivers
        .workspace_in
        .filter_map(|ev| match ev {
            WI::WorkspaceChanged { paths } => Some(paths),
            _ => None,
        })
        .sample_combine(&state)
        .map(|(paths, s)| {
            let b = &s.browser;
            let Some(ref root) = b.root else {
                return Mut::WorkspaceChanged { dirs: vec![] };
            };
            // Collect parent dirs of changed paths that are currently visible
            // (root is always visible, expanded dirs are visible).
            let mut dirs_to_refresh = HashSet::new();
            for p in &paths {
                if let Some(parent) = p.parent() {
                    if parent == root.as_path() || b.expanded_dirs.contains(parent) {
                        dirs_to_refresh.insert(parent.to_path_buf());
                    }
                }
            }
            Mut::WorkspaceChanged {
                dirs: dirs_to_refresh.into_iter().collect(),
            }
        })
        .stream();

    let git_changed_s = drivers
        .workspace_in
        .filter(|ev| matches!(ev, WI::GitChanged))
        .map(|_| Mut::GitChanged)
        .stream();

    let session_s = session_of::session_of(&drivers.workspace_in);

    // UndoFlushed needs buffer lookup → sample_combine with state
    let undo_flushed_s = drivers
        .workspace_in
        .filter(|ev| matches!(ev, WI::UndoFlushed { .. }))
        .sample_combine(&state)
        .map(|(ev, s)| {
            let WI::UndoFlushed {
                file_path,
                chain_id,
                last_seen_seq,
                ..
            } = ev
            else {
                unreachable!()
            };
            let path = s
                .buffers
                .values()
                .find(|b| b.path_buf() == Some(&file_path))
                .and_then(|b| b.path_buf().cloned());
            Mut::UndoFlushed {
                path,
                chain_id,
                last_seen_seq,
            }
        })
        .stream();

    // NotifyEvent needs hash→path lookup → sample_combine with state.
    // All filtering/dedup is handled by the model (change_seq, content_hash).
    let notify_s = drivers
        .workspace_in
        .filter(|ev| matches!(ev, WI::NotifyEvent { .. }))
        .sample_combine(&state)
        .map(|(ev, s)| {
            let WI::NotifyEvent { file_path_hash } = ev else {
                unreachable!()
            };
            let path = s.notify_hash_to_buffer.get(&file_path_hash).cloned();
            Mut::NotifyEvent { path }
        })
        .stream();

    // Sync results: full doc application in combinator chain
    let sync_s = sync_of(&drivers.workspace_in, &state);

    let keymap_s = state
        .filter_map(|s| s.config_keys.as_ref().map(|ck| ck.file.clone()))
        .dedupe()
        .map(|keys| {
            keys.as_ref()
                .clone()
                .into_keymap()
                .map(|km| Rc::new(km))
                .map_err(|e| Alert::Warn(e))
        })
        .map(|r| match r {
            Ok(v) => Mut::Keymap(v),
            Err(a) => Mut::alert(a),
        })
        .stream();

    let actions_s = actions_of(&drivers.terminal_in, &state);
    let buffers_s = buffers_of(&drivers.docstore_in, &state);
    let process_s = process_of(&state);
    let preview_s = preview_of(&state);
    let lsp_s = lsp_of::lsp_of(&drivers.lsp_in, &state);

    // ── 2. Build up muts from driver input and derived streams ──

    let muts: Stream<Mut> = drivers
        .config_keys_in
        .map(|r| match r {
            Ok(v) => Mut::ConfigKeys(v),
            Err(a) => Mut::alert(a),
        })
        .or(drivers.config_theme_in.map(|r| match r {
            Ok(v) => Mut::ConfigTheme(v),
            Err(a) => Mut::alert(a),
        }));

    let direct_actions_s = drivers.actions_in.map(|a| Mut::Action(a)).stream();

    // Split timers: undo_flush goes through a chain that samples state,
    // other timers go directly to the reducer.
    let timers_s = drivers
        .timers_in
        .filter(|t| t.name != "undo_flush")
        .map(|t| Mut::TimerFired(t.name))
        .stream();

    let undo_flush_s = drivers
        .timers_in
        .filter(|t| t.name == "undo_flush")
        .sample_combine(&state)
        .map(|(_, s)| s)
        .flat_map(|s: Rc<AppState>| {
            s.buffers
                .values()
                .filter(|b| b.path().is_some())
                .filter(|b| !b.is_preview())
                .filter(|b| b.undo_history_len() > b.persisted_undo_len() || b.is_dirty())
                .filter_map(|b| {
                    let file_path = b.path_buf().cloned().unwrap();
                    let chain_id = b
                        .chain_id()
                        .map(String::from)
                        .unwrap_or_else(led_workspace::new_chain_id);
                    let mut undo = b.undo_history().clone();
                    undo.flush_pending();
                    let raw_entries = undo.entries_from(b.persisted_undo_len());
                    if raw_entries.is_empty() {
                        return None;
                    }
                    let entries: Vec<Vec<u8>> = raw_entries
                        .iter()
                        .filter_map(|e| rmp_serde::to_vec(e).ok())
                        .collect();
                    if entries.is_empty() {
                        return None;
                    }
                    let undo_cursor = undo.entry_count();
                    Some(Mut::UndoFlushReady {
                        path: b.path_buf().cloned().unwrap(),
                        flush: led_state::UndoFlush {
                            file_path,
                            chain_id,
                            content_hash: b.content_hash().0,
                            undo_cursor,
                            distance_from_save: undo.distance_from_save(),
                            entries,
                        },
                    })
                })
                .collect::<Vec<_>>()
        });

    let fs_dir_listed_s = drivers
        .fs_in
        .filter_map(|ev| match ev {
            led_fs::FsIn::DirListed { path, entries } => Some(Mut::DirListed(path, entries)),
            _ => None,
        })
        .stream();

    let fs_find_file_listed_s = drivers
        .fs_in
        .filter_map(|ev| match ev {
            led_fs::FsIn::FindFileListed { dir, entries } => Some((dir, entries)),
            _ => None,
        })
        .sample_combine(&state)
        .filter_map(|((dir, entries), s)| {
            let ff = s.find_file.as_ref()?;
            // Validate the listing matches current input
            let expanded = find_file::expand_path(&ff.input);
            let expected_dir = if ff.input.ends_with('/') {
                expanded
            } else {
                expanded
                    .parent()
                    .unwrap_or(std::path::Path::new("/"))
                    .to_path_buf()
            };
            if dir != expected_dir {
                return None;
            }
            let mut ff = ff.clone();
            ff.completions = entries;
            ff.selected = None;
            Some(Mut::FindFileListed(ff))
        })
        .stream();

    let clipboard_s = drivers
        .clipboard_in
        .map(|ev| match ev {
            led_clipboard::ClipboardIn::Text(text) => text,
        })
        .sample_combine(&state)
        .map(|(text, s)| {
            // Fall back to kill ring content when system clipboard has no text
            // (e.g. an image is in the clipboard).
            let text = if text.is_empty() {
                s.kill_ring.content.clone()
            } else {
                text
            };
            (text, s)
        })
        .filter(|(text, _)| !text.is_empty())
        .filter_map(|(text, s)| {
            let dims = s.dims?;
            let path = s.active_buffer.as_ref()?;
            let buf = s.buffers.get(path)?;
            let mut buf = (**buf).clone();
            action::close_group_on_move(&mut buf);
            buf.clear_mark();
            let (r, c, a) = edit::yank(&mut buf, &text);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            action::close_group_on_move(&mut buf);
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            Some(Mut::BufferUpdate(path.clone(), buf, ChangeReason::Edit))
        })
        .stream();

    let syntax_s = drivers
        .syntax_in
        .map(|syn| Mut::SyntaxUpdate {
            path: syn.path,
            version: syn.doc_version,
            highlights: syn.highlights,
            bracket_pairs: syn.bracket_pairs,
            indent: syn.indent,
            indent_row: syn.indent_row,
            reindent_chars: syn.reindent_chars,
        })
        .stream();

    let git_file_s = drivers
        .git_in
        .filter(|ev| matches!(ev, led_git::GitIn::FileStatuses { .. }))
        .map(|ev| match ev {
            led_git::GitIn::FileStatuses { statuses, branch } => {
                Mut::GitFileStatuses { statuses, branch }
            }
            _ => unreachable!(),
        })
        .stream();

    let git_line_s = drivers
        .git_in
        .filter(|ev| matches!(ev, led_git::GitIn::LineStatuses { .. }))
        .map(|ev| match ev {
            led_git::GitIn::LineStatuses { path, statuses } => {
                Mut::GitLineStatuses { path, statuses }
            }
            _ => unreachable!(),
        })
        .stream();

    let file_search_results_s = drivers
        .file_search_in
        .filter(|ev| matches!(ev, led_file_search::FileSearchIn::Results { .. }))
        .map(|ev| match ev {
            led_file_search::FileSearchIn::Results { results } => results,
            _ => unreachable!(),
        })
        .sample_combine(&state)
        .filter_map(|(results, s)| {
            let mut fs = s.file_search.clone()?;
            fs.results = results;
            fs.rebuild_flat_hits();
            let preview = fs
                .selected_hit()
                .map(|(group, hit)| led_state::PreviewRequest {
                    path: group.path.clone(),
                    row: hit.row,
                    col: hit.col,
                });
            Some(Mut::FileSearchResults(fs, preview))
        })
        .stream();

    let file_search_replace_s = drivers
        .file_search_in
        .filter(|ev| matches!(ev, led_file_search::FileSearchIn::ReplaceComplete { .. }))
        .map(|ev| match ev {
            led_file_search::FileSearchIn::ReplaceComplete {
                results,
                replaced_count,
            } => (results, replaced_count),
            _ => unreachable!(),
        })
        .sample_combine(&state)
        .filter_map(|((results, count), s)| {
            let mut fs = s.file_search.clone()?;
            fs.results = results;
            fs.rebuild_flat_hits();
            let preview = fs
                .selected_hit()
                .map(|(group, hit)| led_state::PreviewRequest {
                    path: group.path.clone(),
                    row: hit.row,
                    col: hit.col,
                });
            Some(Mut::FileSearchReplaceComplete(fs, preview, count))
        })
        .stream();

    let file_search_s: Stream<Mut> = Stream::new();
    file_search_results_s.forward(&file_search_s);
    file_search_replace_s.forward(&file_search_s);

    workspace_s.forward(&muts);
    workspace_changed_s.forward(&muts);
    git_changed_s.forward(&muts);
    workspace_misc_s.forward(&muts);
    session_s.forward(&muts);
    undo_flushed_s.forward(&muts);
    notify_s.forward(&muts);
    sync_s.forward(&muts);
    keymap_s.forward(&muts);
    actions_s.forward(&muts);
    direct_actions_s.forward(&muts);
    buffers_s.forward(&muts);
    process_s.forward(&muts);
    timers_s.forward(&muts);
    undo_flush_s.forward(&muts);
    fs_dir_listed_s.forward(&muts);
    fs_find_file_listed_s.forward(&muts);
    clipboard_s.forward(&muts);
    syntax_s.forward(&muts);
    git_file_s.forward(&muts);
    git_line_s.forward(&muts);
    file_search_s.forward(&muts);
    preview_s.forward(&muts);
    lsp_s.forward(&muts);

    let ui_evict_s = drivers
        .ui_in
        .map(|ev| match ev {
            led_ui::UiIn::EvictOneBuffer => Mut::EvictOneBuffer,
        })
        .stream();
    ui_evict_s.forward(&muts);

    // ── 3. Reduce ──

    muts.fold_into(&state, Rc::new(init), |s, m| {
        log::trace!("model: {}", m.name());
        let mut s = Rc::unwrap_or_clone(s);
        match m {
            Mut::ActivateBuffer(path) => {
                s.active_buffer = Some(path.clone());
                s.git_mut().pending_line_scan.set(Some(path));
                action::reveal_active_buffer(&mut s);
            }
            Mut::Action(a) => {
                action::handle_action(&mut s, a);
            }
            Mut::KbdMacroSetCount(n) => {
                s.kbd_macro.execute_count = Some(n);
            }
            Mut::EvictOneBuffer => action::evict_one_buffer(&mut s),
            Mut::Alert { info, warn } => {
                s.alerts.info = info;
                s.alerts.warn = warn;
            }
            Mut::BufferOpen {
                path,
                doc_id,
                doc,
                cursor,
                scroll,
                tab_order,
                is_preview,
                activate,
                notify_hash,
                session_restore_done,
                clear_pending_jump,
                undo_entries,
                persisted_undo_len,
                chain_id,
                last_seen_seq,
                distance_from_save,
            } => {
                let will_activate = activate || s.active_buffer.is_none();
                if will_activate {
                    s.active_buffer = Some(path.clone());
                }
                s.session.positions.remove(&path);

                // Ensure buffer exists, then materialize
                if !s.buffers.contains_key(&path) {
                    s.buffers_mut()
                        .insert(path.clone(), Rc::new(BufferState::new(path.clone())));
                }
                if let Some(buf) = s.buf_mut(&path) {
                    buf.materialize(doc_id, doc, false);
                    buf.set_cursor(
                        led_core::Row(cursor.0),
                        led_core::Col(cursor.1),
                        led_core::Col(cursor.1),
                    );
                    buf.set_scroll(led_core::Row(scroll.0), led_core::SubLine(scroll.1));
                    buf.set_tab_order(led_core::TabOrder(tab_order));
                    buf.set_preview(is_preview);
                    if !undo_entries.is_empty() {
                        buffers_of::apply_undo_entries(buf, &undo_entries);
                    }
                    let content_hash = buf.content_hash();
                    buf.restore_session(
                        persisted_undo_len,
                        chain_id,
                        last_seen_seq,
                        content_hash,
                        distance_from_save,
                    );
                    if distance_from_save != 0 {
                        buf.mark_modified_if_dirty();
                    }
                }
                s.notify_hash_to_buffer.insert(notify_hash, path.clone());
                action::renumber_tabs(&mut s);
                if session_restore_done {
                    s.session.restore_phase = SessionRestorePhase::Done;
                    s.session.active_tab_order = None;
                    resolve_focus(&mut s);
                } else if s.session.restore_phase == SessionRestorePhase::Done && will_activate {
                    s.focus = PanelSlot::Main;
                }
                if clear_pending_jump {
                    s.jump.pending_position = None;
                }
                if will_activate {
                    action::reveal_active_buffer(&mut s);
                }
                // Apply pending search-replace if this file was opened for replace_all
                file_search::apply_pending_replace(&mut s, &path);
            }
            Mut::BufferSaved {
                path,
                buf,
                undo_clear_path,
            } => {
                let filename = buf
                    .path()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned());
                s.buffers_mut().insert(path.clone(), Rc::new(buf));
                s.git_mut().pending_file_scan.set(());
                s.git_mut().pending_line_scan.set(Some(path));
                if let Some(path) = undo_clear_path {
                    s.pending_undo_clear.set(path);
                }
                s.save_done.set(());
                if let Some(name) = filename {
                    s.alerts.info = Some(format!("Saved {name}"));
                }
            }
            Mut::BufferSavedAs {
                path,
                buf,
                new_path,
                undo_clear_path,
            } => {
                // Update notify hash: remove old, insert new
                let old_hash = s
                    .notify_hash_to_buffer
                    .iter()
                    .find(|(_, v)| **v == path)
                    .map(|(k, _)| k.clone());
                if let Some(h) = old_hash {
                    s.notify_hash_to_buffer.remove(&h);
                }
                let new_hash = led_workspace::path_hash(&new_path);
                s.notify_hash_to_buffer.insert(new_hash, new_path.clone());

                // Remove old path entry, insert under new path
                s.buffers_mut().remove(&path);
                let filename = new_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned());
                s.buffers_mut().insert(new_path.clone(), Rc::new(buf));
                // Update active_buffer if it was pointing to the old path
                if s.active_buffer.as_ref() == Some(&path) {
                    s.active_buffer = Some(new_path.clone());
                }
                s.git_mut().pending_file_scan.set(());
                s.git_mut().pending_line_scan.set(Some(new_path));
                if let Some(path) = undo_clear_path {
                    s.pending_undo_clear.set(path);
                }
                if let Some(name) = filename {
                    s.alerts.info = Some(format!("Saved {name}"));
                }
                action::reveal_active_buffer(&mut s);
            }
            Mut::BufferUpdate(path, mut buf, reason) => {
                buf.set_change_reason(reason);
                s.buffers_mut().insert(path, Rc::new(buf));
            }
            Mut::ConfigKeys(v) => s.config_keys = Some(v),
            Mut::DirListed(path, entries) => {
                let b = s.browser_mut();
                b.dir_contents.insert(path, entries);
                b.rebuild_entries();
                b.complete_pending_reveal();
                action::browser_scroll_to_selected(&mut s);
            }
            Mut::FileSearchResults(fs, preview) => {
                s.file_search = Some(fs);
                if let Some(req) = preview {
                    s.preview.pending.set(Some(req));
                }
            }
            Mut::FileSearchReplaceComplete(fs, preview, count) => {
                s.file_search = Some(fs);
                if let Some(req) = preview {
                    s.preview.pending.set(Some(req));
                }
                s.alerts.info = Some(format!("Replaced {count} occurrence(s)"));
            }
            Mut::FindFileListed(ff) => {
                s.find_file = Some(ff);
            }
            Mut::ConfigTheme(v) => s.config_theme = Some(v),
            Mut::GitFileStatuses { statuses, branch } => {
                s.git_mut().branch = branch;
                s.git_mut().file_statuses = statuses;
            }
            Mut::GitLineStatuses { path, statuses } => {
                if !s.buffers.contains_key(&path) {
                    s.buffers_mut()
                        .insert(path.clone(), Rc::new(BufferState::new(path.clone())));
                }
                if let Some(buf) = s.buf_mut(&path) {
                    buf.set_git_line_statuses(statuses);
                }
            }
            Mut::ForceRedraw(v) => s.force_redraw = v,
            Mut::Keymap(v) => s.keymap = Some(v),
            Mut::PreviewOpen {
                buf,
                notify_hash,
                remove_old_path,
                remove_old_hash,
                pre_preview_buffer,
            } => {
                remove_old_path.as_ref().map(|p| s.buffers_mut().remove(p));
                remove_old_hash.map(|h| s.notify_hash_to_buffer.remove(&h));
                s.preview.pre_preview_buffer = pre_preview_buffer;
                let path = buf
                    .path_buf()
                    .cloned()
                    .expect("preview buffer must have path");
                s.notify_hash_to_buffer.insert(notify_hash, path.clone());
                s.buffers_mut().insert(path.clone(), Rc::new(buf));
                s.active_buffer = Some(path.clone());
                s.preview.buffer = Some(path);
                action::renumber_tabs(&mut s);
            }
            Mut::PreviewActivateExisting {
                path,
                row,
                col,
                remove_old_path,
                remove_old_hash,
                pre_preview_buffer,
            } => {
                remove_old_path.as_ref().map(|p| s.buffers_mut().remove(p));
                remove_old_hash.map(|h| s.notify_hash_to_buffer.remove(&h));
                s.preview.pre_preview_buffer = pre_preview_buffer;
                s.active_buffer = Some(path.clone());
                s.buf_mut(&path).map(|buf| {
                    buf.set_cursor(led_core::Row(row), led_core::Col(col), led_core::Col(col));
                });
                action::renumber_tabs(&mut s);
            }
            Mut::Resize(w, h) => {
                s.dims = Some(Dimensions::new(w, h, s.show_side_panel));
            }
            Mut::SessionOpenFailed { path } => {
                s.session.positions.remove(&path);
                if s.session.restore_phase == SessionRestorePhase::Restoring
                    && s.session.positions.is_empty()
                {
                    s.session.restore_phase = SessionRestorePhase::Done;
                    resolve_focus(&mut s);
                }
            }
            Mut::SessionRestored {
                restore_phase,
                active_tab_order,
                show_side_panel,
                restored_focus,
                positions,
                pending_opens,
                browser_selected,
                browser_scroll_offset,
                browser_expanded_dirs,
                jump_entries,
                jump_index,
                pending_lists,
            } => {
                s.session.restore_phase = restore_phase;
                s.session.active_tab_order = active_tab_order;
                s.show_side_panel = show_side_panel;
                if let Some(ref mut dims) = s.dims {
                    dims.show_side_panel = show_side_panel;
                }
                s.session.restored_focus = restored_focus;
                s.session.positions = positions;
                let b = s.browser_mut();
                b.selected = browser_selected;
                b.scroll_offset = browser_scroll_offset;
                b.expanded_dirs = browser_expanded_dirs;
                s.jump.entries = jump_entries;
                s.jump.index = jump_index;
                if !pending_opens.is_empty() {
                    s.session.pending_opens.set(pending_opens);
                }
                if !pending_lists.is_empty() {
                    s.pending_lists.set(pending_lists);
                }
                // Resolve focus for the None case (Done with no session)
                if s.session.restore_phase == SessionRestorePhase::Done
                    && s.startup.arg_paths.is_empty()
                {
                    resolve_focus(&mut s);
                }
            }
            Mut::SessionSaved => {
                s.session.saved = true;
            }
            Mut::WatchersReady => {
                s.session.watchers_ready = true;
            }
            Mut::NotifyEvent { path } => {
                if let Some(path) = path {
                    s.pending_sync_check.set(path);
                }
            }
            Mut::UndoFlushReady { path, flush } => {
                if let Some(buf) = s.buf_mut(&path) {
                    // Close the undo group on the actual buffer doc to keep
                    // it consistent with persisted_undo_len. Without this,
                    // subsequent edits can append to the already-flushed open
                    // group, making undo_groups_from(persisted) return empty.
                    buf.close_undo_group();
                    buf.undo_flush_started(flush.chain_id.clone(), flush.undo_cursor);
                }
                s.pending_undo_flush.set(Some(flush));
            }
            Mut::UndoFlushed {
                path,
                chain_id,
                last_seen_seq,
            } => {
                if let Some(path) = path {
                    if let Some(buf) = s.buf_mut(&path) {
                        buf.undo_flush_confirmed(chain_id, last_seen_seq);
                    }
                }
            }
            Mut::SyntaxUpdate {
                path,
                version,
                highlights,
                bracket_pairs,
                indent,
                indent_row,
                reindent_chars,
            } => {
                let tab_stop = s.dims.map(|d| d.tab_stop);
                if let Some(buf) = s.buf_mut(&path) {
                    buf.set_reindent_chars(reindent_chars);
                    // Check if indent will modify the doc — if so, skip
                    // storing highlights from this response (their character
                    // offsets would be wrong after the doc changes). The
                    // indent change triggers a new SyntaxOut which produces
                    // correct highlights for the indented doc.
                    let will_indent = indent_row.is_some_and(|row| {
                        buf.pending_indent_row() == Some(row)
                            && (indent.is_some()
                                || (buf.pending_tab_fallback() && tab_stop.is_some()))
                    });
                    if !will_indent {
                        buf.offer_syntax(highlights, bracket_pairs, led_core::DocVersion(version));
                    }
                    if let Some(row) = indent_row {
                        if buf.pending_indent_row() == Some(row) && buf.version().0 == version {
                            let was_tab = buf.pending_tab_fallback();
                            buf.request_indent(None, false);
                            if let Some(new_indent) = &indent {
                                let cursor_on_row = buf.cursor_row().0 == row;
                                edit::apply_indent(buf, row, new_indent, cursor_on_row);
                            } else if was_tab {
                                if let Some(ts) = tab_stop {
                                    edit::insert_soft_tab(buf, ts);
                                }
                            }
                            buf.close_group_on_move();
                            buf.mark_modified_if_dirty();
                        }
                    }
                }
            }
            Mut::Suspend(v) => {
                s.suspend = v;
                if !v {
                    s.git_mut().pending_file_scan.set(());
                }
            }
            Mut::TimerFired(name) => handle_timer(&mut s, name),
            Mut::TouchArgFiles { entries } => {
                for (path, tab_order) in entries {
                    if let Some(buf) = s.buf_mut(&path) {
                        buf.touch();
                        buf.set_tab_order(led_core::TabOrder(tab_order));
                    }
                }
                action::renumber_tabs(&mut s);
            }
            Mut::Workspace {
                workspace,
                initial_dirs,
            } => {
                let b = s.browser_mut();
                b.root = Some(workspace.root.clone());
                b.dir_contents.clear();
                b.rebuild_entries();
                s.pending_lists.set(initial_dirs);
                s.git_mut().pending_file_scan.set(());
                s.workspace = Some(Rc::new(workspace));
            }
            Mut::WorkspaceChanged { dirs } => {
                if !dirs.is_empty() {
                    s.pending_lists.set(dirs);
                }
                s.git_mut().pending_file_scan.set(());
            }
            Mut::GitChanged => {
                s.git_mut().pending_file_scan.set(());
                if let Some(path) = s.active_buffer.clone() {
                    s.git_mut().pending_line_scan.set(Some(path));
                }
            }

            // ── LSP ──
            Mut::LspNavigate { path, row, col } => {
                // Record current position in jump list
                if let Some(ref active) = s.active_buffer {
                    if let Some(buf) = s.buffers.get(active) {
                        if let Some(p) = buf.path_buf() {
                            let pos = led_state::JumpPosition {
                                path: p.clone(),
                                row: buf.cursor_row().0,
                                col: buf.cursor_col().0,
                                scroll_offset: buf.scroll_row().0,
                            };
                            jump::record_jump(&mut s, pos);
                        }
                    }
                }
                // Check if file is already open
                let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let existing = s
                    .buffers
                    .values()
                    .find(|b| {
                        b.path_buf().map_or(false, |p| {
                            std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == canonical
                        })
                    })
                    .and_then(|b| b.path_buf().cloned());
                if let Some(existing_path) = existing {
                    s.active_buffer = Some(existing_path.clone());
                    let half = s.dims.map_or(10, |d| d.buffer_height() / 2);
                    if let Some(buf) = s.buf_mut(&existing_path) {
                        let r = row.min(buf.doc().line_count().saturating_sub(1));
                        buf.set_cursor(led_core::Row(r), led_core::Col(col), led_core::Col(col));
                        buf.set_scroll(
                            led_core::Row(buf.cursor_row().0.saturating_sub(half)),
                            led_core::SubLine(0),
                        );
                    }
                    action::reveal_active_buffer(&mut s);
                } else {
                    s.pending_open.set(Some(path.clone()));
                    s.jump.pending_position = Some(led_state::JumpPosition {
                        path,
                        row,
                        col,
                        scroll_offset: 0,
                    });
                }
            }
            Mut::LspEdits { edits } => {
                let is_empty = edits.iter().all(|fe| fe.edits.is_empty());
                for fe in edits {
                    if let Some(buf) = s.buf_mut(&fe.path) {
                        apply_text_edits(buf, &fe.edits);
                        buf.close_group_on_move();
                    }
                }
                // Format-done signal (empty edits) → trigger pending save
                if is_empty && s.lsp.pending_save_after_format {
                    s.lsp_mut().pending_save_after_format = false;
                    // Apply built-in cleanup after LSP format, before save
                    if let Some(path) = s.active_buffer.clone() {
                        if let Some(buf) = s.buf_mut(&path) {
                            buf.apply_save_cleanup();
                        }
                    }
                    s.save_request.set(());
                }
            }
            Mut::LspCompletion {
                items,
                prefix_start_col,
            } => {
                if items.is_empty() {
                    s.lsp_mut().completion = None;
                } else {
                    s.lsp_mut().completion = Some(led_state::CompletionState {
                        items,
                        prefix_start_col,
                        selected: 0,
                        scroll_offset: 0,
                    });
                }
            }
            Mut::LspCodeActions { actions } => {
                if actions.is_empty() {
                    s.lsp_mut().code_actions = None;
                } else {
                    s.lsp_mut().code_actions = Some(led_state::CodeActionPickerState {
                        actions,
                        selected: 0,
                    });
                    s.focus = PanelSlot::Overlay;
                }
            }
            Mut::LspDiagnostics {
                path,
                diagnostics,
                content_hash,
            } => {
                // Ensure buffer exists (create unmaterialized if needed)
                if !s.buffers.contains_key(&path) {
                    s.buffers_mut()
                        .insert(path.clone(), Rc::new(BufferState::new(path.clone())));
                }
                if let Some(buf) = s.buf_mut(&path) {
                    buf.offer_diagnostics(diagnostics, led_core::ContentHash(content_hash));
                }
            }
            Mut::LspInlayHints { path, hints } => {
                if !s.buffers.contains_key(&path) {
                    s.buffers_mut()
                        .insert(path.clone(), Rc::new(BufferState::new(path.clone())));
                }
                if let Some(buf) = s.buf_mut(&path) {
                    buf.set_inlay_hints(hints);
                }
            }
            Mut::LspProgress {
                server_name,
                busy,
                detail,
            } => {
                let lsp = s.lsp_mut();
                lsp.server_name = server_name;
                lsp.busy = busy;
                lsp.progress = detail.map(|d| led_state::LspProgress {
                    title: d,
                    message: None,
                });
            }
            Mut::LspTriggerChars {
                extensions,
                triggers,
            } => {
                for buf in s.buffers_mut().values_mut() {
                    let ext = buf
                        .path()
                        .and_then(|p| p.extension())
                        .and_then(|e| e.to_str())
                        .unwrap_or("");
                    if extensions.iter().any(|x| x == ext) {
                        Rc::make_mut(buf).set_completion_triggers(triggers.clone());
                    }
                }
            }
        }
        Rc::new(s)
    });

    state
}

/// Apply focus after session restore completes.
/// Priority: arg_dir (browser reveal) → restored focus (if valid) → open buffer → file browser.
fn resolve_focus(s: &mut AppState) {
    if let Some(ref dir) = s.startup.arg_dir {
        let dir = dir.clone();
        s.session.restored_focus.take(); // consume
        let new_dirs = s.browser_mut().reveal(&dir);
        if !new_dirs.is_empty() {
            s.pending_lists.set(new_dirs);
        }
        action::browser_scroll_to_selected(s);
        s.focus = PanelSlot::Side;
        return;
    }

    let restored = s.session.restored_focus.take();

    if !s.buffers.is_empty() {
        // There are open buffers. Honour restored focus if it's Main
        // (the buffer exists to receive it) or Side.
        s.focus = restored.unwrap_or(PanelSlot::Main);
    } else {
        // No buffers — file browser is the only usable panel.
        s.focus = PanelSlot::Side;
    }
}

fn handle_timer(state: &mut AppState, name: &'static str) {
    match name {
        "alert_clear" => {
            state.alerts.clear();
        }
        "git_file_scan" => {
            state.git_mut().scan_seq.set(());
        }
        "spinner" => {
            state.lsp_mut().spinner_tick = state.lsp.spinner_tick.wrapping_add(1);
        }
        "tab_linger" => {
            if let Some(path) = state.active_buffer.clone() {
                if let Some(buf) = state.buf_mut(&path) {
                    buf.touch();
                }
            }
        }
        "undo_flush" => {
            // Handled by the undo_flush_s combinator chain, not here.
            // The timer fires → chain samples state → produces UndoFlushReady.
        }
        _ => {}
    }
}

#[derive(Clone)]
enum Mut {
    ActivateBuffer(PathBuf),
    Action(Action),
    EvictOneBuffer,
    KbdMacroSetCount(usize),
    Alert {
        info: Option<String>,
        warn: Option<String>,
    },
    BufferOpen {
        path: PathBuf,
        doc_id: DocId,
        doc: Arc<dyn Doc>,
        cursor: (usize, usize),
        scroll: (usize, usize),
        tab_order: usize,
        is_preview: bool,
        activate: bool,
        notify_hash: String,
        session_restore_done: bool,
        clear_pending_jump: bool,
        /// Session restore: undo entries + persistence state
        undo_entries: Vec<Vec<u8>>,
        persisted_undo_len: usize,
        chain_id: Option<String>,
        last_seen_seq: i64,
        distance_from_save: i32,
    },
    BufferSaved {
        path: PathBuf,
        buf: BufferState,
        undo_clear_path: Option<PathBuf>,
    },
    BufferSavedAs {
        path: PathBuf,
        buf: BufferState,
        new_path: PathBuf,
        undo_clear_path: Option<PathBuf>,
    },
    BufferUpdate(PathBuf, BufferState, ChangeReason),
    ConfigKeys(ConfigFile<Keys>),
    ConfigTheme(ConfigFile<Theme>),
    DirListed(std::path::PathBuf, Vec<led_fs::DirEntry>),
    FileSearchResults(
        led_state::file_search::FileSearchState,
        Option<led_state::PreviewRequest>,
    ),
    FileSearchReplaceComplete(
        led_state::file_search::FileSearchState,
        Option<led_state::PreviewRequest>,
        usize,
    ),
    FindFileListed(led_state::FindFileState),
    GitFileStatuses {
        statuses: HashMap<PathBuf, HashSet<FileStatus>>,
        branch: Option<String>,
    },
    GitLineStatuses {
        path: PathBuf,
        statuses: Vec<led_core::git::LineStatus>,
    },
    ForceRedraw(u64),
    Keymap(Rc<Keymap>),
    PreviewOpen {
        buf: BufferState,
        notify_hash: String,
        remove_old_path: Option<PathBuf>,
        remove_old_hash: Option<String>,
        pre_preview_buffer: Option<PathBuf>,
    },
    PreviewActivateExisting {
        path: PathBuf,
        row: usize,
        col: usize,
        remove_old_path: Option<PathBuf>,
        remove_old_hash: Option<String>,
        pre_preview_buffer: Option<PathBuf>,
    },
    Resize(u16, u16),
    NotifyEvent {
        path: Option<std::path::PathBuf>,
    },
    SessionOpenFailed {
        path: std::path::PathBuf,
    },
    SessionRestored {
        restore_phase: SessionRestorePhase,
        active_tab_order: Option<usize>,
        show_side_panel: bool,
        restored_focus: Option<PanelSlot>,
        positions: HashMap<PathBuf, led_workspace::SessionBuffer>,
        pending_opens: Vec<PathBuf>,
        browser_selected: usize,
        browser_scroll_offset: usize,
        browser_expanded_dirs: HashSet<PathBuf>,
        jump_entries: std::collections::VecDeque<led_state::JumpPosition>,
        jump_index: usize,
        pending_lists: Vec<PathBuf>,
    },
    SessionSaved,
    WatchersReady,
    Suspend(bool),
    SyntaxUpdate {
        path: PathBuf,
        version: u64,
        highlights: Vec<(usize, HighlightSpan)>,
        bracket_pairs: Vec<BracketPair>,
        indent: Option<String>,
        indent_row: Option<usize>,
        reindent_chars: Arc<[char]>,
    },
    UndoFlushed {
        path: Option<PathBuf>,
        chain_id: String,
        last_seen_seq: i64,
    },
    UndoFlushReady {
        path: PathBuf,
        flush: led_state::UndoFlush,
    },
    TimerFired(&'static str),
    TouchArgFiles {
        entries: Vec<(PathBuf, usize)>,
    },
    Workspace {
        workspace: Workspace,
        initial_dirs: Vec<PathBuf>,
    },
    WorkspaceChanged {
        dirs: Vec<PathBuf>,
    },
    GitChanged,
    // LSP
    LspNavigate {
        path: PathBuf,
        row: usize,
        col: usize,
    },
    LspEdits {
        edits: Vec<led_lsp::FileEdit>,
    },
    LspCompletion {
        items: Vec<led_lsp::CompletionItem>,
        prefix_start_col: usize,
    },
    LspCodeActions {
        actions: Vec<String>,
    },
    LspDiagnostics {
        path: PathBuf,
        diagnostics: Vec<led_lsp::Diagnostic>,
        content_hash: u64,
    },
    LspInlayHints {
        path: PathBuf,
        hints: Vec<led_lsp::InlayHint>,
    },
    LspProgress {
        server_name: String,
        busy: bool,
        detail: Option<String>,
    },
    LspTriggerChars {
        extensions: Vec<String>,
        triggers: Vec<String>,
    },
}

impl Mut {
    fn name(&self) -> &'static str {
        match self {
            Mut::ActivateBuffer(_) => "ActivateBuffer",
            Mut::Action(_) => "Action",
            Mut::Alert { .. } => "Alert",
            Mut::BufferOpen { .. } => "BufferOpen",
            Mut::BufferSaved { .. } => "BufferSaved",
            Mut::BufferSavedAs { .. } => "BufferSavedAs",
            Mut::BufferUpdate(_, _, _) => "BufferUpdate",
            Mut::ConfigKeys(_) => "ConfigKeys",
            Mut::ConfigTheme(_) => "ConfigTheme",
            Mut::DirListed(_, _) => "DirListed",
            Mut::FileSearchResults(..) => "FileSearchResults",
            Mut::FileSearchReplaceComplete(..) => "FileSearchReplaceComplete",
            Mut::FindFileListed(_) => "FindFileListed",
            Mut::GitFileStatuses { .. } => "GitFileStatuses",
            Mut::GitLineStatuses { .. } => "GitLineStatuses",
            Mut::ForceRedraw(_) => "ForceRedraw",
            Mut::Keymap(_) => "Keymap",
            Mut::PreviewOpen { .. } => "PreviewOpen",
            Mut::PreviewActivateExisting { .. } => "PreviewActivateExisting",
            Mut::Resize(_, _) => "Resize",
            Mut::NotifyEvent { .. } => "NotifyEvent",
            Mut::SessionOpenFailed { .. } => "SessionOpenFailed",
            Mut::SessionRestored { .. } => "SessionRestored",
            Mut::SessionSaved => "SessionSaved",
            Mut::WatchersReady => "WatchersReady",
            Mut::Suspend(_) => "Suspend",
            Mut::SyntaxUpdate { .. } => "SyntaxUpdate",
            Mut::UndoFlushed { .. } => "UndoFlushed",
            Mut::UndoFlushReady { .. } => "UndoFlushReady",
            Mut::TimerFired(_) => "TimerFired",
            Mut::TouchArgFiles { .. } => "TouchArgFiles",
            Mut::Workspace { .. } => "Workspace",
            Mut::WorkspaceChanged { .. } => "WorkspaceChanged",
            Mut::GitChanged => "GitChanged",
            Mut::LspNavigate { .. } => "LspNavigate",
            Mut::LspEdits { .. } => "LspEdits",
            Mut::LspCompletion { .. } => "LspCompletion",
            Mut::LspCodeActions { .. } => "LspCodeActions",
            Mut::LspDiagnostics { .. } => "LspDiagnostics",
            Mut::LspInlayHints { .. } => "LspInlayHints",
            Mut::LspProgress { .. } => "LspProgress",
            Mut::LspTriggerChars { .. } => "LspTriggerChars",
            Mut::EvictOneBuffer => "EvictOneBuffer",
            Mut::KbdMacroSetCount(_) => "KbdMacroSetCount",
        }
    }

    fn alert(a: Alert) -> Self {
        match a {
            Alert::Info(v) => Mut::Alert {
                info: Some(v),
                warn: None,
            },
            Alert::Warn(v) => Mut::Alert {
                info: None,
                warn: Some(v),
            },
        }
    }
}

// ── LSP edit helper ──

/// Apply text edits to a buffer in reverse document order.
fn apply_text_edits(buf: &mut BufferState, edits: &[led_lsp::TextEdit]) {
    let mut sorted: Vec<&led_lsp::TextEdit> = edits.iter().collect();
    sorted.sort_by(|a, b| {
        let row_cmp = b.start_row.cmp(&a.start_row);
        if row_cmp == std::cmp::Ordering::Equal {
            b.start_col.cmp(&a.start_col)
        } else {
            row_cmp
        }
    });

    action::close_group_on_move(buf);
    // Flush any pending undo group and pre-seed a new one with the actual
    // cursor position so that undo restores the cursor correctly.
    let cursor_char =
        led_core::CharOffset(buf.doc().line_to_char(buf.cursor_row()).0 + buf.cursor_col().0);
    buf.close_undo_group();
    buf.begin_undo_group(cursor_char);
    for te in sorted {
        let start = led_core::CharOffset(
            buf.doc().line_to_char(led_core::Row(te.start_row)).0 + te.start_col,
        );
        let end =
            led_core::CharOffset(buf.doc().line_to_char(led_core::Row(te.end_row)).0 + te.end_col);
        if start != end {
            buf.remove_text(start, end);
        }
        if !te.new_text.is_empty() {
            buf.insert_text(start, &te.new_text);
        }
    }
}

// ── Preview combinator ──

/// Helper: look up the notify hash for a buffer path.
fn notify_hash_for(s: &AppState, path: &std::path::Path) -> Option<String> {
    s.notify_hash_to_buffer
        .iter()
        .find(|(_, v)| v.as_path() == path)
        .map(|(k, _)| k.clone())
}

/// Classify a pending_preview request against current state and produce
/// the appropriate Mut. Cases A and B are handled here; Case C (new file)
/// returns None — the derived docstore stream handles it.
fn resolve_preview(s: &AppState) -> Option<Mut> {
    let req = (*s.preview.pending).as_ref()?;

    // Case A: same file already previewed → reposition via BufferUpdate
    if let Some(ref preview_path) = s.preview.buffer {
        if let Some(buf) = s.buffers.get(preview_path) {
            if buf.path_buf() == Some(&req.path) {
                let mut buf = (**buf).clone();
                let row = req.row.min(buf.doc().line_count().saturating_sub(1));
                buf.set_cursor(
                    led_core::Row(row),
                    led_core::Col(req.col),
                    led_core::Col(req.col),
                );
                let buffer_height = s.dims.map_or(20, |d| d.buffer_height());
                buf.set_scroll(
                    led_core::Row(row.saturating_sub(buffer_height / 2)),
                    led_core::SubLine(0),
                );
                return Some(Mut::BufferUpdate(
                    preview_path.clone(),
                    buf,
                    ChangeReason::Edit,
                ));
            }
        }
    }

    // Case B: already open as real buffer → activate temporarily
    if let Some(existing) = s
        .buffers
        .values()
        .find(|b| b.path_buf() == Some(&req.path) && !b.is_preview())
    {
        let path = existing.path_buf().cloned().expect("matched by path");
        let row = req.row.min(existing.doc().line_count().saturating_sub(1));
        let col = req.col;

        let remove_old_path = s.preview.buffer.clone();
        let remove_old_hash = remove_old_path
            .as_deref()
            .and_then(|p| notify_hash_for(s, p));
        let pre_preview_buffer =
            if s.preview.buffer.is_none() && s.preview.pre_preview_buffer.is_none() {
                s.active_buffer.clone()
            } else {
                s.preview.pre_preview_buffer.clone()
            };

        return Some(Mut::PreviewActivateExisting {
            path,
            row,
            col,
            remove_old_path,
            remove_old_hash,
            pre_preview_buffer,
        });
    }

    // Case C: new file — handled by derived, not here
    None
}

fn preview_of(state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    state
        .dedupe_by(|s| s.preview.pending.version())
        .filter(|s| s.preview.pending.version() > 0)
        .filter(|s| s.preview.pending.is_some())
        .filter_map(|s| resolve_preview(&s))
        .stream()
}
