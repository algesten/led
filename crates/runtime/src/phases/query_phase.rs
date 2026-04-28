//! Query phase: build the cross-phase locals (load/save/list/find-file
//! actions, render frame, syntax cmds) the Execute phase consumes.

use led_driver_buffers_core::{LoadAction, SaveAction};
use led_driver_find_file_core::FindFileCmd;
use led_driver_fs_list_core::ListCmd;
use led_driver_syntax_core::SyntaxCmd;
use led_driver_terminal_core::Frame;
use crate::query::{
    self, file_list_action, file_load_action, file_save_action, find_file_action,
    render_frame, AlertsInput, BrowserUiInput, EditedBuffersInput, FindFileInput,
    FsTreeInput, PendingSavesInput, StoreLoadedInput, TabsActiveInput, TabsOpenInput,
    TerminalDimsInput,
};
use crate::Atoms;

/// Cross-phase locals the Execute phase consumes.
pub(crate) struct QueryOut {
    pub load_actions: imbl::Vector<LoadAction>,
    pub save_actions: Vec<SaveAction>,
    pub list_actions: Vec<ListCmd>,
    pub find_file_actions: Vec<FindFileCmd>,
    pub frame: Option<Frame>,
    pub syntax_cmds: std::sync::Arc<Vec<SyntaxCmd>>,
}

pub(crate) fn run(atoms: &Atoms) -> QueryOut {
    let Atoms {
        tabs,
        edits,
        store,
        terminal,
        alerts,
        browser,
        fs,
        find_file,
        isearch,
        file_search,
        syntax,
        diagnostics,
        lsp_status,
        completions,
        lsp_extras,
        git,
        kbd_macro,
        ..
    } = atoms;

    let load_actions = file_load_action(
        StoreLoadedInput::new(store),
        TabsOpenInput::new(tabs),
    );
    let save_actions = file_save_action(
        PendingSavesInput::new(edits),
        EditedBuffersInput::new(edits),
    );
    let list_actions = file_list_action(query::BrowserDerivedInputs {
        fs: FsTreeInput::new(fs),
        ui: BrowserUiInput::new(browser),
        tabs: TabsActiveInput::new(tabs),
        edits: EditedBuffersInput::new(edits),
    });
    let find_file_actions = find_file_action(FindFileInput::new(find_file));
    let render_tick = if lsp_status.any_busy() {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 / 80)
            .unwrap_or(0)
    } else {
        0
    };
    let frame = render_frame(query::RenderInputs {
        term: TerminalDimsInput::new(terminal),
        edits: EditedBuffersInput::new(edits),
        store: StoreLoadedInput::new(store),
        tabs: TabsActiveInput::new(tabs),
        alerts: AlertsInput::new(alerts),
        browser: BrowserUiInput::new(browser),
        fs: FsTreeInput::new(fs),
        overlays: query::OverlaysInput::new(find_file, isearch, file_search),
        syntax: query::SyntaxStatesInput::new(syntax),
        diagnostics: query::DiagnosticsStatesInput::new(diagnostics),
        lsp: query::LspStatusesInput::new(lsp_status),
        completions: query::CompletionsSessionInput::new(completions),
        lsp_extras: query::LspExtrasOverlayInput::new(lsp_extras),
        git: query::GitStateInput::new(git),
        render_tick,
        kbd_macro: query::KbdMacroRecordingInput::new(kbd_macro),
    });

    let syntax_cmds = query::desired_syntax_parses(
        query::SyntaxStatesInput::new(syntax),
        EditedBuffersInput::new(edits),
    );

    QueryOut {
        load_actions,
        save_actions,
        list_actions,
        find_file_actions,
        frame,
        syntax_cmds,
    }
}
