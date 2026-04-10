use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Action, PanelSlot};
use led_state::AppState;

use super::edit;
use super::{Mut, has_blocking_overlay};
use super::{file_search, find_file};

/// Find-file, save-as, open-file-search, and LSP rename activation streams.
pub fn find_file_of(raw_actions: &Stream<Action>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    // ── FindFile ──

    let find_file_parent_s = raw_actions
        .filter(|a| matches!(a, Action::FindFile))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .stream();

    let find_file_set_s = find_file_parent_s
        .map(|(_, s)| {
            let (fs, _) = find_file::compute_activate(&s);
            Mut::SetFindFile(fs)
        })
        .stream();

    let find_file_list_s = find_file_parent_s
        .map(|(_, s)| {
            let (_, (dir, prefix, show_hidden)) = find_file::compute_activate(&s);
            Mut::SetPendingFindFileList(dir, prefix, show_hidden)
        })
        .stream();

    // ── SaveAs ──

    let save_as_parent_s = raw_actions
        .filter(|a| matches!(a, Action::SaveAs))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .stream();

    let save_as_set_s = save_as_parent_s
        .map(|(_, s)| {
            let (fs, _) = find_file::compute_activate_save_as(&s);
            Mut::SetFindFile(fs)
        })
        .stream();

    let save_as_list_s = save_as_parent_s
        .map(|(_, s)| {
            let (_, (dir, prefix, show_hidden)) = find_file::compute_activate_save_as(&s);
            Mut::SetPendingFindFileList(dir, prefix, show_hidden)
        })
        .stream();

    // ── OpenFileSearch ──

    let open_file_search_parent_s = raw_actions
        .filter(|a| matches!(a, Action::OpenFileSearch))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .stream();

    let open_file_search_set_s = open_file_search_parent_s
        .map(|(_, s)| {
            let (fs, _) = file_search::compute_activate(&s);
            Mut::SetFileSearch(fs)
        })
        .stream();

    let open_file_search_focus_s = open_file_search_parent_s
        .map(|_| Mut::SetShowSidePanel(true))
        .stream();

    let open_file_search_focus2_s = open_file_search_parent_s
        .map(|_| Mut::SetFocus(PanelSlot::Side))
        .stream();

    // Clear mark if selected text was used as initial query
    let open_file_search_clear_mark_s = open_file_search_parent_s
        .filter(|(_, s)| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .and_then(|buf| edit::selected_text(buf))
                .is_some()
        })
        .filter_map(|(_, s)| {
            let path = s.active_tab.clone()?;
            let mut buf = (**s.buffers.get(&path)?).clone();
            buf.clear_mark();
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    // Trigger search if selected text was used
    let open_file_search_trigger_s = open_file_search_parent_s
        .filter(|(_, s)| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .and_then(|buf| edit::selected_text(buf))
                .is_some()
        })
        .map(|_| Mut::TriggerFileSearch)
        .stream();

    // ── LspRename ──

    let lsp_rename_parent_s = raw_actions
        .filter(|a| matches!(a, Action::LspRename))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .stream();

    let lsp_rename_set_s = lsp_rename_parent_s
        .filter_map(|(_, s)| {
            let path = s.active_tab.as_ref()?;
            let buf = s.buffers.get(path)?;
            let word = super::action::word_under_cursor(buf);
            let cursor = word.len();
            Some(Mut::SetLspRename(led_state::RenameState {
                input: word,
                cursor,
            }))
        })
        .stream();

    let lsp_rename_focus_s = lsp_rename_parent_s
        .filter(|(_, s)| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .is_some()
        })
        .map(|_| Mut::SetFocus(PanelSlot::Overlay))
        .stream();

    let merged: Stream<Mut> = Stream::new();
    find_file_set_s.forward(&merged);
    find_file_list_s.forward(&merged);
    save_as_set_s.forward(&merged);
    save_as_list_s.forward(&merged);
    open_file_search_set_s.forward(&merged);
    open_file_search_focus_s.forward(&merged);
    open_file_search_focus2_s.forward(&merged);
    open_file_search_clear_mark_s.forward(&merged);
    open_file_search_trigger_s.forward(&merged);
    lsp_rename_set_s.forward(&merged);
    lsp_rename_focus_s.forward(&merged);
    merged
}
