use std::pin::Pin;
use std::sync::Arc;

use led_config_file::{ConfigDir, ConfigFileOut};
use led_core::{AStream, FanoutStreamExt, StreamOpsExt};
use led_state::AppState;
use led_storage::StorageOut;
use led_workspace::StartDir;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

pub struct Derived {
    pub workspace: Pin<Box<dyn AStream<StartDir>>>,
    pub config_file_out: broadcast::Sender<ConfigFileOut>,
    pub storage: Pin<Box<dyn AStream<StorageOut>>>,
}

impl Derived {
    pub fn new(state_tx: &broadcast::Sender<Arc<AppState>>) -> Self {
        let workspace = state_tx
            .latest()
            .map(|s| s.startup.start_dir.clone())
            .dedupe()
            .map(StartDir);

        let config_file_out = state_tx
            .latest()
            .filter_map(|s| {
                s.workspace.as_ref().map(|w| {
                    ConfigFileOut::ConfigDir(ConfigDir {
                        config: w.config.clone(),
                        read_only: !w.primary,
                    })
                })
            })
            // Only emit values when it changes
            .dedupe()
            .broadcast();

        // Placeholder: no storage commands until buffers exist on AppState.
        let storage = tokio_stream::pending();

        Derived {
            workspace: Box::pin(workspace),
            config_file_out,
            storage: Box::pin(storage),
        }
    }
}
