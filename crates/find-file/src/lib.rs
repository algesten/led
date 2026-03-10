use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};

use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot, Theme,
};

// ---------------------------------------------------------------------------
// Completion entry
// ---------------------------------------------------------------------------

struct Completion {
    name: String,
    full: PathBuf,
    is_dir: bool,
}

// ---------------------------------------------------------------------------
// Pure helper functions
// ---------------------------------------------------------------------------

fn prev_char_boundary(s: &str, byte_pos: usize) -> usize {
    s[..byte_pos]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_len(s: &str, byte_pos: usize) -> usize {
    s[byte_pos..]
        .chars()
        .next()
        .map(|c| c.len_utf8())
        .unwrap_or(0)
}

fn wrap_selection_up(current: Option<usize>, len: usize) -> usize {
    match current {
        Some(0) | None => len - 1,
        Some(i) => i - 1,
    }
}

fn wrap_selection_down(current: Option<usize>, len: usize) -> usize {
    match current {
        None => 0,
        Some(i) if i + 1 >= len => 0,
        Some(i) => i + 1,
    }
}

fn truncate_to_width(name: &str, max: usize) -> String {
    if name.len() > max {
        format!("{}…", &name[..max.saturating_sub(1)])
    } else {
        format!("{name:max$}")
    }
}

// ---------------------------------------------------------------------------
// Style helpers
// ---------------------------------------------------------------------------

fn completion_row_style(is_selected: bool, is_dir: bool, focused: bool, theme: &Theme) -> Style {
    if is_selected {
        if focused {
            theme.get("browser.selected").to_style()
        } else {
            theme.get("browser.selected_unfocused").to_style()
        }
    } else if is_dir {
        theme.get("browser.directory").to_style()
    } else {
        theme.get("browser.file").to_style()
    }
}

fn status_entry_style(is_selected: bool, base_style: Style, status_fg: Style) -> Style {
    if is_selected {
        Style::default()
            .fg(status_fg.fg.unwrap_or(ratatui::style::Color::Reset))
            .bg(base_style.bg.unwrap_or(ratatui::style::Color::Reset))
    } else {
        status_fg
    }
}

// ---------------------------------------------------------------------------
// FindFilePanel
// ---------------------------------------------------------------------------

pub struct FindFilePanel {
    active: bool,
    input: String,
    cursor: usize,
    completions: Vec<Completion>,
    selected: Option<usize>,
    show_side: bool,
    status_only_claims: Vec<PanelClaim>,
    status_and_side_claims: Vec<PanelClaim>,
    inactive_claims: Vec<PanelClaim>,
}

impl FindFilePanel {
    pub fn new() -> Self {
        Self {
            active: false,
            input: String::new(),
            cursor: 0,
            completions: Vec::new(),
            selected: None,
            show_side: false,
            status_only_claims: vec![PanelClaim {
                slot: PanelSlot::StatusBar,
                priority: 30,
            }],
            status_and_side_claims: vec![
                PanelClaim {
                    slot: PanelSlot::StatusBar,
                    priority: 30,
                },
                PanelClaim {
                    slot: PanelSlot::Side,
                    priority: 20,
                },
            ],
            inactive_claims: Vec::new(),
        }
    }

    // -- Deactivation -------------------------------------------------------

    fn deactivate(&mut self) -> Vec<Effect> {
        self.active = false;
        self.completions.clear();
        self.selected = None;
        self.show_side = false;
        vec![
            Effect::Emit(Event::PreviewClosed),
            Effect::FocusPanel(PanelSlot::Main),
        ]
    }

    // -- Path abbreviation ---------------------------------------------------

    fn abbreviate_home(path: &str) -> String {
        if let Some(home) = dirs::home_dir() {
            let home = home.to_string_lossy();
            if path.starts_with(home.as_ref()) {
                return format!("~{}", &path[home.len()..]);
            }
        }
        path.to_string()
    }

    // -- Path expansion -----------------------------------------------------

    fn expand_path(input: &str) -> PathBuf {
        let input = if input.starts_with('~') {
            if let Some(home) = dirs::home_dir() {
                home.join(&input[1..].trim_start_matches('/'))
                    .to_string_lossy()
                    .into_owned()
            } else {
                input.to_string()
            }
        } else {
            input.to_string()
        };

        let path = Path::new(&input);
        let mut result = PathBuf::new();
        for comp in path.components() {
            match comp {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    result.pop();
                }
                other => result.push(other),
            }
        }
        result
    }

    // -- Completion computation ---------------------------------------------

    fn compute_completions(dir: &Path, prefix: &str) -> Vec<Completion> {
        let entries = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => return Vec::new(),
        };

        let show_hidden = prefix.starts_with('.');
        let prefix_lower = prefix.to_lowercase();

        let mut completions: Vec<Completion> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !show_hidden && name.starts_with('.') {
                    return None;
                }
                if !name.to_lowercase().starts_with(&prefix_lower) {
                    return None;
                }
                let is_dir = e.file_type().map_or(false, |ft| ft.is_dir());
                let display = if is_dir {
                    format!("{name}/")
                } else {
                    name.clone()
                };
                Some(Completion {
                    name: display,
                    full: e.path(),
                    is_dir,
                })
            })
            .collect();

        completions.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        completions
    }

    fn recompute(&mut self) {
        let expanded = Self::expand_path(&self.input);
        if self.input.ends_with('/') {
            self.completions = Self::compute_completions(&expanded, "");
        } else {
            let parent = expanded.parent().unwrap_or(Path::new("/"));
            let prefix = expanded
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.completions = Self::compute_completions(parent, &prefix);
        }
        self.selected = None;
    }

    // -- Tab completion -----------------------------------------------------

    /// The directory prefix as typed by the user (everything up to and including the last `/`).
    fn input_dir_prefix(&self) -> &str {
        match self.input.rfind('/') {
            Some(i) => &self.input[..=i],
            None => "",
        }
    }

    fn tab_complete(&mut self) {
        let expanded = Self::expand_path(&self.input);

        // Rule 1: input ends with / and is a directory → show contents
        if self.input.ends_with('/') && expanded.is_dir() {
            self.show_side = true;
            self.completions = Self::compute_completions(&expanded, "");
            self.selected = None;
            return;
        }

        let parent = expanded.parent().unwrap_or(Path::new("/"));
        let prefix = expanded
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir_prefix = self.input_dir_prefix().to_string();

        // If the input itself is a directory (no trailing slash), complete to dir/
        if expanded.is_dir() && !prefix.is_empty() {
            self.input = format!("{dir_prefix}{prefix}/");
            self.cursor = self.input.len();
            self.completions = Self::compute_completions(&expanded, "");
            self.selected = None;
            return;
        }

        let matches = Self::compute_completions(parent, &prefix);

        if matches.is_empty() {
            self.completions = matches;
            self.selected = None;
            return;
        }

        if matches.len() == 1 {
            // Rule 2: single match — complete fully
            let name = &matches[0].name; // already has trailing / for dirs
            self.input = format!("{dir_prefix}{name}");
            self.cursor = self.input.len();
            if matches[0].is_dir {
                let dir = Self::expand_path(&self.input);
                self.completions = Self::compute_completions(&dir, "");
            } else {
                self.completions = matches;
            }
            self.selected = None;
            return;
        }

        // Rule 3: multiple matches — complete longest common prefix, open side panel
        self.show_side = true;
        let common = Self::longest_common_prefix(&matches);
        if common.len() > prefix.len() {
            self.input = format!("{dir_prefix}{common}");
            self.cursor = self.input.len();
        }
        self.completions = matches;
        self.selected = None;
    }

    fn longest_common_prefix(completions: &[Completion]) -> String {
        if completions.is_empty() {
            return String::new();
        }
        // Use the raw name (without trailing / for dirs) for prefix computation
        let names: Vec<String> = completions
            .iter()
            .map(|c| {
                c.full
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();

        let first = &names[0];
        let mut prefix_len = first.len();
        for name in &names[1..] {
            prefix_len = prefix_len.min(name.len());
            for (i, (a, b)) in first.chars().zip(name.chars()).enumerate() {
                if a.to_lowercase().ne(b.to_lowercase()) {
                    prefix_len = prefix_len.min(i);
                    break;
                }
            }
        }
        first[..prefix_len].to_string()
    }

    // -- Preview ------------------------------------------------------------

    fn preview_selected(&self) -> Vec<Effect> {
        if let Some(sel) = self.selected {
            if let Some(comp) = self.completions.get(sel) {
                if !comp.is_dir {
                    return vec![Effect::Emit(Event::PreviewFile {
                        path: comp.full.clone(),
                        row: 0,
                        col: 0,
                        match_len: 0,
                    })];
                }
            }
        }
        Vec::new()
    }

    // -- Open file ----------------------------------------------------------

    fn open_file(&mut self, path: PathBuf) -> Vec<Effect> {
        self.active = false;
        self.completions.clear();
        self.selected = None;
        self.show_side = false;
        vec![Effect::Emit(Event::ConfirmSearch {
            path,
            row: 0,
            col: 0,
        })]
    }

    // -- Drawing helpers ----------------------------------------------------

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext) {
        let style = ctx.theme.get("status_bar.style").to_style();
        let prompt = format!(" Find file: {}", self.input);
        let width = area.width as usize;
        let display = if prompt.len() > width {
            &prompt[..width]
        } else {
            &prompt
        };
        let padding = width.saturating_sub(display.len());
        let bar = format!("{display}{:padding$}", "");
        let paragraph = ratatui::widgets::Paragraph::new(bar).style(style);
        frame.render_widget(paragraph, area);

        // Cursor position: after " Find file: " + cursor offset in input
        let prefix_len = " Find file: ".len() as u16;
        let cx = area.x + prefix_len + self.cursor as u16;
        if cx < area.x + area.width {
            ctx.cursor_pos = Some((cx, area.y));
        }
    }

    fn draw_side_panel(&self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext) {
        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(ctx.theme.get("browser.border").to_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.completions.is_empty() || inner.width == 0 || inner.height == 0 {
            return;
        }

        let buf = frame.buffer_mut();
        let height = inner.height as usize;

        // Scroll to keep selected visible
        let scroll = if let Some(sel) = self.selected {
            if sel < height { 0 } else { sel - height + 1 }
        } else {
            0
        };

        for (i, comp) in self
            .completions
            .iter()
            .skip(scroll)
            .take(height)
            .enumerate()
        {
            let y = inner.y + i as u16;
            let is_selected = self.selected == Some(scroll + i);
            let style = completion_row_style(is_selected, comp.is_dir, ctx.focused, &ctx.theme);

            let max = inner.width as usize;
            let name = &comp.name;

            // Apply git status color to the entire entry + status letter
            let sd = if !comp.is_dir {
                ctx.file_statuses
                    .file_statuses(&comp.full)
                    .and_then(|s| led_core::file_status::resolve_display(s))
            } else {
                None
            };

            if let Some(sd) = sd {
                let status_fg = ctx.theme.get(sd.theme_key).to_style();
                let entry_style = status_entry_style(is_selected, style, status_fg);
                let name_width = max.saturating_sub(1);
                let display = truncate_to_width(name, name_width);
                buf.set_string(inner.x, y, &display, entry_style);
                buf.set_string(
                    inner.x + name_width as u16,
                    y,
                    &sd.letter.to_string(),
                    entry_style,
                );
            } else {
                let display = truncate_to_width(name, max);
                buf.set_string(inner.x, y, &display, style);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Component impl
// ---------------------------------------------------------------------------

impl Component for FindFilePanel {
    fn panel_claims(&self) -> &[PanelClaim] {
        if !self.active {
            &self.inactive_claims
        } else if self.show_side {
            &self.status_and_side_claims
        } else {
            &self.status_only_claims
        }
    }

    fn context_name(&self) -> Option<&str> {
        if self.active { Some("find_file") } else { None }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::FindFileOpened { dir } => {
                self.active = true;
                self.show_side = false;
                let path = Self::abbreviate_home(&dir.to_string_lossy());
                self.input = if path.ends_with('/') {
                    path
                } else {
                    format!("{path}/")
                };
                self.cursor = self.input.len();
                self.completions.clear();
                self.selected = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_action(&mut self, action: Action, _ctx: &mut Context) -> Vec<Effect> {
        if !self.active {
            return Vec::new();
        }

        match action {
            // Lifecycle actions — ignore silently
            Action::Tick
            | Action::FocusGained
            | Action::FocusLost
            | Action::SaveSession
            | Action::RestoreSession
            | Action::Flush => Vec::new(),

            Action::InsertChar(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.recompute();
                Vec::new()
            }

            Action::DeleteBackward => {
                if self.cursor > 0 {
                    let prev = prev_char_boundary(&self.input, self.cursor);
                    self.input.drain(prev..self.cursor);
                    self.cursor = prev;
                    self.recompute();
                }
                Vec::new()
            }

            Action::InsertTab => {
                self.tab_complete();
                Vec::new()
            }

            Action::InsertNewline => {
                if let Some(sel) = self.selected {
                    if let Some(comp) = self.completions.get(sel) {
                        if comp.is_dir {
                            let dir_prefix = self.input_dir_prefix().to_string();
                            self.input = format!("{dir_prefix}{}", comp.name);
                            self.cursor = self.input.len();
                            self.recompute();
                            return Vec::new();
                        } else {
                            let path = comp.full.clone();
                            return self.open_file(path);
                        }
                    }
                }
                // No selection — try the input directly
                let expanded = Self::expand_path(&self.input);
                if expanded.is_file() {
                    return self.open_file(expanded);
                }
                if expanded.is_dir() && self.input.ends_with('/') {
                    self.recompute();
                    return Vec::new();
                }
                Vec::new()
            }

            Action::MoveUp => {
                if self.completions.is_empty() {
                    return Vec::new();
                }
                let dir_prefix = self.input_dir_prefix().to_string();
                self.selected = Some(wrap_selection_up(self.selected, self.completions.len()));
                if let Some(sel) = self.selected {
                    if let Some(comp) = self.completions.get(sel) {
                        self.input = format!("{dir_prefix}{}", comp.name);
                        self.cursor = self.input.len();
                    }
                }
                self.preview_selected()
            }

            Action::MoveDown => {
                if self.completions.is_empty() {
                    return Vec::new();
                }
                let dir_prefix = self.input_dir_prefix().to_string();
                self.selected = Some(wrap_selection_down(self.selected, self.completions.len()));
                if let Some(sel) = self.selected {
                    if let Some(comp) = self.completions.get(sel) {
                        self.input = format!("{dir_prefix}{}", comp.name);
                        self.cursor = self.input.len();
                    }
                }
                self.preview_selected()
            }

            Action::LineStart => {
                self.cursor = 0;
                Vec::new()
            }

            Action::LineEnd => {
                self.cursor = self.input.len();
                Vec::new()
            }

            Action::KillLine => {
                self.input.truncate(self.cursor);
                self.recompute();
                Vec::new()
            }

            Action::DeleteForward => {
                if self.cursor < self.input.len() {
                    let len = next_char_len(&self.input, self.cursor);
                    self.input.drain(self.cursor..self.cursor + len);
                    self.recompute();
                }
                Vec::new()
            }

            Action::Abort => self.deactivate(),

            Action::MoveLeft => {
                if self.cursor > 0 {
                    self.cursor = prev_char_boundary(&self.input, self.cursor);
                }
                Vec::new()
            }

            Action::MoveRight => {
                if self.cursor < self.input.len() {
                    self.cursor += next_char_len(&self.input, self.cursor);
                }
                Vec::new()
            }

            // Any other action — deactivate and don't consume
            _ => self.deactivate(),
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext) {
        match ctx.slot {
            PanelSlot::StatusBar => self.draw_status_bar(frame, area, ctx),
            PanelSlot::Side => self.draw_side_panel(frame, area, ctx),
            _ => {}
        }
    }
}
