use std::sync::Arc;

use led_config_file::ConfigFile;
use led_core::Startup;
use led_core::keys::Keys;
use led_core::theme::Theme;
pub use led_workspace::Workspace;

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub startup: Arc<Startup>,
    pub workspace: Option<Workspace>,
    pub config_keys: Option<ConfigFile<Keys>>,
    pub config_theme: Option<ConfigFile<Theme>>,
    pub info: Option<String>,
    pub warn: Option<String>,
}

impl AppState {
    pub fn new(startup: Startup) -> Self {
        Self {
            startup: Arc::new(startup),
            ..Default::default()
        }
    }
}
