use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use led_core::{Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot};
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
    scroll_offset: usize,
}

impl FileBrowser {
    pub fn new(root: PathBuf) -> Self {
        let mut browser = Self {
            root,
            entries: Vec::new(),
            selected: 0,
            expanded_dirs: HashSet::new(),
            scroll_offset: 0,
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

    fn selected_file_path(&self) -> Option<&PathBuf> {
        let entry = self.entries.get(self.selected)?;
        match entry.kind {
            EntryKind::File => Some(&entry.path),
            _ => None,
        }
    }

    fn preview_selected(&self) -> Vec<Effect> {
        if let Some(path) = self.selected_file_path() {
            vec![Effect::Emit(Event::PreviewFile {
                path: path.clone(),
                row: 0,
                col: 0,
                match_len: 0,
            })]
        } else {
            vec![Effect::Emit(Event::PreviewClosed)]
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
                self.preview_selected()
            }
            Action::MoveDown => {
                self.move_down();
                self.preview_selected()
            }
            Action::PageUp => {
                self.page_up(ctx.viewport_height);
                self.preview_selected()
            }
            Action::PageDown => {
                self.page_down(ctx.viewport_height);
                self.preview_selected()
            }
            Action::FileStart => {
                self.selected = 0;
                self.preview_selected()
            }
            Action::FileEnd => {
                if !self.entries.is_empty() {
                    self.selected = self.entries.len() - 1;
                }
                self.preview_selected()
            }
            Action::ExpandDir => {
                self.expand_selected();
                self.preview_selected()
            }
            Action::CollapseDir => {
                self.collapse_selected();
                self.preview_selected()
            }
            Action::CollapseAll => {
                self.expanded_dirs.clear();
                self.selected = 0;
                self.rebuild();
                self.preview_selected()
            }
            Action::OpenSelected => {
                if let Some(entry) = self.entries.get(self.selected) {
                    if matches!(entry.kind, EntryKind::File) {
                        let path = entry.path.clone();
                        vec![Effect::Emit(Event::ConfirmSearch {
                            path,
                            row: 0,
                            col: 0,
                        })]
                    } else {
                        self.open_selected();
                        self.preview_selected()
                    }
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
            Action::FocusLost => {
                vec![Effect::Emit(Event::PreviewClosed)]
            }
            Action::FocusGained => vec![],
            Action::SaveSession => {
                ctx.kv
                    .insert("browser.selected".into(), self.selected.to_string());
                ctx.kv.insert(
                    "browser.scroll_offset".into(),
                    self.scroll_offset.to_string(),
                );
                let dirs: Vec<String> = self
                    .expanded_dirs
                    .iter()
                    .map(|d| d.to_string_lossy().into_owned())
                    .collect();
                ctx.kv
                    .insert("browser.expanded_dirs".into(), dirs.join("\n"));
                vec![]
            }
            Action::RestoreSession => {
                let selected: usize = ctx
                    .kv
                    .get("browser.selected")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let scroll_offset: usize = ctx
                    .kv
                    .get("browser.scroll_offset")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let dirs: HashSet<PathBuf> = ctx
                    .kv
                    .get("browser.expanded_dirs")
                    .map(|s| {
                        s.lines()
                            .filter(|l| !l.is_empty())
                            .map(PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_default();
                self.set_expanded_dirs(dirs);
                self.selected = selected.min(self.entries.len().saturating_sub(1));
                self.scroll_offset = scroll_offset;
                vec![]
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

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext) {
        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(ctx.theme.get("browser.border").to_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let height = inner.height as usize;
        if height == 0 {
            return;
        }

        // Scroll-into-view: adjust offset only when selection escapes the viewport
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + height {
            self.scroll_offset = self.selected - height + 1;
        }
        let browser_scroll = self.scroll_offset;

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

                // Apply git status color to the entire entry
                let file_status_set = match &entry.kind {
                    EntryKind::File => ctx.file_statuses.file_statuses(&entry.path).cloned(),
                    EntryKind::Directory { .. } => {
                        let s = ctx.file_statuses.directory_statuses(&entry.path);
                        if s.is_empty() { None } else { Some(s) }
                    }
                };
                let status_display = file_status_set
                    .as_ref()
                    .and_then(|s| led_core::file_status::resolve_display(s));

                if let Some(ref sd) = status_display {
                    let status_fg = ctx.theme.get(sd.theme_key).to_style();
                    let entry_style = if is_selected {
                        ratatui::style::Style::default()
                            .fg(status_fg.fg.unwrap_or(ratatui::style::Color::Reset))
                            .bg(style.bg.unwrap_or(ratatui::style::Color::Reset))
                    } else {
                        status_fg
                    };
                    let name_width = max_width.saturating_sub(1);
                    let truncated: String = if display.len() > name_width {
                        display[..name_width].to_string()
                    } else {
                        display
                    };
                    let pad = name_width.saturating_sub(truncated.len());
                    let name_part = format!("{truncated}{:pad$}", "");
                    lines.push(Line::from(vec![
                        Span::styled(name_part, entry_style),
                        Span::styled(
                            if is_dir {
                                "\u{23fa}".to_string()
                            } else {
                                sd.letter.to_string()
                            },
                            entry_style,
                        ),
                    ]));
                } else {
                    let padded = format!("{:<width$}", display, width = max_width);
                    lines.push(Line::from(Span::styled(padded, style)));
                }
            } else {
                lines.push(Line::from(""));
            }
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn context_name(&self) -> Option<&str> {
        Some("browser")
    }
}
