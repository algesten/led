use led_buffer::Buffer;
use led_core::logging::SharedLog;
use led_core::{
    Action, Component, Context, DrawContext, Effect, Event, PanelClaim, PanelSlot, TabDescriptor,
};
use ratatui::Frame;
use ratatui::layout::Rect;

pub struct Messages {
    buffer: Buffer,
    log: SharedLog,
    last_synced: usize,
    active: bool,
    claims: Vec<PanelClaim>,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn should_auto_scroll(cursor_row: usize, line_count: usize) -> bool {
    let last_line = line_count.saturating_sub(1);
    cursor_row >= last_line
}

fn compute_auto_scroll_position(new_last: usize, current_scroll: usize) -> usize {
    if new_last > current_scroll + 20 {
        new_last.saturating_sub(10)
    } else {
        current_scroll
    }
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

        let doc = self.buffer.local_doc.as_mut().expect("messages local doc");

        let was_at_end = should_auto_scroll(self.buffer.cursor_row, doc.line_count());

        for entry in entries.iter().skip(skip) {
            let secs = entry.elapsed.as_secs_f64();
            let line = format!("[{secs:>10.3}] {:<5} {}\n", entry.level, entry.message);
            let len = doc.len_chars();
            doc.insert(len, &line);
        }

        self.last_synced = total;

        // Auto-scroll if user was at the end
        if was_at_end {
            let new_last = doc.line_count().saturating_sub(1);
            self.buffer.cursor_row = new_last;
            self.buffer.cursor_col = 0;
            self.buffer.scroll_offset =
                compute_auto_scroll_position(new_last, self.buffer.scroll_offset);
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
            Action::Abort => self.buffer.handle_action(action, ctx),
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
