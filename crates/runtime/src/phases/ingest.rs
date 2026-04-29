//! Ingest phase, decomposed into per-source sub-phases.
//!
//! Each `ingest_*` function destructures only the `Sources` fields
//! it actually touches, keeping the borrow checker happy without
//! threading 40 arguments through.

use std::sync::Arc;

use led_core::{CanonPath, EphemeralContentHash, SavedVersion};
use led_driver_buffers_core::LoadState;
use led_driver_clipboard_core::ClipboardResult;
use led_driver_git_core::GitEvent;
use led_driver_lsp_core::{LspCmd, LspEvent};
use led_driver_session_core::SessionEvent;
use led_state_diagnostics::{BufferDiagnostics, LspServerStatus};
use led_state_lifecycle::Phase;
use led_state_syntax::{Language, SyntaxState};
use led_state_tabs::TabId;

use crate::apply::edit::{auto_advance_arrow_follow, seed_edit_from_load};
use crate::apply::fs::{apply_workspace_tree_delta, reconcile_external_change};
use crate::apply::lsp::{
    completion_prefix, identifier_start_col, LspEditApply, LspGotoApply,
};
use crate::apply::session::{apply_pending_undo_restore, apply_session_kv, apply_sync_result};
use crate::dispatch;
use crate::phases::TickEnv;
use crate::query::{self, EditedBuffersInput};
use crate::{diag_offer, Sources, LspNotified, INFO_TTL};

/// Tick-start clock update + per-tick expiry sweeps.
pub(crate) fn ingest_clock(sources: &mut Sources) {
    let Sources {
        alerts,
        find_file,
        clock,
        ..
    } = sources;
    clock.now = std::time::Instant::now();
    alerts.expire_info(clock.now);
    if let Some(ff) = find_file.as_mut() {
        ff.input.expire_hint(clock.now);
    }
}

/// File-watch ingest: drain events, apply browser-tree deltas, fan
/// out reread / sync-check / LSP-watched-files dispatches in the
/// same tick the events landed.
pub(crate) fn ingest_file_watch(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        edits,
        store,
        fs,
        file_watch,
        lsp_watched_globs,
        undo_persistence,
        git_scan_pending,
        session,
        ..
    } = sources;

    env.drivers.file_watch.process(file_watch);

    if let Some(_root) = fs.root.as_ref()
        && !env.no_workspace
        && session.init_done
    {
        if apply_workspace_tree_delta(file_watch, edits, fs) {
            *git_scan_pending = true;
        }
        let reread_paths = query::external_reread_targets(
            query::FileWatchEventsInput::new(file_watch),
            EditedBuffersInput::new(edits),
        );
        if !reread_paths.is_empty() {
            let reread_cmds: Vec<led_driver_buffers_core::LoadAction> = reread_paths
                .iter()
                .map(|p| led_driver_buffers_core::LoadAction::Reread(p.clone()))
                .collect();
            env.drivers.file.execute(reread_cmds.iter(), store);
        }
        if env.resolved_config_dir.is_some() {
            let hash_index = query::notify_hash_index(EditedBuffersInput::new(edits));
            let sync_cmds = query::sync_check_cmds(
                query::FileWatchEventsInput::new(file_watch),
                query::HashIndexInput::new(&hash_index),
                query::UndoPersistenceInput::new(undo_persistence),
            );
            if !sync_cmds.is_empty() {
                env.drivers.session.execute(sync_cmds.iter());
            }
        }
        let lsp_watch_cmds = query::lsp_watched_file_notifications(
            query::FileWatchEventsInput::new(file_watch),
            query::LspWatchedGlobsInput::new(lsp_watched_globs),
        );
        for cmd in lsp_watch_cmds.iter() {
            if let LspCmd::DidChangeWatchedFiles { server, changes } = cmd {
                env.trace.lsp_did_change_watched_files(server, changes.len());
            }
        }
        if !lsp_watch_cmds.is_empty() {
            env.drivers.lsp.execute(lsp_watch_cmds.iter());
        }
    }
}

/// File-read driver completions: rereads, then initial loads,
/// then resume-check bookkeeping.
pub(crate) fn ingest_file_completions(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        tabs,
        edits,
        store,
        browser,
        fs,
        syntax,
        path_chains,
        lsp_notified,
        session,
        undo_persistence,
        resume_check_pending,
        lifecycle,
        git_scan_pending,
        ..
    } = sources;

    let file_completions = env.drivers.file.process(store);
    for reread in &file_completions.rereads {
        reconcile_external_change(reread, edits, fs, git_scan_pending);
    }
    for completion in file_completions.initials {
        let detected = path_chains
            .get(&completion.path)
            .and_then(Language::from_chain)
            .or_else(|| Language::from_path(&completion.path));
        let inserted = seed_edit_from_load(
            edits,
            completion.path.clone(),
            completion.rope.clone(),
        );

        if inserted {
            apply_pending_undo_restore(
                &completion.path,
                edits,
                session,
                undo_persistence,
            );
            let is_active = tabs
                .active
                .and_then(|id| tabs.open.iter().find(|t| t.id == id))
                .is_some_and(|t| t.path == completion.path);
            if is_active {
                let ancestors = led_state_browser::ancestors_of(
                    fs,
                    &browser.expanded_dirs,
                    Some(&completion.path),
                );
                for p in ancestors {
                    browser.expanded_dirs.insert(p);
                }
            }
        }
        if let Some(lang) = detected {
            syntax
                .by_path
                .entry(completion.path.clone())
                .or_insert_with(|| SyntaxState::new(lang));
        }
        if inserted {
            let (version, saved, hash) = edits
                .buffers
                .get(&completion.path)
                .map(|eb| {
                    (
                        eb.version,
                        eb.saved_version,
                        EphemeralContentHash::of_rope(&eb.rope).persist(),
                    )
                })
                .unwrap_or_default();
            env.drivers.lsp.execute(std::iter::once(&LspCmd::BufferOpened {
                path: completion.path.clone(),
                language: detected,
                rope: completion.rope.clone(),
                hash,
            }));
            lsp_notified.insert(
                completion.path.clone(),
                LspNotified {
                    version,
                    saved_version: saved,
                },
            );
        }

        for tab in tabs.open.iter_mut() {
            if tab.path != completion.path {
                continue;
            }
            let rope = &completion.rope;
            let line_count = rope.len_lines();
            if let Some(pc) = tab.pending_cursor.take() {
                let line = pc.line.min(line_count.saturating_sub(1));
                let line_start = rope.line_to_char(line);
                let line_end = if line + 1 < line_count {
                    rope.line_to_char(line + 1)
                } else {
                    rope.len_chars()
                };
                let line_len = line_end.saturating_sub(line_start);
                let col = pc.col.min(line_len);
                tab.cursor = led_state_tabs::Cursor {
                    line,
                    col,
                    preferred_col: col,
                };
            }
            if let Some(ps) = tab.pending_scroll.take() {
                let top = ps.top.min(line_count.saturating_sub(1));
                tab.scroll = led_state_tabs::Scroll {
                    top,
                    top_sub_line: ps.top_sub_line,
                };
            }
        }
        *resume_check_pending = true;
    }

    if *resume_check_pending {
        *resume_check_pending = false;
        if matches!(lifecycle.phase, Phase::Resuming) {
            let still_pending = tabs
                .open
                .iter()
                .any(|t| t.pending_cursor.is_some() || t.pending_scroll.is_some());
            if !still_pending {
                lifecycle.phase = Phase::Running;
            }
        }
    }
}

/// LSP event drain: diagnostics, status, completions, goto, edits,
/// code-actions, inlay hints, dynamic watch registrations.
pub(crate) fn ingest_lsp_events(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        tabs,
        edits,
        alerts,
        jumps,
        browser,
        terminal,
        path_chains,
        diagnostics,
        lsp_status,
        completions,
        completions_pending,
        lsp_extras,
        lsp_pending,
        lsp_watched_globs,
        ..
    } = sources;

    for ev in env.drivers.lsp.process() {
        match ev {
            LspEvent::Diagnostics {
                path,
                hash,
                diagnostics: diags,
            } => {
                let Some(eb) = edits.buffers.get(&path) else {
                    continue;
                };
                let transformed = match crate::diag_offer::offer_diagnostics(eb, hash, diags) {
                    diag_offer::OfferOutcome::Accept(d) => d,
                    diag_offer::OfferOutcome::Reject => continue,
                };
                let current_hash =
                    EphemeralContentHash::of_rope(&eb.rope).persist();
                if transformed.is_empty() {
                    diagnostics.by_path.remove(&path);
                } else {
                    diagnostics.by_path.insert(
                        path,
                        BufferDiagnostics::new(current_hash, transformed),
                    );
                }
            }
            LspEvent::Ready { server } => {
                let entry = lsp_status
                    .by_server
                    .entry(server)
                    .or_insert_with(LspServerStatus::default);
                entry.ready = true;
                entry.busy = false;
                entry.detail = None;
            }
            LspEvent::Progress { server, busy, detail } => {
                let entry = lsp_status
                    .by_server
                    .entry(server)
                    .or_insert_with(LspServerStatus::default);
                entry.busy = busy;
                entry.detail = detail;
                if !busy {
                    entry.ready = true;
                }
            }
            LspEvent::Error { server, message } => {
                alerts.set_warn(server.to_string(), format!("LSP {server}: {message}"));
                if let Some(entry) = lsp_status.by_server.get_mut(&server) {
                    entry.busy = false;
                    entry.detail = None;
                }
            }
            LspEvent::Completion {
                path,
                seq,
                items,
                prefix_line,
                prefix_start_col,
            } => {
                if seq != completions_pending.seq_gen {
                    continue;
                }
                let Some(tab) = tabs.open.iter().find(|t| t.path == path) else {
                    continue;
                };
                if items.is_empty() {
                    completions.dismiss();
                    continue;
                }
                let prefix_start_col = match prefix_start_col {
                    Some(units) => {
                        let pl = prefix_line as usize;
                        if pl >= edits.buffers.get(&path).map_or(0, |eb| eb.rope.len_lines())
                        {
                            continue;
                        }
                        let eb = edits.buffers.get(&path).expect("checked above");
                        led_core::utf16_units_to_grapheme_col(eb.rope.line(pl), units) as u32
                    }
                    None => identifier_start_col(
                        edits,
                        &path,
                        prefix_line as usize,
                        tab.cursor.col,
                    ),
                };
                let prefix = completion_prefix(
                    edits,
                    &path,
                    tab,
                    prefix_line as usize,
                    prefix_start_col as usize,
                );
                let filtered = led_state_completions::refilter(&items, &prefix);
                if filtered.is_empty() {
                    completions.dismiss();
                    continue;
                }
                if filtered.len() == 1
                    && led_state_completions::is_identity_match(
                        &items[filtered[0]],
                        &prefix,
                    )
                {
                    completions.dismiss();
                    continue;
                }
                completions.session =
                    Some(led_state_completions::CompletionSession {
                        tab: tab.id,
                        path,
                        seq,
                        prefix_line,
                        prefix_start_col,
                        items,
                        filtered: std::sync::Arc::new(filtered),
                        selected: 0,
                        scroll: 0,
                    });
            }
            LspEvent::CompletionResolved { .. } => {
                // Stage 5 handles the post-commit apply.
            }
            LspEvent::GotoDefinition { seq, location } => {
                LspGotoApply {
                    tabs,
                    edits,
                    jumps,
                    alerts,
                    lsp_pending,
                    terminal,
                    browser,
                    path_chains,
                }
                .apply(seq, location);
            }
            LspEvent::Edits {
                seq,
                origin,
                edits: file_edits,
            } => {
                let _ = lsp_extras; // not needed by apply
                LspEditApply {
                    edits,
                    tabs,
                    alerts,
                    lsp_pending,
                }
                .apply(seq, origin, &file_edits);
            }
            LspEvent::CodeActions {
                path,
                seq,
                actions,
            } => {
                if lsp_pending.latest_code_action_seq != Some(seq) {
                    // Stale response; drop.
                } else if !actions.is_empty() {
                    dispatch::install_code_action_picker(
                        lsp_extras,
                        path,
                        seq,
                        actions,
                    );
                }
            }
            LspEvent::InlayHints {
                path,
                version,
                hints,
            } => {
                if !lsp_extras.inlay_hints_enabled {
                    continue;
                }
                let current_version = edits
                    .buffers
                    .get(&path)
                    .map(|eb| eb.version)
                    .unwrap_or_default();
                if version != current_version {
                    continue;
                }
                lsp_pending.inlay_hints_by_path.insert(
                    path,
                    led_state_lsp::BufferInlayHints {
                        version,
                        hints,
                    },
                );
            }
            LspEvent::WatchedFilesRegistered {
                server,
                registration_id,
                globs,
            } => {
                lsp_watched_globs.register(server, registration_id, globs);
            }
            LspEvent::WatchedFilesUnregistered {
                server,
                registration_id,
            } => {
                lsp_watched_globs.unregister(&server, &registration_id);
            }
        }
    }
}

/// File-write driver completions: install saved rope as new disk
/// baseline, surface alerts, mark git scan pending.
pub(crate) fn ingest_file_writes(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        edits,
        store,
        alerts,
        clock,
        git_scan_pending,
        ..
    } = sources;

    for done in env.drivers.file_write.process() {
        let basename = done
            .path
            .file_name()
            .map(|os| os.to_string_lossy().into_owned())
            .unwrap_or_else(|| done.path.display().to_string());
        match done.result {
            Ok(rope) => {
                store
                    .loaded
                    .insert(done.path.clone(), LoadState::Ready(rope));
                if let Some(eb) = edits.buffers.get_mut(&done.path) {
                    eb.saved_version =
                        eb.saved_version.max(SavedVersion(done.version.0));
                    let hash =
                        EphemeralContentHash::of_rope(&eb.rope).persist();
                    eb.history.insert_save_point(hash);
                    eb.disk_content_hash = hash;
                }
                alerts.clear_warn(&basename);
                alerts.set_info(format!("Saved {basename}"), clock.now, INFO_TTL);
                *git_scan_pending = true;
            }
            Err(msg) => {
                alerts.set_warn(basename.clone(), format!("save {basename}: {msg}"));
            }
        }
    }
}

/// Fs-list driver completions: install entries (or failure marker)
/// into the browser tree cache.
pub(crate) fn ingest_fs_list(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources { fs, .. } = sources;
    let fs_completions = env.drivers.fs_list.process();
    for done in fs_completions {
        match done.result {
            Ok(entries) => {
                fs.failed_dirs.remove(&done.path);
                fs.dir_contents
                    .insert(done.path, imbl::Vector::from_iter(entries));
            }
            Err(_) => {
                fs.dir_contents.remove(&done.path);
                fs.failed_dirs.insert(done.path);
            }
        }
    }
}

/// Find-file driver completions: install matching listings into
/// the overlay; auto-advance in arrow-follow mode.
pub(crate) fn ingest_find_file(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        tabs,
        find_file,
        ..
    } = sources;
    for done in env.drivers.find_file.process() {
        let Some(ff) = find_file.as_mut() else {
            continue;
        };
        let (dir_part, prefix) = led_state_find_file::split_input(&ff.input.text);
        if dir_part.is_empty() {
            continue;
        }
        let expected_dir = led_core::UserPath::new(led_state_find_file::expand_path(
            dir_part,
        ))
        .canonicalize();
        if done.dir != expected_dir || done.prefix != prefix {
            continue;
        }
        ff.completions = done.entries;
        auto_advance_arrow_follow(ff, tabs);
    }
}

/// File-search driver completions: install search results, then
/// apply replace-all completions (alert with combined disk + memory
/// counts).
pub(crate) fn ingest_file_search(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        edits,
        alerts,
        clock,
        file_search,
        ..
    } = sources;
    for done in env.drivers.file_search.process() {
        let Some(fs_state) = file_search.as_mut() else {
            continue;
        };
        if done.query != fs_state.query.text
            || done.case_sensitive != fs_state.case_sensitive
            || done.use_regex != fs_state.use_regex
        {
            continue;
        }
        fs_state.results = done.groups;
        fs_state.flat_hits = done.flat;
        fs_state.hit_replacements =
            vec![None; fs_state.flat_hits.len()];
        if let led_state_file_search::FileSearchSelection::Result(i) =
            fs_state.selection
            && i >= fs_state.flat_hits.len()
        {
            fs_state.selection =
                led_state_file_search::FileSearchSelection::SearchInput;
        }
        fs_state.scroll_offset = 0;
    }

    for done in env.drivers.file_search.process_replace() {
        let memory = std::mem::take(&mut edits.pending_replace_in_memory);
        let memory_total: usize = memory.iter().map(|m| m.count).sum();
        let total = done.total_replacements + memory_total;
        alerts.set_info(
            format!("Replaced {total} occurrence(s)"),
            clock.now,
            INFO_TTL,
        );
    }
}

/// Syntax driver completions: install fresh tree + tokens; clear
/// stale `in_flight_version`.
pub(crate) fn ingest_syntax(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources { edits, syntax, .. } = sources;
    for done in env.drivers.syntax.process() {
        let Some(state) = syntax.by_path.get_mut(&done.path) else {
            continue;
        };
        if state.in_flight_version == Some(done.version) {
            state.in_flight_version = None;
        }
        let current_version = edits
            .buffers
            .get(&done.path)
            .map(|eb| eb.version)
            .unwrap_or_default();
        if done.version < state.version || done.version > current_version {
            continue;
        }
        state.language = done.language;
        state.tree = Some(done.tree);
        state.tree_rope = Some(done.tree_rope);
        state.tokens = done.tokens;
        state.version = done.version;
    }
}

/// Session driver events: Restored / SessionSaved / UndoFlushed /
/// Failed / SyncResult. Promotes Phase::Starting → Resuming/Running
/// based on whether tabs have any pending cursors to apply.
pub(crate) fn ingest_session(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        tabs,
        edits,
        alerts,
        jumps,
        browser,
        path_chains,
        session,
        undo_persistence,
        resume_check_pending,
        lifecycle,
        file_watch,
        ..
    } = sources;

    let mut session_just_restored = false;
    for ev in env.drivers.session.process() {
        match ev {
            SessionEvent::Restored { primary, restored } => {
                session.primary = primary;
                session.init_done = true;
                if let Some(data) = restored {
                    for sb in &data.buffers {
                        if let Some(undo) = &sb.undo {
                            session
                                .pending_undo
                                .insert(sb.path.clone(), undo.clone());
                        }
                    }
                    let materialised: Vec<CanonPath> = session
                        .pending_undo
                        .keys()
                        .filter(|p| edits.buffers.contains_key(*p))
                        .cloned()
                        .collect();
                    for path in materialised {
                        apply_pending_undo_restore(
                            &path,
                            edits,
                            session,
                            undo_persistence,
                        );
                    }
                    let mut new_tabs: imbl::Vector<led_state_tabs::Tab> =
                        tabs.open.clone();
                    for sb in &data.buffers {
                        if let Some(existing) = new_tabs
                            .iter_mut()
                            .find(|t| t.path == sb.path)
                        {
                            if existing.pending_cursor.is_none() {
                                existing.pending_cursor = Some(sb.cursor);
                            }
                            if existing.pending_scroll.is_none() {
                                existing.pending_scroll = Some(sb.scroll);
                            }
                            continue;
                        }
                        let id = TabId(
                            new_tabs
                                .iter()
                                .map(|t| t.id.0)
                                .max()
                                .unwrap_or(0)
                                + 1,
                        );
                        let chain = led_core::UserPath::new(
                            sb.path.as_path(),
                        )
                        .resolve_chain();
                        path_chains.insert(sb.path.clone(), chain);
                        new_tabs.push_back(led_state_tabs::Tab {
                            id,
                            path: sb.path.clone(),
                            pending_cursor: Some(sb.cursor),
                            pending_scroll: Some(sb.scroll),
                            ..Default::default()
                        });
                    }
                    if tabs.active.is_none()
                        && let Some(t) =
                            new_tabs.get(data.active_tab_order)
                    {
                        tabs.active = Some(t.id);
                    }
                    tabs.open = new_tabs;
                    browser.visible = data.show_side_panel;
                    apply_session_kv(&data.kv, browser, jumps);
                    session.last_saved = Some(data);
                } else {
                    session.last_saved = None;
                }
                session_just_restored = true;
            }
            SessionEvent::SessionSaved => {
                session.saved = true;
            }
            SessionEvent::UndoFlushed {
                path,
                chain_id,
                persisted_undo_len,
                last_seq,
            } => {
                if let Some(tracker) = undo_persistence.get_mut(&path)
                    && tracker.chain_id == chain_id
                {
                    tracker.persisted_len = persisted_undo_len;
                    tracker.last_seq = last_seq;
                }
            }
            SessionEvent::Failed { message } => {
                alerts.set_warn(
                    "session".to_string(),
                    format!("session: {message}"),
                );
                session.saved = true;
                session.init_done = true;
            }
            SessionEvent::SyncResult { kind } => {
                apply_sync_result(kind, edits, undo_persistence, file_watch);
            }
        }
    }
    if session_just_restored && !tabs.open.is_empty() {
        // We just synthesised tabs with pending cursors —
        // bookkeeping flag tells the loop to re-evaluate
        // `Phase::Resuming` after the current execute pass.
        *resume_check_pending = true;
        if matches!(lifecycle.phase, Phase::Starting) {
            lifecycle.phase = Phase::Resuming;
        }
    } else if session_just_restored {
        // Restored with empty session OR non-primary — no
        // tabs to wait for.
        if matches!(lifecycle.phase, Phase::Starting | Phase::Resuming) {
            lifecycle.phase = Phase::Running;
        }
    }
}

/// Git driver events: file statuses + per-path line statuses
/// (anchored to the buffer's disk-content hash).
pub(crate) fn ingest_git(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources { edits, git, .. } = sources;
    for ev in env.drivers.git.process() {
        match ev {
            GitEvent::FileStatuses { statuses, branch } => {
                git.branch = branch;
                let mut imbl_map: imbl::HashMap<
                    CanonPath,
                    imbl::HashSet<led_core::IssueCategory>,
                > = imbl::HashMap::default();
                for (path, cats) in statuses {
                    let mut imbl_set: imbl::HashSet<led_core::IssueCategory> =
                        imbl::HashSet::default();
                    for c in cats {
                        imbl_set.insert(c);
                    }
                    imbl_map.insert(path, imbl_set);
                }
                git.file_statuses = imbl_map;
            }
            GitEvent::LineStatuses { path, statuses } => {
                if statuses.is_empty() {
                    git.line_statuses.remove(&path);
                } else {
                    let anchor_hash = edits
                        .buffers
                        .get(&path)
                        .map(|eb| eb.disk_content_hash)
                        .unwrap_or_default();
                    git.line_statuses.insert(
                        path,
                        led_state_git::GitLineStatuses {
                            anchor_hash,
                            statuses: Arc::new(statuses),
                        },
                    );
                }
            }
        }
    }
}

/// Clipboard completions: paste-on-yank, kill-ring fallback,
/// write acknowledgement.
pub(crate) fn ingest_clipboard(sources: &mut Sources, env: &TickEnv<'_>) {
    let Sources {
        tabs,
        edits,
        kill_ring,
        clip,
        browser,
        terminal,
        ..
    } = sources;
    for done in env.drivers.clipboard.process() {
        let content_cols = dispatch::editor_content_cols(terminal, browser);
        match done.result {
            Ok(ClipboardResult::Text(Some(text))) => {
                if let Some(target) = clip.pending_yank.take() {
                    dispatch::apply_yank(tabs, edits, target, &text, content_cols);
                }
                clip.read_in_flight = false;
            }
            Ok(ClipboardResult::Text(None)) | Err(_) => {
                if let Some(target) = clip.pending_yank.take()
                    && let Some(fallback) = kill_ring.latest.clone()
                {
                    dispatch::apply_yank(tabs, edits, target, &fallback, content_cols);
                }
                clip.read_in_flight = false;
            }
            Ok(ClipboardResult::Written) => {}
        }
    }
}

/// Browser-selection snap: pin the side-panel cursor to the active
/// tab's path unless the user is currently arrow-navigating the
/// side panel itself.
pub(crate) fn ingest_browser_snap(sources: &mut Sources) {
    let Sources { tabs, browser, .. } = sources;
    if !matches!(browser.focus, led_state_browser::Focus::Side) {
        let active_path_now: Option<&CanonPath> = tabs
            .active
            .and_then(|id| tabs.open.iter().find(|t| t.id == id))
            .map(|t| &t.path);
        if let Some(p) = active_path_now
            && browser.selected_path.as_ref() != Some(p)
        {
            browser.selected_path = Some(p.clone());
        }
    }
}
