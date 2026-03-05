use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot, Waker,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::search::search_worker;
use crate::types::{FileGroup, FlatHit, SearchRequest};

pub struct FileSearch {
    active: bool,
    query: String,
    cursor_pos: usize,
    select_all: bool,
    case_sensitive: bool,
    use_regex: bool,
    results: Vec<FileGroup>,
    flat_hits: Vec<FlatHit>,
    selected: usize,
    scroll_offset: usize,
    root: PathBuf,
    search_tx: mpsc::Sender<SearchRequest>,
    result_rx: mpsc::Receiver<Vec<FileGroup>>,
    #[allow(dead_code)]
    waker: Option<Waker>,
    cursor_screen_pos: Option<(u16, u16)>,
    active_claims: Vec<PanelClaim>,
    inactive_claims: Vec<PanelClaim>,
}

impl FileSearch {
    pub fn new(root: PathBuf, waker: Option<Waker>) -> Self {
        let (search_tx, search_rx) = mpsc::channel::<SearchRequest>();
        let (result_tx, result_rx) = mpsc::channel::<Vec<FileGroup>>();

        let waker_clone = waker.clone();
        thread::spawn(move || search_worker(search_rx, result_tx, waker_clone));

        Self {
            active: false,
            query: String::new(),
            cursor_pos: 0,
            select_all: false,
            case_sensitive: false,
            use_regex: false,
            results: Vec::new(),
            flat_hits: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            root,
            search_tx,
            result_rx,
            waker,
            cursor_screen_pos: None,
            active_claims: vec![PanelClaim {
                slot: PanelSlot::Side,
                priority: 20,
            }],
            inactive_claims: vec![],
        }
    }

    fn trigger_search(&self) {
        if self.query.is_empty() {
            return;
        }
        let _ = self.search_tx.send(SearchRequest {
            query: self.query.clone(),
            root: self.root.clone(),
            case_sensitive: self.case_sensitive,
            use_regex: self.use_regex,
        });
    }

    fn rebuild_flat_hits(&mut self) {
        self.flat_hits.clear();
        for (gi, group) in self.results.iter().enumerate() {
            for (hi, _) in group.hits.iter().enumerate() {
                self.flat_hits.push(FlatHit {
                    group_idx: gi,
                    hit_idx: hi,
                });
            }
        }
        if self.selected >= self.flat_hits.len() {
            self.selected = self.flat_hits.len().saturating_sub(1);
        }
    }

    fn poll_results(&mut self) -> Vec<Effect> {
        let mut got_results = false;
        while let Ok(results) = self.result_rx.try_recv() {
            self.results = results;
            got_results = true;
        }
        if got_results {
            self.rebuild_flat_hits();
            return self.preview_selected();
        }
        vec![]
    }

    fn preview_selected(&self) -> Vec<Effect> {
        if let Some((group, hit)) = self.selected_hit() {
            let match_len = hit.line_text[hit.match_start..hit.match_end].chars().count();
            vec![Effect::Emit(Event::PreviewFile {
                path: group.path.clone(),
                row: hit.row,
                col: hit.col,
                match_len,
            })]
        } else {
            vec![]
        }
    }

    fn selected_hit(&self) -> Option<(&FileGroup, &crate::types::SearchHit)> {
        let flat = self.flat_hits.get(self.selected)?;
        let group = &self.results[flat.group_idx];
        let hit = &group.hits[flat.hit_idx];
        Some((group, hit))
    }

    /// Compute which row in the rendered list a given flat_hit index occupies.
    /// Each file group header takes 1 row, each hit takes 1 row.
    fn flat_hit_to_row(&self, flat_idx: usize) -> usize {
        if flat_idx >= self.flat_hits.len() {
            return 0;
        }
        let target = &self.flat_hits[flat_idx];
        let mut row = 0;
        for (gi, group) in self.results.iter().enumerate() {
            row += 1; // file header
            if gi == target.group_idx {
                row += target.hit_idx;
                return row;
            }
            row += group.hits.len();
        }
        row
    }

    #[allow(dead_code)]
    fn total_display_rows(&self) -> usize {
        let mut rows = 0;
        for group in &self.results {
            rows += 1 + group.hits.len(); // header + hits
        }
        rows
    }
}

// ---------------------------------------------------------------------------
// Component implementation
// ---------------------------------------------------------------------------

impl Component for FileSearch {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn panel_claims(&self) -> &[PanelClaim] {
        if self.active {
            &self.active_claims
        } else {
            &self.inactive_claims
        }
    }

    fn context_name(&self) -> Option<&str> {
        if self.active {
            Some("file_search")
        } else {
            None
        }
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        // Always poll for search results
        let mut effects = self.poll_results();

        match action {
            Action::CloseFileSearch | Action::Abort => {
                self.active = false;
                effects.push(Effect::Emit(Event::PreviewClosed));
                effects.push(Effect::FocusPanel(PanelSlot::Main));
                return effects;
            }
            Action::InsertChar(c) => {
                if self.select_all {
                    self.query.clear();
                    self.cursor_pos = 0;
                    self.select_all = false;
                }
                let byte_pos = self
                    .query
                    .char_indices()
                    .nth(self.cursor_pos)
                    .map(|(i, _)| i)
                    .unwrap_or(self.query.len());
                self.query.insert(byte_pos, c);
                self.cursor_pos += 1;
                self.selected = 0;
                self.scroll_offset = 0;
                self.trigger_search();
                effects
            }
            Action::DeleteBackward => {
                if self.select_all && !self.query.is_empty() {
                    self.query.clear();
                    self.cursor_pos = 0;
                    self.select_all = false;
                    self.results.clear();
                    self.flat_hits.clear();
                    self.selected = 0;
                    return effects;
                }
                if self.cursor_pos > 0 {
                    let byte_pos = self
                        .query
                        .char_indices()
                        .nth(self.cursor_pos - 1)
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    let next_byte = self
                        .query
                        .char_indices()
                        .nth(self.cursor_pos)
                        .map(|(i, _)| i)
                        .unwrap_or(self.query.len());
                    self.query.replace_range(byte_pos..next_byte, "");
                    self.cursor_pos -= 1;
                    self.selected = 0;
                    self.scroll_offset = 0;
                    if self.query.is_empty() {
                        self.results.clear();
                        self.flat_hits.clear();
                    } else {
                        self.trigger_search();
                    }
                }
                effects
            }
            Action::DeleteForward => {
                let char_len = self.query.chars().count();
                if self.cursor_pos < char_len {
                    let byte_pos = self
                        .query
                        .char_indices()
                        .nth(self.cursor_pos)
                        .map(|(i, _)| i)
                        .unwrap_or(self.query.len());
                    let next_byte = self
                        .query
                        .char_indices()
                        .nth(self.cursor_pos + 1)
                        .map(|(i, _)| i)
                        .unwrap_or(self.query.len());
                    self.query.replace_range(byte_pos..next_byte, "");
                    self.selected = 0;
                    self.scroll_offset = 0;
                    if self.query.is_empty() {
                        self.results.clear();
                        self.flat_hits.clear();
                    } else {
                        self.trigger_search();
                    }
                }
                effects
            }
            Action::KillLine => {
                self.select_all = false;
                let byte_pos = self
                    .query
                    .char_indices()
                    .nth(self.cursor_pos)
                    .map(|(i, _)| i)
                    .unwrap_or(self.query.len());
                self.query.truncate(byte_pos);
                self.selected = 0;
                self.scroll_offset = 0;
                if self.query.is_empty() {
                    self.results.clear();
                    self.flat_hits.clear();
                } else {
                    self.trigger_search();
                }
                effects
            }
            Action::MoveLeft => {
                self.select_all = false;
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                }
                effects
            }
            Action::MoveRight => {
                self.select_all = false;
                let char_len = self.query.chars().count();
                if self.cursor_pos < char_len {
                    self.cursor_pos += 1;
                }
                effects
            }
            Action::LineStart => {
                self.select_all = false;
                self.cursor_pos = 0;
                effects
            }
            Action::LineEnd => {
                self.select_all = false;
                self.cursor_pos = self.query.chars().count();
                effects
            }
            Action::MoveUp => {
                self.select_all = false;
                if self.selected > 0 {
                    self.selected -= 1;
                }
                effects.extend(self.preview_selected());
                return effects;
            }
            Action::MoveDown => {
                self.select_all = false;
                if !self.flat_hits.is_empty() && self.selected + 1 < self.flat_hits.len() {
                    self.selected += 1;
                }
                effects.extend(self.preview_selected());
                return effects;
            }
            Action::PageUp => {
                self.select_all = false;
                self.selected = self.selected.saturating_sub(ctx.viewport_height);
                effects.extend(self.preview_selected());
                return effects;
            }
            Action::PageDown => {
                self.select_all = false;
                if !self.flat_hits.is_empty() {
                    self.selected =
                        (self.selected + ctx.viewport_height).min(self.flat_hits.len() - 1);
                }
                effects.extend(self.preview_selected());
                return effects;
            }
            Action::OpenSelected | Action::InsertNewline => {
                if let Some((group, hit)) = self.selected_hit() {
                    let path = group.path.clone();
                    let row = hit.row;
                    let col = hit.col;
                    effects.push(Effect::Emit(Event::ConfirmSearch { path, row, col }));
                    return effects;
                } else {
                    return effects;
                }
            }
            Action::ToggleSearchCase => {
                self.case_sensitive = !self.case_sensitive;
                self.trigger_search();
                effects
            }
            Action::ToggleSearchRegex => {
                self.use_regex = !self.use_regex;
                self.trigger_search();
                effects
            }
            Action::Tick => {
                effects.extend(self.poll_results());
                return effects;
            }
            _ => return effects,
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::FileSearchOpened { selected_text } => {
                self.active = true;
                if let Some(text) = selected_text {
                    self.query = text.clone();
                    self.cursor_pos = self.query.chars().count();
                    self.select_all = false;
                    self.selected = 0;
                    self.scroll_offset = 0;
                    self.trigger_search();
                } else {
                    self.select_all = true;
                }
            }
            _ => {}
        }
        vec![]
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &DrawContext) {
        let _ = self.poll_results();

        let block = Block::default()
            .borders(Borders::RIGHT)
            .border_style(ctx.theme.get("search.border").to_style());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 3 || inner.width < 4 {
            return;
        }

        let width = inner.width as usize;

        // Row 0: modifier toggles
        let toggle_y = inner.y;
        {
            let on_style = ctx.theme.get("search.toggle_on").to_style();
            let off_style = ctx.theme.get("search.toggle_off").to_style();

            let case_style = if self.case_sensitive {
                on_style
            } else {
                off_style
            };
            let regex_style = if self.use_regex { on_style } else { off_style };

            let spans = vec![
                Span::styled(" Aa ", case_style),
                Span::raw(" "),
                Span::styled(" .* ", regex_style),
            ];
            let toggle_area = Rect::new(inner.x, toggle_y, inner.width, 1);
            frame.render_widget(Paragraph::new(Line::from(spans)), toggle_area);
        }

        // Row 1: search input box
        let input_y = inner.y + 1;
        {
            let input_style = if ctx.focused {
                ctx.theme.get("search.input").to_style()
            } else {
                ctx.theme.get("search.input_unfocused").to_style()
            };

            let display_query: String = if self.query.len() > width {
                self.query.chars().take(width).collect()
            } else {
                format!("{:<w$}", self.query, w = width)
            };
            let line = Line::from(Span::styled(display_query, input_style));
            let input_area = Rect::new(inner.x, input_y, inner.width, 1);
            frame.render_widget(Paragraph::new(vec![line]), input_area);

            // Store cursor position
            if ctx.focused {
                let cursor_x = inner.x + self.cursor_pos.min(width) as u16;
                self.cursor_screen_pos = Some((cursor_x, input_y));
            }
        }

        // Rows 2+: search results
        let results_y = inner.y + 2;
        let results_height = (inner.height as usize).saturating_sub(2);
        if results_height == 0 {
            return;
        }

        // Adjust scroll to keep selected visible
        if !self.flat_hits.is_empty() {
            let sel_row = self.flat_hit_to_row(self.selected);
            if sel_row < self.scroll_offset {
                self.scroll_offset = sel_row;
            } else if sel_row >= self.scroll_offset + results_height {
                self.scroll_offset = sel_row - results_height + 1;
            }
        }

        // Build display rows
        let header_style = ctx.theme.get("search.file_header").to_style();
        let hit_style = ctx.theme.get("search.hit").to_style();
        let match_style = ctx.theme.get("search.match").to_style();
        let selected_style = if ctx.focused {
            ctx.theme.get("search.selected").to_style()
        } else {
            ctx.theme.get("search.selected_unfocused").to_style()
        };

        let selected_flat = if self.flat_hits.is_empty() {
            None
        } else {
            Some(&self.flat_hits[self.selected])
        };

        let mut display_row: usize = 0;
        let mut rendered: usize = 0;

        for (gi, group) in self.results.iter().enumerate() {
            // File header row
            if display_row >= self.scroll_offset {
                if rendered >= results_height {
                    break;
                }
                let header_text: String = if group.relative.len() > width {
                    group.relative[..width].to_string()
                } else {
                    group.relative.clone()
                };
                let padded = format!("{:<w$}", header_text, w = width);
                let line = Line::from(Span::styled(padded, header_style));
                let row_area =
                    Rect::new(inner.x, results_y + rendered as u16, inner.width, 1);
                frame.render_widget(Paragraph::new(vec![line]), row_area);
                rendered += 1;
            }
            display_row += 1;

            // Hit rows
            for (hi, hit) in group.hits.iter().enumerate() {
                if display_row >= self.scroll_offset {
                    if rendered >= results_height {
                        break;
                    }
                    let is_selected = selected_flat
                        .map_or(false, |f| f.group_idx == gi && f.hit_idx == hi);

                    let base_style = if is_selected {
                        selected_style
                    } else {
                        hit_style
                    };

                    // Build spans with match highlighting
                    let prefix = format!("{:>4}: ", hit.row + 1);
                    let avail = width.saturating_sub(prefix.len());

                    // Compute a window around the match
                    let line_chars: Vec<char> = hit.line_text.chars().collect();
                    let match_char_start = hit.line_text[..hit.match_start].chars().count();
                    let match_char_end = hit.line_text[..hit.match_end].chars().count();

                    // Center the match in the available width
                    let match_len = match_char_end - match_char_start;
                    let context_before = avail.saturating_sub(match_len) / 2;
                    let win_start = match_char_start.saturating_sub(context_before);
                    let win_end = (win_start + avail).min(line_chars.len());
                    let win_start = if win_end.saturating_sub(avail) < win_start {
                        win_end.saturating_sub(avail)
                    } else {
                        win_start
                    };

                    let visible: String = line_chars[win_start..win_end].iter().collect();
                    let ms_in_win = match_char_start.saturating_sub(win_start);
                    let me_in_win =
                        (match_char_end.saturating_sub(win_start)).min(visible.chars().count());

                    let before: String = visible.chars().take(ms_in_win).collect();
                    let matched: String =
                        visible.chars().skip(ms_in_win).take(me_in_win - ms_in_win).collect();
                    let after: String = visible.chars().skip(me_in_win).collect();

                    let pad_needed = avail.saturating_sub(visible.chars().count());
                    let after_padded = format!("{after}{:pad$}", "", pad = pad_needed);

                    let mut spans = vec![Span::styled(prefix, base_style)];
                    if !before.is_empty() {
                        spans.push(Span::styled(before, base_style));
                    }
                    if !matched.is_empty() {
                        let ms = if is_selected { selected_style } else { match_style };
                        spans.push(Span::styled(matched, ms));
                    }
                    if !after_padded.is_empty() {
                        spans.push(Span::styled(after_padded, base_style));
                    }

                    let row_area =
                        Rect::new(inner.x, results_y + rendered as u16, inner.width, 1);
                    frame.render_widget(Paragraph::new(Line::from(spans)), row_area);
                    rendered += 1;
                }
                display_row += 1;
            }
        }

        // Fill remaining rows
        for r in rendered..results_height {
            let row_area = Rect::new(inner.x, results_y + r as u16, inner.width, 1);
            frame.render_widget(Paragraph::new(""), row_area);
        }
    }

    fn cursor_screen_pos(&self) -> Option<(u16, u16)> {
        if self.active {
            self.cursor_screen_pos
        } else {
            None
        }
    }

    fn save_session(&self, _ctx: &mut Context) {}
    fn restore_session(&mut self, _ctx: &mut Context) {}

    fn default_theme_toml(&self) -> &'static str {
        r#"
[search]
border             = "$muted"
input              = { bg = "$bright_black" }
input_unfocused    = { bg = "$bright_black" }
toggle_on          = "$inverse_active"
toggle_off         = "$inverse_inactive"
file_header        = "$accent"
hit                = "$normal"
match              = { fg = "$bright_yellow", bold = true }
selected           = "$inverse_active"
selected_unfocused = "$inverse_inactive"
"#
    }
}
