use std::sync::Arc;

use led_config_file::ConfigFile;
use led_core::keys::Keys;
use led_core::theme::Theme;
use led_core::{AStream, StreamOpsExt};
use led_state::{AppState, Workspace};
use tokio_stream::StreamExt;

use crate::Drivers;
use crate::model::alerts::all_alerts;

mod alerts;

pub fn model(drivers: Drivers, init: AppState) -> impl AStream<Arc<AppState>> {
    let workspace_s = drivers.workspace.map(|v| Mut::Workspace(v));

    let (config_keys_s, config_keys_alert_s) = {
        let (o, e) = drivers.config_file_keys.split_result();
        (o.map(Mut::ConfigKeys), e)
    };
    let (config_theme_s, config_theme_alert_s) = {
        let (o, e) = drivers.config_file_theme.split_result();
        (o.map(Mut::ConfigTheme), e)
    };

    let alert_s = config_keys_alert_s.or(config_theme_alert_s);
    let (alert_info_s, alert_warn_s) = all_alerts(alert_s);

    workspace_s
        .or(config_keys_s)
        .or(config_theme_s)
        .or(alert_info_s)
        .or(alert_warn_s)
        //
        .inspect(|m| log::trace!("{:#?}", m))
        .reduce(init, |s, m| match m {
            Mut::Workspace(v) => s.workspace = Some(v),
            Mut::ConfigKeys(v) => s.config_keys = Some(v),
            Mut::ConfigTheme(v) => s.config_theme = Some(v),
            Mut::Info(v) => s.info = v,
            Mut::Warn(v) => s.warn = v,
        })
        .inspect(|a| log::trace!("{:#?}", a))
        .map(|state| Arc::new(state))
}

#[derive(Debug)]
enum Mut {
    Workspace(Workspace),
    ConfigKeys(ConfigFile<Keys>),
    ConfigTheme(ConfigFile<Theme>),
    Info(Option<String>),
    Warn(Option<String>),
}
