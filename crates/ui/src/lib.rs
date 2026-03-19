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
        .map(|s| {
            let dims = s.dims?;

            // File search cursor: position in side panel input row
            if let Some(ref fs) = s.file_search {
                let cx = fs
                    .cursor_pos
                    .min(dims.side_panel_width.saturating_sub(2) as usize)
                    as u16;
                let cy = 1u16; // row 1 of side panel (row 0 = toggles)
                return Some((cx, cy));
            }

            // Find-file cursor: absolute position on the status bar
            if let Some(ref ff) = s.find_file {
                let prefix_len = " Find file: ".len() as u16;
                let cx = prefix_len + ff.cursor as u16;
                let cy = dims.viewport_height.saturating_sub(dims.status_bar_height);
                if cx < dims.viewport_width {
                    return Some((cx, cy));
                }
                return None;
            }

            // Buffer cursor: compute relative, then add buffer area offset
            let (rel_cx, rel_cy) =
                display::cursor_inputs(&s).and_then(|c| display::compute_cursor_pos(&c))?;
            let buf_x = dims.side_width();
            let buf_y = 0u16; // buffer area starts at top of editor area
            Some((buf_x + rel_cx, buf_y + rel_cy))
        })
        .dedupe()
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
        .map(|s| {
            let fs = display::file_search_inputs(&s);
            let ff = if fs.is_none() {
                display::find_file_completion_inputs(&s)
            } else {
                None
            };
            let browser = if fs.is_none() && ff.is_none() {
                display::browser_inputs(&s)
            } else {
                None
            };
            (fs, ff, browser)
        })
        .dedupe()
        .map(|(fs, ff, browser)| {
            if let Some(f) = fs {
                return display::build_file_search_lines(&f);
            }
            if let Some(f) = ff {
                return display::build_find_file_completion_lines(&f);
            }
            browser
                .map(|b| display::build_browser_lines(&b))
                .unwrap_or_else(|| Rc::new(Vec::new()))
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
