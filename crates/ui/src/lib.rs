use std::io;
use std::rc::Rc;
use std::sync::Arc;

use led_core::combine;
use led_core::rx::Stream;
use led_state::AppState;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;

mod display;
mod render;
mod style;

pub struct Ui;

/// One-way driver: renders state to the terminal.
pub fn driver(state: Stream<Arc<AppState>>) -> Ui {
    let mut terminal = setup();

    let display_s = state
        .filter_map(|s| display::display_inputs(&s))
        .dedupe()
        .map(|d| display::build_display_lines(&d))
        .stream();

    let cursor_s = state
        .filter_map(|s| display::cursor_inputs(&s))
        .dedupe()
        .map(|c| display::compute_cursor_pos(&c))
        .stream();

    let status_s = state
        .map(|s| display::status_inputs(&s))
        .dedupe()
        .map(|s| display::build_status_content(&s))
        .stream();

    let tabs_s = state
        .filter_map(|s| display::tabs_inputs(&s))
        .dedupe()
        .map(|t| display::build_tab_entries(&t))
        .stream();

    let layout_s = state
        .map(|s| display::layout_inputs(&s))
        .dedupe()
        .filter_map(|l| display::build_layout(&l))
        .stream();

    let render_s = combine!(display_s, cursor_s, status_s, tabs_s, layout_s);

    let mut last_redraw = 0u64;

    render_s.on(
        move |(lines, cursor, status, tabs, layout): &(
            Rc<Vec<Line<'static>>>,
            Option<(u16, u16)>,
            Rc<String>,
            Rc<display::TabsInputs>,
            display::LayoutInfo,
        )| {
            let clear = layout.force_redraw != last_redraw;
            last_redraw = layout.force_redraw;
            if clear {
                terminal.clear().ok();
            }
            if let Err(e) = terminal.draw(|frame| {
                render::render(frame, layout, lines, *cursor, status, tabs);
            }) {
                log::error!("render error: {}", e);
            }
        },
    );

    Ui
}

fn setup() -> Terminal<CrosstermBackend<io::Stdout>> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::new(backend).expect("create terminal")
}
