use std::sync::Arc;
use std::time::Duration;

use led_config_file::{ConfigDir, ConfigFileOut};
use led_core::rx::Stream;
use led_docstore::DocStoreOut;
use led_fs::FsOut;
use led_state::{AppState, SessionRestorePhase};
use led_timers::{Schedule, TimersOut};
use led_workspace::{SessionBuffer, SessionData, WorkspaceOut};

pub struct Derived {
    pub ui: Stream<Arc<AppState>>,
    pub workspace_out: Stream<WorkspaceOut>,
    pub docstore_out: Stream<DocStoreOut>,
    pub config_file_out: Stream<ConfigFileOut>,
    pub timers_out: Stream<TimersOut>,
    pub fs_out: Stream<FsOut>,
}

pub fn derived(state: Stream<Arc<AppState>>) -> Derived {
    let ui = state.map(|s| s).stream();
    let workspace_init = state
        .map(|s| s.startup.clone())
        .dedupe()
        .map(|startup| WorkspaceOut::Init { startup })
        .stream();

    // Session save: triggered once when quit transitions to true (primary only)
    let session_save = state
        .dedupe_by(|s| s.quit)
        .filter(|s| s.quit)
        .filter(|s| s.workspace.as_ref().is_some_and(|w| w.primary))
        .map(|s| {
            let buffers: Vec<SessionBuffer> = s
                .buffers
                .values()
                .filter(|b| b.path.is_some())
                .map(|b| SessionBuffer {
                    file_path: b.path.clone().unwrap(),
                    tab_order: b.tab_order,
                    cursor_row: b.cursor_row,
                    cursor_col: b.cursor_col,
                    scroll_row: b.scroll_row,
                    scroll_sub_line: b.scroll_sub_line,
                })
                .collect();

            let active_tab_order = s
                .active_buffer
                .and_then(|id| s.buffers.get(&id))
                .map(|b| b.tab_order)
                .unwrap_or(0);

            WorkspaceOut::SaveSession {
                data: SessionData {
                    buffers,
                    active_tab_order,
                    show_side_panel: s.show_side_panel,
                },
            }
        })
        .stream();

    // Undo flush: convert domain UndoFlush data into WorkspaceOut commands
    let undo_flush = state
        .dedupe_by(|s| s.pending_undo_flushes.version())
        .filter(|s| s.pending_undo_flushes.version() > 0)
        .flat_map(|s| {
            (*s.pending_undo_flushes)
                .iter()
                .map(|f| WorkspaceOut::FlushUndo {
                    file_path: f.file_path.clone(),
                    chain_id: f.chain_id.clone(),
                    content_hash: f.content_hash,
                    undo_cursor: f.undo_cursor,
                    distance_from_save: 0,
                    entries: f.entries.clone(),
                })
                .collect::<Vec<_>>()
        });

    // Undo clear: triggered after save completes
    let undo_clear = state
        .dedupe_by(|s| s.pending_undo_clear.version())
        .filter(|s| s.pending_undo_clear.version() > 0)
        .map(|s| WorkspaceOut::ClearUndo {
            file_path: (*s.pending_undo_clear).clone(),
        })
        .stream();

    // Sync check: triggered by notify events
    let sync_check = state
        .dedupe_by(|s| s.pending_sync_check.version())
        .filter(|s| s.pending_sync_check.version() > 0)
        .flat_map(|s| {
            let checks: Vec<_> = (*s.pending_sync_check)
                .iter()
                .filter_map(|file_path| {
                    let buf = s
                        .buffers
                        .values()
                        .find(|b| b.path.as_ref() == Some(file_path))?;
                    Some(WorkspaceOut::CheckSync {
                        file_path: file_path.clone(),
                        last_seen_seq: buf.last_seen_seq,
                        current_chain_id: buf.chain_id.clone(),
                    })
                })
                .collect();
            checks
        });

    let workspace_out: Stream<WorkspaceOut> = Stream::new();
    workspace_init.forward(&workspace_out);
    session_save.forward(&workspace_out);
    undo_flush.forward(&workspace_out);
    undo_clear.forward(&workspace_out);
    sync_check.forward(&workspace_out);

    let config_file_out = state
        .filter_map(|s| s.workspace.clone())
        .dedupe()
        .map(|w| ConfigDir {
            config: w.config.clone(),
            read_only: !w.primary,
        })
        .map(ConfigFileOut::ConfigDir)
        .stream();

    // File opens from startup args — gated on session restore being done
    let startup_open = state
        .filter(|s| s.session_restore_phase == SessionRestorePhase::Done)
        .map(|s| s.startup.arg_paths.clone())
        .filter(|paths| !paths.is_empty())
        .dedupe()
        .flat_map(|paths| paths.into_iter().map(|path| DocStoreOut::Open { path }));

    // File opens from session restore
    let session_open = state
        .dedupe_by(|s| s.pending_session_opens.version())
        .filter(|s| s.pending_session_opens.version() > 0)
        .map(|s| (*s.pending_session_opens).clone())
        .flat_map(|paths| paths.into_iter().map(|path| DocStoreOut::Open { path }));

    // File opens from browser
    let browser_open = state
        .dedupe_by(|s| s.pending_open.version())
        .filter(|s| s.pending_open.version() > 0)
        .filter(|s| s.pending_open.is_some())
        .map(|s| DocStoreOut::Open {
            path: (*s.pending_open).clone().unwrap(),
        })
        .stream();

    // Save
    let save_out = state
        .dedupe_by(|s| s.save_request.version())
        .filter(|s| s.save_request.version() > 0)
        .filter(|s| s.active_buffer.is_some())
        .map(|s| {
            let buf = &s.buffers[&s.active_buffer.unwrap()];
            DocStoreOut::Save {
                id: buf.doc_id,
                doc: buf.doc.clone(),
            }
        })
        .stream();

    let docstore_out: Stream<DocStoreOut> = Stream::new();
    startup_open.forward(&docstore_out);
    session_open.forward(&docstore_out);
    browser_open.forward(&docstore_out);
    save_out.forward(&docstore_out);

    // Timers: schedule alert clear when info/warn appears
    let alert_timer = state
        .map(|s| s.info.is_some() || s.warn.is_some())
        .dedupe()
        .filter(|has_alert| *has_alert)
        .map(|_| TimersOut::Set {
            name: "alert_clear",
            duration: Duration::from_secs(3),
            schedule: Schedule::Replace,
        })
        .stream();

    // Undo flush rate limiter: when any buffer has unpersisted undo,
    // schedule a 200ms one-shot. KeepExisting means it won't reset if
    // already counting down — flush at most once per 200ms.
    let undo_timer = state
        .map(|s| {
            s.buffers
                .values()
                .any(|b| b.path.is_some() && b.doc.undo_history_len() > b.persisted_undo_len)
        })
        .dedupe()
        .filter(|dirty| *dirty)
        .map(|_| TimersOut::Set {
            name: "undo_flush",
            duration: Duration::from_millis(200),
            schedule: Schedule::KeepExisting,
        })
        .stream();

    let timers_out: Stream<TimersOut> = Stream::new();
    alert_timer.forward(&timers_out);
    undo_timer.forward(&timers_out);

    // FS: directory listing requests
    let fs_out = state
        .dedupe_by(|s| s.pending_lists.version())
        .filter(|s| s.pending_lists.version() > 0)
        .map(|s| (*s.pending_lists).clone())
        .flat_map(|paths| paths.into_iter().map(|path| FsOut::ListDir { path }));

    Derived {
        ui,
        workspace_out,
        docstore_out,
        config_file_out,
        timers_out,
        fs_out,
    }
}
