use std::rc::Rc;

use led_core::Action;
use led_core::rx::Stream;
use led_state::AppState;

use super::{Mut, has_active_lsp, has_blocking_overlay};

/// Save streams: save, save-no-format, save-all.
pub fn save_of(raw_actions: &Stream<Action>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    let save_parent_s = raw_actions
        .filter(|a| matches!(a, Action::Save))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .stream();

    // Save with LSP: begin_save + request format
    let save_lsp_buf_s = save_parent_s
        .filter(|(_, s)| has_active_lsp(s))
        .filter_map(|(_, s)| {
            let path = s.active_tab.clone()?;
            let mut buf = (**s.buffers.get(&path)?).clone();
            buf.begin_save();
            buf.touch();
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    let save_lsp_format_s = save_parent_s
        .filter(|(_, s)| has_active_lsp(s))
        .map(|_| Mut::SetPendingSaveAfterFormat)
        .stream();

    let save_lsp_request_s = save_parent_s
        .filter(|(_, s)| has_active_lsp(s))
        .map(|_| Mut::LspRequestPending(Some(led_state::LspRequest::Format)))
        .stream();

    let save_lsp_alert_s = save_parent_s
        .filter(|(_, s)| has_active_lsp(s))
        .map(|_| Mut::Alert {
            info: Some("Formatting...".into()),
        })
        .stream();

    // Save without LSP: begin_save + cleanup + trigger save
    let save_direct_buf_s = save_parent_s
        .filter(|(_, s)| !has_active_lsp(s))
        .filter_map(|(_, s)| {
            let path = s.active_tab.clone()?;
            let mut buf = (**s.buffers.get(&path)?).clone();
            buf.begin_save();
            buf.touch();
            buf.apply_save_cleanup();
            buf.record_diag_save_point();
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    let save_direct_request_s = save_parent_s
        .filter(|(_, s)| !has_active_lsp(s))
        .map(|_| Mut::SaveRequest)
        .stream();

    // SaveNoFormat: begin_save + cleanup + save (no LSP format)
    let save_no_format_buf_s = raw_actions
        .filter(|a| matches!(a, Action::SaveNoFormat))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .filter_map(|(_, s)| {
            let path = s.active_tab.clone()?;
            let mut buf = (**s.buffers.get(&path)?).clone();
            buf.begin_save();
            buf.touch();
            buf.record_diag_save_point();
            Some(Mut::BufferUpdate(path, buf))
        })
        .stream();

    let save_no_format_request_s = raw_actions
        .filter(|a| matches!(a, Action::SaveNoFormat))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .map(|_| Mut::SaveRequest)
        .stream();

    // SaveAll: begin_save on all dirty buffers + trigger save_all
    let save_all_bufs_s = raw_actions
        .filter(|a| matches!(a, Action::SaveAll))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .flat_map(|(_, s)| {
            s.buffers
                .values()
                .filter(|b| b.is_dirty() && b.path().is_some())
                .map(|b| {
                    let path = b.path().cloned().unwrap();
                    let mut buf = (**b).clone();
                    buf.begin_save();
                    (path, buf)
                })
                .map(|(path, buf)| Mut::BufferUpdate(path, buf))
                .collect::<Vec<_>>()
        });

    let save_all_request_s = raw_actions
        .filter(|a| matches!(a, Action::SaveAll))
        .sample_combine(state)
        .filter(|(_, s)| {
            !has_blocking_overlay(s)
                && s.buffers
                    .values()
                    .any(|b| b.is_dirty() && b.path().is_some())
        })
        .map(|_| Mut::SaveAllRequest)
        .stream();

    let merged: Stream<Mut> = Stream::new();
    save_lsp_buf_s.forward(&merged);
    save_lsp_format_s.forward(&merged);
    save_lsp_request_s.forward(&merged);
    save_lsp_alert_s.forward(&merged);
    save_direct_buf_s.forward(&merged);
    save_direct_request_s.forward(&merged);
    save_no_format_buf_s.forward(&merged);
    save_no_format_request_s.forward(&merged);
    save_all_bufs_s.forward(&merged);
    save_all_request_s.forward(&merged);
    merged
}
