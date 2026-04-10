use std::rc::Rc;

use led_core::rx::Stream;
use led_core::{Action, PanelSlot};
use led_state::AppState;

use super::{Mut, compute_cycle_tab, has_blocking_overlay};

/// Simple UI action streams: toggle panels, quit, suspend, resize, yank,
/// LSP requests, tab cycling, inlay hints toggle.
pub fn ui_actions_of(raw_actions: &Stream<Action>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    let toggle_side_panel_s = raw_actions
        .filter(|a| matches!(a, Action::ToggleSidePanel))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .map(|(_, s)| Mut::SetShowSidePanel(!s.show_side_panel))
        .stream();

    let toggle_focus_s = raw_actions
        .filter(|a| matches!(a, Action::ToggleFocus))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .map(|(_, s)| {
            Mut::SetFocus(match s.focus {
                PanelSlot::Main => PanelSlot::Side,
                PanelSlot::Side => PanelSlot::Main,
                other => other,
            })
        })
        .stream();

    let quit_s = raw_actions
        .filter(|a| matches!(a, Action::Quit))
        .map(|_| Mut::SetPhase(led_state::Phase::Exiting))
        .stream();

    let suspend_s = raw_actions
        .filter(|a| matches!(a, Action::Suspend))
        .map(|_| Mut::SetPhase(led_state::Phase::Suspended))
        .stream();

    // Simple LSP request actions
    let lsp_goto_def_s = raw_actions
        .filter(|a| matches!(a, Action::LspGotoDefinition))
        .map(|_| Mut::LspRequestPending(Some(led_state::LspRequest::GotoDefinition)))
        .stream();

    let lsp_format_s = raw_actions
        .filter(|a| matches!(a, Action::LspFormat))
        .map(|_| Mut::LspRequestPending(Some(led_state::LspRequest::Format)))
        .stream();

    let lsp_code_action_s = raw_actions
        .filter(|a| matches!(a, Action::LspCodeAction))
        .map(|_| Mut::LspRequestPending(Some(led_state::LspRequest::CodeAction)))
        .stream();

    // Yank triggers clipboard read
    let yank_s = raw_actions
        .filter(|a| matches!(a, Action::Yank))
        .map(|_| Mut::PendingYank)
        .stream();

    // Resize (from test harness -- terminal resize already handled in actions_of)
    let action_resize_s = raw_actions
        .filter_map(|a| match a {
            Action::Resize(w, h) => Some(Mut::Resize(w, h)),
            _ => None,
        })
        .stream();

    // Tab cycling: pure computation of next/prev tab path
    let next_tab_s = raw_actions
        .filter(|a| matches!(a, Action::NextTab))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .filter_map(|(_, s)| compute_cycle_tab(&s, 1))
        .map(Mut::ActivateBuffer)
        .stream();

    let prev_tab_s = raw_actions
        .filter(|a| matches!(a, Action::PrevTab))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .filter_map(|(_, s)| compute_cycle_tab(&s, -1))
        .map(Mut::ActivateBuffer)
        .stream();

    // LspToggleInlayHints: toggle flag + clear hints
    let lsp_toggle_hints_s = raw_actions
        .filter(|a| matches!(a, Action::LspToggleInlayHints))
        .sample_combine(state)
        .filter(|(_, s)| !has_blocking_overlay(s))
        .map(|(_, s)| Mut::ToggleInlayHints(!s.lsp.inlay_hints_enabled))
        .stream();

    let merged: Stream<Mut> = Stream::new();
    toggle_side_panel_s.forward(&merged);
    toggle_focus_s.forward(&merged);
    quit_s.forward(&merged);
    suspend_s.forward(&merged);
    lsp_goto_def_s.forward(&merged);
    lsp_format_s.forward(&merged);
    lsp_code_action_s.forward(&merged);
    yank_s.forward(&merged);
    action_resize_s.forward(&merged);
    next_tab_s.forward(&merged);
    prev_tab_s.forward(&merged);
    lsp_toggle_hints_s.forward(&merged);
    merged
}
