use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::theme::Theme;
use led_core::{BufferId, Doc, PanelSlot, Startup};
pub use led_workspace::Workspace;

#[derive(Clone)]
pub struct BufferState {
    pub id: BufferId,
    pub doc: Box<dyn Doc>,
    pub path: Option<PathBuf>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_row: usize,
    pub tab_order: usize,
}

impl fmt::Debug for BufferState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferState")
            .field("id", &self.id)
            .field("path", &self.path)
            .field("cursor_row", &self.cursor_row)
            .field("cursor_col", &self.cursor_col)
            .field("scroll_row", &self.scroll_row)
            .field("tab_order", &self.tab_order)
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub startup: Arc<Startup>,
    pub workspace: Option<Workspace>,
    pub config_keys: Option<ConfigFile<Keys>>,
    pub config_theme: Option<ConfigFile<Theme>>,
    pub keymap: Option<Arc<Keymap>>,
    pub focus: PanelSlot,
    pub show_side_panel: bool,
    pub viewport: (u16, u16),
    pub quit: bool,
    pub suspend: bool,
    pub force_redraw: u64,
    pub info: Option<String>,
    pub warn: Option<String>,
    pub buffers: HashMap<BufferId, BufferState>,
    pub active_buffer: Option<BufferId>,
    pub next_buffer_id: u64,
}

impl AppState {
    pub fn new(startup: Startup) -> Self {
        Self {
            startup: Arc::new(startup),
            show_side_panel: true,
            ..Default::default()
        }
    }
}
