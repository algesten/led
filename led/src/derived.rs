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

    // Session save: triggered when quit is set and instance is primary
    let session_save = state
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

    let workspace_out: Stream<WorkspaceOut> = Stream::new();
    workspace_init.forward(&workspace_out);
    session_save.forward(&workspace_out);

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
    let timers_out = state
        .map(|s| s.info.is_some() || s.warn.is_some())
        .dedupe()
        .filter(|has_alert| *has_alert)
        .map(|_| TimersOut::Set {
            name: "alert_clear",
            duration: Duration::from_secs(3),
            schedule: Schedule::Replace,
        })
        .stream();

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
