//! Query phase: build the cross-phase locals (load/save/list/find-file
//! actions, render frame, syntax cmds) the Execute phase consumes.

use std::sync::Arc;

use led_driver_buffers_core::{LoadAction, SaveAction};
use led_driver_find_file_core::FindFileCmd;
use led_driver_fs_list_core::ListCmd;
use led_driver_lsp_core::LspCmd;
use led_driver_session_core::SessionCmd;
use led_driver_syntax_core::SyntaxCmd;
use led_driver_terminal_core::Frame;
use crate::phases::TickEnv;
use crate::query::{
    self, file_list_action, file_load_action, file_save_action, find_file_action,
    render_frame, AlertsInput, BrowserUiInput, EditedBuffersInput, FindFileInput,
    FsTreeInput, PendingSavesInput, StoreLoadedInput, TabsActiveInput, TabsOpenInput,
    TerminalDimsInput,
};
use crate::Sources;

/// Cross-phase locals the Execute phase consumes.
pub(crate) struct QueryOut {
    pub load_actions: imbl::Vector<LoadAction>,
    pub save_actions: Vec<SaveAction>,
    pub list_actions: Vec<ListCmd>,
    pub find_file_actions: Vec<FindFileCmd>,
    pub frame: Option<Frame>,
    pub syntax_cmds: Arc<Vec<SyntaxCmd>>,
    /// File-watch derived: which open buffers had their on-disk
    /// content modified this tick and should be rerread. Empty
    /// when the workspace gate is closed.
    pub external_reread_cmds: Vec<LoadAction>,
    /// File-watch derived: cross-instance sync-check fan-out. One
    /// `SessionCmd::CheckSync` per open buffer whose hash showed
    /// up under `<config>/notify/`. Empty when the workspace or
    /// config-dir gate is closed.
    pub session_sync_cmds: Arc<Vec<SessionCmd>>,
    /// File-watch derived: `DidChangeWatchedFiles` notifications
    /// to language servers, one per affected server. Empty when
    /// the workspace gate is closed.
    pub lsp_watch_cmds: Arc<Vec<LspCmd>>,
    /// `LspCmd::BufferOpened` for every buffer that exists in
    /// `edits.buffers` but has no entry in `lsp_notified`. The
    /// execute phase ships them and writes the `lsp_notified`
    /// record (so the next tick's diff is empty). Paired with
    /// [`Self::buffer_opened_notified`] so execute can stamp the
    /// `(version, saved_version)` it learnt this tick without
    /// re-reading `edits` (no new buffers are added between
    /// query and execute inside one tick).
    pub buffer_opened_cmds: Vec<LspCmd>,
    /// `(path, version, saved_version)` triples that correspond
    /// 1:1 with [`Self::buffer_opened_cmds`]. Execute folds these
    /// into `lsp_notified` after dispatching the cmds.
    pub buffer_opened_notified:
        Vec<(led_core::CanonPath, led_core::BufferVersion, led_core::SavedVersion)>,
}

pub(crate) fn run(sources: &Sources, env: &TickEnv<'_>) -> QueryOut {
    let Sources {
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
        session,
        clock,
        fs_list_driver,
        file_watch,
        lsp_watched_globs,
        lsp_notified,
        path_chains,
        undo_persistence,
        ..
    } = sources;

    let load_actions = file_load_action(
        StoreLoadedInput::new(store),
        TabsOpenInput::new(tabs),
    );
    let save_actions = file_save_action(
        PendingSavesInput::new(edits),
        EditedBuffersInput::new(edits),
    );
    let list_actions = file_list_action(
        query::BrowserDerivedInputs {
            fs: FsTreeInput::new(fs),
            ui: BrowserUiInput::new(browser),
            tabs: TabsActiveInput::new(tabs),
            edits: EditedBuffersInput::new(edits),
        },
        query::FsListDriverInput::new(fs_list_driver),
    );
    let find_file_actions = find_file_action(FindFileInput::new(find_file));
    let render_tick = if lsp_status.any_busy() {
        clock
            .wall_now
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
        session: query::SessionPrimaryInput::new(session),
    });

    let syntax_cmds = query::desired_syntax_parses(
        query::SyntaxStatesInput::new(syntax),
        EditedBuffersInput::new(edits),
    );

    // ── File-watch derived: gated by the same conditions as the
    // legacy ingest_file_watch fan-out. When the gate is closed
    // (no workspace root, no_workspace flag, or session not yet
    // initialised) the memos are skipped and we emit empty vecs.
    let workspace_gate =
        fs.root.is_some() && !env.no_workspace && session.init_done;
    let external_reread_cmds: Vec<LoadAction> = if workspace_gate {
        let reread_paths = query::external_reread_targets(
            query::FileWatchEventsInput::new(file_watch),
            EditedBuffersInput::new(edits),
        );
        reread_paths
            .iter()
            .map(|p| LoadAction::Reread(p.clone()))
            .collect()
    } else {
        Vec::new()
    };
    let session_sync_cmds: Arc<Vec<SessionCmd>> =
        if workspace_gate && env.resolved_config_dir.is_some() {
            let hash_index = query::notify_hash_index(EditedBuffersInput::new(edits));
            query::sync_check_cmds(
                query::FileWatchEventsInput::new(file_watch),
                query::HashIndexInput::new(&hash_index),
                query::UndoPersistenceInput::new(undo_persistence),
            )
        } else {
            Arc::new(Vec::new())
        };
    let lsp_watch_cmds: Arc<Vec<LspCmd>> = if workspace_gate {
        query::lsp_watched_file_notifications(
            query::FileWatchEventsInput::new(file_watch),
            query::LspWatchedGlobsInput::new(lsp_watched_globs),
        )
    } else {
        Arc::new(Vec::new())
    };

    // ── LSP BufferOpened derived: every path that has an
    // `EditedBuffer` but no `lsp_notified` record is newly
    // materialised this tick (the ingest_file_completions hook
    // inserted the `EditedBuffer`; we never told the LSP driver
    // about it). Execute ships one `BufferOpened` per path and
    // stamps `lsp_notified` so subsequent ticks skip.
    //
    // `path_chains` is consulted for symlinked dotfiles
    // (`feedback_language_detection_chain`) — the chain captures
    // the user-typed basename, which `Language::from_chain`
    // resolves before falling back to the canonical path's
    // extension.
    let (buffer_opened_cmds, buffer_opened_notified) =
        derive_buffer_opened(edits, lsp_notified, path_chains);

    QueryOut {
        load_actions,
        save_actions,
        list_actions,
        find_file_actions,
        frame,
        syntax_cmds,
        external_reread_cmds,
        session_sync_cmds,
        lsp_watch_cmds,
        buffer_opened_cmds,
        buffer_opened_notified,
    }
}

/// Diff `edits.buffers` against `lsp_notified` and emit one
/// `LspCmd::BufferOpened` per buffer that has not yet been
/// announced to the LSP driver. Returned in two arrays of equal
/// length so the execute phase can stamp `lsp_notified` after
/// dispatching, without re-reading `edits`.
///
/// Order follows `edits.buffers`' iteration order — stable per
/// tick. The LSP driver de-duplicates per path inside its own
/// state machine, so emit order is not observable from goldens.
fn derive_buffer_opened(
    edits: &led_state_buffer_edits::BufferEdits,
    lsp_notified: &imbl::HashMap<led_core::CanonPath, crate::LspNotified>,
    path_chains: &std::collections::HashMap<led_core::CanonPath, led_core::PathChain>,
) -> (
    Vec<LspCmd>,
    Vec<(led_core::CanonPath, led_core::BufferVersion, led_core::SavedVersion)>,
) {
    let mut cmds: Vec<LspCmd> = Vec::new();
    let mut notified: Vec<(
        led_core::CanonPath,
        led_core::BufferVersion,
        led_core::SavedVersion,
    )> = Vec::new();
    for (path, eb) in edits.buffers.iter() {
        if lsp_notified.contains_key(path) {
            continue;
        }
        let language = path_chains
            .get(path)
            .and_then(led_state_syntax::Language::from_chain)
            .or_else(|| led_state_syntax::Language::from_path(path));
        let hash = led_core::EphemeralContentHash::of_rope(&eb.rope).persist();
        cmds.push(LspCmd::BufferOpened {
            path: path.clone(),
            language,
            rope: eb.rope.clone(),
            hash,
        });
        notified.push((path.clone(), eb.version, eb.saved_version));
    }
    (cmds, notified)
}
