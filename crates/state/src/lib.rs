use std::sync::Arc;

use led_core::Startup;
pub use led_workspace::Workspace;

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub startup: Arc<Startup>,
    pub workspace: Option<Workspace>,
}

impl AppState {
    pub fn new(config: Startup) -> Self {
        Self {
            startup: Arc::new(config),
            ..Default::default()
        }
    }
}
