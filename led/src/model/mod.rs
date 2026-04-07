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
use led_core::CanonPath;
use led_core::git::FileStatus;
use led_core::keys::{Keymap, Keys};
use led_core::rx::Stream;
use led_core::theme::Theme;

use led_core::{Action, Alert, Doc, PanelSlot};
use led_state::{AppState, BracketPair, BufferState, Dimensions, HighlightSpan, Phase};
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
                    if parent == *root || b.expanded_dirs.contains(&parent) {
                        dirs_to_refresh.insert(parent);
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
                .find(|b| b.path() == Some(&file_path))
                .and_then(|b| b.path().cloned());
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
    let lsp_s = lsp_of::lsp_of(&drivers.lsp_in, &state);

    // Resume complete: all session files resolved (opened or failed).
    let resume_complete_s = state
        .filter(|s| s.phase == Phase::Resuming)
        .filter(|s| {
            !s.session.resume.is_empty()
                && s.session
                    .resume
                    .iter()
                    .all(|e| e.state != led_state::ResumeState::Pending)
        })
        .map(|_| Mut::ResumeComplete)
        .stream();

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
                .filter(|b| {
                    let path = b.path().unwrap();
                    !s.tabs.iter().any(|t| *t.path() == *path && t.is_preview())
                })
                .filter(|b| b.undo_history_len() > b.persisted_undo_len() || b.is_dirty())
                .filter_map(|b| {
                    let file_path = b.path().cloned().unwrap();
                    let chain_id = b
                        .chain_id()
                        .map(String::from)
                        .unwrap_or_else(led_workspace::new_chain_id);
                    let mut undo = b.undo_history().clone();
                    undo.flush_pending();
                    let entries: Vec<led_core::UndoEntry> =
                        undo.entries_from(b.persisted_undo_len()).to_vec();
                    if entries.is_empty() {
                        return None;
                    }
                    let undo_cursor = undo.entry_count();
                    Some(Mut::UndoFlushReady {
                        path: b.path().cloned().unwrap(),
                        flush: led_state::UndoFlush {
                            file_path,
                            chain_id,
                            content_hash: b.content_hash(),
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
                led_core::UserPath::new(&expanded).canonicalize()
            } else {
                led_core::UserPath::new(expanded.parent().unwrap_or(std::path::Path::new("/")))
                    .canonicalize()
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
            let path = s.active_tab.as_ref()?;
            let buf = s.buffers.get(path)?;
            let mut buf = (**buf).clone();
            action::close_group_on_move(&mut buf);
            buf.clear_mark();
            let (r, c, a) = edit::yank(&mut buf, &text);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            action::close_group_on_move(&mut buf);
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            Some(Mut::BufferUpdate(path.clone(), buf))
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
    resume_complete_s.forward(&muts);
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
                s.active_tab = Some(path.clone());
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
            Mut::ResumeComplete => {
                log::info!("phase: {:?} → Running (ResumeComplete)", s.phase);
                s.phase = Phase::Running;

                // Resolve active tab from session's saved order.
                if let Some(order) = s.session.active_tab_order.take() {
                    let non_preview_tabs: Vec<_> =
                        s.tabs.iter().filter(|t| !t.is_preview()).collect();
                    if let Some(tab) = non_preview_tabs.get(order) {
                        s.active_tab = Some(tab.path().clone());
                    }
                }
                // Fall back: if active_tab is not set or points to a missing buffer,
                // pick the first materialized tab.
                let active_valid = s.active_tab.as_ref().map_or(false, |p| {
                    s.buffers.get(p).is_some_and(|b| b.is_materialized())
                });
                if !active_valid {
                    s.active_tab = s
                        .tabs
                        .iter()
                        .find(|t| s.buffers.get(t.path()).is_some_and(|b| b.is_materialized()))
                        .map(|t| t.path().clone());
                }

                ensure_startup_arg_buffers(&mut s);

                // CLI arg files override session's active tab.
                if let Some(arg_path) = s.startup.arg_paths.last() {
                    s.active_tab = Some(arg_path.clone());
                }

                resolve_focus(&mut s);
            }
            Mut::BufferOpen {
                path,
                doc,
                cursor,
                scroll,
                activate,
                notify_hash,
                undo_entries,
                persisted_undo_len,
                chain_id,
                last_seen_seq,
                distance_from_save,
            } => {
                log::info!("BufferOpen: {}", path.display());
                s.session.positions.remove(&path);

                // Mark resume entry as Opened
                if let Some(entry) = s.session.resume.iter_mut().find(|e| e.path == path) {
                    entry.state = led_state::ResumeState::Opened;
                }

                // Activation: during Resuming, don't activate — ResumeComplete handles it.
                let will_activate = if s.phase == Phase::Resuming {
                    false
                } else {
                    activate || s.active_tab.is_none()
                };
                if will_activate {
                    s.active_tab = Some(path.clone());
                }

                // Ensure buffer exists, then materialize
                if !s.buffers.contains_key(&path) {
                    s.buffers_mut()
                        .insert(path.clone(), Rc::new(BufferState::new(path.clone())));
                }
                if let Some(buf) = s.buf_mut(&path) {
                    buf.materialize(doc, false);
                    buf.set_cursor(
                        led_core::Row(cursor.0),
                        led_core::Col(cursor.1),
                        led_core::Col(cursor.1),
                    );
                    buf.set_scroll(led_core::Row(scroll.0), led_core::SubLine(scroll.1));
                    if !undo_entries.is_empty() {
                        buf.apply_persisted_entries(&undo_entries);
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
                let is_preview_tab = s.tabs.iter().any(|t| t.is_preview() && *t.path() == path);
                if s.phase == Phase::Running && will_activate && !is_preview_tab {
                    s.focus = PanelSlot::Main;
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
                // Update active_tab if it was pointing to the old path
                if s.active_tab.as_ref() == Some(&path) {
                    s.active_tab = Some(new_path.clone());
                }
                s.git_mut().pending_file_scan.set(());
                if let Some(path) = undo_clear_path {
                    s.pending_undo_clear.set(path);
                }
                if let Some(name) = filename {
                    s.alerts.info = Some(format!("Saved {name}"));
                }
                action::reveal_active_buffer(&mut s);
            }
            Mut::BufferUpdate(path, buf) => {
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
                    action::set_preview(&mut s, req.path, req.row, req.col);
                }
            }
            Mut::FileSearchReplaceComplete(fs, preview, count) => {
                s.file_search = Some(fs);
                if let Some(req) = preview {
                    action::set_preview(&mut s, req.path, req.row, req.col);
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
            Mut::Resize(w, h) => {
                s.dims = Some(Dimensions::new(w, h, s.show_side_panel));
            }
            Mut::SessionOpenFailed { path } => {
                s.session.positions.remove(&path);
                s.buffers_mut().remove(&path);
                s.tabs.retain(|t| *t.path() != path);

                // Mark resume entry as Failed — ResumeComplete handles the transition.
                if let Some(entry) = s.session.resume.iter_mut().find(|e| e.path == path) {
                    entry.state = led_state::ResumeState::Failed;
                }
            }
            Mut::SessionRestored {
                active_tab_order,
                show_side_panel,
                positions,
                pending_opens,
                browser_selected,
                browser_scroll_offset,
                browser_expanded_dirs,
                jump_entries,
                jump_index,
                pending_lists,
            } => {
                s.session.active_tab_order = active_tab_order;
                s.show_side_panel = show_side_panel;
                if let Some(ref mut dims) = s.dims {
                    dims.show_side_panel = show_side_panel;
                }
                s.session.positions = positions;
                let b = s.browser_mut();
                b.selected = browser_selected;
                b.scroll_offset = browser_scroll_offset;
                b.expanded_dirs = browser_expanded_dirs;
                s.jump.entries = jump_entries;
                s.jump.index = jump_index;
                if !pending_opens.is_empty() {
                    s.session.resume = pending_opens
                        .iter()
                        .map(|p| led_state::ResumeEntry {
                            path: p.clone(),
                            state: led_state::ResumeState::Pending,
                        })
                        .collect();
                    for path in &pending_opens {
                        if !s.tabs.iter().any(|t| *t.path() == *path) {
                            s.tabs.push_back(led_state::Tab::new(path.clone()));
                        }
                        if !s.buffers.contains_key(path) {
                            let buf = BufferState::new(path.clone());
                            s.buffers_mut().insert(path.clone(), Rc::new(buf));
                        }
                    }
                    log::info!("phase: {:?} → Resuming (SessionRestored)", s.phase);
                    s.phase = Phase::Resuming;
                } else {
                    log::info!(
                        "phase: {:?} → Running (SessionRestored, no resume)",
                        s.phase
                    );
                    s.phase = Phase::Running;
                    ensure_startup_arg_buffers(&mut s);
                    resolve_focus(&mut s);
                }
                if !pending_lists.is_empty() {
                    s.pending_lists.set(pending_lists);
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
                        buf.offer_syntax(highlights, bracket_pairs, version);
                    }
                    if let Some(row) = indent_row {
                        if buf.pending_indent_row() == Some(row) && buf.version() == version {
                            let was_tab = buf.pending_tab_fallback();
                            buf.request_indent(None, false);
                            if let Some(new_indent) = &indent {
                                let cursor_on_row = buf.cursor_row() == row;
                                edit::apply_indent(buf, *row, new_indent, cursor_on_row);
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
            Mut::Resumed => {
                log::info!("phase: {:?} → Running (Resumed)", s.phase);
                s.phase = Phase::Running;
                s.git_mut().pending_file_scan.set(());
            }
            Mut::TimerFired(name) => handle_timer(&mut s, name),
            Mut::TouchArgFiles { entries } => {
                for path in &entries {
                    if let Some(buf) = s.buf_mut(path) {
                        buf.touch();
                    }
                }
                // Reorder tabs: move arg files to end in arg order
                if entries.len() > 1 {
                    for path in &entries {
                        if let Some(pos) = s.tabs.iter().position(|t| *t.path() == *path) {
                            let tab = s.tabs.remove(pos).unwrap();
                            s.tabs.push_back(tab);
                        }
                    }
                }
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
            }

            // ── LSP ──
            Mut::LspNavigate { path, row, col } => {
                // Record current position in jump list
                if let Some(ref active) = s.active_tab {
                    if let Some(buf) = s.buffers.get(active) {
                        if let Some(p) = buf.path() {
                            let pos = led_state::JumpPosition {
                                path: p.clone(),
                                row: buf.cursor_row(),
                                col: buf.cursor_col(),
                                scroll_offset: buf.scroll_row(),
                            };
                            jump::record_jump(&mut s, pos);
                        }
                    }
                }
                // Check if file is already open (paths are CanonPath, simple == works)
                let existing = s
                    .buffers
                    .values()
                    .find(|b| b.path() == Some(&path))
                    .and_then(|b| b.path().cloned());
                if let Some(existing_path) = existing {
                    s.active_tab = Some(existing_path.clone());
                    let half = s.dims.map_or(10, |d| d.buffer_height() / 2);
                    if let Some(buf) = s.buf_mut(&existing_path) {
                        let r = (*row).min(buf.doc().line_count().saturating_sub(1));
                        buf.set_cursor(led_core::Row(r), col, col);
                        buf.set_scroll(
                            led_core::Row(buf.cursor_row().0.saturating_sub(half)),
                            led_core::SubLine(0),
                        );
                    }
                    action::reveal_active_buffer(&mut s);
                } else {
                    request_open(&mut s, path.clone(), false);
                    if let Some(tab) = s.tabs.iter_mut().find(|t| *t.path() == path) {
                        let half = s.dims.map_or(10, |d| d.buffer_height() / 2);
                        tab.set_cursor(row, col, led_core::Row(row.saturating_sub(half)));
                    }
                    s.active_tab = Some(path);
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
                if !is_empty {
                    log::info!("save: received LSP format edits");
                }
                if is_empty && s.lsp.pending_save_after_format {
                    log::info!("save: format done, triggering save");
                    s.lsp_mut().pending_save_after_format = false;
                    // Apply built-in cleanup after LSP format, before save
                    if let Some(path) = s.active_tab.clone() {
                        if let Some(buf) = s.buf_mut(&path) {
                            buf.apply_save_cleanup();
                            buf.record_diag_save_point();
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
                        prefix_start_col: *prefix_start_col,
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
                    buf.offer_diagnostics(diagnostics, content_hash);
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

        // Implicit dematerialization: any buffer not referenced by a tab
        // that isn't already Unmaterialized gets dematerialized. Catches
        // both Materialized (kill buffer) and Requested (preview scrolled
        // past before the docstore responded). Version is bumped to
        // invalidate any in-flight Open response.
        let tabs = &s.tabs;
        let buffers = Rc::make_mut(&mut s.buffers);
        for buf in buffers.values_mut() {
            let has_tab = buf
                .path()
                .map_or(false, |p| tabs.iter().any(|t| *t.path() == *p));
            if !has_tab && !buf.is_unmaterialized() {
                Rc::make_mut(buf).dematerialize();
            }
        }
        s.notify_hash_to_buffer
            .retain(|_, p| s.tabs.iter().any(|t| *t.path() == *p));

        // Apply pending cursor from tabs to materialized buffers.
        for tab in s.tabs.iter_mut() {
            if !tab.has_pending_cursor() {
                continue;
            }
            let Some(buf) = s.buffers.get(tab.path()) else {
                continue;
            };
            if !buf.is_materialized() {
                continue;
            }
            let (row, col, scroll_row) = tab.take_cursor().unwrap();
            buf.set_cursor(row, col, col);
            buf.set_scroll(scroll_row, led_core::SubLine(0));
        }

        Rc::new(s)
    });

    state
}

/// Resolve focus when entering Running.
/// Called exactly once per Init/Resuming → Running transition.
fn resolve_focus(s: &mut AppState) {
    if let Some(ref dir) = s.startup.arg_dir {
        let dir = dir.clone();
        let new_dirs = s.browser_mut().reveal(&dir);
        if !new_dirs.is_empty() {
            s.pending_lists.set(new_dirs);
        }
        action::browser_scroll_to_selected(s);
        s.focus = PanelSlot::Side;
        return;
    }

    if !s.buffers.is_empty() {
        s.focus = PanelSlot::Main;
    } else {
        s.focus = PanelSlot::Side;
    }
}

/// Create unmaterialized buffer entries for startup arg files.
fn ensure_startup_arg_buffers(s: &mut AppState) {
    if s.startup.arg_paths.is_empty() {
        return;
    }
    for path in s.startup.arg_paths.clone().iter() {
        if !s.tabs.iter().any(|t| *t.path() == *path) {
            s.tabs.push_back(led_state::Tab::new(path.clone()));
        }
        if !s.buffers.contains_key(path) {
            let mut buf = BufferState::new(path.clone());
            buf.set_create_if_missing(true);
            s.buffers_mut().insert(path.clone(), Rc::new(buf));
        }
    }
}

/// Ensure a buffer entry exists for `path`. If the buffer is not yet materialized,
/// the unified materialization stream in derived will emit `DocStoreOut::Open`.
pub(crate) fn request_open(s: &mut AppState, path: CanonPath, create_if_missing: bool) {
    if !s.tabs.iter().any(|t| *t.path() == path) {
        s.tabs.push_back(led_state::Tab::new(path.clone()));
    }
    if !s.buffers.contains_key(&path) {
        let mut buf = BufferState::new(path.clone());
        buf.set_create_if_missing(create_if_missing);
        s.buffers_mut().insert(path, Rc::new(buf));
    } else if let Some(buf) = s.buf_mut(&path) {
        if !buf.is_materialized() {
            buf.set_create_if_missing(create_if_missing);
        }
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
            if let Some(path) = state.active_tab.clone() {
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
    ActivateBuffer(CanonPath),
    Action(Action),
    EvictOneBuffer,
    KbdMacroSetCount(usize),
    Alert {
        info: Option<String>,
        warn: Option<String>,
    },
    ResumeComplete,
    BufferOpen {
        path: CanonPath,
        doc: Arc<dyn Doc>,
        cursor: (usize, usize),
        scroll: (usize, usize),
        activate: bool,
        notify_hash: String,
        /// Session restore: undo entries + persistence state
        undo_entries: Vec<led_core::UndoEntry>,
        persisted_undo_len: usize,
        chain_id: Option<String>,
        last_seen_seq: i64,
        distance_from_save: i32,
    },
    BufferSaved {
        path: CanonPath,
        buf: BufferState,
        undo_clear_path: Option<CanonPath>,
    },
    BufferSavedAs {
        path: CanonPath,
        buf: BufferState,
        new_path: CanonPath,
        undo_clear_path: Option<CanonPath>,
    },
    BufferUpdate(CanonPath, BufferState),
    ConfigKeys(ConfigFile<Keys>),
    ConfigTheme(ConfigFile<Theme>),
    DirListed(CanonPath, Vec<led_fs::DirEntry>),
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
        statuses: HashMap<CanonPath, HashSet<FileStatus>>,
        branch: Option<String>,
    },
    GitLineStatuses {
        path: CanonPath,
        statuses: Vec<led_core::git::LineStatus>,
    },
    ForceRedraw(led_core::RedrawSeq),
    Keymap(Rc<Keymap>),
    Resize(u16, u16),
    NotifyEvent {
        path: Option<CanonPath>,
    },
    SessionOpenFailed {
        path: CanonPath,
    },
    SessionRestored {
        active_tab_order: Option<usize>,
        show_side_panel: bool,
        positions: HashMap<CanonPath, led_workspace::SessionBuffer>,
        pending_opens: Vec<CanonPath>,
        browser_selected: usize,
        browser_scroll_offset: usize,
        browser_expanded_dirs: HashSet<CanonPath>,
        jump_entries: std::collections::VecDeque<led_state::JumpPosition>,
        jump_index: usize,
        pending_lists: Vec<CanonPath>,
    },
    SessionSaved,
    WatchersReady,
    Resumed,
    SyntaxUpdate {
        path: CanonPath,
        version: led_core::DocVersion,
        highlights: Rc<Vec<(led_core::Row, HighlightSpan)>>,
        bracket_pairs: Vec<BracketPair>,
        indent: Option<String>,
        indent_row: Option<led_core::Row>,
        reindent_chars: Arc<[char]>,
    },
    UndoFlushed {
        path: Option<CanonPath>,
        chain_id: String,
        last_seen_seq: i64,
    },
    UndoFlushReady {
        path: CanonPath,
        flush: led_state::UndoFlush,
    },
    TimerFired(&'static str),
    TouchArgFiles {
        entries: Vec<CanonPath>,
    },
    Workspace {
        workspace: Workspace,
        initial_dirs: Vec<CanonPath>,
    },
    WorkspaceChanged {
        dirs: Vec<CanonPath>,
    },
    GitChanged,
    // LSP
    LspNavigate {
        path: CanonPath,
        row: led_core::Row,
        col: led_core::Col,
    },
    LspEdits {
        edits: Vec<led_lsp::FileEdit>,
    },
    LspCompletion {
        items: Vec<led_lsp::CompletionItem>,
        prefix_start_col: led_core::Col,
    },
    LspCodeActions {
        actions: Vec<String>,
    },
    LspDiagnostics {
        path: CanonPath,
        diagnostics: Vec<led_lsp::Diagnostic>,
        content_hash: led_core::PersistedContentHash,
    },
    LspInlayHints {
        path: CanonPath,
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

            Mut::ResumeComplete => "ResumeComplete",
            Mut::BufferOpen { .. } => "BufferOpen",
            Mut::BufferSaved { .. } => "BufferSaved",
            Mut::BufferSavedAs { .. } => "BufferSavedAs",
            Mut::BufferUpdate(_, _) => "BufferUpdate",
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

            Mut::Resize(_, _) => "Resize",
            Mut::NotifyEvent { .. } => "NotifyEvent",
            Mut::SessionOpenFailed { .. } => "SessionOpenFailed",
            Mut::SessionRestored { .. } => "SessionRestored",
            Mut::SessionSaved => "SessionSaved",
            Mut::WatchersReady => "WatchersReady",
            Mut::Resumed => "Resumed",
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
        let start = led_core::CharOffset(buf.doc().line_to_char(te.start_row).0 + *te.start_col);
        let end = led_core::CharOffset(buf.doc().line_to_char(te.end_row).0 + *te.end_col);
        if start != end {
            buf.remove_text(start, end);
        }
        if !te.new_text.is_empty() {
            buf.insert_text(start, &te.new_text);
        }
    }
}

// ── Preview combinator ──
