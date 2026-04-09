use std::rc::Rc;

use led_core::rx::Stream;
use led_lsp::LspIn;
use led_state::AppState;

use super::Mut;

pub fn lsp_of(lsp_in: &Stream<LspIn>, state: &Stream<Rc<AppState>>) -> Stream<Mut> {
    let navigate_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Navigate { .. }))
        .map(|ev| match ev {
            LspIn::Navigate { path, row, col } => Mut::LspNavigate { path, row, col },
            _ => unreachable!(),
        })
        .stream();

    let edits_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Edits { .. }))
        .sample_combine(state)
        .map(|(ev, _s)| match ev {
            LspIn::Edits { edits } => Mut::LspEdits { edits },
            _ => unreachable!(),
        })
        .stream();

    let completion_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Completion { .. }))
        .map(|ev| match ev {
            LspIn::Completion {
                items,
                prefix_start_col,
            } => Mut::LspCompletion {
                items,
                prefix_start_col,
            },
            _ => unreachable!(),
        })
        .stream();

    let code_actions_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::CodeActions { .. }))
        .map(|ev| match ev {
            LspIn::CodeActions { actions } => Mut::LspCodeActions { actions },
            _ => unreachable!(),
        })
        .stream();

    let diagnostics_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Diagnostics { .. }))
        .map(|ev| match ev {
            LspIn::Diagnostics {
                path,
                diagnostics,
                content_hash,
            } => Mut::LspDiagnostics {
                path,
                diagnostics,
                content_hash,
            },
            _ => unreachable!(),
        })
        .stream();

    let inlay_hints_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::InlayHints { .. }))
        .map(|ev| match ev {
            LspIn::InlayHints { path, hints } => Mut::LspInlayHints { path, hints },
            _ => unreachable!(),
        })
        .stream();

    let progress_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Progress { .. }))
        .map(|ev| match ev {
            LspIn::Progress {
                server_name,
                busy,
                detail,
            } => Mut::LspProgress {
                server_name,
                busy,
                detail,
            },
            _ => unreachable!(),
        })
        .stream();

    let error_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::Error { .. }))
        .map(|ev| match ev {
            LspIn::Error { message } => Mut::Warn {
                key: "lsp".to_string(),
                message: format!("LSP: {}", message),
            },
            _ => unreachable!(),
        })
        .stream();

    let trigger_chars_s = lsp_in
        .filter(|ev| matches!(ev, LspIn::TriggerChars { .. }))
        .map(|ev| match ev {
            LspIn::TriggerChars {
                extensions,
                triggers,
            } => Mut::LspTriggerChars {
                extensions,
                triggers,
            },
            _ => unreachable!(),
        })
        .stream();

    let muts: Stream<Mut> = Stream::new();
    navigate_s.forward(&muts);
    edits_s.forward(&muts);
    completion_s.forward(&muts);
    code_actions_s.forward(&muts);
    diagnostics_s.forward(&muts);
    inlay_hints_s.forward(&muts);
    progress_s.forward(&muts);
    error_s.forward(&muts);
    trigger_chars_s.forward(&muts);
    muts
}
