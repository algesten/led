use std::path::PathBuf;

use led_core::{Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot};
use ratatui::Frame;
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct JumpPosition {
    path: PathBuf,
    row: usize,
    col: usize,
    scroll_offset: usize,
}

pub struct JumpList {
    list: Vec<JumpPosition>,
    index: usize,
}

impl JumpList {
    pub fn new() -> Self {
        Self {
            list: Vec::new(),
            index: 0,
        }
    }
}

impl Component for JumpList {
    fn panel_claims(&self) -> &[PanelClaim] {
        &[]
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::SaveSession => {
                if let Ok(json) = serde_json::to_string(&self.list) {
                    ctx.kv.insert("jump_list.entries".into(), json);
                    ctx.kv
                        .insert("jump_list.index".into(), self.index.to_string());
                }
            }
            Action::RestoreSession => {
                if let Some(json) = ctx.kv.get("jump_list.entries") {
                    if let Ok(entries) = serde_json::from_str::<Vec<JumpPosition>>(json) {
                        self.list = entries;
                        self.index = ctx
                            .kv
                            .get("jump_list.index")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(self.list.len());
                    }
                }
            }
            _ => {}
        }
        vec![]
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::RecordJump {
                path,
                row,
                col,
                scroll_offset,
            } => {
                // Truncate forward history
                self.list.truncate(self.index);
                self.list.push(JumpPosition {
                    path: path.clone(),
                    row: *row,
                    col: *col,
                    scroll_offset: *scroll_offset,
                });
                // Cap at 100 entries
                if self.list.len() > 100 {
                    self.list.remove(0);
                }
                self.index = self.list.len();
                vec![]
            }
            Event::JumpBack {
                path,
                row,
                col,
                scroll_offset,
            } => {
                if self.index == 0 {
                    return vec![];
                }
                // If at present (past end), save current position first
                if self.index == self.list.len() {
                    self.list.push(JumpPosition {
                        path: path.clone(),
                        row: *row,
                        col: *col,
                        scroll_offset: *scroll_offset,
                    });
                }
                self.index -= 1;
                let pos = &self.list[self.index];
                vec![
                    Effect::Emit(Event::PreviewClosed),
                    Effect::Emit(Event::OpenFile(pos.path.clone())),
                    Effect::Emit(Event::GoToPosition {
                        path: pos.path.clone(),
                        row: pos.row,
                        col: pos.col,
                        scroll_offset: Some(pos.scroll_offset),
                    }),
                    Effect::FocusPanel(PanelSlot::Main),
                ]
            }
            Event::JumpForward => {
                if self.index + 1 >= self.list.len() {
                    return vec![];
                }
                self.index += 1;
                let pos = &self.list[self.index];
                vec![
                    Effect::Emit(Event::PreviewClosed),
                    Effect::Emit(Event::OpenFile(pos.path.clone())),
                    Effect::Emit(Event::GoToPosition {
                        path: pos.path.clone(),
                        row: pos.row,
                        col: pos.col,
                        scroll_offset: Some(pos.scroll_offset),
                    }),
                    Effect::FocusPanel(PanelSlot::Main),
                ]
            }
            _ => vec![],
        }
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut DrawContext) {}
}
