use std::io;
use std::sync::Arc;

use led_core::rx::Stream;
use led_state::AppState;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod render;
mod style;

pub struct Ui;

/// One-way driver: renders state to the terminal.
pub fn driver(state: Stream<Arc<AppState>>) -> Ui {
    let mut terminal = setup();
    let mut last_redraw = 0u64;

    state.on(move |s: &Arc<AppState>| {
        let clear = s.force_redraw != last_redraw;
        last_redraw = s.force_redraw;
        if clear {
            terminal.clear().ok();
        }
        if let Err(e) = terminal.draw(|frame| render::render(s, frame)) {
            log::error!("render error: {}", e);
        }
    });

    Ui
}

fn setup() -> Terminal<CrosstermBackend<io::Stdout>> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::new(backend).expect("create terminal")
}
