use std::sync::Arc;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::theme::Theme;
use led_core::{PanelSlot, Startup};
pub use led_workspace::Workspace;

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub startup: Arc<Startup>,
    pub workspace: Option<Workspace>,
    pub config_keys: Option<ConfigFile<Keys>>,
    pub config_theme: Option<ConfigFile<Theme>>,
    pub keymap: Option<Arc<Keymap>>,
    pub focus: PanelSlot,
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
