use std::sync::Arc;

mod actions_of;
mod buffers_of;
mod edit;
mod mov;
mod process_of;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{Action, Alert, BufferId, PanelSlot};
use led_state::{
    AppState, BufferState, Dimensions, EditKind, EntryKind, SaveState, SessionRestorePhase,
};
use led_workspace::Workspace;

use crate::Drivers;
use crate::model::actions_of::actions_of;
use crate::model::buffers_of::buffers_of;
use crate::model::process_of::process_of;

pub fn model(drivers: Drivers, init: AppState) -> Stream<Arc<AppState>> {
    let state: Stream<Arc<AppState>> = Stream::new();

    // ── 1. Derive from hoisted state ──

    let workspace_s = drivers
        .workspace_in
        .map(|ev| match ev {
            led_workspace::WorkspaceIn::Workspace { workspace } => Mut::Workspace(workspace),
            led_workspace::WorkspaceIn::SessionRestored { session } => {
                Mut::SessionRestored(session)
            }
            led_workspace::WorkspaceIn::SessionSaved => Mut::SessionSaved,
            led_workspace::WorkspaceIn::WorkspaceChanged { workspace } => Mut::Workspace(workspace),
        })
        .stream();

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
    let timers_s = drivers.timers_in.map(|t| Mut::TimerFired(t.name)).stream();
    let fs_s = drivers
        .fs_in
        .map(|ev| match ev {
            led_fs::FsIn::DirListed { path, entries } => Mut::DirListed(path, entries),
        })
        .stream();

    workspace_s.forward(&muts);
    keymap_s.forward(&muts);
    actions_s.forward(&muts);
    direct_actions_s.forward(&muts);
    buffers_s.forward(&muts);
    process_s.forward(&muts);
    timers_s.forward(&muts);
    fs_s.forward(&muts);

    // ── 3. Reduce ──

    muts.fold_into(&state, Arc::new(init), |s, m| {
        let mut s = Arc::unwrap_or_clone(s);
        match m {
            Mut::ActivateBuffer(id) => s.active_buffer = Some(id),
            Mut::Action(a) => handle_action(&mut s, a),
            Mut::Alert { info, warn } => {
                s.info = info;
                s.warn = warn;
            }
            Mut::BufferOpen(buf, next_id) => {
                // During session restore, only activate the tab matching
                // session_active_tab_order. Otherwise activate every new buffer.
                let is_restoring = s.session_restore_phase == SessionRestorePhase::Restoring;
                let should_activate = if is_restoring {
                    s.session_active_tab_order == Some(buf.tab_order)
                } else {
                    true
                };
                if should_activate {
                    s.active_buffer = Some(buf.id);
                } else if s.active_buffer.is_none() {
                    s.active_buffer = Some(buf.id);
                }

                // Remove from pending session positions
                if let Some(ref path) = buf.path {
                    s.session_positions.remove(path);
                }

                s.buffers.insert(buf.id, buf);
                s.next_buffer_id = next_id;

                // Check if session restore is complete
                if is_restoring && s.session_positions.is_empty() {
                    s.session_restore_phase = SessionRestorePhase::Done;
                    s.session_active_tab_order = None;
                }
            }
            Mut::BufferUpdate(id, buf) => {
                s.buffers.insert(id, buf);
            }
            Mut::ConfigKeys(v) => s.config_keys = Some(v),
            Mut::DirListed(path, entries) => {
                s.browser.dir_contents.insert(path, entries);
                s.browser.rebuild_entries();
            }
            Mut::ConfigTheme(v) => s.config_theme = Some(v),
            Mut::ForceRedraw(v) => s.force_redraw = v,
            Mut::Keymap(v) => s.keymap = Some(v),
            Mut::Resize(w, h) => {
                s.dims = Some(Dimensions::new(w, h, s.show_side_panel));
            }
            Mut::SessionRestored(session) => {
                handle_session_restored(&mut s, session);
            }
            Mut::SessionSaved => {
                s.session_saved = true;
            }
            Mut::Suspend(v) => s.suspend = v,
            Mut::TimerFired(name) => handle_timer(&mut s, name),
            Mut::Workspace(v) => {
                let root = v.root.clone();
                s.workspace = Some(Arc::new(v));
                // Initialize or refresh browser tree
                s.browser.root = Some(root.clone());
                s.browser.dir_contents.clear();
                let mut dirs_to_list = vec![root];
                dirs_to_list.extend(s.browser.expanded_dirs.iter().cloned());
                s.pending_lists.set(dirs_to_list);
                s.browser.rebuild_entries();
            }
        }
        Arc::new(s)
    });

    state
}

fn handle_action(state: &mut AppState, action: Action) {
    match action {
        // ── UI ──
        Action::ToggleSidePanel => {
            state.show_side_panel = !state.show_side_panel;
            if let Some(ref mut dims) = state.dims {
                dims.show_side_panel = state.show_side_panel;
            }
        }
        Action::ToggleFocus => {
            state.focus = match state.focus {
                PanelSlot::Main => PanelSlot::Side,
                PanelSlot::Side => PanelSlot::Main,
                other => other,
            };
        }
        Action::Quit => {
            state.quit = true;
        }
        Action::Suspend => {
            state.suspend = true;
        }

        // ── Resize ──
        Action::Resize(w, h) => {
            state.dims = Some(Dimensions::new(w, h, state.show_side_panel));
        }

        // ── Movement (routed by focus) ──
        Action::MoveUp
        | Action::MoveDown
        | Action::PageUp
        | Action::PageDown
        | Action::FileStart
        | Action::FileEnd => {
            if state.focus == PanelSlot::Side {
                handle_browser_nav(state, &action);
            } else {
                handle_editor_movement(state, &action);
            }
        }
        Action::MoveLeft => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::move_left(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::MoveRight => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::move_right(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::LineStart => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::line_start(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::LineEnd => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::line_end(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),

        // ── Browser ──
        Action::ExpandDir => handle_browser_expand(state),
        Action::CollapseDir => handle_browser_collapse(state),
        Action::CollapseAll => handle_browser_collapse_all(state),
        Action::OpenSelected => handle_browser_open(state),

        // ── Editing ──
        Action::InsertChar(ch) => with_buf(state, |buf, dims| {
            maybe_close_group(buf, EditKind::Insert, ch);
            let (doc, r, c, _) = edit::insert_char(buf, ch);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            buf.last_edit_kind = Some(EditKind::Insert);
        }),
        Action::InsertNewline => with_buf(state, |buf, dims| {
            close_group_on_move(buf);
            let (doc, r, c, _) = edit::insert_newline(buf);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
        }),
        Action::InsertTab => with_buf(state, |buf, dims| {
            maybe_close_group(buf, EditKind::Insert, ' ');
            let (doc, r, c, _) = edit::insert_tab(buf, dims);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            buf.last_edit_kind = Some(EditKind::Insert);
        }),
        Action::DeleteBackward => with_buf(state, |buf, dims| {
            if buf.last_edit_kind != Some(EditKind::Delete) {
                buf.doc = buf.doc.close_undo_group();
            }
            if let Some((doc, r, c, _)) = edit::delete_backward(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
                buf.last_edit_kind = Some(EditKind::Delete);
            }
        }),
        Action::DeleteForward => with_buf(state, |buf, dims| {
            if buf.last_edit_kind != Some(EditKind::Delete) {
                buf.doc = buf.doc.close_undo_group();
            }
            if let Some((doc, r, c, _)) = edit::delete_forward(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
                buf.last_edit_kind = Some(EditKind::Delete);
            }
        }),
        Action::KillLine => with_buf(state, |buf, dims| {
            close_group_on_move(buf);
            if let Some((doc, r, c, _)) = edit::kill_line(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            }
        }),

        // ── Undo / Redo ──
        Action::Undo => with_buf(state, |buf, dims| {
            close_group_on_move(buf);
            if let Some((doc, cursor)) = buf.doc.undo() {
                let row = doc.char_to_line(cursor);
                let col = cursor - doc.line_to_char(row);
                buf.doc = doc;
                buf.cursor_row = row;
                buf.cursor_col = col;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            }
        }),
        Action::Redo => with_buf(state, |buf, dims| {
            close_group_on_move(buf);
            if let Some((doc, cursor)) = buf.doc.redo() {
                let row = doc.char_to_line(cursor);
                let col = cursor - doc.line_to_char(row);
                buf.doc = doc;
                buf.cursor_row = row;
                buf.cursor_col = col;
                buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            }
        }),

        // ── Save ──
        Action::Save => {
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buffers.get_mut(&id) {
                    close_group_on_move(buf);
                    buf.save_state = SaveState::Saving;
                }
            }
            state.save_request.set(());
        }

        // ── Tabs ──
        Action::NextTab => cycle_tab(state, 1),
        Action::PrevTab => cycle_tab(state, -1),
        Action::KillBuffer => kill_buffer(state),

        _ => {}
    }
}

fn cycle_tab(state: &mut AppState, direction: i32) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    let mut tabs: Vec<(BufferId, usize)> = state
        .buffers
        .iter()
        .map(|(id, buf)| (*id, buf.tab_order))
        .collect();
    tabs.sort_by_key(|&(_, order)| order);

    let Some(pos) = tabs.iter().position(|&(id, _)| id == active_id) else {
        return;
    };
    let len = tabs.len() as i32;
    let next = ((pos as i32 + direction).rem_euclid(len)) as usize;
    state.active_buffer = Some(tabs[next].0);
}

fn kill_buffer(state: &mut AppState) {
    let Some(active_id) = state.active_buffer else {
        return;
    };
    let Some(buf) = state.buffers.get(&active_id) else {
        return;
    };

    // Don't kill dirty buffers (no modal yet)
    if buf.doc.dirty() {
        state.warn = Some("Buffer has unsaved changes".into());
        return;
    }

    let filename = buf
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let killed_order = buf.tab_order;

    // Find next buffer to activate (next by tab_order, wrapping)
    let mut tabs: Vec<(BufferId, usize)> = state
        .buffers
        .iter()
        .filter(|(id, _)| **id != active_id)
        .map(|(id, buf)| (*id, buf.tab_order))
        .collect();
    tabs.sort_by_key(|&(_, order)| order);

    let next_active = tabs
        .iter()
        .find(|&&(_, order)| order > killed_order)
        .or_else(|| tabs.last())
        .map(|&(id, _)| id);

    state.buffers.remove(&active_id);
    state.active_buffer = next_active;

    if state.buffers.is_empty() {
        state.focus = PanelSlot::Side;
    }

    state.info = Some(format!("Killed {filename}"));
}

fn handle_session_restored(state: &mut AppState, session: Option<led_workspace::RestoredSession>) {
    use led_workspace::SessionRestorePhase;

    match session {
        Some(session) => {
            state.session_restore_phase = SessionRestorePhase::Restoring;
            state.session_active_tab_order = Some(session.active_tab_order);
            state.show_side_panel = session.show_side_panel;
            for buf in session.buffers {
                state.session_positions.insert(buf.file_path.clone(), buf);
            }
            let paths: Vec<_> = state.session_positions.keys().cloned().collect();
            state.pending_session_opens.set(paths);
        }
        None => {
            state.session_restore_phase = SessionRestorePhase::Done;
        }
    }
}

fn handle_timer(state: &mut AppState, name: &'static str) {
    match name {
        "alert_clear" => {
            state.info = None;
            state.warn = None;
        }
        _ => {}
    }
}

// ── Editor movement (focus = Main) ──

fn handle_editor_movement(state: &mut AppState, action: &Action) {
    match action {
        Action::MoveUp => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::move_up(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::MoveDown => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::move_down(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::PageUp => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::page_up(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::PageDown => with_buf(state, |buf, dims| {
            let (r, c, a) = mov::page_down(buf, dims);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::FileStart => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::file_start();
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        Action::FileEnd => with_buf(state, |buf, dims| {
            let (r, c, _) = mov::file_end(&*buf.doc);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = mov::reset_affinity(buf, dims);
            close_group_on_move(buf);
        }),
        _ => {}
    }
}

// ── Browser navigation (focus = Side) ──

fn handle_browser_nav(state: &mut AppState, action: &Action) {
    let len = state.browser.entries.len();
    if len == 0 {
        return;
    }
    let height = state.dims.map_or(20, |d| d.buffer_height());
    match action {
        Action::MoveUp => {
            state.browser.selected = state.browser.selected.saturating_sub(1);
        }
        Action::MoveDown => {
            state.browser.selected = (state.browser.selected + 1).min(len - 1);
        }
        Action::PageUp => {
            state.browser.selected = state.browser.selected.saturating_sub(height);
        }
        Action::PageDown => {
            state.browser.selected = (state.browser.selected + height).min(len - 1);
        }
        Action::FileStart => {
            state.browser.selected = 0;
        }
        Action::FileEnd => {
            state.browser.selected = len - 1;
        }
        _ => {}
    }
    // Keep selection visible
    if state.browser.selected < state.browser.scroll_offset {
        state.browser.scroll_offset = state.browser.selected;
    } else if state.browser.selected >= state.browser.scroll_offset + height {
        state.browser.scroll_offset = state.browser.selected + 1 - height;
    }
}

fn handle_browser_expand(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    if !matches!(entry.kind, EntryKind::Directory { expanded: false }) {
        return;
    }
    let path = entry.path.clone();
    state.browser.expanded_dirs.insert(path.clone());
    if state.browser.dir_contents.contains_key(&path) {
        state.browser.rebuild_entries();
    } else {
        state.pending_lists.set(vec![path]);
    }
}

fn handle_browser_collapse(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    let collapse_path = match &entry.kind {
        EntryKind::Directory { expanded: true } => entry.path.clone(),
        _ => {
            // File or collapsed dir — collapse parent
            match entry.path.parent() {
                Some(parent) if state.browser.expanded_dirs.contains(parent) => {
                    parent.to_path_buf()
                }
                _ => return,
            }
        }
    };
    state.browser.expanded_dirs.remove(&collapse_path);
    state.browser.rebuild_entries();
    // Move selection to the collapsed directory
    if let Some(pos) = state
        .browser
        .entries
        .iter()
        .position(|e| e.path == collapse_path)
    {
        state.browser.selected = pos;
    }
}

fn handle_browser_collapse_all(state: &mut AppState) {
    state.browser.expanded_dirs.clear();
    state.browser.rebuild_entries();
    state.browser.selected = 0;
    state.browser.scroll_offset = 0;
}

fn handle_browser_open(state: &mut AppState) {
    let Some(entry) = state.browser.entries.get(state.browser.selected) else {
        return;
    };
    match &entry.kind {
        EntryKind::File => {
            state.pending_open.set(Some(entry.path.clone()));
            state.focus = PanelSlot::Main;
        }
        EntryKind::Directory { expanded } => {
            if *expanded {
                handle_browser_collapse(state);
            } else {
                handle_browser_expand(state);
            }
        }
    }
}

/// Run `f` on the active buffer, then ensure cursor stays visible.
fn with_buf(state: &mut AppState, f: impl FnOnce(&mut BufferState, &Dimensions)) {
    let dims = match state.dims {
        Some(d) => d,
        None => return,
    };
    if let Some(id) = state.active_buffer {
        if let Some(buf) = state.buffers.get_mut(&id) {
            f(buf, &dims);
            let (sr, ssl) = mov::adjust_scroll(buf, &dims);
            buf.scroll_row = sr;
            buf.scroll_sub_line = ssl;
            // Track save state: transition to Modified when doc becomes dirty
            if buf.doc.dirty() && buf.save_state == SaveState::Clean {
                buf.save_state = SaveState::Modified;
            }
        }
    }
}

/// Close undo group and clear edit kind tracking.
fn close_group_on_move(buf: &mut BufferState) {
    if buf.last_edit_kind.is_some() {
        buf.doc = buf.doc.close_undo_group();
        buf.last_edit_kind = None;
    }
}

/// Close undo group if the edit kind changes or on word boundary (whitespace after non-whitespace).
fn maybe_close_group(buf: &mut BufferState, kind: EditKind, ch: char) {
    if buf.last_edit_kind != Some(kind) {
        buf.doc = buf.doc.close_undo_group();
    } else if kind == EditKind::Insert {
        // Word boundary: whitespace after non-whitespace
        if ch.is_whitespace() {
            let line = buf.doc.line(buf.cursor_row);
            let prev = line.chars().nth(buf.cursor_col.saturating_sub(1));
            if let Some(p) = prev {
                if !p.is_whitespace() {
                    buf.doc = buf.doc.close_undo_group();
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Mut {
    ActivateBuffer(BufferId),
    Action(Action),
    Alert {
        info: Option<String>,
        warn: Option<String>,
    },
    BufferOpen(BufferState, u64),
    BufferUpdate(BufferId, BufferState),
    ConfigKeys(ConfigFile<Keys>),
    ConfigTheme(ConfigFile<Theme>),
    DirListed(std::path::PathBuf, Vec<led_fs::DirEntry>),
    ForceRedraw(u64),
    Keymap(Arc<Keymap>),
    Resize(u16, u16),
    SessionRestored(Option<led_workspace::RestoredSession>),
    SessionSaved,
    Suspend(bool),
    TimerFired(&'static str),
    Workspace(Workspace),
}

impl Mut {
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
