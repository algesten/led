use std::path::PathBuf;

use led_core::{Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot};
use ratatui::Frame;
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
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

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Compute the new list and index after recording a jump position.
/// Truncates forward history and caps at 100 entries.
fn record_jump_position(
    list: &[JumpPosition],
    index: usize,
    pos: JumpPosition,
) -> (Vec<JumpPosition>, usize) {
    let mut new_list: Vec<JumpPosition> = list[..index].to_vec();
    new_list.push(pos);
    if new_list.len() > 100 {
        new_list.remove(0);
    }
    let new_index = new_list.len();
    (new_list, new_index)
}

/// Compute the new index and the position to jump to when going back.
/// Returns `None` if already at the beginning.
fn compute_jump_back(
    list: &[JumpPosition],
    index: usize,
    current: JumpPosition,
) -> Option<(Vec<JumpPosition>, usize)> {
    if index == 0 {
        return None;
    }
    let mut new_list = list.to_vec();
    let mut new_index = index;
    // If at present (past end), save current position first
    if index == list.len() {
        new_list.push(current);
    }
    new_index -= 1;
    Some((new_list, new_index))
}

/// Compute the new index when jumping forward.
/// Returns `None` if already at the end.
fn compute_jump_forward(list: &[JumpPosition], index: usize) -> Option<usize> {
    if index + 1 >= list.len() {
        return None;
    }
    Some(index + 1)
}

fn jump_effects(pos: &JumpPosition) -> Vec<Effect> {
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
                let pos = JumpPosition {
                    path: path.clone(),
                    row: *row,
                    col: *col,
                    scroll_offset: *scroll_offset,
                };
                let (new_list, new_index) = record_jump_position(&self.list, self.index, pos);
                self.list = new_list;
                self.index = new_index;
                vec![]
            }
            Event::JumpBack {
                path,
                row,
                col,
                scroll_offset,
            } => {
                let current = JumpPosition {
                    path: path.clone(),
                    row: *row,
                    col: *col,
                    scroll_offset: *scroll_offset,
                };
                let Some((new_list, new_index)) =
                    compute_jump_back(&self.list, self.index, current)
                else {
                    return vec![];
                };
                self.list = new_list;
                self.index = new_index;
                jump_effects(&self.list[self.index])
            }
            Event::JumpForward => {
                let Some(new_index) = compute_jump_forward(&self.list, self.index) else {
                    return vec![];
                };
                self.index = new_index;
                jump_effects(&self.list[self.index])
            }
            _ => vec![],
        }
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut DrawContext) {}
}
