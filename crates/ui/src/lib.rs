use std::io;
use std::rc::Rc;

use led_core::combine;
use led_core::rx::Stream;
use led_state::AppState;
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

mod display;
mod render;
mod style;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiIn {
    EvictOneBuffer,
}

/// Two-way driver: renders state to the terminal, reports tab overflow.
pub fn driver(state: Stream<Rc<AppState>>) -> Stream<UiIn> {
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
                use led_state::file_search::FileSearchSelection;
                let max_col = dims.side_panel_width.saturating_sub(2) as usize;
                match fs.selection {
                    FileSearchSelection::SearchInput => {
                        let cx = fs.cursor_pos.min(max_col) as u16;
                        let cy = 1u16; // row 1 (row 0 = toggles)
                        return Some((cx, cy));
                    }
                    FileSearchSelection::ReplaceInput => {
                        let cx = fs.replace_cursor_pos.min(max_col) as u16;
                        let cy = 2u16; // row 2
                        return Some((cx, cy));
                    }
                    FileSearchSelection::Result(_) => {
                        // No text cursor on result rows
                        return None;
                    }
                }
            }

            // Find-file / save-as cursor: absolute position on the status bar
            if let Some(ref ff) = s.find_file {
                let label = match ff.mode {
                    led_state::FindFileMode::Open => " Find file: ",
                    led_state::FindFileMode::SaveAs => " Save as: ",
                };
                let prefix_len = label.len() as u16;
                let cx = prefix_len + ff.cursor as u16;
                let cy = dims.viewport_height.saturating_sub(dims.status_bar_height);
                if cx < dims.viewport_width {
                    return Some((cx, cy));
                }
                return None;
            }

            // Hide cursor when focus is not on the editor
            if s.focus != led_core::PanelSlot::Main {
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

    let overlay_s = state.map(|s| display::overlay_inputs(&s)).dedupe().stream();

    let render_s = combine!(
        display_s, cursor_s, status_s, tabs_s, layout_s, browser_s, overlay_s
    );

    let mut last_redraw = 0u64;
    let mut last_viewport = (0u16, 0u16);

    render_s.on(
        move |opt: Option<&(
            Rc<Vec<Line<'static>>>,
            Option<(u16, u16)>,
            Rc<String>,
            Rc<display::TabsInputs>,
            display::LayoutInfo,
            Rc<Vec<Line<'static>>>,
            display::OverlayContent,
        )>| {
            let Some((lines, cursor, status, tabs, layout, browser, overlay)) = opt else {
                return;
            };

            // Resize the fixed viewport when the terminal dimensions change.
            let vp = (layout.dims.viewport_width, layout.dims.viewport_height);
            if vp != last_viewport {
                last_viewport = vp;
                let area = Rect::new(0, 0, vp.0, vp.1);
                terminal.resize(area).ok();
            }

            let clear = layout.force_redraw != last_redraw;
            last_redraw = layout.force_redraw;
            if clear {
                terminal.clear().ok();
            }
            if let Err(e) = terminal.draw(|frame| {
                render::render(
                    frame, layout, lines, *cursor, status, tabs, browser, overlay,
                );
            }) {
                log::error!("render error: {}", e);
            }
        },
    );

    // Tab overflow detection: emit EvictOneBuffer when tabs overflow
    // and there is a non-active clean buffer that can actually be evicted.
    let overflow_s = state
        .filter(|s| tabs_overflow(s))
        .filter(|s| {
            s.tabs.iter().any(|tab| {
                !tab.is_preview
                    && Some(&tab.path) != s.active_tab.as_ref()
                    && s.buffers
                        .get(&tab.path)
                        .is_some_and(|b| b.is_materialized() && !b.is_dirty())
            })
        })
        .map(|_| UiIn::EvictOneBuffer)
        .stream();

    overflow_s
}

/// Check whether any buffer tab overflows the tab bar.
fn tabs_overflow(s: &AppState) -> bool {
    let Some(dims) = s.dims else { return false };
    let Some(tabs) = display::tabs_inputs(s) else {
        return false;
    };
    let editor_width = dims.editor_width();
    let mut x = tabs.gutter_width.saturating_sub(1);
    let mut first = true;
    for entry in &tabs.entries {
        if !first {
            x += 1;
        }
        first = false;
        let width = entry.label.chars().count() as u16;
        if x + width > editor_width {
            return true;
        }
        x += width;
    }
    false
}

fn setup() -> Terminal<CrosstermBackend<io::Stdout>> {
    let backend = CrosstermBackend::new(io::stdout());
    // Query the terminal size once at startup, then use a Fixed viewport
    // so ratatui skips the autoresize syscall (open /dev/tty) on every draw().
    // Subsequent resizes are driven by TerminalInput::Resize events.
    use ratatui::backend::Backend;
    let size = backend.size().expect("query terminal size");
    let area = Rect::new(0, 0, size.width, size.height);
    Terminal::with_options(backend, TerminalOptions { viewport: Viewport::Fixed(area) })
        .expect("create terminal")
}
