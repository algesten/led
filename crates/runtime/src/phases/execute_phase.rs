//! Execute phase: ship `QueryOut` actions to drivers, with the
//! per-save WorkspaceClearUndo trace + UndoPersistTracker reset
//! follow-up.

use led_core::UndoDbSeq;
use led_driver_buffers_core::SaveAction;
use led_driver_file_search_core::{FileSearchCmd, FileSearchReplaceCmd, FileSearchSingleReplaceCmd};
use led_driver_lsp_core::LspCmd;
use led_driver_session_core::SessionCmd;

use crate::apply::session::new_chain_id;
use crate::phases::query_phase::QueryOut;
use crate::phases::TickEnv;
use crate::{LspNotified, Sources, UndoPersistTracker};

pub(crate) fn run(sources: &mut Sources, env: &TickEnv<'_>, q: &QueryOut) {
    let Sources {
        edits,
        store,
        fs,
        find_file,
        find_file_driver,
        fs_list_driver,
        file_search,
        file_search_driver,
        file_write_driver,
        session_driver,
        syntax,
        undo_persistence,
        clock,
        lsp_notified,
        ..
    } = sources;

    env.drivers
        .fs_list
        .execute(q.list_actions.iter(), fs_list_driver);

    env.drivers.file.execute(q.load_actions.iter(), store);

    // ── File-watch fan-out (moved out of Ingest per the phase
    // contract: Ingest writes external facts in, Execute ships
    // intent out). Empty vecs short-circuit; the workspace gate
    // is folded into query_phase.
    if !q.external_reread_cmds.is_empty() {
        env.drivers
            .file
            .execute(q.external_reread_cmds.iter(), store);
    }
    if !q.session_sync_cmds.is_empty() {
        env.drivers
            .session
            .execute(q.session_sync_cmds.iter(), session_driver);
    }
    for cmd in q.lsp_watch_cmds.iter() {
        if let LspCmd::DidChangeWatchedFiles { server, changes } = cmd {
            env.trace.lsp_did_change_watched_files(server, changes.len());
        }
    }
    if !q.lsp_watch_cmds.is_empty() {
        env.drivers.lsp.execute(q.lsp_watch_cmds.iter());
    }

    // ── LSP BufferOpened fan-out (moved out of Ingest per Theme E).
    // Ship the cmds, then stamp `lsp_notified` so the next query
    // tick's diff skips these paths. The (path, version,
    // saved_version) trio is the tuple the LSP buffer-changed
    // memo subsequently uses to decide whether further
    // `BufferChanged` cmds are required.
    if !q.buffer_opened_cmds.is_empty() {
        env.drivers.lsp.execute(q.buffer_opened_cmds.iter());
        for (path, version, saved_version) in &q.buffer_opened_notified {
            lsp_notified.insert(
                path.clone(),
                LspNotified {
                    version: *version,
                    saved_version: *saved_version,
                },
            );
        }
    }

    // ── Shutdown dispatch (Theme E). The cmd is `Some` exactly
    // when the orchestrator's `check_quit_gate` (run later in
    // this tick, after every dispatch phase) will return `true`
    // and break the outer loop. Co-located here so the driver
    // gets `Shutdown` in the same tick the loop exits.
    if let Some(cmd) = &q.shutdown_cmd {
        env.drivers
            .session
            .execute(std::iter::once(cmd), session_driver);
    }

    if !q.find_file_actions.is_empty()
        && let Some(ff) = find_file.as_mut()
    {
        ff.pending_find_file_list.clear();
    }
    env.drivers
        .find_file
        .execute(q.find_file_actions.iter(), find_file_driver);

    if let Some(fs_state) = file_search.as_mut()
        && !fs_state.pending_search.is_empty()
    {
        if let Some(root) = fs.root.as_ref() {
            let cmds: Vec<FileSearchCmd> = fs_state
                .pending_search
                .drain(..)
                .map(|req| FileSearchCmd {
                    root: root.clone(),
                    query: req.query,
                    case_sensitive: req.case_sensitive,
                    use_regex: req.use_regex,
                })
                .collect();
            env.drivers
                .file_search
                .execute(cmds.iter(), file_search_driver);
        } else {
            fs_state.pending_search.clear();
        }
    }

    if !edits.pending_replace_all.is_empty() {
        let cmds: Vec<FileSearchReplaceCmd> = edits
            .pending_replace_all
            .drain(..)
            .map(|p| FileSearchReplaceCmd {
                root: p.root,
                query: p.query,
                replacement: p.replacement,
                case_sensitive: p.case_sensitive,
                use_regex: p.use_regex,
                skip_paths: p.skip_paths,
            })
            .collect();
        env.drivers
            .file_search
            .execute_replace(cmds.iter(), file_search_driver);
    }

    if !edits.pending_single_replace.is_empty() {
        let cmds: Vec<FileSearchSingleReplaceCmd> = edits
            .pending_single_replace
            .drain(..)
            .map(|p| FileSearchSingleReplaceCmd {
                path: p.path,
                line: p.line,
                match_start: p.match_start,
                match_end: p.match_end,
                original: p.original,
                replacement: p.replacement,
            })
            .collect();
        env.drivers
            .file_search
            .execute_single_replace(cmds.iter(), file_search_driver);
    }
    let _ = env
        .drivers
        .file_search
        .process_single_replace(file_search_driver);

    for action in &q.save_actions {
        match action {
            SaveAction::Save { path, .. } => {
                edits.pending_saves.remove(path);
            }
            SaveAction::SaveAs { from, .. } => {
                edits.pending_save_as.remove(from);
            }
        }
    }
    env.drivers
        .file_write
        .execute(q.save_actions.iter(), file_write_driver);

    for action in &q.save_actions {
        let (path, is_save_as) = match action {
            SaveAction::Save { path, .. } => (path, false),
            SaveAction::SaveAs { from, .. } => (from, true),
        };
        env.drivers.session.execute(
            std::iter::once(&SessionCmd::ClearUndo {
                path: path.clone(),
            }),
            session_driver,
        );
        if let Some(eb) = edits.buffers.get(path) {
            undo_persistence.insert(
                path.clone(),
                UndoPersistTracker {
                    chain_id: new_chain_id(clock),
                    persisted_len: eb.history.past_groups().len(),
                    last_seq: UndoDbSeq(0),
                },
            );
        }
        if is_save_as {
            env.trace.file_reopen_existing(path);
        }
    }

    for cmd in q.syntax_cmds.iter() {
        if let Some(state) = syntax.by_path.get_mut(&cmd.path) {
            state.in_flight_version = Some(cmd.version);
        }
    }
    if !q.syntax_cmds.is_empty() {
        env.drivers.syntax.execute(q.syntax_cmds.iter());
    }
}
