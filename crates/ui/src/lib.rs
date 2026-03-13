use std::io;
use std::sync::Arc;

use led_core::{AStream, StreamOpsExt};
use led_state::AppState;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio_stream::StreamExt;

mod render;
mod style;

pub struct Ui;

pub fn driver(state: impl AStream<Arc<AppState>>) -> Ui {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).expect("create terminal");

    let frames = state.reduce((0u64, false, None), |(last_redraw, _, _), s| {
        let clear = s.force_redraw != last_redraw;
        (s.force_redraw, clear, Some(s))
    });

    tokio::spawn(async move {
        tokio::pin!(frames);
        while let Some((_, clear, Some(s))) = frames.next().await {
            if clear {
                terminal.clear().ok();
            }
            if let Err(e) = terminal.draw(|frame| render::render(&s, frame)) {
                log::error!("render error: {}", e);
            }
        }
    });

    Ui
}
