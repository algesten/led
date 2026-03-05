use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

// ---------------------------------------------------------------------------
// Tree types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory { expanded: bool },
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub depth: usize,
    pub kind: EntryKind,
}

// ---------------------------------------------------------------------------
// FileBrowser
// ---------------------------------------------------------------------------

pub struct FileBrowser {
    pub root: PathBuf,
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    expanded_dirs: HashSet<PathBuf>,
}

impl FileBrowser {
    pub fn new(root: PathBuf) -> Self {
        let mut browser = Self {
            root,
            entries: Vec::new(),
            selected: 0,
            expanded_dirs: HashSet::new(),
        };
        browser.rebuild();
        browser
    }

    pub fn rebuild(&mut self) {
        self.entries.clear();
        self.walk_dir(&self.root.clone(), 0);
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }

    fn walk_dir(&mut self, dir: &PathBuf, depth: usize) {
        let Ok(read_dir) = fs::read_dir(dir) else {
            return;
        };

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.starts_with('.') {
                continue;
            }

            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }

        dirs.sort();
        files.sort();

        for path in dirs {
            let expanded = self.expanded_dirs.contains(&path);
            self.entries.push(TreeEntry {
                path: path.clone(),
                depth,
                kind: EntryKind::Directory { expanded },
            });
            if expanded {
                self.walk_dir(&path, depth + 1);
            }
        }

        for path in files {
            self.entries.push(TreeEntry {
                path,
                depth,
                kind: EntryKind::File,
            });
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.selected = self.selected.saturating_sub(page_size);
    }

    pub fn page_down(&mut self, page_size: usize) {
        self.selected = (self.selected + page_size).min(self.entries.len().saturating_sub(1));
    }

    pub fn open_selected(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?.clone();
        match entry.kind {
            EntryKind::File => Some(entry.path),
            EntryKind::Directory { expanded } => {
                if expanded {
                    self.expanded_dirs.remove(&entry.path);
                } else {
                    self.expanded_dirs.insert(entry.path);
                }
                self.rebuild();
                None
            }
        }
    }

    pub fn expand_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        if matches!(entry.kind, EntryKind::Directory { expanded: false }) {
            let path = entry.path.clone();
            self.expanded_dirs.insert(path);
            self.rebuild();
        }
    }

    pub fn collapse_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return;
        };

        match &entry.kind {
            EntryKind::Directory { expanded: true } => {
                self.expanded_dirs.remove(&entry.path);
                self.rebuild();
            }
            _ => {
                if let Some(parent) = entry.path.parent() {
                    let parent_path = parent.to_path_buf();
                    if parent_path != self.root {
                        self.expanded_dirs.remove(&parent_path);
                        self.rebuild();
                        for (i, e) in self.entries.iter().enumerate() {
                            if e.path == parent_path {
                                self.selected = i;
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn expanded_dirs(&self) -> &HashSet<PathBuf> {
        &self.expanded_dirs
    }

    pub fn set_expanded_dirs(&mut self, dirs: HashSet<PathBuf>) {
        self.expanded_dirs = dirs;
        self.rebuild();
    }

    pub fn reveal(&mut self, path: &std::path::Path) {
        let mut ancestors = Vec::new();
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir == self.root {
                break;
            }
            ancestors.push(dir.to_path_buf());
            current = dir.parent();
        }
        for dir in ancestors {
            self.expanded_dirs.insert(dir);
        }
        self.rebuild();
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.path == path {
                self.selected = i;
                return;
            }
        }
    }

    pub fn display_name(entry: &TreeEntry) -> String {
        let name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let indent = "  ".repeat(entry.depth);

        match &entry.kind {
            EntryKind::Directory { expanded: true } => format!("{indent}\u{25bd} {name}"),
            EntryKind::Directory { expanded: false } => format!("{indent}\u{25b7} {name}"),
            EntryKind::File => format!("{indent}  {name}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Component implementation
// ---------------------------------------------------------------------------

impl Component for FileBrowser {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn panel_claims(&self) -> &[PanelClaim] {
        &[PanelClaim {
            slot: PanelSlot::Side,
            priority: 10,
        }]
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::MoveUp => {
                self.move_up();
                vec![]
            }
            Action::MoveDown => {
                self.move_down();
                vec![]
            }
            Action::PageUp => {
                self.page_up(ctx.viewport_height);
                vec![]
            }
            Action::PageDown => {
                self.page_down(ctx.viewport_height);
                vec![]
            }
            Action::ExpandDir => {
                self.expand_selected();
                vec![]
            }
            Action::CollapseDir => {
                self.collapse_selected();
                vec![]
            }
            Action::OpenSelected => {
                if let Some(path) = self.open_selected() {
                    vec![
                        Effect::Emit(Event::OpenFile(path)),
                        Effect::FocusPanel(PanelSlot::Main),
                    ]
                } else {
                    vec![]
                }
            }
            Action::OpenSelectedBg => {
                if let Some(path) = self.open_selected() {
                    vec![Effect::Emit(Event::OpenFile(path))]
                } else {
                    vec![]
                }
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::TabActivated { path: Some(path) } => {
                self.reveal(path);
            }
            Event::Resume => {
                self.rebuild();
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &DrawContext) {
        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(ctx.theme.get("browser.border").to_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let height = inner.height as usize;
        if height == 0 {
            return;
        }

        let browser_scroll = if self.selected >= height {
            self.selected - height + 1
        } else {
            0
        };

        let mut lines = Vec::with_capacity(height);

        for i in 0..height {
            let idx = browser_scroll + i;
            if idx < self.entries.len() {
                let entry = &self.entries[idx];
                let name = FileBrowser::display_name(entry);

                let max_width = inner.width as usize;
                let display: String = if name.len() > max_width {
                    name[..max_width].to_string()
                } else {
                    name
                };

                let is_selected = idx == self.selected;
                let is_dir = matches!(entry.kind, EntryKind::Directory { .. });

                let style = if is_selected {
                    if ctx.focused {
                        ctx.theme.get("browser.selected").to_style()
                    } else {
                        ctx.theme.get("browser.selected_unfocused").to_style()
                    }
                } else if is_dir {
                    ctx.theme.get("browser.directory").to_style()
                } else {
                    ctx.theme.get("browser.file").to_style()
                };

                let padded = format!("{:<width$}", display, width = max_width);
                lines.push(Line::from(Span::styled(padded, style)));
            } else {
                lines.push(Line::from(""));
            }
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn save_session(&self, ctx: &mut Context) {
        ctx.kv.insert("browser.selected".into(), self.selected.to_string());
        let dirs: Vec<String> = self.expanded_dirs.iter()
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        ctx.kv.insert("browser.expanded_dirs".into(), dirs.join("\n"));
    }

    fn restore_session(&mut self, ctx: &mut Context) {
        let selected: usize = ctx.kv.get("browser.selected")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let dirs: HashSet<PathBuf> = ctx.kv.get("browser.expanded_dirs")
            .map(|s| s.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
            .unwrap_or_default();

        self.set_expanded_dirs(dirs);
        self.selected = selected.min(self.entries.len().saturating_sub(1));
    }

    fn context_name(&self) -> Option<&str> {
        Some("browser")
    }
}
