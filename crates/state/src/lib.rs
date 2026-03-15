use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use led_config_file::ConfigFile;
use led_core::keys::{Keymap, Keys};
use led_core::theme::Theme;
use led_core::{BufferId, Doc, DocId, PanelSlot, Startup};
pub use led_workspace::Workspace;

#[derive(Debug, Clone, Copy)]
pub struct Dimensions {
    // ── Inputs ──
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub show_side_panel: bool,

    // ── Configurable base values ──
    pub side_panel_width: u16,
    pub min_editor_width: u16,
    pub status_bar_height: u16,
    pub tab_bar_height: u16,
    pub gutter_width: u16,
    pub scroll_margin: usize,
    pub tab_stop: usize,
}

impl Dimensions {
    pub fn new(viewport_width: u16, viewport_height: u16, show_side_panel: bool) -> Self {
        Self {
            viewport_width,
            viewport_height,
            show_side_panel,
            side_panel_width: 25,
            min_editor_width: 25,
            status_bar_height: 1,
            tab_bar_height: 1,
            gutter_width: 2,
            scroll_margin: 3,
            tab_stop: 4,
        }
    }

    /// Does the side panel fit?
    pub fn side_panel_visible(&self) -> bool {
        self.show_side_panel && self.viewport_width > self.side_panel_width + self.min_editor_width
    }

    /// Actual side panel width (0 if hidden)
    pub fn side_width(&self) -> u16 {
        if self.side_panel_visible() {
            self.side_panel_width
        } else {
            0
        }
    }

    /// Width available for the editor area (everything right of side panel)
    pub fn editor_width(&self) -> u16 {
        self.viewport_width.saturating_sub(self.side_width())
    }

    /// Height of the buffer content area (viewport minus status bar and tab bar)
    pub fn buffer_height(&self) -> usize {
        (self.viewport_height as usize)
            .saturating_sub(self.status_bar_height as usize)
            .saturating_sub(self.tab_bar_height as usize)
    }

    /// Width of the text content area (editor minus gutter)
    pub fn text_width(&self) -> usize {
        (self.editor_width() as usize).saturating_sub(self.gutter_width as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    Insert,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveState {
    #[default]
    Clean,
    Modified,
    Saving,
}

#[derive(Clone)]
pub struct BufferState {
    pub id: BufferId,
    pub doc_id: DocId,
    pub doc: Arc<dyn Doc>,
    pub path: Option<PathBuf>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_col_affinity: usize,
    pub scroll_row: usize,
    pub scroll_sub_line: usize,
    pub tab_order: usize,
    pub last_edit_kind: Option<EditKind>,
    pub save_state: SaveState,
}

impl fmt::Debug for BufferState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferState")
            .field("id", &self.id)
            .field("doc_id", &self.doc_id)
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
    pub workspace: Option<Arc<Workspace>>,
    pub config_keys: Option<ConfigFile<Keys>>,
    pub config_theme: Option<ConfigFile<Theme>>,
    pub keymap: Option<Arc<Keymap>>,
    pub focus: PanelSlot,
    pub show_side_panel: bool,
    pub dims: Option<Dimensions>,
    pub quit: bool,
    pub suspend: bool,
    pub force_redraw: u64,
    pub info: Option<String>,
    pub warn: Option<String>,
    pub buffers: HashMap<BufferId, BufferState>,
    pub active_buffer: Option<BufferId>,
    pub next_buffer_id: u64,
    pub save_request: u64,
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
