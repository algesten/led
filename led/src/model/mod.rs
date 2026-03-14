use std::sync::Arc;

mod actions_of;
mod buffers_of;
mod edit;
mod process_of;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{Action, Alert, BufferId, PanelSlot};
use led_state::{AppState, BufferState, EditKind, SaveState};
use led_workspace::Workspace;

use crate::Drivers;
use crate::model::actions_of::actions_of;
use crate::model::buffers_of::buffers_of;
use crate::model::process_of::process_of;

pub fn model(drivers: Drivers, init: AppState) -> Stream<Arc<AppState>> {
    let state: Stream<Arc<AppState>> = Stream::new();

    // ── 1. Derive from hoisted state ──

    let workspace_s = drivers.workspace_in.map(|w| Mut::Workspace(w)).stream();

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

    workspace_s.forward(&muts);
    keymap_s.forward(&muts);
    actions_s.forward(&muts);
    direct_actions_s.forward(&muts);
    buffers_s.forward(&muts);
    process_s.forward(&muts);

    // ── 3. Reduce ──

    muts.fold_into(&state, Arc::new(init), |s, m| {
        let mut s = Arc::unwrap_or_clone(s);
        match m {
            Mut::Action(a) => handle_action(&mut s, a),
            Mut::Alert { info, warn } => {
                s.info = info;
                s.warn = warn;
            }
            Mut::BufferOpen(buf, next_id) => {
                s.active_buffer = Some(buf.id);
                s.buffers.insert(buf.id, buf);
                s.next_buffer_id = next_id;
            }
            Mut::BufferUpdate(id, buf) => {
                s.buffers.insert(id, buf);
            }
            Mut::ConfigKeys(v) => s.config_keys = Some(v),
            Mut::ConfigTheme(v) => s.config_theme = Some(v),
            Mut::ForceRedraw(v) => s.force_redraw = v,
            Mut::Keymap(v) => s.keymap = Some(v),
            Mut::Resize(w, h) => s.viewport = (w, h),
            Mut::Suspend(v) => s.suspend = v,
            Mut::Workspace(v) => s.workspace = Some(Arc::new(v)),
        }
        Arc::new(s)
    });

    state
}

fn handle_action(state: &mut AppState, action: Action) {
    // Buffer area = terminal height minus status bar (1) and tab bar (1)
    let viewport_height = (state.viewport.1 as usize).saturating_sub(2);

    match action {
        // ── UI ──
        Action::ToggleSidePanel => {
            state.show_side_panel = !state.show_side_panel;
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

        // ── Movement ──
        Action::MoveUp => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::move_up(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::MoveDown => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::move_down(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::MoveLeft => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::move_left(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::MoveRight => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::move_right(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::LineStart => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::line_start(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::LineEnd => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::line_end(buf);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::PageUp => with_buf(state, viewport_height, |buf, h| {
            let (r, c, a) = edit::page_up(buf, h);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::PageDown => with_buf(state, viewport_height, |buf, h| {
            let (r, c, a) = edit::page_down(buf, h);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::FileStart => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::file_start();
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),
        Action::FileEnd => with_buf(state, viewport_height, |buf, _| {
            let (r, c, a) = edit::file_end(&*buf.doc);
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            close_group_on_move(buf);
        }),

        // ── Editing ──
        Action::InsertChar(ch) => with_buf(state, viewport_height, |buf, _| {
            maybe_close_group(buf, EditKind::Insert, ch);
            let (doc, r, c, a) = edit::insert_char(buf, ch);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            buf.last_edit_kind = Some(EditKind::Insert);
        }),
        Action::InsertNewline => with_buf(state, viewport_height, |buf, _| {
            close_group_on_move(buf);
            let (doc, r, c, a) = edit::insert_newline(buf);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
        }),
        Action::InsertTab => with_buf(state, viewport_height, |buf, _| {
            maybe_close_group(buf, EditKind::Insert, ' ');
            let (doc, r, c, a) = edit::insert_tab(buf);
            buf.doc = doc;
            buf.cursor_row = r;
            buf.cursor_col = c;
            buf.cursor_col_affinity = a;
            buf.last_edit_kind = Some(EditKind::Insert);
        }),
        Action::DeleteBackward => with_buf(state, viewport_height, |buf, _| {
            if buf.last_edit_kind != Some(EditKind::Delete) {
                buf.doc = buf.doc.close_undo_group();
            }
            if let Some((doc, r, c, a)) = edit::delete_backward(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = a;
                buf.last_edit_kind = Some(EditKind::Delete);
            }
        }),
        Action::DeleteForward => with_buf(state, viewport_height, |buf, _| {
            if buf.last_edit_kind != Some(EditKind::Delete) {
                buf.doc = buf.doc.close_undo_group();
            }
            if let Some((doc, r, c, a)) = edit::delete_forward(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = a;
                buf.last_edit_kind = Some(EditKind::Delete);
            }
        }),
        Action::KillLine => with_buf(state, viewport_height, |buf, _| {
            close_group_on_move(buf);
            if let Some((doc, r, c, a)) = edit::kill_line(buf) {
                buf.doc = doc;
                buf.cursor_row = r;
                buf.cursor_col = c;
                buf.cursor_col_affinity = a;
            }
        }),

        // ── Undo / Redo ──
        Action::Undo => with_buf(state, viewport_height, |buf, _| {
            close_group_on_move(buf);
            if let Some((doc, cursor)) = buf.doc.undo() {
                let row = doc.char_to_line(cursor);
                let col = cursor - doc.line_to_char(row);
                buf.doc = doc;
                buf.cursor_row = row;
                buf.cursor_col = col;
                buf.cursor_col_affinity = col;
            }
        }),
        Action::Redo => with_buf(state, viewport_height, |buf, _| {
            close_group_on_move(buf);
            if let Some((doc, cursor)) = buf.doc.redo() {
                let row = doc.char_to_line(cursor);
                let col = cursor - doc.line_to_char(row);
                buf.doc = doc;
                buf.cursor_row = row;
                buf.cursor_col = col;
                buf.cursor_col_affinity = col;
            }
        }),

        // ── Resize ──
        Action::Resize(w, h) => {
            state.viewport = (w, h);
        }

        // ── Save ──
        Action::Save => {
            if let Some(id) = state.active_buffer {
                if let Some(buf) = state.buffers.get_mut(&id) {
                    close_group_on_move(buf);
                    buf.save_state = SaveState::Saving;
                }
            }
            state.save_request += 1;
        }

        _ => {}
    }
}

/// Run `f` on the active buffer, then ensure cursor stays visible.
fn with_buf(state: &mut AppState, viewport_height: usize, f: impl FnOnce(&mut BufferState, usize)) {
    if let Some(id) = state.active_buffer {
        if let Some(buf) = state.buffers.get_mut(&id) {
            f(buf, viewport_height);
            buf.scroll_row =
                edit::ensure_cursor_visible(buf.cursor_row, buf.scroll_row, viewport_height);
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
    Action(Action),
    Alert {
        info: Option<String>,
        warn: Option<String>,
    },
    BufferOpen(BufferState, u64),
    BufferUpdate(BufferId, BufferState),
    ConfigKeys(ConfigFile<Keys>),
    ConfigTheme(ConfigFile<Theme>),
    ForceRedraw(u64),
    Keymap(Arc<Keymap>),
    Resize(u16, u16),
    Suspend(bool),
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
