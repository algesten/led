use std::io;
use std::rc::Rc;
use std::sync::Arc;

use led_core::combine;
use led_core::rx::Stream;
use led_state::AppState;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::Style;
use ratatui::text::Line;

mod display;
mod render;
mod style;

pub struct Ui;

/// One-way driver: renders state to the terminal.
pub fn driver(state: Stream<Arc<AppState>>) -> Ui {
    let mut terminal = setup();

    let display_s = state
        .map(|s| display::display_inputs(&s))
        .dedupe()
        .map(|opt| match opt {
            Some(d) => display::build_display_lines(&d),
            None => Rc::new(Vec::new()),
        })
        .stream();

    let cursor_s = state
        .map(|s| display::cursor_inputs(&s))
        .dedupe()
        .map(|opt| opt.and_then(|c| display::compute_cursor_pos(&c)))
        .stream();

    let status_s = state
        .map(|s| display::status_inputs(&s))
        .dedupe()
        .map(|s| display::build_status_content(&s))
        .stream();

    let tabs_s = state
        .map(|s| display::tabs_inputs(&s))
        .dedupe()
        .map(|opt| match opt {
            Some(t) => display::build_tab_entries(&t),
            None => Rc::new(display::TabsInputs {
                entries: vec![],
                inactive_style: Style::default(),
                gutter_width: 2,
            }),
        })
        .stream();

    let layout_s = state
        .map(|s| display::layout_inputs(&s))
        .dedupe()
        .filter_map(|l| display::build_layout(&l))
        .stream();

    let browser_s = state
        .map(|s| display::browser_inputs(&s))
        .dedupe()
        .map(|opt| match opt {
            Some(b) => display::build_browser_lines(&b),
            None => Rc::new(Vec::new()),
        })
        .stream();

    let render_s = combine!(display_s, cursor_s, status_s, tabs_s, layout_s, browser_s);

    let mut last_redraw = 0u64;

    render_s.on(
        move |opt: Option<&(
            Rc<Vec<Line<'static>>>,
            Option<(u16, u16)>,
            Rc<String>,
            Rc<display::TabsInputs>,
            display::LayoutInfo,
            Rc<Vec<Line<'static>>>,
        )>| {
            let Some((lines, cursor, status, tabs, layout, browser)) = opt else {
                return;
            };
            let clear = layout.force_redraw != last_redraw;
            last_redraw = layout.force_redraw;
            if clear {
                terminal.clear().ok();
            }
            if let Err(e) = terminal.draw(|frame| {
                render::render(frame, layout, lines, *cursor, status, tabs, browser);
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
