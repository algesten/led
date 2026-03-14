use std::sync::Arc;

use led_config_file::{ConfigDir, ConfigFileOut};
use led_core::Startup;
use led_core::rx::Stream;
use led_docstore::DocStoreOut;
use led_state::AppState;

pub struct Derived {
    pub ui: Stream<Arc<AppState>>,
    pub workspace_out: Stream<Arc<Startup>>,
    pub docstore_out: Stream<DocStoreOut>,
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

    let docstore_out = state
        .filter_map(|s| s.startup.arg_path.clone())
        .dedupe()
        .map(|path| DocStoreOut::Open { path })
        .stream();

    Derived {
        ui,
        workspace_out,
        docstore_out,
        config_file_out,
    }
}
