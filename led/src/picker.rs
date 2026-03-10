use std::path::PathBuf;

use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot, PickerKind,
};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

pub struct Picker {
    active: bool,
    title: String,
    items: Vec<String>,
    selected: usize,
    source_path: PathBuf,
    kind: PickerKind,
    active_claims: Vec<PanelClaim>,
    inactive_claims: Vec<PanelClaim>,
}

impl Picker {
    pub fn new() -> Self {
        Self {
            active: false,
            title: String::new(),
            items: Vec::new(),
            selected: 0,
            source_path: PathBuf::new(),
            kind: PickerKind::default(),
            active_claims: vec![PanelClaim {
                slot: PanelSlot::Overlay,
                priority: 10,
            }],
            inactive_claims: vec![],
        }
    }

    fn confirm(&self) -> Vec<Effect> {
        match &self.kind {
            PickerKind::CodeAction => {
                vec![Effect::Emit(Event::LspCodeActionResolve {
                    path: self.source_path.clone(),
                    index: self.selected,
                })]
            }
            PickerKind::Outline { rows } => {
                if let Some(&row) = rows.get(self.selected) {
                    vec![Effect::Emit(Event::GoToPosition {
                        path: self.source_path.clone(),
                        row,
                        col: 0,
                        scroll_offset: None,
                    })]
                } else {
                    vec![]
                }
            }
        }
    }
}

impl Component for Picker {
    fn panel_claims(&self) -> &[PanelClaim] {
        if self.active {
            &self.active_claims
        } else {
            &self.inactive_claims
        }
    }

    fn handle_action(&mut self, action: Action, _ctx: &mut Context) -> Vec<Effect> {
        if !self.active {
            return vec![];
        }
        match action {
            Action::MoveUp => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                vec![]
            }
            Action::MoveDown => {
                if self.selected + 1 < self.items.len() {
                    self.selected += 1;
                }
                vec![]
            }
            Action::InsertNewline => {
                let effects = self.confirm();
                self.active = false;
                let mut result = effects;
                result.push(Effect::FocusPanel(PanelSlot::Main));
                result
            }
            Action::Abort => {
                self.active = false;
                vec![Effect::FocusPanel(PanelSlot::Main)]
            }
            _ => vec![],
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        if let Event::ShowPicker {
            title,
            items,
            source_path,
            kind,
        } = event
        {
            self.active = true;
            self.title = title.clone();
            self.items = items.clone();
            self.selected = 0;
            self.source_path = source_path.clone();
            self.kind = kind.clone();
            return vec![Effect::FocusPanel(PanelSlot::Overlay)];
        }
        vec![]
    }

    fn draw(&mut self, frame: &mut Frame, _area: Rect, _ctx: &mut DrawContext) {
        if !self.active {
            return;
        }

        let area = frame.area();

        let max_item_len = self.items.iter().map(|s| s.len()).max().unwrap_or(10);
        let width = (max_item_len as u16 + 4)
            .max(self.title.len() as u16 + 4)
            .min(area.width.saturating_sub(4));
        let height = (self.items.len() as u16 + 3).min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let visible_items = inner.height as usize;
        let scroll = if self.selected >= visible_items {
            self.selected - visible_items + 1
        } else {
            0
        };

        for (i, item) in self.items.iter().enumerate().skip(scroll).take(visible_items) {
            let row = (i - scroll) as u16;
            if row >= inner.height {
                break;
            }
            let style = if i == self.selected {
                Style::default().bg(ratatui::style::Color::DarkGray)
            } else {
                Style::default()
            };
            let truncated: String = item.chars().take(inner.width as usize).collect();
            let item_area = Rect::new(inner.x, inner.y + row, inner.width, 1);
            let paragraph = Paragraph::new(truncated).style(style);
            frame.render_widget(paragraph, item_area);
        }
    }

    fn context_name(&self) -> Option<&str> {
        if self.active {
            Some("picker")
        } else {
            None
        }
    }
}
