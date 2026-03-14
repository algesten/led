use std::sync::Arc;

mod actions_of;
mod buffers_of;
mod process_of;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{Action, Alert, BufferId, PanelSlot};
use led_state::{AppState, BufferState};
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

    workspace_s.forward(&muts);
    keymap_s.forward(&muts);
    actions_s.forward(&muts);
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
    match action {
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
        _ => {}
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
