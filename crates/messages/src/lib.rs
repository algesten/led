use led_core::logging::SharedLog;
use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot, TabDescriptor,
};
use led_buffer::Buffer;
use ratatui::Frame;
use ratatui::layout::Rect;

pub struct Messages {
    buffer: Buffer,
    log: SharedLog,
    last_synced: usize,
    active: bool,
    claims: Vec<PanelClaim>,
}

impl Messages {
    pub fn new(log: SharedLog) -> Self {
        let mut buffer = Buffer::empty();
        buffer.read_only = true;
        Self {
            buffer,
            log,
            last_synced: 0,
            active: false,
            claims: vec![
                PanelClaim {
                    slot: PanelSlot::Main,
                    priority: 10,
                },
                PanelClaim {
                    slot: PanelSlot::StatusBar,
                    priority: 10,
                },
            ],
        }
    }

    fn sync(&mut self) {
        let Ok(buf) = self.log.lock() else { return };
        let total = buf.total_pushed();
        if total == self.last_synced {
            return;
        }

        let new_count = total - self.last_synced;
        let entries = buf.entries();
        // The new entries are in the tail of the deque
        let skip = entries.len().saturating_sub(new_count);

        // Check if cursor is at the last line before appending
        let last_line = self.buffer.line_count().saturating_sub(1);
        let was_at_end = self.buffer.cursor_row >= last_line;

        for entry in entries.iter().skip(skip) {
            let secs = entry.elapsed.as_secs_f64();
            let line = format!("[{secs:>10.3}] {:<5} {}\n", entry.level, entry.message);
            self.buffer.append_text(&line);
        }

        self.last_synced = total;

        // Auto-scroll if user was at the end
        if was_at_end {
            let new_last = self.buffer.line_count().saturating_sub(1);
            self.buffer.cursor_row = new_last;
            self.buffer.cursor_col = 0;
            // Scroll so the last line is visible
            if new_last > self.buffer.scroll_offset + 20 {
                self.buffer.scroll_offset = new_last.saturating_sub(10);
            }
        }
    }
}

impl Component for Messages {
    fn panel_claims(&self) -> &[PanelClaim] {
        &self.claims
    }

    fn tab(&self) -> Option<TabDescriptor> {
        if self.active {
            Some(TabDescriptor {
                label: "*Messages*".into(),
                dirty: false,
                path: None,
                preview: false,
                read_only: true,
            })
        } else {
            None
        }
    }

    fn handle_action(&mut self, action: Action, ctx: &mut Context) -> Vec<Effect> {
        match action {
            Action::Tick => {
                self.sync();
                self.buffer.handle_action(action, ctx)
            }
            Action::KillBuffer => {
                self.active = false;
                vec![Effect::FocusPanel(PanelSlot::Main)]
            }
            Action::Abort => {
                self.active = false;
                vec![Effect::FocusPanel(PanelSlot::Main)]
            }
            _ => self.buffer.handle_action(action, ctx),
        }
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> Vec<Effect> {
        match event {
            Event::OpenMessages => {
                self.active = true;
                self.sync();
                vec![]
            }
            _ => vec![],
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, ctx: &mut DrawContext) {
        self.sync();
        self.buffer.draw(frame, area, ctx);
    }

    fn cursor_position(&self) -> Option<(usize, usize, usize)> {
        self.buffer.cursor_position()
    }
}
