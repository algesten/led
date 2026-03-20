use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use led_config_file::{ConfigDir, ConfigFileOut};
use led_core::BufferId;
use led_core::rx::Stream;
use led_docstore::DocStoreOut;
use led_fs::FsOut;
use led_state::{AppState, SessionRestorePhase};
use led_syntax::SyntaxOut;
use led_timers::{Schedule, TimersOut};
use led_workspace::{SessionBuffer, SessionData, WorkspaceOut};

pub struct Derived {
    pub ui: Stream<Arc<AppState>>,
    pub workspace_out: Stream<WorkspaceOut>,
    pub docstore_out: Stream<DocStoreOut>,
    pub config_file_out: Stream<ConfigFileOut>,
    pub timers_out: Stream<TimersOut>,
    pub fs_out: Stream<FsOut>,
    pub clipboard_out: Stream<led_clipboard::ClipboardOut>,
    pub syntax_out: Stream<SyntaxOut>,
    pub git_out: Stream<led_git::GitOut>,
    pub file_search_out: Stream<led_file_search::FileSearchOut>,
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
                .filter(|b| !b.is_preview)
                .map(|b| SessionBuffer {
                    file_path: b.path.clone().unwrap(),
                    tab_order: b.tab_order,
                    cursor_row: b.cursor_row,
                    cursor_col: b.cursor_col,
                    scroll_row: b.scroll_row,
                    scroll_sub_line: b.scroll_sub_line,
                    undo: None,
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
                    kv: build_session_kv(&s),
                },
            }
        })
        .stream();

    // Undo flush: convert domain UndoFlush into WorkspaceOut command
    let undo_flush = state
        .dedupe_by(|s| s.pending_undo_flush.version())
        .filter(|s| s.pending_undo_flush.is_some())
        .map(|s| {
            let f = (*s.pending_undo_flush).as_ref().unwrap();
            WorkspaceOut::FlushUndo {
                file_path: f.file_path.clone(),
                chain_id: f.chain_id.clone(),
                content_hash: f.content_hash,
                undo_cursor: f.undo_cursor,
                distance_from_save: f.distance_from_save,
                entries: f.entries.clone(),
            }
        })
        .stream();

    // Undo clear: triggered after save completes
    let undo_clear = state
        .dedupe_by(|s| s.pending_undo_clear.version())
        .filter(|s| s.pending_undo_clear.version() > 0)
        .map(|s| WorkspaceOut::ClearUndo {
            file_path: (*s.pending_undo_clear).clone(),
        })
        .stream();

    // Sync check: triggered by notify events (single file path)
    let sync_check = state
        .dedupe_by(|s| s.pending_sync_check.version())
        .filter(|s| s.pending_sync_check.version() > 0)
        .map(|s| {
            let file_path: std::path::PathBuf = (*s.pending_sync_check).clone();
            let buf = s
                .buffers
                .values()
                .find(|b| b.path.as_ref() == Some(&file_path));
            let (last_seen_seq, current_chain_id) = match buf {
                Some(b) => (b.last_seen_seq, b.chain_id.clone()),
                None => (0, None),
            };
            WorkspaceOut::CheckSync {
                file_path,
                last_seen_seq,
                current_chain_id,
            }
        })
        .stream();

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

    // File opens from startup args — gated on session restore being done.
    // Only opens files that aren't already in a buffer; already-open files
    // are activated directly via ActivateBuffer in the model (see process_of).
    let startup_open = state
        .dedupe_by(|s| s.session.restore_phase == SessionRestorePhase::Done)
        .filter(|s| s.session.restore_phase == SessionRestorePhase::Done)
        .filter(|s| !s.startup.arg_paths.is_empty())
        .flat_map(|s| {
            let open_paths: std::collections::HashSet<&std::path::Path> = s
                .buffers
                .values()
                .filter_map(|b| b.path.as_deref())
                .collect();
            let base = s
                .buffers
                .values()
                .map(|b| b.tab_order)
                .max()
                .map_or(0, |m| m + 1);
            s.startup
                .arg_paths
                .iter()
                .filter(|p| !open_paths.contains(p.as_path()))
                .cloned()
                .enumerate()
                .map(move |(i, path)| DocStoreOut::Open {
                    path,
                    tab_order: base + i,
                    create_if_missing: true,
                })
                .collect::<Vec<_>>()
        });

    // File opens from session restore — tab_order from session positions
    let session_open = state
        .dedupe_by(|s| s.session.pending_opens.version())
        .filter(|s| s.session.pending_opens.version() > 0)
        .map(|s| {
            let positions = &s.session.positions;
            (*s.session.pending_opens)
                .iter()
                .map(|path| {
                    let tab_order = positions.get(path).map(|sp| sp.tab_order).unwrap_or(0);
                    DocStoreOut::Open {
                        path: path.clone(),
                        tab_order,
                        create_if_missing: false,
                    }
                })
                .collect::<Vec<_>>()
        })
        .flat_map(|cmds| cmds);

    // File opens from browser
    let browser_open = state
        .dedupe_by(|s| s.pending_open.version())
        .filter(|s| s.pending_open.version() > 0)
        .filter(|s| s.pending_open.is_some())
        .map(|s| DocStoreOut::Open {
            path: (*s.pending_open).clone().unwrap(),
            tab_order: s
                .buffers
                .values()
                .map(|b| b.tab_order)
                .max()
                .map_or(0, |m| m + 1),
            create_if_missing: true,
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

    // Preview open: Case C (new file, not already in any buffer)
    let preview_open = state
        .dedupe_by(|s| s.preview.pending.version())
        .filter(|s| s.preview.pending.version() > 0)
        .filter(|s| s.preview.pending.is_some())
        .filter(|s| {
            let req_path = (*s.preview.pending).as_ref().map(|r| &r.path);
            !s.buffers.values().any(|b| b.path.as_ref() == req_path)
        })
        .map(|s| {
            let req = (*s.preview.pending).as_ref().unwrap();
            DocStoreOut::Open {
                path: req.path.clone(),
                tab_order: s
                    .buffers
                    .values()
                    .map(|b| b.tab_order)
                    .max()
                    .map_or(0, |m| m + 1),
                create_if_missing: false,
            }
        })
        .stream();

    let docstore_out: Stream<DocStoreOut> = Stream::new();
    startup_open.forward(&docstore_out);
    session_open.forward(&docstore_out);
    browser_open.forward(&docstore_out);
    save_out.forward(&docstore_out);
    preview_open.forward(&docstore_out);

    // Timers: schedule alert clear when info/warn appears
    let alert_timer = state
        .map(|s| s.alerts.has_alert())
        .dedupe()
        .filter(|has_alert| *has_alert)
        .map(|_| TimersOut::Set {
            name: "alert_clear",
            duration: Duration::from_secs(3),
            schedule: Schedule::Replace,
        })
        .stream();

    // Undo flush rate limiter: schedule a 200ms one-shot when any buffer
    // has unpersisted entries (dirty or undo-inverse entries beyond
    // persisted_undo_len). Dedupe on max version so each new edit or
    // undo re-fires. KeepExisting means rapid edits don't reset the
    // countdown.
    let undo_timer = state
        .map(|s| {
            s.buffers
                .values()
                .filter(|b| {
                    b.path.is_some()
                        && !b.is_preview
                        && (b.doc.undo_history_len() > b.persisted_undo_len || b.doc.dirty())
                })
                .map(|b| b.doc.version())
                .max()
        })
        .dedupe()
        .filter(|v| v.is_some())
        .map(|_| TimersOut::Set {
            name: "undo_flush",
            duration: Duration::from_millis(200),
            schedule: Schedule::KeepExisting,
        })
        .stream();

    let timers_out: Stream<TimersOut> = Stream::new();
    alert_timer.forward(&timers_out);
    undo_timer.forward(&timers_out);

    // FS: browser directory listing requests
    let browser_list = state
        .dedupe_by(|s| s.pending_lists.version())
        .filter(|s| s.pending_lists.version() > 0)
        .map(|s| (*s.pending_lists).clone())
        .flat_map(|paths| paths.into_iter().map(|path| FsOut::ListDir { path }));

    // FS: find-file listing requests
    let ff_list = state
        .dedupe_by(|s| s.pending_find_file_list.version())
        .filter(|s| s.pending_find_file_list.version() > 0)
        .filter(|s| s.pending_find_file_list.is_some())
        .map(|s| {
            let (dir, prefix, show_hidden) = (*s.pending_find_file_list).clone().unwrap();
            FsOut::FindFileList {
                dir,
                prefix,
                show_hidden,
            }
        })
        .stream();

    let fs_out: Stream<FsOut> = Stream::new();
    browser_list.forward(&fs_out);
    ff_list.forward(&fs_out);

    // Clipboard: sync kill_ring to system clipboard on change
    let clipboard_write = state
        .map(|s| s.kill_ring.content.clone())
        .dedupe()
        .filter(|s| !s.is_empty())
        .map(led_clipboard::ClipboardOut::Write)
        .stream();

    // Clipboard: read from system clipboard on yank request
    let clipboard_read = state
        .dedupe_by(|s| s.kill_ring.pending_yank.version())
        .filter(|s| s.kill_ring.pending_yank.version() > 0)
        .map(|_| led_clipboard::ClipboardOut::Read)
        .stream();

    let clipboard_out: Stream<led_clipboard::ClipboardOut> = Stream::new();
    clipboard_write.forward(&clipboard_out);
    clipboard_read.forward(&clipboard_out);

    // Syntax: only recompute on doc version or scroll changes.
    // Cursor-only moves don't trigger syntax work — bracket matching
    // updates on the next edit or scroll, which is fast enough.
    // Only trigger syntax on doc version and scroll changes.
    // cursor_row included so bracket matching updates per-line during
    // vertical movement; cursor_col excluded so horizontal movement
    // within a line skips syntax entirely — bracket match updates
    // on next vertical move or edit.
    let syntax_key =
        |s: &Arc<AppState>| -> (Option<(BufferId, u64, usize, usize, Option<usize>)>, usize) {
            let buf_info = s.active_buffer.and_then(|id| {
                let buf = s.buffers.get(&id)?;
                Some((
                    id,
                    buf.doc.version(),
                    buf.scroll_row,
                    buf.cursor_row,
                    buf.pending_indent_row,
                ))
            });
            (buf_info, s.buffers.len())
        };

    let known_bufs: Rc<RefCell<HashSet<BufferId>>> = Rc::new(RefCell::new(HashSet::new()));
    let known_bufs2 = known_bufs.clone();

    let syntax_changed = state
        .dedupe_by(syntax_key)
        .filter(|s| s.active_buffer.is_some())
        .filter_map(|s| {
            let id = s.active_buffer?;
            let buf = s.buffers.get(&id)?;
            let path = buf.path.clone()?;
            let buffer_height = s.dims.map_or(50, |d| d.buffer_height());
            Some(SyntaxOut::BufferChanged {
                buf_id: id,
                path,
                doc: buf.doc.clone(),
                version: buf.doc.version(),
                edit_ops: buf.doc.pending_edit_ops(),
                scroll_row: buf.scroll_row,
                buffer_height,
                cursor_row: buf.cursor_row,
                cursor_col: buf.cursor_col,
                indent_row: buf.pending_indent_row,
            })
        })
        .stream();

    // Track buffer lifecycle: emit BufferClosed for removed buffers
    let syntax_lifecycle = state
        .dedupe_by(|s| s.buffers.len())
        .map(move |s| {
            let mut known = known_bufs2.borrow_mut();
            let current: HashSet<BufferId> = s.buffers.keys().copied().collect();
            let removed: Vec<BufferId> = known.difference(&current).copied().collect();
            *known = current;
            removed
        })
        .filter(|removed| !removed.is_empty())
        .map(|removed| {
            removed
                .into_iter()
                .map(|buf_id| SyntaxOut::BufferClosed { buf_id })
                .collect::<Vec<_>>()
        })
        .stream();

    let syntax_out: Stream<SyntaxOut> = Stream::new();
    syntax_changed.forward(&syntax_out);
    // Fan-in lifecycle events
    {
        let target = syntax_out.clone();
        syntax_lifecycle.on(move |opt: Option<&Vec<SyntaxOut>>| {
            if let Some(events) = opt {
                for ev in events {
                    target.push(ev.clone());
                }
            }
        });
    }

    // Git: schedule 50ms coalescing timer when file scan requested
    let git_file_timer = state
        .dedupe_by(|s| s.git.pending_file_scan.version())
        .filter(|s| s.git.pending_file_scan.version() > 0)
        .map(|_| TimersOut::Set {
            name: "git_file_scan",
            duration: Duration::from_millis(50),
            schedule: Schedule::Replace,
        })
        .stream();

    // Git: emit ScanFiles after timer fires (git_scan_seq bumped by handle_timer)
    let git_file_scan = state
        .dedupe_by(|s| s.git.scan_seq.version())
        .filter(|s| s.git.scan_seq.version() > 0)
        .filter(|s| s.workspace.is_some())
        .map(|s| {
            let root = s.workspace.as_ref().unwrap().root.clone();
            led_git::GitOut::ScanFiles { root }
        })
        .stream();

    // Git: emit ScanLines immediately on tab switch / save
    let git_line_scan = state
        .dedupe_by(|s| s.git.pending_line_scan.version())
        .filter(|s| s.git.pending_line_scan.version() > 0)
        .filter(|s| s.git.pending_line_scan.is_some())
        .filter(|s| s.workspace.is_some())
        .map(|s| {
            let root = s.workspace.as_ref().unwrap().root.clone();
            let path = (*s.git.pending_line_scan).clone().unwrap();
            led_git::GitOut::ScanLines { root, path }
        })
        .stream();

    git_file_timer.forward(&timers_out);

    let git_out: Stream<led_git::GitOut> = Stream::new();
    git_file_scan.forward(&git_out);
    git_line_scan.forward(&git_out);

    let file_search_out = state
        .dedupe_by(|s| s.pending_file_search.version())
        .filter(|s| s.pending_file_search.version() > 0)
        .filter(|s| s.pending_file_search.is_some())
        .map(|s| {
            let req = (*s.pending_file_search).as_ref().unwrap();
            led_file_search::FileSearchOut::Search {
                query: req.query.clone(),
                root: req.root.clone(),
                case_sensitive: req.case_sensitive,
                use_regex: req.use_regex,
            }
        })
        .stream();

    Derived {
        ui,
        workspace_out,
        docstore_out,
        config_file_out,
        timers_out,
        fs_out,
        clipboard_out,
        syntax_out,
        git_out,
        file_search_out,
    }
}

fn build_session_kv(s: &AppState) -> HashMap<String, String> {
    let mut kv = HashMap::new();
    let focus_str = match s.focus {
        led_core::PanelSlot::Main => "main",
        led_core::PanelSlot::Side => "side",
        _ => "main",
    };
    kv.insert("focus".into(), focus_str.into());
    kv.insert("browser.selected".into(), s.browser.selected.to_string());
    kv.insert(
        "browser.scroll_offset".into(),
        s.browser.scroll_offset.to_string(),
    );
    let dirs: Vec<String> = s
        .browser
        .expanded_dirs
        .iter()
        .map(|d| d.to_string_lossy().into_owned())
        .collect();
    if !dirs.is_empty() {
        kv.insert("browser.expanded_dirs".into(), dirs.join("\n"));
    }
    // Jump list
    if let Ok(json) = serde_json::to_string(&s.jump.entries) {
        kv.insert("jump_list.entries".into(), json);
        kv.insert("jump_list.index".into(), s.jump.index.to_string());
    }
    kv
}
