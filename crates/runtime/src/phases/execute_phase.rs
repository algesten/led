//! Execute phase: ship `QueryOut` actions to drivers, with the
//! per-save WorkspaceClearUndo trace + UndoPersistTracker reset
//! follow-up.

use led_core::UndoDbSeq;
use led_driver_buffers_core::SaveAction;
use led_driver_file_search_core::{FileSearchCmd, FileSearchReplaceCmd, FileSearchSingleReplaceCmd};
use led_driver_session_core::SessionCmd;

use crate::apply::session::new_chain_id;
use crate::phases::query_phase::QueryOut;
use crate::phases::TickEnv;
use crate::{Sources, UndoPersistTracker};

pub(crate) fn run(sources: &mut Sources, env: &TickEnv<'_>, q: &QueryOut) {
    let Sources {
        edits,
        store,
        fs,
        find_file,
        file_search,
        syntax,
        undo_persistence,
        ..
    } = sources;

    env.drivers.fs_list.execute(q.list_actions.iter());

    env.drivers.file.execute(q.load_actions.iter(), store);

    if !q.find_file_actions.is_empty()
        && let Some(ff) = find_file.as_mut()
    {
        ff.pending_find_file_list.clear();
    }
    env.drivers.find_file.execute(q.find_file_actions.iter());

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
            env.drivers.file_search.execute(cmds.iter());
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
        env.drivers.file_search.execute_replace(cmds.iter());
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
        env.drivers.file_search.execute_single_replace(cmds.iter());
    }
    let _ = env.drivers.file_search.process_single_replace();

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
    env.drivers.file_write.execute(q.save_actions.iter());

    for action in &q.save_actions {
        let (path, is_save_as) = match action {
            SaveAction::Save { path, .. } => (path, false),
            SaveAction::SaveAs { from, .. } => (from, true),
        };
        env.drivers
            .session
            .execute(std::iter::once(&SessionCmd::ClearUndo {
                path: path.clone(),
            }));
        if let Some(eb) = edits.buffers.get(path) {
            undo_persistence.insert(
                path.clone(),
                UndoPersistTracker {
                    chain_id: new_chain_id(),
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
