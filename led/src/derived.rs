use std::sync::Arc;

use led_config_file::{ConfigDir, ConfigFileOut};
use led_core::Startup;
use led_core::rx::Stream;
use led_state::AppState;
use led_storage::StorageOut;

pub struct Derived {
    pub ui: Stream<Arc<AppState>>,
    pub workspace_out: Stream<Arc<Startup>>,
    pub storage_out: Stream<StorageOut>,
    pub config_file_out: Stream<ConfigFileOut>,
}

pub fn derived(state: Stream<Arc<AppState>>) -> Derived {
    let ui = state.map(|s| s).stream();
    let workspace_out = state.map(|s| s.startup.clone()).dedupe().stream();

    let config_file_out = state
        .filter_map(|s| s.workspace.clone())
        .dedupe()
        .map(|w| ConfigDir {
            config: w.config.clone(),
            read_only: !w.primary,
        })
        .map(ConfigFileOut::ConfigDir)
        .stream();

    let storage_out = state
        .filter_map(|s| s.startup.arg_path.clone())
        .dedupe()
        .map(StorageOut::Open)
        .stream();

    Derived {
        ui,
        workspace_out,
        storage_out,
        config_file_out,
    }
}
