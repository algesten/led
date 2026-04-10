use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

mod action;
mod actions_of;
mod buffers_of;
mod edit;
mod editing_of;
pub(crate) mod file_search;
pub(crate) mod find_file;
mod find_file_of;
mod gh_pr_of;
mod isearch_of;
mod jump;
mod jump_of;
mod kill_of;
mod lsp_of;
mod mov;
mod movement_of;
mod process_of;
mod save_of;
mod search;
mod session_of;
mod sync_of;
mod ui_actions_of;

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

    let session_s = session_of::session_of(&drivers.workspace_in, &state);

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

    let (actions_muts_s, keyboard_actions_s) = actions_of(&drivers.terminal_in, &state);

    // Unified raw action stream: keyboard + test harness
    let raw_actions: Stream<Action> = Stream::new();
    keyboard_actions_s.forward(&raw_actions);
    drivers.actions_in.forward(&raw_actions);

    // Single sample_combine for both isearch_of and unmigrated_actions_s.
    // This ensures both see the SAME state snapshot for each action.
    let actions_with_state: Stream<(Action, Rc<AppState>)> = raw_actions.sample_combine(&state);

    let isearch_s = isearch_of::isearch_of(&actions_with_state, &state, is_migrated);

    // LSP code action picker: absorbs all actions.
    let lsp_code_action_picker_s = actions_with_state
        .filter(|(_, s)| s.lsp.code_actions.is_some())
        .filter(|(a, _)| !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend))
        .map(|(a, _)| Mut::LspCodeActionPickerAction(a))
        .stream();

    // LSP rename overlay: absorbs all actions when focused.
    let lsp_rename_action_s = actions_with_state
        .filter(|(_, s)| s.lsp.rename.is_some() && s.focus == PanelSlot::Overlay)
        .filter(|(a, _)| !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend))
        .map(|(a, _)| Mut::LspRenameAction(a))
        .stream();

    // LSP completion: when active, route actions (except pass-through) to handler.
    let lsp_completion_action_s = actions_with_state
        .filter(|(a, s)| {
            s.lsp.completion.is_some()
                && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend)
        })
        .map(|(a, _)| Mut::LspCompletionAction(a))
        .stream();

    // File search: when active, route actions to FileSearchAction Mut.
    // Pass through: Resize, Quit, Suspend (and catch-all on input deactivates + passes).
    let file_search_action_s = actions_with_state
        .filter(|(a, s)| {
            s.file_search.is_some()
                && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend)
        })
        .map(|(a, _)| Mut::FileSearchAction(a))
        .stream();

    // Find file: when active, route actions to FindFileAction Mut.
    let find_file_action_s = actions_with_state
        .filter(|(a, s)| {
            s.find_file.is_some()
                && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend)
        })
        .map(|(a, _)| Mut::FindFileAction(a))
        .stream();

    // Only unmigrated actions not consumed by any modal go through handle_action.
    let unmigrated_actions_s = actions_with_state
        .filter(|(a, s)| {
            !is_migrated(a)
                && !isearch_of::is_consumed_by_isearch(a, s)
                && !(s.file_search.is_some()
                    && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend))
                && !(s.find_file.is_some()
                    && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend))
                && !(s.lsp.completion.is_some()
                    && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend))
                && !(s.lsp.code_actions.is_some()
                    && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend))
                && !(s.lsp.rename.is_some()
                    && s.focus == PanelSlot::Overlay
                    && !matches!(a, Action::Resize(..) | Action::Quit | Action::Suspend))
        })
        .map(|(a, _)| Mut::Action(a))
        .stream();

    // ── Pre-match guard streams (apply to migrated actions) ──
    // These replicate the cross-cutting logic from handle_action's pre-match guards.

    let macro_record_s = raw_actions
        .filter(|a| is_migrated(a))
        .filter(|a| {
            !matches!(
                a,
                Action::KbdMacroStart | Action::KbdMacroEnd | Action::KbdMacroExecute
            )
        })
        .sample_combine(&state)
        .filter(|(_, s)| s.kbd_macro.recording)
        .map(|(a, _)| Mut::KbdMacroRecord(a))
        .stream();

    let confirm_kill_accept_s = raw_actions
        .filter(|a| matches!(a, Action::InsertChar('y' | 'Y')))
        .sample_combine(&state)
        .filter(|(_, s)| s.confirm_kill)
        .map(|_| Mut::ForceKillBuffer)
        .stream();

    let confirm_kill_dismiss_s = raw_actions
        .filter(|a| is_migrated(a))
        .sample_combine(&state)
        .filter(|(_, s)| s.confirm_kill)
        .map(|_| Mut::DismissConfirmKill)
        .stream();

    let kill_ring_break_s = raw_actions
        .filter(|a| is_migrated(a))
        .filter(|a| !matches!(a, Action::KillLine))
        .sample_combine(&state)
        .filter(|(_, s)| !s.confirm_kill)
        .map(|_| Mut::BreakKillAccumulation)
        .stream();
    let buffers_s = buffers_of(&drivers.docstore_in, &state);
    let process_s = process_of(&state);
    let lsp_s = lsp_of::lsp_of(&drivers.lsp_in, &state);

    // Resume complete: all session files resolved (opened or failed).
    // Common parent stream — branches into children that each produce one Mut.
    let resume_complete_s: Stream<Rc<AppState>> = state
        .filter(|s| s.phase == Phase::Resuming)
        .filter(|s| {
            !s.session.resume.is_empty()
                && s.session
                    .resume
                    .iter()
                    .all(|e| e.state != led_state::ResumeState::Pending)
        })
        .stream();

    let resume_phase_s = resume_complete_s
        .map(|_| Mut::SetPhase(Phase::Running))
        .stream();

    let resume_tab_s = resume_complete_s
        .filter_map(|s| resolve_resume_active_tab(&s))
        .map(Mut::SetActiveTab)
        .stream();

    let resume_ensure_tabs_s = resume_complete_s.flat_map(|s| {
        s.startup
            .arg_paths
            .iter()
            .map(|p| Mut::EnsureTab(p.clone(), true))
            .collect::<Vec<_>>()
    });

    let resume_focus_s = resume_complete_s
        .map(|s| Mut::SetFocus(resolve_focus_slot(&s)))
        .stream();

    let resume_reveal_s = resume_complete_s
        .filter_map(|s| s.startup.arg_dir.clone())
        .map(Mut::BrowserReveal)
        .stream();

    // ── Action streams (migrated from handle_action) ──
    //
    // All actions still flow through Mut::Action → handle_action for:
    // - Macro recording (pre-match guard)
    // - Confirm kill dismissal (pre-match guard)
    // - Modal interception (completion, code actions, rename, file search, etc.)
    //
    // handle_action's main match uses catch-all for migrated actions.
    // These streams produce the actual Muts. They filter out blocking overlays
    // (code_actions, rename) which absorb all actions.

    // ── Action streams (extracted to _of files) ──
    let ui_actions_s = ui_actions_of::ui_actions_of(&raw_actions, &state);
    let gh_pr_s = gh_pr_of::gh_pr_of(&drivers.gh_pr_in, &raw_actions, &state);
    let movement_s = movement_of::movement_of(&raw_actions, &actions_with_state, &state);
    let editing_s = editing_of::editing_of(&raw_actions, &actions_with_state, &state);
    let kill_s = kill_of::kill_of(&actions_with_state);
    let save_s = save_of::save_of(&raw_actions, &state);
    let jump_s = jump_of::jump_of(&raw_actions, &state);
    let find_file_s2 = find_file_of::find_file_of(&raw_actions, &state);

    // NextIssue/PrevIssue stay in handle_action — navigate_to_position
    // requires imperative state mutation (request_open, set active_tab, etc.)

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
                .map(|b| {
                    let path = b.path().cloned().unwrap();
                    let chain_id = b
                        .chain_id()
                        .map(String::from)
                        .unwrap_or_else(led_workspace::new_chain_id);
                    (path, chain_id, b.content_hash(), flush_entries(b))
                })
                .filter(|(_, _, _, fe)| !fe.entries.is_empty())
                .map(|(path, chain_id, content_hash, fe)| Mut::UndoFlushReady {
                    path: path.clone(),
                    flush: led_state::UndoFlush {
                        file_path: path,
                        chain_id,
                        content_hash,
                        undo_cursor: fe.undo_cursor,
                        distance_from_save: fe.distance_from_save,
                        entries: fe.entries,
                    },
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
        .filter_map(|((dir, entries), s)| Some((dir, entries, s.find_file.as_ref()?.clone())))
        .filter(|(dir, _, ff)| *dir == find_file::expected_dir(&ff.input))
        .map(|(_, entries, mut ff)| {
            ff.completions = entries;
            ff.selected = None;
            Mut::FindFileListed(ff)
        })
        .stream();

    let clipboard_s = drivers
        .clipboard_in
        .map(|ev| match ev {
            led_clipboard::ClipboardIn::Text(text) => text,
        })
        .sample_combine(&state)
        // Fall back to kill ring content when system clipboard has no text.
        .map(|(text, s)| {
            if text.is_empty() {
                (s.kill_ring.content.clone(), s)
            } else {
                (text, s)
            }
        })
        .filter(|(text, _)| !text.is_empty())
        .filter_map(|(text, s)| Some((text, s.dims?, s.active_tab.clone()?, s)))
        .filter_map(|(text, dims, path, s)| {
            let buf = (**s.buffers.get(&path)?).clone();
            Some((text, dims, path, buf))
        })
        .map(|(text, dims, path, mut buf)| {
            action::close_group_on_move(&mut buf);
            buf.clear_mark();
            let (r, c, a) = edit::yank(&mut buf, &text);
            buf.set_cursor(led_core::Row(r), led_core::Col(c), led_core::Col(a));
            action::close_group_on_move(&mut buf);
            let (sr, ssl) = mov::adjust_scroll(&buf, &dims);
            buf.set_scroll(led_core::Row(sr), led_core::SubLine(ssl));
            Mut::BufferUpdate(path, buf)
        })
        .stream();

    // ── Syntax: common parent, split highlights vs indent ──

    let syntax_parent_s: Stream<_> = drivers.syntax_in.sample_combine(&state);

    // Child 1: offer highlights when indent won't invalidate them
    let syntax_highlights_s = syntax_parent_s
        .filter(|(syn, s)| !syntax_will_indent(syn, s))
        .map(|(syn, _)| Mut::SyntaxHighlights {
            path: syn.path,
            version: syn.doc_version,
            highlights: syn.highlights,
            bracket_pairs: syn.bracket_pairs,
            reindent_chars: syn.reindent_chars,
        })
        .stream();

    // Child 2: apply indent when indent_row matches and version is current
    let syntax_indent_s = syntax_parent_s
        .filter(|(syn, s)| syntax_can_apply_indent(syn, s))
        .map(|(syn, s)| {
            let tab_stop = s.dims.map(|d| d.tab_stop);
            Mut::ApplyIndent {
                path: syn.path,
                indent: syn.indent,
                indent_row: syn.indent_row.unwrap(),
                tab_stop,
                reindent_chars: syn.reindent_chars,
            }
        })
        .stream();

    // Child 3: set reindent_chars only (will_indent but version mismatch)
    let syntax_reindent_s = syntax_parent_s
        .filter(|(syn, s)| syntax_will_indent(syn, s) && !syntax_can_apply_indent(syn, s))
        .map(|(syn, _)| Mut::SetReindentChars {
            path: syn.path,
            chars: syn.reindent_chars,
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
        .filter_map(|ev| match ev {
            led_file_search::FileSearchIn::Results { results } => Some(results),
            _ => None,
        })
        .sample_combine(&state)
        .filter_map(|(results, s)| Some((results, s.file_search.clone()?)))
        .map(|(results, mut fs)| {
            fs.results = results;
            fs.rebuild_flat_hits();
            let preview = preview_for_selected_hit(&fs);
            Mut::FileSearchResults(fs, preview)
        })
        .stream();

    let file_search_replace_s = drivers
        .file_search_in
        .filter_map(|ev| match ev {
            led_file_search::FileSearchIn::ReplaceComplete {
                results,
                replaced_count,
            } => Some((results, replaced_count)),
            _ => None,
        })
        .sample_combine(&state)
        .filter_map(|((results, count), s)| Some((results, count, s.file_search.clone()?)))
        .map(|(results, count, mut fs)| {
            fs.results = results;
            fs.rebuild_flat_hits();
            let preview = preview_for_selected_hit(&fs);
            Mut::FileSearchReplaceComplete(fs, preview, count)
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
    actions_muts_s.forward(&muts);
    // Pre-match guard streams for migrated actions
    macro_record_s.forward(&muts);
    confirm_kill_dismiss_s.forward(&muts);
    kill_ring_break_s.forward(&muts);
    confirm_kill_accept_s.forward(&muts);
    // Action streams (extracted to _of files)
    ui_actions_s.forward(&muts);
    gh_pr_s.forward(&muts);
    movement_s.forward(&muts);
    editing_s.forward(&muts);
    kill_s.forward(&muts);
    save_s.forward(&muts);
    jump_s.forward(&muts);
    find_file_s2.forward(&muts);
    // Modal streams + unmigrated — all share the same actions_with_state snapshot.
    isearch_s.forward(&muts);
    lsp_code_action_picker_s.forward(&muts);
    lsp_rename_action_s.forward(&muts);
    lsp_completion_action_s.forward(&muts);
    file_search_action_s.forward(&muts);
    find_file_action_s.forward(&muts);
    unmigrated_actions_s.forward(&muts);
    buffers_s.forward(&muts);
    process_s.forward(&muts);
    timers_s.forward(&muts);
    undo_flush_s.forward(&muts);
    fs_dir_listed_s.forward(&muts);
    fs_find_file_listed_s.forward(&muts);
    clipboard_s.forward(&muts);
    syntax_highlights_s.forward(&muts);
    syntax_indent_s.forward(&muts);
    syntax_reindent_s.forward(&muts);
    git_file_s.forward(&muts);
    git_line_s.forward(&muts);
    file_search_s.forward(&muts);
    resume_phase_s.forward(&muts);
    resume_ensure_tabs_s.forward(&muts);
    resume_tab_s.forward(&muts);
    resume_focus_s.forward(&muts);
    resume_reveal_s.forward(&muts);
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
            Mut::SetPrInfo(info) => {
                s.git_mut().pr = info;
            }
            Mut::SetPendingOpenUrl(url) => {
                s.pending_open_url.set(Some(url));
            }
            Mut::Alert { info } => {
                s.alerts.info = info;
            }
            Mut::Warn { key, message } => {
                s.alerts.set_warn(key, message);
            }
            Mut::SetPhase(phase) => {
                log::info!("phase: {:?} → {:?}", s.phase, phase);
                s.phase = phase;
            }
            Mut::SetActiveTab(path) => {
                s.active_tab = Some(path);
            }
            Mut::SetFocus(focus) => {
                s.focus = focus;
            }
            Mut::EnsureTab(path, create_if_missing) => {
                if !s.tabs.iter().any(|t| *t.path() == path) {
                    s.tabs.push_back(led_state::Tab::new(path.clone()));
                }
                if !s.buffers.contains_key(&path) {
                    let mut buf = BufferState::new(path.clone());
                    if create_if_missing {
                        buf.set_create_if_missing(true);
                    }
                    s.buffers_mut().insert(path, Rc::new(buf));
                }
            }
            Mut::BrowserReveal(dir) => {
                let new_dirs = s.browser_mut().reveal(&dir);
                if !new_dirs.is_empty() {
                    s.pending_lists.set(new_dirs);
                }
                action::browser_scroll_to_selected(&mut s);
            }
            Mut::JumpRecord(pos) => {
                jump::record_jump(&mut s, pos);
            }
            Mut::RequestOpen(path) => {
                request_open(&mut s, path, false);
            }
            Mut::SetTabPendingCursor {
                path,
                row,
                col,
                scroll_row,
            } => {
                if let Some(tab) = s.tabs.iter_mut().find(|t| *t.path() == path) {
                    tab.set_cursor(row, col, scroll_row);
                }
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
                handle_buffer_saved_as(&mut s, path, buf, new_path, undo_clear_path);
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
            Mut::SetActiveTabOrder(order) => {
                s.session.active_tab_order = order;
            }
            Mut::SetShowSidePanel(show) => {
                s.show_side_panel = show;
                if let Some(ref mut dims) = s.dims {
                    dims.show_side_panel = show;
                }
            }
            Mut::SetSessionPositions(positions) => {
                s.session.positions = positions;
            }
            Mut::SetBrowserState {
                selected,
                scroll_offset,
                expanded_dirs,
            } => {
                let b = s.browser_mut();
                b.selected = selected;
                b.scroll_offset = scroll_offset;
                b.expanded_dirs = expanded_dirs;
            }
            Mut::SetJumpState { entries, index } => {
                s.jump.entries = entries;
                s.jump.index = index;
            }
            Mut::SetPendingLists(dirs) => {
                s.pending_lists.set(dirs);
            }
            Mut::SetResumeEntries(paths) => {
                s.session.resume = paths
                    .iter()
                    .map(|p| led_state::ResumeEntry {
                        path: p.clone(),
                        state: led_state::ResumeState::Pending,
                    })
                    .collect();
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
            Mut::SyntaxHighlights {
                path,
                version,
                highlights,
                bracket_pairs,
                reindent_chars,
            } => {
                if let Some(buf) = s.buf_mut(&path) {
                    buf.set_reindent_chars(reindent_chars);
                    buf.offer_syntax(highlights, bracket_pairs, version);
                }
            }
            Mut::ApplyIndent {
                path,
                indent,
                indent_row,
                tab_stop,
                reindent_chars,
            } => {
                if let Some(buf) = s.buf_mut(&path) {
                    buf.set_reindent_chars(reindent_chars);
                    let was_tab = buf.pending_tab_fallback();
                    buf.request_indent(None, false);
                    if let Some(ref new_indent) = indent {
                        let cursor_on_row = buf.cursor_row() == indent_row;
                        edit::apply_indent(buf, *indent_row, new_indent, cursor_on_row);
                    } else if was_tab {
                        if let Some(ts) = tab_stop {
                            edit::insert_soft_tab(buf, ts);
                        }
                    }
                    buf.close_group_on_move();
                }
            }
            Mut::SetReindentChars { path, chars } => {
                if let Some(buf) = s.buf_mut(&path) {
                    buf.set_reindent_chars(chars);
                }
            }
            Mut::Resumed => {
                log::info!("phase: {:?} → Running (Resumed)", s.phase);
                s.phase = Phase::Running;
                s.git_mut().pending_file_scan.set(());
            }
            Mut::TimerFired(name) => handle_timer(&mut s, name),
            Mut::TouchArgFiles { entries } => {
                touch_and_reorder_arg_files(&mut s, &entries);
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
            Mut::LspRequestPending(request) => {
                s.lsp_mut().pending_request.set(request);
            }
            Mut::PendingYank => {
                s.kill_ring.pending_yank.set(());
            }
            Mut::KbdMacroRecord(a) => {
                s.kbd_macro.current.push(a);
            }
            Mut::DismissConfirmKill => {
                s.confirm_kill = false;
                s.alerts.info = None;
            }
            Mut::BreakKillAccumulation => {
                s.kill_ring.break_accumulation();
            }
            Mut::LspCodeActionPickerAction(a) => {
                action::handle_code_action_picker(&mut s, &a);
            }
            Mut::LspRenameAction(a) => {
                action::handle_rename_action(&mut s, &a);
            }
            Mut::LspCompletionAction(a) => {
                action::handle_completion_action(&mut s, &a);
            }
            Mut::FileSearchAction(a) => {
                file_search::handle_file_search_action(&mut s, &a);
            }
            Mut::FindFileAction(a) => {
                find_file::handle_find_file_action(&mut s, &a);
            }
            Mut::KillRingAccumulate(text) => {
                s.kill_ring.accumulate(&text);
            }
            Mut::KillRingSet(text) => {
                s.kill_ring.set(text);
            }
            Mut::SetFindFile(fs) => {
                s.find_file = Some(fs);
            }
            Mut::SetPendingFindFileList(dir, prefix, show_hidden) => {
                s.pending_find_file_list
                    .set(Some((dir, prefix, show_hidden)));
            }
            Mut::SetFileSearch(fs) => {
                s.file_search = Some(fs);
            }
            Mut::SetLspRename(rename) => {
                s.lsp_mut().rename = Some(rename);
            }
            Mut::TriggerFileSearch => {
                file_search::trigger_search(&mut s);
            }
            Mut::ForceKillBuffer => {
                action::force_kill_buffer(&mut s);
            }
            Mut::SearchAccept(path) => {
                if let Some(buf) = s.buf_mut(&path) {
                    search::search_accept(buf);
                }
            }
            Mut::SetPendingSaveAfterFormat => {
                log::info!("save: requesting LSP format");
                s.lsp_mut().pending_save_after_format = true;
            }
            Mut::SaveRequest => {
                s.save_request.set(());
            }
            Mut::SaveAllRequest => {
                s.save_all_request.set(());
            }
            Mut::SetJumpIndex(index) => {
                s.jump.index = index;
            }
            Mut::ToggleInlayHints(enabled) => {
                s.lsp_mut().inlay_hints_enabled = enabled;
                if !enabled {
                    for buf in s.buffers_mut().values_mut() {
                        Rc::make_mut(buf).clear_inlay_hints();
                    }
                }
            }
            Mut::LspEdits { edits } => {
                for fe in edits {
                    if let Some(buf) = s.buf_mut(&fe.path) {
                        apply_text_edits(buf, &fe.edits);
                        buf.close_group_on_move();
                    }
                }
            }
            Mut::LspFormatDone => {
                log::info!("save: format done, triggering save");
                s.lsp_mut().pending_save_after_format = false;
                s.save_request.set(());
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
                apply_trigger_chars(&mut s, &extensions, &triggers);
            }
        }

        gc_orphan_buffers(&mut s);
        gc_notify_hashes(&mut s);
        apply_pending_cursors(&mut s);

        Rc::new(s)
    });

    state
}

/// Pure: compute active tab for resume from session order, fallback, or CLI args.
fn resolve_resume_active_tab(s: &AppState) -> Option<CanonPath> {
    // CLI args take priority
    if let Some(arg_path) = s.startup.arg_paths.last() {
        return Some(arg_path.clone());
    }
    // Session's saved tab order
    if let Some(order) = s.session.active_tab_order {
        let non_preview: Vec<_> = s.tabs.iter().filter(|t| !t.is_preview()).collect();
        if let Some(tab) = non_preview.get(order) {
            return Some(tab.path().clone());
        }
    }
    // Fallback: first materialized tab
    s.tabs
        .iter()
        .find(|t| s.buffers.get(t.path()).is_some_and(|b| b.is_materialized()))
        .map(|t| t.path().clone())
}

/// Pure: compute focus slot when entering Running.
/// Pure: compute the next/prev tab path by cycling through materialized non-preview tabs.
pub(super) fn compute_cycle_tab(s: &AppState, direction: i32) -> Option<CanonPath> {
    let active_path = s.active_tab.as_ref()?;
    let tabs: Vec<&CanonPath> = s
        .tabs
        .iter()
        .filter(|t| !t.is_preview())
        .filter(|t| {
            s.buffers
                .get(t.path())
                .map_or(false, |b| b.is_materialized())
        })
        .map(|t| t.path())
        .collect();
    let pos = tabs.iter().position(|p| *p == active_path)?;
    let len = tabs.len() as i32;
    let next = ((pos as i32 + direction).rem_euclid(len)) as usize;
    Some(tabs[next].clone())
}

/// True when the active buffer has an LSP server (used for format-before-save).
fn is_migrated(action: &Action) -> bool {
    matches!(
        action,
        Action::ToggleSidePanel
            | Action::ToggleFocus
            | Action::Quit
            | Action::Suspend
            | Action::LspGotoDefinition
            | Action::LspFormat
            | Action::LspCodeAction
            | Action::Yank
            | Action::Resize(..)
            | Action::LineStart
            | Action::LineEnd
            | Action::MatchBracket
            | Action::Undo
            | Action::Redo
            | Action::SetMark
            | Action::NextTab
            | Action::PrevTab
            | Action::LspToggleInlayHints
            | Action::Save
            | Action::SaveAll
            | Action::SaveNoFormat
            | Action::SaveForce
            | Action::JumpBack
            | Action::JumpForward
            | Action::InsertChar(_)
            | Action::InsertNewline
            | Action::InsertTab
            | Action::DeleteBackward
            | Action::DeleteForward
            | Action::KillLine
            | Action::KillRegion
            | Action::FindFile
            | Action::SaveAs
            | Action::OpenFileSearch
            | Action::LspRename
            | Action::SortImports
    )
}

pub(super) fn has_active_lsp(s: &AppState) -> bool {
    s.active_tab
        .as_ref()
        .and_then(|path| s.buffers.get(path))
        .and_then(|b| b.path())
        .is_some_and(|_| !s.lsp.server_name.is_empty())
}

/// True when a blocking overlay is active that absorbs all actions.
/// Migrated action streams must not fire in this state.
pub(super) fn active_buf(s: &AppState) -> Option<&BufferState> {
    s.active_tab
        .as_ref()
        .and_then(|p| s.buffers.get(p))
        .map(|b| &**b)
}

pub(super) fn is_word_boundary(buf: &BufferState) -> bool {
    led_core::with_line_buf(|line| {
        buf.doc().line(buf.cursor_row(), line);
        line.chars()
            .nth(buf.cursor_col().0.saturating_sub(1))
            .map_or(false, |p| !p.is_whitespace())
    })
}

pub(super) fn is_indent_in_flight(s: &AppState) -> bool {
    active_buf(s).map_or(false, |b| b.pending_indent_row().is_some())
}

pub(super) fn has_blocking_overlay(s: &AppState) -> bool {
    s.lsp.code_actions.is_some() || (s.lsp.rename.is_some() && s.focus == PanelSlot::Overlay)
}

/// True when a modal dialog that captures editing/movement input is active.
/// Editor action streams must not fire in this state.
/// Input modal that consumes editing/movement actions.
/// Excludes isearch — handled by isearch_of.rs streams.
pub(super) fn has_input_modal(s: &AppState) -> bool {
    s.file_search.is_some() || s.find_file.is_some()
}

/// Like has_input_modal but also checks isearch. Used by editing action
/// streams that must not fire when isearch consumes the same actions.
pub(super) fn has_any_input_modal(s: &AppState) -> bool {
    has_input_modal(s) || isearch_of::is_in_isearch_pub(s)
}

pub(super) fn resolve_focus_slot(s: &AppState) -> PanelSlot {
    if s.startup.arg_dir.is_some() {
        PanelSlot::Side
    } else if !s.buffers.is_empty() || !s.startup.arg_paths.is_empty() {
        PanelSlot::Main
    } else {
        PanelSlot::Side
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

struct FlushEntries {
    entries: Vec<led_core::UndoEntry>,
    undo_cursor: usize,
    distance_from_save: i32,
}

fn flush_entries(b: &BufferState) -> FlushEntries {
    let mut undo = b.undo_history().clone();
    undo.flush_pending();
    FlushEntries {
        entries: undo.entries_from(b.persisted_undo_len()).to_vec(),
        undo_cursor: undo.entry_count(),
        distance_from_save: undo.distance_from_save(),
    }
}

fn apply_trigger_chars(s: &mut AppState, extensions: &[String], triggers: &[String]) {
    for buf in s.buffers_mut().values_mut() {
        let ext = buf
            .path()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if extensions.iter().any(|x| x == ext) {
            Rc::make_mut(buf).set_completion_triggers(triggers.to_vec());
        }
    }
}

fn touch_and_reorder_arg_files(s: &mut AppState, entries: &[CanonPath]) {
    for path in entries {
        if let Some(buf) = s.buf_mut(path) {
            buf.touch();
        }
    }
    if entries.len() > 1 {
        for path in entries {
            if let Some(pos) = s.tabs.iter().position(|t| *t.path() == *path) {
                let tab = s.tabs.remove(pos).unwrap();
                s.tabs.push_back(tab);
            }
        }
    }
}

fn handle_buffer_saved_as(
    s: &mut AppState,
    path: CanonPath,
    buf: BufferState,
    new_path: CanonPath,
    undo_clear_path: Option<CanonPath>,
) {
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
    action::reveal_active_buffer(s);
}

/// Will this syntax response modify the document via indent?
/// If so, highlights should be skipped (offsets would be wrong).
fn syntax_will_indent(syn: &led_syntax::SyntaxIn, s: &AppState) -> bool {
    let Some(row) = syn.indent_row else {
        return false;
    };
    let Some(buf) = s.buffers.get(&syn.path) else {
        return false;
    };
    let tab_stop = s.dims.map(|d| d.tab_stop);
    buf.pending_indent_row() == Some(row)
        && (syn.indent.is_some() || (buf.pending_tab_fallback() && tab_stop.is_some()))
}

/// Can we apply the indent from this syntax response?
/// Requires indent_row match AND version match.
fn syntax_can_apply_indent(syn: &led_syntax::SyntaxIn, s: &AppState) -> bool {
    let Some(row) = syn.indent_row else {
        return false;
    };
    let Some(buf) = s.buffers.get(&syn.path) else {
        return false;
    };
    buf.pending_indent_row() == Some(row) && buf.version() == syn.doc_version
}

fn preview_for_selected_hit(
    fs: &led_state::file_search::FileSearchState,
) -> Option<led_state::PreviewRequest> {
    let (group, hit) = fs.selected_hit()?;
    Some(led_state::PreviewRequest {
        path: group.path.clone(),
        row: hit.row,
        col: hit.col,
    })
}

/// Dematerialize any buffer not referenced by a tab. Catches both
/// Materialized (kill buffer) and Requested (preview scrolled past
/// before the docstore responded). Version is bumped to invalidate
/// any in-flight Open response.
fn gc_orphan_buffers(s: &mut AppState) {
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
}

/// Remove notify-hash entries for paths that no longer have a tab.
fn gc_notify_hashes(s: &mut AppState) {
    s.notify_hash_to_buffer
        .retain(|_, p| s.tabs.iter().any(|t| *t.path() == *p));
}

/// Apply pending cursor from tabs to materialized buffers.
fn apply_pending_cursors(s: &mut AppState) {
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
        "pr_settle" => {
            state.git_mut().pr_settle_seq.set(());
        }
        "pr_poll" => {
            state.git_mut().pr_poll_seq.set(());
        }
        _ => {}
    }
}

#[derive(Clone)]
enum Mut {
    ActivateBuffer(CanonPath),
    Action(Action),
    SetPhase(Phase),
    SetActiveTab(CanonPath),
    SetFocus(PanelSlot),
    EnsureTab(CanonPath, bool),
    BrowserReveal(CanonPath),
    SetActiveTabOrder(Option<usize>),
    SetShowSidePanel(bool),
    SetSessionPositions(HashMap<CanonPath, led_workspace::SessionBuffer>),
    SetBrowserState {
        selected: usize,
        scroll_offset: usize,
        expanded_dirs: HashSet<CanonPath>,
    },
    SetJumpState {
        entries: std::collections::VecDeque<led_state::JumpPosition>,
        index: usize,
    },
    SetPendingLists(Vec<CanonPath>),
    SetResumeEntries(Vec<CanonPath>),
    LspRequestPending(Option<led_state::LspRequest>),
    PendingYank,
    KbdMacroRecord(Action),
    DismissConfirmKill,
    BreakKillAccumulation,
    SearchAccept(CanonPath),
    ForceKillBuffer,
    KillRingAccumulate(String),
    KillRingSet(String),
    SetFindFile(led_state::FindFileState),
    SetPendingFindFileList(CanonPath, String, bool),
    SetFileSearch(led_state::file_search::FileSearchState),
    TriggerFileSearch,
    SetLspRename(led_state::RenameState),
    LspCodeActionPickerAction(Action),
    LspRenameAction(Action),
    LspCompletionAction(Action),
    FileSearchAction(Action),
    FindFileAction(Action),
    ToggleInlayHints(bool),
    SetPendingSaveAfterFormat,
    SaveRequest,
    SaveAllRequest,
    SetJumpIndex(usize),
    JumpRecord(led_state::JumpPosition),
    RequestOpen(CanonPath),
    LspFormatDone,
    SetTabPendingCursor {
        path: CanonPath,
        row: led_core::Row,
        col: led_core::Col,
        scroll_row: led_core::Row,
    },
    EvictOneBuffer,
    SetPrInfo(Option<led_state::PrInfo>),
    SetPendingOpenUrl(String),
    KbdMacroSetCount(usize),
    Alert {
        info: Option<String>,
    },
    Warn {
        key: String,
        message: String,
    },
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
    SessionSaved,
    WatchersReady,
    Resumed,
    SyntaxHighlights {
        path: CanonPath,
        version: led_core::DocVersion,
        highlights: Rc<Vec<(led_core::Row, HighlightSpan)>>,
        bracket_pairs: Vec<BracketPair>,
        reindent_chars: Arc<[char]>,
    },
    ApplyIndent {
        path: CanonPath,
        indent: Option<String>,
        indent_row: led_core::Row,
        tab_stop: Option<usize>,
        reindent_chars: Arc<[char]>,
    },
    SetReindentChars {
        path: CanonPath,
        chars: Arc<[char]>,
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
            Mut::SetPhase(_) => "SetPhase",
            Mut::SetActiveTab(_) => "SetActiveTab",
            Mut::SetFocus(_) => "SetFocus",
            Mut::EnsureTab(..) => "EnsureTab",
            Mut::BrowserReveal(_) => "BrowserReveal",
            Mut::Alert { .. } => "Alert",
            Mut::Warn { .. } => "Warn",

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
            Mut::SetActiveTabOrder(_) => "SetActiveTabOrder",
            Mut::SetShowSidePanel(_) => "SetShowSidePanel",
            Mut::SetSessionPositions(_) => "SetSessionPositions",
            Mut::SetBrowserState { .. } => "SetBrowserState",
            Mut::SetJumpState { .. } => "SetJumpState",
            Mut::SetPendingLists(_) => "SetPendingLists",
            Mut::SetResumeEntries(_) => "SetResumeEntries",
            Mut::SessionSaved => "SessionSaved",
            Mut::WatchersReady => "WatchersReady",
            Mut::Resumed => "Resumed",
            Mut::SyntaxHighlights { .. } => "SyntaxHighlights",
            Mut::ApplyIndent { .. } => "ApplyIndent",
            Mut::SetReindentChars { .. } => "SetReindentChars",
            Mut::UndoFlushed { .. } => "UndoFlushed",
            Mut::UndoFlushReady { .. } => "UndoFlushReady",
            Mut::TimerFired(_) => "TimerFired",
            Mut::TouchArgFiles { .. } => "TouchArgFiles",
            Mut::Workspace { .. } => "Workspace",
            Mut::WorkspaceChanged { .. } => "WorkspaceChanged",
            Mut::GitChanged => "GitChanged",
            Mut::JumpRecord(_) => "JumpRecord",
            Mut::RequestOpen(_) => "RequestOpen",
            Mut::SetTabPendingCursor { .. } => "SetTabPendingCursor",
            Mut::LspRequestPending(_) => "LspRequestPending",
            Mut::PendingYank => "PendingYank",
            Mut::KbdMacroRecord(_) => "KbdMacroRecord",
            Mut::DismissConfirmKill => "DismissConfirmKill",
            Mut::BreakKillAccumulation => "BreakKillAccumulation",
            Mut::SearchAccept(_) => "SearchAccept",
            Mut::KillRingAccumulate(_) => "KillRingAccumulate",
            Mut::KillRingSet(_) => "KillRingSet",
            Mut::SetFindFile(_) => "SetFindFile",
            Mut::SetPendingFindFileList(..) => "SetPendingFindFileList",
            Mut::SetFileSearch(_) => "SetFileSearch",
            Mut::TriggerFileSearch => "TriggerFileSearch",
            Mut::SetLspRename(_) => "SetLspRename",
            Mut::ForceKillBuffer => "ForceKillBuffer",
            Mut::LspCodeActionPickerAction(_) => "LspCodeActionPickerAction",
            Mut::LspRenameAction(_) => "LspRenameAction",
            Mut::LspCompletionAction(_) => "LspCompletionAction",
            Mut::FileSearchAction(_) => "FileSearchAction",
            Mut::FindFileAction(_) => "FindFileAction",
            Mut::ToggleInlayHints(_) => "ToggleInlayHints",
            Mut::SetPendingSaveAfterFormat => "SetPendingSaveAfterFormat",
            Mut::SaveRequest => "SaveRequest",
            Mut::SaveAllRequest => "SaveAllRequest",
            Mut::SetJumpIndex(_) => "SetJumpIndex",
            Mut::LspEdits { .. } => "LspEdits",
            Mut::LspFormatDone => "LspFormatDone",
            Mut::LspCompletion { .. } => "LspCompletion",
            Mut::LspCodeActions { .. } => "LspCodeActions",
            Mut::LspDiagnostics { .. } => "LspDiagnostics",
            Mut::LspInlayHints { .. } => "LspInlayHints",
            Mut::LspProgress { .. } => "LspProgress",
            Mut::LspTriggerChars { .. } => "LspTriggerChars",
            Mut::EvictOneBuffer => "EvictOneBuffer",
            Mut::SetPrInfo(_) => "SetPrInfo",
            Mut::SetPendingOpenUrl(_) => "SetPendingOpenUrl",
            Mut::KbdMacroSetCount(_) => "KbdMacroSetCount",
        }
    }

    fn alert(a: Alert) -> Self {
        match a {
            Alert::Info(v) => Mut::Alert { info: Some(v) },
            Alert::Warn(v) => Mut::Alert { info: Some(v) },
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
