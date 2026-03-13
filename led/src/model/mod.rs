use std::sync::Arc;

mod actions_of;
mod alerts_of;
mod keymap_of;
mod process_of;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::theme::Theme;
use led_core::{AStream, Action, FanoutStreamExt, PanelSlot, StreamOpsExt};
use led_state::{AppState, Workspace};
use led_storage::StorageIn;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::Drivers;
use crate::model::actions_of::{TerminalEvent, actions_of};
use crate::model::alerts_of::alerts_of;
use crate::model::keymap_of::keymap_of;
use crate::model::process_of::process_of;

pub fn model(drivers: Drivers, init: AppState) -> impl AStream<Arc<AppState>> {
    let (state_tx, _rx) = broadcast::channel(10);

    let process_s = process_of(state_tx.one_by_one());

    let terminal_s = actions_of(state_tx.one_by_one(), drivers.input);

    let workspace_s = drivers.workspace.map(|v| Mut::Workspace(v));

    let (config_keys_s, config_keys_alert_s) = {
        let (o, e) = drivers.config_file_keys.split_result();
        (o.map(Mut::ConfigKeys), e)
    };
    let (config_theme_s, config_theme_alert_s) = {
        let (o, e) = drivers.config_file_theme.split_result();
        (o.map(Mut::ConfigTheme), e)
    };

    let (storage_s, storage_alert_s) = {
        let (o, e) = drivers.storage.split_result();
        (o.map(Mut::Storage), e)
    };

    let (keymap_s, keymap_alert_s) = {
        let keys_s = state_tx
            .latest()
            .filter_map(|s| Some(s.config_keys.as_ref()?.file.clone()))
            .dedupe();
        let (o, e) = keymap_of(keys_s).split_result();
        (o.map(Mut::Keymap), e)
    };

    let alert_s = config_keys_alert_s
        .or(config_theme_alert_s)
        .or(keymap_alert_s)
        .or(storage_alert_s);
    let (alert_info_s, alert_warn_s) = alerts_of(alert_s);

    let terminal_mut_s = terminal_s.map(|ev| match ev {
        TerminalEvent::Action(a) => Mut::Action(a),
        TerminalEvent::Resize(w, h) => Mut::Resize(w, h),
    });

    workspace_s
        .or(process_s)
        .or(config_keys_s)
        .or(keymap_s)
        .or(config_theme_s)
        .or(storage_s)
        .or(alert_info_s)
        .or(alert_warn_s)
        .or(terminal_mut_s)
        //
        .inspect(|m| log::trace!("{:#?}", m))
        .reduce(init, |mut s, m| {
            match m {
                Mut::Info(v) => s.info = v,
                Mut::Warn(v) => s.warn = v,
                Mut::ForceRedraw(v) => s.force_redraw = v,
                Mut::Suspend(v) => s.suspend = v,
                Mut::Keymap(v) => s.keymap = Some(v),
                Mut::Action(a) => handle_action(&mut s, a),
                Mut::Resize(w, h) => s.viewport = (w, h),
                Mut::Workspace(v) => s.workspace = Some(v),
                Mut::ConfigKeys(v) => s.config_keys = Some(v),
                Mut::ConfigTheme(v) => s.config_theme = Some(v),
                Mut::Storage(_v) => { /* placeholder until BufferState exists */ }
            }
            s
        })
        .inspect(|a| log::trace!("{:#?}", a))
        .map(|a| Arc::new(a))
        .inspect(move |a| {
            state_tx.send(a.clone()).unwrap();
        })
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

#[derive(Debug)]
enum Mut {
    ForceRedraw(u64),
    Suspend(bool),
    Action(Action),
    Resize(u16, u16),
    Storage(StorageIn),
    Keymap(Arc<Keymap>),
    Workspace(Workspace),
    Info(Option<String>),
    Warn(Option<String>),
    ConfigKeys(ConfigFile<Keys>),
    ConfigTheme(ConfigFile<Theme>),
}
