use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use led_core::{CanonPath, SyntaxSeq};

use led_config_file::{ConfigDir, ConfigFileOut};
use led_core::rx::Stream;
use led_docstore::DocStoreOut;
use led_fs::FsOut;
use led_lsp::LspOut;
use led_state::{AppState, ChangeReason, LspRequest, Phase, SyntaxRequest};
use led_syntax::SyntaxOut;
use led_timers::{Schedule, TimersOut};
use led_workspace::{SessionBuffer, SessionData, WorkspaceOut};

/// Key for deduping on materialized buffer paths.
fn loaded_buf_paths(s: &Rc<AppState>) -> Vec<CanonPath> {
    let mut paths: Vec<CanonPath> = s
        .buffers
        .values()
        .filter(|b| b.is_materialized())
        .filter_map(|b| b.path().cloned())
        .collect();
    paths.sort();
    paths
}

pub struct Derived {
    pub ui: Stream<Rc<AppState>>,
    pub workspace_out: Stream<WorkspaceOut>,
    pub docstore_out: Stream<DocStoreOut>,
    pub config_file_out: Stream<ConfigFileOut>,
    pub timers_out: Stream<TimersOut>,
    pub fs_out: Stream<FsOut>,
    pub clipboard_out: Stream<led_clipboard::ClipboardOut>,
    pub syntax_out: Stream<SyntaxOut>,
    pub git_out: Stream<led_git::GitOut>,
    pub gh_pr_out: Stream<led_gh_pr::GhPrOut>,
    pub file_search_out: Stream<led_file_search::FileSearchOut>,
    pub lsp_out: Stream<LspOut>,
    pub open_url: Stream<String>,
}

pub fn derived(state: Stream<Rc<AppState>>, git_activity: Stream<()>) -> Derived {
    // Suppress render while an async indent is in flight — the next
    // render after the driver responds shows newline + correct indent
    // in one atomic visual update, eliminating cursor flash.
    let ui = state
        .filter(|s| {
            s.active_tab
                .as_ref()
                .and_then(|path| s.buffers.get(path))
                .map_or(true, |b| b.pending_indent_row().is_none())
        })
        .map(|s| s)
        .stream();
    let workspace_init = state
        .map(|s| s.startup.clone())
        .dedupe()
        .map(|startup| WorkspaceOut::Init { startup })
        .stream();

    // Session save: triggered once when quit transitions to true (primary only).
    // Standalone mode has no workspace → naturally skipped.
    let session_save = state
        .dedupe_by(|s| s.phase == Phase::Exiting)
        .filter(|s| s.phase == Phase::Exiting)
        .filter(|s| s.workspace.loaded().is_some_and(|w| w.primary))
        .map(|s| {
            let buffers: Vec<SessionBuffer> = s
                .tabs
                .iter()
                .filter(|t| !t.is_preview())
                .enumerate()
                .filter_map(|(i, t)| {
                    let b = s.buffers.get(t.path())?;
                    if !b.is_materialized() || b.path().is_none() {
                        return None;
                    }
                    let canon = b.path().unwrap();
                    let ws = s.workspace.loaded().unwrap();
                    let file_path = canon.to_user_path(&ws.root, &ws.user_root);
                    Some(SessionBuffer {
                        file_path,
                        tab_order: i,
                        cursor_row: b.cursor_row(),
                        cursor_col: b.cursor_col(),
                        scroll_row: b.scroll_row(),
                        scroll_sub_line: b.scroll_sub_line(),
                        undo: None,
                    })
                })
                .collect();

            let active_tab_order = s
                .active_tab
                .as_ref()
                .and_then(|path| {
                    s.tabs
                        .iter()
                        .filter(|t| !t.is_preview())
                        .position(|t| *t.path() == *path)
                })
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
            let file_path: CanonPath = (*s.pending_sync_check).clone();
            let buf = s.buffers.values().find(|b| b.path() == Some(&file_path));
            let (last_seen_seq, current_chain_id) = match buf {
                Some(b) => (b.last_seen_seq(), b.chain_id().map(|s| s.to_owned())),
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

    // Config file driver: needs a config dir to read keys.toml/theme.toml.
    // In normal mode this is `workspace.config` (read/write for primary,
    // read-only for secondary). In standalone mode there is no workspace,
    // but we still need a keymap — otherwise every keystroke (including
    // C-c) is dropped. Fall back to `startup.config_dir` as read-only so
    // the keymap loads without touching session state.
    let config_file_out = state
        .filter_map(|s| match &s.workspace {
            led_state::WorkspaceState::Loaded(w) => Some((w.config.clone(), !w.primary)),
            led_state::WorkspaceState::Standalone => Some((s.startup.config_dir.clone(), true)),
            led_state::WorkspaceState::Loading => None,
        })
        .dedupe()
        .map(|(config, read_only)| ConfigDir { config, read_only })
        .map(ConfigFileOut::ConfigDir)
        .stream();

    // Unified buffer materialization: emit DocStoreOut::Open for any buffer
    // that exists in state but hasn't been loaded from disk yet.
    // The docstore driver deduplicates in-flight requests.
    // Unified buffer materialization: intersect open tabs with
    // non-materialized buffers. Diagnostic-only buffers are not in
    // tabs, so they never get materialized.
    fn tabs_needing_open(s: &Rc<AppState>) -> Vec<CanonPath> {
        let result: Vec<CanonPath> = s
            .tabs
            .iter()
            .filter(|t| {
                s.buffers
                    .get(t.path())
                    .map_or(true, |b| b.is_unmaterialized())
            })
            .map(|t| t.path().clone())
            .collect();
        result
    }

    let materialize = state
        .dedupe_by(tabs_needing_open)
        .filter(|s| !tabs_needing_open(s).is_empty())
        .flat_map(|s| {
            tabs_needing_open(&s)
                .into_iter()
                .map(|path| {
                    // Mark as Requested via interior mutability so subsequent
                    // invocations of tabs_needing_open skip this buffer.
                    if let Some(b) = s.buffers.get(&path) {
                        b.mark_requested();
                    }
                    let create_if_missing = s
                        .buffers
                        .get(&path)
                        .map(|b| b.create_if_missing())
                        .unwrap_or(false);
                    DocStoreOut::Open {
                        path,
                        create_if_missing,
                    }
                })
                .collect::<Vec<_>>()
        });

    // Save
    let save_out = state
        .dedupe_by(|s| s.save_request.version())
        .filter(|s| s.save_request.version() > 0)
        .filter(|s| s.active_tab.is_some())
        .map(|s| {
            let buf = &s.buffers[s.active_tab.as_ref().unwrap()];
            DocStoreOut::Save {
                path: buf.path().cloned().unwrap(),
                doc: buf.doc().clone(),
            }
        })
        .stream();

    // Save all dirty buffers
    let save_all_out = state
        .dedupe_by(|s| s.save_all_request.version())
        .filter(|s| s.save_all_request.version() > 0)
        .map(|s| {
            s.buffers
                .values()
                .filter(|b| b.is_materialized() && b.save_in_flight() && b.path().is_some())
                .map(|b| DocStoreOut::Save {
                    path: b.path().cloned().unwrap(),
                    doc: b.doc().clone(),
                })
                .collect::<Vec<_>>()
        })
        .flat_map(|cmds| cmds);

    // Save as: write active buffer to a new path
    let save_as_out = state
        .dedupe_by(|s| s.pending_save_as.version())
        .filter(|s| s.pending_save_as.version() > 0)
        .filter(|s| s.pending_save_as.is_some())
        .filter(|s| s.active_tab.is_some())
        .map(|s| {
            let buf = &s.buffers[s.active_tab.as_ref().unwrap()];
            let path = (*s.pending_save_as).clone().unwrap();
            DocStoreOut::SaveAs {
                path: buf.path().cloned().unwrap(),
                doc: buf.doc().clone(),
                new_path: path,
            }
        })
        .stream();

    let docstore_out: Stream<DocStoreOut> = Stream::new();
    materialize.forward(&docstore_out);
    save_out.forward(&docstore_out);
    save_all_out.forward(&docstore_out);
    save_as_out.forward(&docstore_out);

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
                    b.is_materialized()
                        && b.path().is_some()
                        && !s
                            .tabs
                            .iter()
                            .any(|t| t.is_preview() && b.path() == Some(t.path()))
                        && (b.undo_history_len() > b.persisted_undo_len() || b.is_dirty())
                })
                .map(|b| b.version())
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

    // Spinner: start a repeated 80ms timer while LSP is busy, cancel when idle.
    let spinner_timer = state
        .map(|s| s.lsp.busy)
        .dedupe()
        .map(|busy| {
            if busy {
                TimersOut::Set {
                    name: "spinner",
                    duration: Duration::from_millis(80),
                    schedule: Schedule::Repeated,
                }
            } else {
                TimersOut::Cancel { name: "spinner" }
            }
        })
        .stream();

    // Tab linger: reset 3s timer whenever active buffer changes.
    // If the user stays on a tab for 3s, the timer fires and updates last_used.
    // Rapid NextTab/PrevTab resets the timer, so stepping past doesn't count.
    let linger_timer = state
        .dedupe_by(|s| s.active_tab.clone())
        .filter(|s| s.active_tab.is_some())
        .map(|_| TimersOut::Set {
            name: "tab_linger",
            duration: Duration::from_secs(3),
            schedule: Schedule::Replace,
        })
        .stream();

    let timers_out: Stream<TimersOut> = Stream::new();
    alert_timer.forward(&timers_out);
    undo_timer.forward(&timers_out);
    spinner_timer.forward(&timers_out);
    linger_timer.forward(&timers_out);

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

    // Syntax: request-based stream.  Buffers set a pending syntax
    // request internally (Full or Partial) when mutated.  This stream
    // emits SyntaxOut::BufferChanged for every buffer with a pending
    // request.
    fn syntax_seq_key(s: &Rc<AppState>) -> SyntaxSeq {
        s.buffers
            .values()
            .filter(|b| b.is_materialized())
            .map(|b| b.pending_syntax_seq())
            .max()
            .unwrap_or(SyntaxSeq(0))
    }

    let syntax_requests = state
        .dedupe_by(syntax_seq_key)
        .filter(|s| syntax_seq_key(s) > SyntaxSeq(0))
        .map(|s| {
            let buffer_height = s.dims.map_or(50, |d| d.buffer_height());
            s.buffers
                .values()
                .filter(|b| b.is_materialized() && b.pending_syntax_request().is_some())
                .filter_map(|b| {
                    let path = b.path().cloned()?;
                    let is_active = s.active_tab.as_ref() == Some(&path);
                    let indent_row = if is_active {
                        b.pending_indent_row()
                    } else {
                        None
                    };
                    let edit_ops = match b.pending_syntax_request()? {
                        SyntaxRequest::Full => vec![],
                        SyntaxRequest::Partial { edit_ops } => edit_ops.clone(),
                    };
                    Some(SyntaxOut::BufferChanged {
                        path,
                        language: b.language(),
                        doc: b.doc().clone(),
                        version: b.version(),
                        edit_ops,
                        scroll_row: b.scroll_row(),
                        buffer_height,
                        cursor_row: b.cursor_row(),
                        cursor_col: b.cursor_col(),
                        indent_row,
                    })
                })
                .collect::<Vec<_>>()
        })
        .stream();

    // Viewport-only: scroll/cursor changes on the active buffer when
    // no pending syntax request exists.  The driver recomputes visible
    // highlights without reparsing.
    let syntax_viewport = state
        .dedupe_by(|s| {
            s.active_tab.as_ref().and_then(|path| {
                let buf = s.buffers.get(path)?;
                Some((path.clone(), buf.scroll_row(), buf.cursor_row()))
            })
        })
        .filter(|s| {
            s.active_tab
                .as_ref()
                .and_then(|p| s.buffers.get(p))
                .is_some_and(|b| b.is_materialized())
        })
        .filter_map(|s| {
            let path = s.active_tab.as_ref()?;
            let buf = s.buffers.get(path)?;
            let path = buf.path().cloned()?;
            let buffer_height = s.dims.map_or(50, |d| d.buffer_height());
            Some(SyntaxOut::BufferChanged {
                path,
                language: buf.language(),
                doc: buf.doc().clone(),
                version: buf.version(),
                edit_ops: vec![],
                scroll_row: buf.scroll_row(),
                buffer_height,
                cursor_row: buf.cursor_row(),
                cursor_col: buf.cursor_col(),
                indent_row: None,
            })
        })
        .stream();

    // Track buffer lifecycle: emit BufferClosed for removed buffers
    let known_bufs: Rc<RefCell<HashSet<CanonPath>>> = Rc::new(RefCell::new(HashSet::new()));
    let known_bufs2 = known_bufs.clone();

    let syntax_lifecycle = state
        .dedupe_by(loaded_buf_paths)
        .map(move |s| {
            let mut known = known_bufs2.borrow_mut();
            let current: HashSet<CanonPath> = s
                .buffers
                .values()
                .filter(|b| b.is_materialized())
                .filter_map(|b| b.path().cloned())
                .collect();
            let removed: Vec<CanonPath> = known.difference(&current).cloned().collect();
            *known = current;
            removed
        })
        .filter(|removed| !removed.is_empty())
        .map(|removed| {
            removed
                .into_iter()
                .map(|path| SyntaxOut::BufferClosed { path })
                .collect::<Vec<_>>()
        })
        .stream();

    let syntax_out: Stream<SyntaxOut> = Stream::new();
    syntax_viewport.forward(&syntax_out);
    // Fan-in request events (Vec<SyntaxOut>)
    {
        let target = syntax_out.clone();
        syntax_requests.on(move |opt: Option<&Vec<SyntaxOut>>| {
            if let Some(events) = opt {
                for ev in events {
                    target.push(ev.clone());
                }
            }
        });
    }
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

    // Git: emit ScanFiles after timer fires (git_scan_seq bumped by handle_timer).
    // Standalone mode stays dormant — `loaded()` returns None.
    let git_file_scan = state
        .dedupe_by(|s| s.git.scan_seq.version())
        .filter(|s| s.git.scan_seq.version() > 0)
        .filter_map(|s| s.workspace.loaded().map(|w| w.root.clone()))
        .map(|root| led_git::GitOut::ScanFiles { root })
        .stream();

    git_file_timer.forward(&timers_out);

    let git_out: Stream<led_git::GitOut> = Stream::new();
    git_file_scan.forward(&git_out);

    let file_search_search_out = state
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

    let file_search_replace_out = state
        .dedupe_by(|s| s.pending_file_replace.version())
        .filter(|s| s.pending_file_replace.version() > 0)
        .filter(|s| s.pending_file_replace.is_some())
        .map(|s| {
            let req = (*s.pending_file_replace).as_ref().unwrap();
            led_file_search::FileSearchOut::Replace {
                query: req.query.clone(),
                replacement: req.replacement.clone(),
                root: req.root.clone(),
                case_sensitive: req.case_sensitive,
                use_regex: req.use_regex,
                scope: req.scope.clone(),
                skip_paths: req.skip_paths.clone(),
            }
        })
        .stream();

    let file_search_out: Stream<led_file_search::FileSearchOut> = Stream::new();
    file_search_search_out.forward(&file_search_out);
    file_search_replace_out.forward(&file_search_out);

    // ── LSP ──

    // Init: emit when workspace root becomes available.
    // Standalone mode stays in Loading/Standalone → LSP never starts.
    let lsp_init = state
        .filter_map(|s| s.workspace.loaded().cloned())
        .dedupe()
        .map(|w| LspOut::Init {
            root: w.root.clone(),
        })
        .stream();

    let lsp_out: Stream<LspOut> = Stream::new();

    // Buffer lifecycle: track open/close
    let lsp_known_bufs: Rc<RefCell<HashSet<CanonPath>>> = Rc::new(RefCell::new(HashSet::new()));

    let lsp_buf_opened = state
        .dedupe_by(loaded_buf_paths)
        .map(move |s: Rc<AppState>| {
            let mut known = lsp_known_bufs.borrow_mut();
            let current: HashSet<CanonPath> = s
                .buffers
                .values()
                .filter(|b| b.is_materialized())
                .filter_map(|b| b.path().cloned())
                .collect();
            let added: Vec<CanonPath> = current.difference(&known).cloned().collect();
            let removed: Vec<CanonPath> = known.difference(&current).cloned().collect();
            *known = current;
            // Emit opened events
            let mut events: Vec<LspOut> = Vec::new();
            for path in added {
                if let Some(buf) = s.buffers.values().find(|b| b.path() == Some(&path)) {
                    events.push(LspOut::BufferOpened {
                        path,
                        language: buf.language(),
                        doc: buf.doc().clone(),
                    });
                }
            }
            // No didClose — the LSP has its own file watcher.
            let _ = removed;
            events
        })
        .filter(|events| !events.is_empty())
        .stream();

    // Fan-in lifecycle events
    {
        let target = lsp_out.clone();
        lsp_buf_opened.on(move |opt: Option<&Vec<LspOut>>| {
            if let Some(events) = opt {
                for ev in events {
                    target.push(ev.clone());
                }
            }
        });
    }

    // BufferChanged: emit for any buffer whose version changed.
    let lsp_known_versions: Rc<RefCell<HashMap<CanonPath, u64>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let lsp_buf_changed = state
        .dedupe_by(|s| s.buffers.values().map(|b| b.version().0).sum::<u64>())
        .flat_map(move |s: Rc<AppState>| {
            let mut known = lsp_known_versions.borrow_mut();
            s.buffers
                .values()
                .filter_map(|buf| {
                    let path = buf.path()?;
                    let ver = buf.version().0;
                    let prev = known.insert(path.clone(), ver);
                    (prev.is_some() && prev != Some(ver)).then(|| {
                        let do_save = matches!(
                            buf.change_reason(),
                            ChangeReason::LocalSave | ChangeReason::ExternalFileChange
                        );
                        (
                            path.clone(),
                            buf.doc().clone(),
                            buf.pending_edit_ops(),
                            do_save,
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
        .map(|(path, doc, edit_ops, do_save)| LspOut::BufferChanged {
            path,
            doc,
            edit_ops,
            do_save,
        })
        .stream();

    // RequestDiagnostics signal 1: PersistedContentHash changed while Running.
    let lsp_request_diag_hash = state
        .map(|s: Rc<AppState>| {
            (
                s.phase,
                s.buffers
                    .values()
                    // 2 ^ 20 ~= 1 million files before this overflows
                    .map(|b| (b.content_hash().0 & 0x0000_0fff_ffff_ff) + b.saved_version().0)
                    .sum::<u64>(),
            )
        })
        .dedupe()
        .filter(|(phase, _)| *phase == Phase::Running)
        .map(|_| LspOut::RequestDiagnostics)
        .stream();

    // RequestDiagnostics signal 2: transition to Running.
    let lsp_request_diag_running = state
        .map(|s: Rc<AppState>| s.phase)
        .dedupe()
        .filter(|phase| *phase == Phase::Running)
        .map(|_| LspOut::RequestDiagnostics)
        .stream();

    // InlayHints: viewport-driven request
    let lsp_inlay_hints = state
        .dedupe_by(|s| {
            if !s.lsp.inlay_hints_enabled {
                return None;
            }
            s.active_tab.as_ref().and_then(|path| {
                let buf = s.buffers.get(path)?;
                Some((path.clone(), *buf.scroll_row() / 5, buf.version()))
            })
        })
        .filter(|s| s.lsp.inlay_hints_enabled)
        .filter(|s| s.active_tab.is_some())
        .filter_map(|s| {
            let active_path = s.active_tab.as_ref()?;
            let buf = s.buffers.get(active_path)?;
            let path = buf.path().cloned()?;
            let start_row = buf.scroll_row();
            let buffer_height = s.dims.map_or(50, |d| d.buffer_height());
            let end_row = led_core::Row(*start_row + buffer_height + 10);
            Some(LspOut::InlayHints {
                path,
                start_row,
                end_row,
            })
        })
        .stream();

    // Feature requests: watch pending_request version
    let lsp_requests = state
        .dedupe_by(|s| s.lsp.pending_request.version())
        .filter(|s| s.lsp.pending_request.version() > 0)
        .filter(|s| s.lsp.pending_request.is_some())
        .filter_map(|s| {
            let req = (*s.lsp.pending_request).as_ref()?;
            let active_path = s.active_tab.as_ref()?;
            let buf = s.buffers.get(active_path)?;
            let path = buf.path().cloned()?;
            let row = buf.cursor_row();
            let col = buf.cursor_col();
            match req {
                LspRequest::GotoDefinition => Some(LspOut::GotoDefinition { path, row, col }),
                LspRequest::Format => Some(LspOut::Format { path }),
                LspRequest::CodeAction => {
                    let (end_row, end_col) = buf.mark().unwrap_or((row, col));
                    let (sr, sc, er, ec) = if (row, col) <= (end_row, end_col) {
                        (row, col, end_row, end_col)
                    } else {
                        (end_row, end_col, row, col)
                    };
                    Some(LspOut::CodeAction {
                        path,
                        start_row: sr,
                        start_col: sc,
                        end_row: er,
                        end_col: ec,
                    })
                }
                LspRequest::Rename { new_name } => Some(LspOut::Rename {
                    path,
                    row,
                    col,
                    new_name: new_name.clone(),
                }),
                LspRequest::Complete => Some(LspOut::Complete { path, row, col }),
                LspRequest::CodeActionSelect { index } => {
                    Some(LspOut::CodeActionSelect { index: *index })
                }
                LspRequest::CompleteAccept { index } => {
                    Some(LspOut::CompleteAccept { index: *index })
                }
            }
        })
        .stream();

    // Shutdown
    let lsp_shutdown = state
        .dedupe_by(|s| s.phase == Phase::Exiting)
        .filter(|s| s.phase == Phase::Exiting)
        .map(|_| LspOut::Shutdown)
        .stream();

    lsp_init.forward(&lsp_out);
    lsp_buf_changed.forward(&lsp_out);
    lsp_request_diag_hash.forward(&lsp_out);
    lsp_request_diag_running.forward(&lsp_out);
    lsp_inlay_hints.forward(&lsp_out);
    lsp_requests.forward(&lsp_out);
    lsp_shutdown.forward(&lsp_out);

    // ── PR settle timer (git_activity bypasses AppState) ──
    let pr_settle_timer = git_activity
        .map(|_| TimersOut::Set {
            name: "pr_settle",
            duration: Duration::from_secs(2),
            schedule: Schedule::Replace,
        })
        .stream();
    pr_settle_timer.forward(&timers_out);

    // ── PR load command ──
    let gh_pr_out: Stream<led_gh_pr::GhPrOut> = Stream::new();

    // Initial load / branch change: fires when branch first becomes known or switches
    let gh_pr_initial = state
        .dedupe_by(|s| s.git.branch.clone())
        .filter_map(load_pr_command)
        .stream();

    // Reload after git activity settles (e.g. commit, push, rebase)
    let gh_pr_reload = state
        .dedupe_by(|s| s.git.pr_settle_seq.version())
        .filter(|s| s.git.pr_settle_seq.version() > 0)
        .filter_map(load_pr_command)
        .stream();

    // PR poll: 15s repeating timer, started when a PR appears, cancelled when
    // it disappears. Two streams (set + cancel) instead of one with if/else.
    let pr_present_changed = state.map(|s| s.git.pr.is_some()).dedupe().stream();

    let pr_poll_set = pr_present_changed
        .clone()
        .filter(|has_pr| *has_pr)
        .map(|_| TimersOut::Set {
            name: "pr_poll",
            duration: Duration::from_secs(15),
            schedule: Schedule::Repeated,
        })
        .stream();

    let pr_poll_cancel = pr_present_changed
        .filter(|has_pr| !*has_pr)
        .map(|_| TimersOut::Cancel { name: "pr_poll" })
        .stream();

    pr_poll_set.forward(&timers_out);
    pr_poll_cancel.forward(&timers_out);

    // PR poll command: triggered by pr_poll_seq (bumped by timer handler).
    // Standalone mode has no workspace → dormant.
    let gh_pr_poll = state
        .dedupe_by(|s| s.git.pr_poll_seq.version())
        .filter(|s| s.git.pr_poll_seq.version() > 0)
        .filter(|s| s.git.pr.is_some())
        .filter(|s| s.workspace.is_loaded())
        .map(|s| {
            let pr = s.git.pr.as_ref().unwrap();
            led_gh_pr::GhPrOut::PollPr {
                api_endpoint: pr.api_endpoint.clone(),
                etag: pr.etag.clone(),
                root: s.workspace.loaded().unwrap().root.clone(),
            }
        })
        .stream();

    gh_pr_initial.forward(&gh_pr_out);
    gh_pr_reload.forward(&gh_pr_out);
    gh_pr_poll.forward(&gh_pr_out);

    // ── Open URL (fire-and-forget via consumer in lib.rs) ──
    let open_url = state
        .dedupe_by(|s| s.pending_open_url.version())
        .filter(|s| s.pending_open_url.version() > 0)
        .filter(|s| s.pending_open_url.is_some())
        .map(|s| (*s.pending_open_url).clone().unwrap())
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
        gh_pr_out,
        file_search_out,
        lsp_out,
        open_url,
    }
}

/// Build a `LoadPr` driver command from app state, when both branch and
/// workspace root are available. Returns `None` otherwise (including in
/// standalone mode, where `workspace.loaded()` is None).
fn load_pr_command(s: Rc<AppState>) -> Option<led_gh_pr::GhPrOut> {
    let branch = s.git.branch.clone()?;
    let root = s.workspace.loaded()?.root.clone();
    Some(led_gh_pr::GhPrOut::LoadPr { branch, root })
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
