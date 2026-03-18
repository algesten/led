use std::sync::Arc;

mod action;
mod actions_of;
mod buffers_of;
mod edit;
mod jump;
mod mov;
mod process_of;
mod search;
mod sync_of;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::rx::Stream;
use led_core::theme::Theme;
use std::path::PathBuf;

use led_core::{Action, Alert, BufferId, Doc, PanelSlot, next_change_seq};
use led_state::{
    AppState, BracketPair, BufferState, Dimensions, HighlightSpan, SaveState, SessionRestorePhase,
};
use led_workspace::Workspace;

use crate::Drivers;
use crate::model::actions_of::actions_of;
use crate::model::buffers_of::buffers_of;
use crate::model::process_of::process_of;
use crate::model::sync_of::sync_of;

pub fn model(drivers: Drivers, init: AppState) -> Stream<Arc<AppState>> {
    let state: Stream<Arc<AppState>> = Stream::new();

    // ── 1. Derive from hoisted state ──

    use led_workspace::WorkspaceIn as WI;

    let workspace_s = drivers
        .workspace_in
        .map(|ev| match ev {
            WI::Workspace { workspace } | WI::WorkspaceChanged { workspace } => {
                Some(Mut::Workspace(workspace))
            }
            WI::SessionRestored { session } => Some(Mut::SessionRestored(session)),
            WI::SessionSaved => Some(Mut::SessionSaved),
            WI::WatchersReady => Some(Mut::WatchersReady),
            _ => None, // handled by undo_flushed_s, notify_s, sync_s
        })
        .filter(|opt| opt.is_some())
        .map(|opt| opt.unwrap())
        .stream();

    // UndoFlushed needs buffer lookup → sample_combine with state
    let undo_flushed_s = drivers
        .workspace_in
        .filter(|ev| matches!(ev, WI::UndoFlushed { .. }))
        .sample_combine(&state)
        .map(|(ev, s)| {
            let WI::UndoFlushed {
                file_path,
                chain_id,
                last_seen_seq,
                ..
            } = ev
            else {
                unreachable!()
            };
            let buf_id = s
                .buffers
                .values()
                .find(|b| b.path.as_ref() == Some(&file_path))
                .map(|b| b.id)
                .unwrap_or(BufferId(u64::MAX));
            Mut::UndoFlushed {
                buf_id,
                chain_id,
                last_seen_seq,
            }
        })
        .stream();

    // NotifyEvent needs hash→path lookup → sample_combine with state.
    // All filtering/dedup is handled by the model (change_seq, content_hash).
    let notify_s = drivers
        .workspace_in
        .filter(|ev| matches!(ev, WI::NotifyEvent { .. }))
        .sample_combine(&state)
        .map(|(ev, s)| {
            let WI::NotifyEvent { file_path_hash } = ev else {
                unreachable!()
            };
            let path = s
                .notify_hash_to_buffer
                .get(&file_path_hash)
                .and_then(|id| s.buffers.get(id))
                .and_then(|b| b.path.clone());
            Mut::NotifyEvent { path }
        })
        .stream();

    // Sync results: full doc application in combinator chain
    let sync_s = sync_of(&drivers.workspace_in, &state);

    let keymap_s = state
        .filter_map(|s| s.config_keys.as_ref().map(|ck| ck.file.clone()))
        .dedupe()
        .map(|keys: Arc<Keys>| {
            keys.as_ref()
                .clone()
                .into_keymap()
                .map(|km| Arc::new(km))
                .map_err(|e| Alert::Warn(e))
        })
        .map(|r| match r {
            Ok(v) => Mut::Keymap(v),
            Err(a) => Mut::alert(a),
        })
        .stream();

    let actions_s = actions_of(&drivers.terminal_in, &state);
    let buffers_s = buffers_of(&drivers.docstore_in, &state);
    let process_s = process_of(&state);

    // ── 2. Build up muts from driver input and derived streams ──

    let muts: Stream<Mut> = drivers
        .config_keys_in
        .map(|r| match r {
            Ok(v) => Mut::ConfigKeys(v),
            Err(a) => Mut::alert(a),
        })
        .or(drivers.config_theme_in.map(|r| match r {
            Ok(v) => Mut::ConfigTheme(v),
            Err(a) => Mut::alert(a),
        }));

    let direct_actions_s = drivers.actions_in.map(|a| Mut::Action(a)).stream();

    // Split timers: undo_flush goes through a chain that samples state,
    // other timers go directly to the reducer.
    let timers_s = drivers
        .timers_in
        .filter(|t| t.name != "undo_flush")
        .map(|t| Mut::TimerFired(t.name))
        .stream();

    let undo_flush_s = drivers
        .timers_in
        .filter(|t| t.name == "undo_flush")
        .sample_combine(&state)
        .map(|(_, s)| s)
        .flat_map(|s: Arc<AppState>| {
            s.buffers
                .values()
                .filter(|b| b.path.is_some())
                .filter(|b| b.doc.undo_history_len() > b.persisted_undo_len || b.doc.dirty())
                .filter_map(|b| {
                    let file_path = b.path.clone().unwrap();
                    let chain_id = b
                        .chain_id
                        .clone()
                        .unwrap_or_else(led_workspace::new_chain_id);
                    // Flush pending edits so in-progress ops are captured.
                    let closed_doc = b.doc.close_undo_group();
                    let entries: Vec<Vec<u8>> = closed_doc
                        .undo_entries_from(b.persisted_undo_len)
                        .iter()
                        .filter_map(|e| rmp_serde::to_vec(e).ok())
                        .collect();
                    // Skip flush if there are no new entries to persist.
                    // This prevents ping-pong cascades where SyncApply makes
                    // a buffer dirty but there are no actual new undo entries.
                    if entries.is_empty() {
                        return None;
                    }
                    let undo_cursor = closed_doc.undo_history_len();
                    Some(Mut::UndoFlushReady {
                        buf_id: b.id,
                        flush: led_state::UndoFlush {
                            file_path,
                            chain_id,
                            content_hash: b.content_hash,
                            undo_cursor,
                            distance_from_save: closed_doc.distance_from_save(),
                            entries,
                        },
                    })
                })
                .collect::<Vec<_>>()
        });

    let fs_s = drivers
        .fs_in
        .map(|ev| match ev {
            led_fs::FsIn::DirListed { path, entries } => Mut::DirListed(path, entries),
        })
        .stream();

    let clipboard_s = drivers
        .clipboard_in
        .map(|ev| match ev {
            led_clipboard::ClipboardIn::Text(text) => Mut::ClipboardText(text),
        })
        .stream();

    let syntax_s = drivers
        .syntax_in
        .map(|syn| Mut::SyntaxUpdate {
            buf_id: syn.buf_id,
            version: syn.doc_version,
            highlights: syn.highlights,
            bracket_pairs: syn.bracket_pairs,
            matching_bracket: syn.matching_bracket,
            indent: syn.indent,
        })
        .stream();

    workspace_s.forward(&muts);
    undo_flushed_s.forward(&muts);
    notify_s.forward(&muts);
    sync_s.forward(&muts);
    keymap_s.forward(&muts);
    actions_s.forward(&muts);
    direct_actions_s.forward(&muts);
    buffers_s.forward(&muts);
    process_s.forward(&muts);
    timers_s.forward(&muts);
    undo_flush_s.forward(&muts);
    fs_s.forward(&muts);
    clipboard_s.forward(&muts);
    syntax_s.forward(&muts);

    // ── 3. Reduce ──

    muts.fold_into(&state, Arc::new(init), |s, m| {
        log::trace!("model: {}", m.name());
        let mut s = Arc::unwrap_or_clone(s);
        match m {
            Mut::ActivateBuffer(id) => {
                s.active_buffer = Some(id);
                action::reveal_active_buffer(&mut s);
            }
            Mut::Action(a) => action::handle_action(&mut s, a),
            Mut::Alert { info, warn } => {
                s.info = info;
                s.warn = warn;
            }
            Mut::BufferOpen {
                buf,
                next_id,
                activate,
                notify_hash,
                session_restore_done,
            } => {
                let will_activate = activate || s.active_buffer.is_none();
                if will_activate {
                    s.active_buffer = Some(buf.id);
                }
                if let Some(ref path) = buf.path {
                    s.session_positions.remove(path);
                }
                s.notify_hash_to_buffer.insert(notify_hash, buf.id);
                let buf_id = buf.id;
                let buf_path = buf.path.clone();
                s.buffers.insert(buf.id, buf);
                s.next_buffer_id = next_id;
                action::renumber_tabs(&mut s);
                if session_restore_done {
                    s.session_restore_phase = SessionRestorePhase::Done;
                    s.session_active_tab_order = None;
                }
                // Apply pending jump position if this buffer matches
                if let Some(ref pending) = s.pending_jump_position {
                    if buf_path.as_ref() == Some(&pending.path) {
                        let pending = s.pending_jump_position.take().unwrap();
                        if let Some(buf) = s.buffers.get_mut(&buf_id) {
                            buf.cursor_row =
                                pending.row.min(buf.doc.line_count().saturating_sub(1));
                            buf.cursor_col = pending.col;
                            buf.cursor_col_affinity = pending.col;
                            buf.scroll_row = pending.scroll_offset;
                        }
                    }
                }
                // Resolve focus once restore is done and buffers exist
                if s.session_restore_phase == SessionRestorePhase::Done {
                    resolve_focus(&mut s);
                }
                if will_activate {
                    action::reveal_active_buffer(&mut s);
                }
            }
            Mut::BufferSaved {
                id,
                buf,
                undo_clear_path,
            } => {
                s.buffers.insert(id, buf);
                if let Some(buf) = s.buffers.get_mut(&id) {
                    buf.change_seq = next_change_seq();
                }
                if let Some(path) = undo_clear_path {
                    s.pending_undo_clear.set(path);
                }
            }
            Mut::BufferUpdate(id, buf) => {
                s.buffers.insert(id, buf);
            }
            Mut::ClipboardText(text) => {
                action::yank_text(&mut s, text);
            }
            Mut::ConfigKeys(v) => s.config_keys = Some(v),
            Mut::DirListed(path, entries) => {
                s.browser.dir_contents.insert(path, entries);
                s.browser.rebuild_entries();
                s.browser.complete_pending_reveal();
                action::browser_scroll_to_selected(&mut s);
            }
            Mut::ConfigTheme(v) => s.config_theme = Some(v),
            Mut::ForceRedraw(v) => s.force_redraw = v,
            Mut::Keymap(v) => s.keymap = Some(v),
            Mut::Resize(w, h) => {
                s.dims = Some(Dimensions::new(w, h, s.show_side_panel));
            }
            Mut::SessionOpenFailed { path } => {
                s.session_positions.remove(&path);
                if s.session_restore_phase == SessionRestorePhase::Restoring
                    && s.session_positions.is_empty()
                {
                    s.session_restore_phase = SessionRestorePhase::Done;
                    resolve_focus(&mut s);
                }
            }
            Mut::SessionRestored(session) => match session {
                Some(session) => {
                    s.session_restore_phase = SessionRestorePhase::Restoring;
                    s.session_active_tab_order = Some(session.active_tab_order);
                    s.show_side_panel = session.show_side_panel;
                    // Parse persisted focus for later application
                    s.session_restored_focus = session.kv.get("focus").map(|v| match v.as_str() {
                        "side" => PanelSlot::Side,
                        _ => PanelSlot::Main,
                    });
                    let paths: Vec<_> = session
                        .buffers
                        .iter()
                        .map(|b| b.file_path.clone())
                        .collect();
                    for buf in session.buffers {
                        s.session_positions.insert(buf.file_path.clone(), buf);
                    }
                    // Restore browser state from KV
                    if let Some(v) = session.kv.get("browser.selected") {
                        s.browser.selected = v.parse().unwrap_or(0);
                    }
                    if let Some(v) = session.kv.get("browser.scroll_offset") {
                        s.browser.scroll_offset = v.parse().unwrap_or(0);
                    }
                    if let Some(v) = session.kv.get("browser.expanded_dirs") {
                        s.browser.expanded_dirs = v
                            .lines()
                            .filter(|l| !l.is_empty())
                            .map(PathBuf::from)
                            .collect();
                    }
                    // Restore jump list from KV
                    if let Some(json) = session.kv.get("jump_list.entries") {
                        if let Ok(entries) = serde_json::from_str::<
                            std::collections::VecDeque<led_state::JumpPosition>,
                        >(json)
                        {
                            s.jump_list = entries;
                            s.jump_list_index = session
                                .kv
                                .get("jump_list.index")
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(s.jump_list.len());
                        }
                    }
                    s.pending_session_opens.set(paths);
                    // Request dir listings for restored expanded dirs
                    if !s.browser.expanded_dirs.is_empty() {
                        s.pending_lists
                            .set(s.browser.expanded_dirs.iter().cloned().collect());
                    }
                }
                None => {
                    s.session_restore_phase = SessionRestorePhase::Done;
                    // Only resolve now if no arg_paths will open buffers later
                    if s.startup.arg_paths.is_empty() {
                        resolve_focus(&mut s);
                    }
                }
            },
            Mut::SessionSaved => {
                s.session_saved = true;
            }
            Mut::WatchersReady => {
                s.watchers_ready = true;
            }
            Mut::NotifyEvent { path } => {
                if let Some(path) = path {
                    s.pending_sync_check.set(path);
                }
            }
            Mut::SyncApply {
                buf_id,
                doc,
                chain_id,
                last_seen_seq,
            } => {
                if let Some(buf) = s.buffers.get_mut(&buf_id) {
                    // Guard: skip duplicate application when multiple
                    // FSEvents trigger parallel CheckSyncs for the same data.
                    if last_seen_seq > buf.last_seen_seq {
                        buf.doc = doc;
                        buf.chain_id = chain_id;
                        buf.last_seen_seq = last_seen_seq;
                        buf.persisted_undo_len = buf.doc.undo_history_len();
                        buf.content_hash = buf.doc.content_hash();
                        buf.change_seq = next_change_seq();
                    }
                }
            }
            Mut::SyncReset { buf_id } => {
                if let Some(buf) = s.buffers.get_mut(&buf_id) {
                    buf.last_seen_seq = 0;
                    buf.chain_id = None;
                    buf.persisted_undo_len = buf.doc.undo_history_len();
                    buf.change_seq = next_change_seq();
                    // SyncReset means the undo chain was cleared by a save.
                    // If the buffer is dirty only from remote sync (save_state
                    // still Clean — the user hasn't made local edits), mark
                    // the doc as saved since the file was saved by the other
                    // instance.
                    if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                        buf.doc = buf.doc.mark_saved();
                    }
                }
            }
            Mut::UndoFlushReady { buf_id, flush } => {
                if let Some(buf) = s.buffers.get_mut(&buf_id) {
                    // Close the undo group on the actual buffer doc to keep
                    // it consistent with persisted_undo_len. Without this,
                    // subsequent edits can append to the already-flushed open
                    // group, making undo_groups_from(persisted) return empty.
                    buf.doc = buf.doc.close_undo_group();
                    buf.chain_id = Some(flush.chain_id.clone());
                    buf.persisted_undo_len = flush.undo_cursor;
                    buf.change_seq = next_change_seq();
                }
                s.pending_undo_flush.set(Some(flush));
            }
            Mut::UndoFlushed {
                buf_id,
                chain_id,
                last_seen_seq,
            } => {
                if let Some(buf) = s.buffers.get_mut(&buf_id) {
                    buf.chain_id = Some(chain_id);
                    buf.last_seen_seq = last_seen_seq;
                }
            }
            Mut::SyntaxUpdate {
                buf_id,
                version,
                highlights,
                bracket_pairs,
                matching_bracket,
                indent: _,
            } => {
                if let Some(buf) = s.buffers.get_mut(&buf_id) {
                    if buf.doc.version() == version {
                        buf.syntax_highlights = highlights;
                        buf.bracket_pairs = bracket_pairs;
                        buf.matching_bracket = matching_bracket;
                    }
                }
            }
            Mut::Suspend(v) => s.suspend = v,
            Mut::TimerFired(name) => handle_timer(&mut s, name),
            Mut::Workspace(v) => {
                s.browser.root = Some(v.root.clone());
                s.browser.dir_contents.clear();
                s.browser.rebuild_entries();
                let mut dirs = vec![v.root.clone()];
                dirs.extend(s.browser.expanded_dirs.iter().cloned());
                s.pending_lists.set(dirs);
                s.workspace = Some(Arc::new(v));
            }
        }
        Arc::new(s)
    });

    state
}

/// Apply focus after session restore completes.
/// Priority: restored focus (if valid) → open buffer → file browser.
fn resolve_focus(s: &mut AppState) {
    let restored = s.session_restored_focus.take();
    if !s.buffers.is_empty() {
        // There are open buffers. Honour restored focus if it's Main
        // (the buffer exists to receive it) or Side.
        s.focus = restored.unwrap_or(PanelSlot::Main);
    } else {
        // No buffers — file browser is the only usable panel.
        s.focus = PanelSlot::Side;
    }
}

fn handle_timer(state: &mut AppState, name: &'static str) {
    match name {
        "alert_clear" => {
            state.info = None;
            state.warn = None;
        }
        "undo_flush" => {
            // Handled by the undo_flush_s combinator chain, not here.
            // The timer fires → chain samples state → produces UndoFlushReady.
        }
        _ => {}
    }
}

#[derive(Clone)]
enum Mut {
    ActivateBuffer(BufferId),
    Action(Action),
    Alert {
        info: Option<String>,
        warn: Option<String>,
    },
    BufferOpen {
        buf: BufferState,
        next_id: u64,
        activate: bool,
        notify_hash: String,
        session_restore_done: bool,
    },
    BufferSaved {
        id: BufferId,
        buf: BufferState,
        undo_clear_path: Option<std::path::PathBuf>,
    },
    BufferUpdate(BufferId, BufferState),
    ClipboardText(String),
    ConfigKeys(ConfigFile<Keys>),
    ConfigTheme(ConfigFile<Theme>),
    DirListed(std::path::PathBuf, Vec<led_fs::DirEntry>),
    ForceRedraw(u64),
    Keymap(Arc<Keymap>),
    Resize(u16, u16),
    NotifyEvent {
        path: Option<std::path::PathBuf>,
    },
    SessionOpenFailed {
        path: std::path::PathBuf,
    },
    SessionRestored(Option<led_workspace::RestoredSession>),
    SessionSaved,
    WatchersReady,
    SyncApply {
        buf_id: BufferId,
        doc: Arc<dyn Doc>,
        chain_id: Option<String>,
        last_seen_seq: i64,
    },
    SyncReset {
        buf_id: BufferId,
    },
    Suspend(bool),
    SyntaxUpdate {
        buf_id: BufferId,
        version: u64,
        highlights: Vec<(usize, HighlightSpan)>,
        bracket_pairs: Vec<BracketPair>,
        matching_bracket: Option<(usize, usize)>,
        #[allow(dead_code)]
        indent: Option<String>,
    },
    UndoFlushed {
        buf_id: BufferId,
        chain_id: String,
        last_seen_seq: i64,
    },
    UndoFlushReady {
        buf_id: BufferId,
        flush: led_state::UndoFlush,
    },
    TimerFired(&'static str),
    Workspace(Workspace),
}

impl Mut {
    fn name(&self) -> &'static str {
        match self {
            Mut::ActivateBuffer(_) => "ActivateBuffer",
            Mut::Action(_) => "Action",
            Mut::Alert { .. } => "Alert",
            Mut::BufferOpen { .. } => "BufferOpen",
            Mut::BufferSaved { .. } => "BufferSaved",
            Mut::BufferUpdate(_, _) => "BufferUpdate",
            Mut::ClipboardText(_) => "ClipboardText",
            Mut::ConfigKeys(_) => "ConfigKeys",
            Mut::ConfigTheme(_) => "ConfigTheme",
            Mut::DirListed(_, _) => "DirListed",
            Mut::ForceRedraw(_) => "ForceRedraw",
            Mut::Keymap(_) => "Keymap",
            Mut::Resize(_, _) => "Resize",
            Mut::NotifyEvent { .. } => "NotifyEvent",
            Mut::SessionOpenFailed { .. } => "SessionOpenFailed",
            Mut::SessionRestored(_) => "SessionRestored",
            Mut::SessionSaved => "SessionSaved",
            Mut::WatchersReady => "WatchersReady",
            Mut::SyncApply { .. } => "SyncApply",
            Mut::SyncReset { .. } => "SyncReset",
            Mut::Suspend(_) => "Suspend",
            Mut::SyntaxUpdate { .. } => "SyntaxUpdate",
            Mut::UndoFlushed { .. } => "UndoFlushed",
            Mut::UndoFlushReady { .. } => "UndoFlushReady",
            Mut::TimerFired(_) => "TimerFired",
            Mut::Workspace(_) => "Workspace",
        }
    }

    fn alert(a: Alert) -> Self {
        match a {
            Alert::Info(v) => Mut::Alert {
                info: Some(v),
                warn: None,
            },
            Alert::Warn(v) => Mut::Alert {
                info: None,
                warn: Some(v),
            },
        }
    }
}
