use std::sync::Arc;

use led_core::Config;
pub use led_workspace::Workspace;

#[derive(Clone, Default)]
pub struct AppState {
    pub config: Arc<Config>,
    pub workspace: Option<Workspace>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            ..Default::default()
        }
    }
}
