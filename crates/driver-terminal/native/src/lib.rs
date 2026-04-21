//! Desktop-native async side of the terminal driver.
//!
//! Crossterm-specific: a background thread that polls
//! `crossterm::event::read`, translates crossterm events to the
//! `TermEvent` mirror types in `*-core`, and forwards them via mpsc.
//! Also the `paint` free function (ANSI escape emitter) and the
//! `RawModeGuard` RAII (raw mode + alternate screen). On mobile
//! platforms nothing from this crate applies; a different UI driver
//! takes over entirely.

use std::io::{self, Write};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

use crossterm::event::{
    self as ct_event, Event as CtEvent, KeyCode as CtKeyCode, KeyEvent as CtKeyEvent,
    KeyModifiers as CtKeyModifiers,
};
use led_core::Notifier;
use led_driver_terminal_core::{
    BodyModel, Dims, Frame, KeyCode, KeyEvent, KeyModifiers, Rect, SidePanelModel, StatusBarModel,
    TabBarModel, TermEvent, TerminalInputDriver, Trace,
};

#[cfg(test)]
use led_driver_terminal_core::{Layout, SidePanelRow};

/// Lifecycle marker for the native reader thread.
///
/// Detached on drop for the same reason as `FileReadNative`: joining
/// would deadlock whenever the marker drops before the driver (reverse
/// declaration order in tuple bindings). The worker exits within one
/// `poll()` tick (50ms) once `TerminalInputDriver` drops its receiver
/// and the worker's `send()` returns `Err`. Process exit reaps any
/// straggler.
pub struct TerminalInputNative {
    _marker: (),
}

/// Convenience: wire up `TerminalInputDriver` + its native reader
/// thread, seeding the initial dims from the live tty.
pub fn spawn(
    trace: Arc<dyn Trace>,
    notify: Notifier,
) -> io::Result<(TerminalInputDriver, TerminalInputNative)> {
    let (tx, rx) = mpsc::channel::<TermEvent>();

    // Seed the first render with real dimensions — otherwise the main
    // loop waits for a resize event that may never come.
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = tx.send(TermEvent::Resize(Dims { cols, rows }));
        notify.notify();
    }

    let reader_notify = notify;
    thread::Builder::new()
        .name("led-terminal-input".into())
        .spawn(move || reader_loop(tx, reader_notify))?;

    let driver = TerminalInputDriver::new(rx, trace);
    Ok((driver, TerminalInputNative { _marker: () }))
}

fn reader_loop(tx: Sender<TermEvent>, notify: Notifier) {
    // Blocking read — events push into the channel with zero extra
    // latency. A prior `poll(50ms)` here added perceptible stutter
    // when holding a key down.
    loop {
        match ct_event::read() {
            Ok(CtEvent::Key(k)) => {
                if let Some(ev) = translate_key(k) {
                    if tx.send(TermEvent::Key(ev)).is_err() {
                        return;
                    }
                    notify.notify();
                }
            }
            Ok(CtEvent::Resize(cols, rows)) => {
                if tx.send(TermEvent::Resize(Dims { cols, rows })).is_err() {
                    return;
                }
                notify.notify();
            }
            Ok(_) => {} // mouse/paste/focus ignored at M1
            Err(_) => return,
        }
    }
}

fn translate_key(k: CtKeyEvent) -> Option<KeyEvent> {
    let code = match k.code {
        CtKeyCode::Char(c) => KeyCode::Char(c),
        CtKeyCode::Enter => KeyCode::Enter,
        CtKeyCode::Tab => KeyCode::Tab,
        CtKeyCode::BackTab => KeyCode::BackTab,
        CtKeyCode::Backspace => KeyCode::Backspace,
        CtKeyCode::Delete => KeyCode::Delete,
        CtKeyCode::Esc => KeyCode::Esc,
        CtKeyCode::Left => KeyCode::Left,
        CtKeyCode::Right => KeyCode::Right,
        CtKeyCode::Up => KeyCode::Up,
        CtKeyCode::Down => KeyCode::Down,
        CtKeyCode::Home => KeyCode::Home,
        CtKeyCode::End => KeyCode::End,
        CtKeyCode::PageUp => KeyCode::PageUp,
        CtKeyCode::PageDown => KeyCode::PageDown,
        CtKeyCode::F(n) => KeyCode::F(n),
        _ => return None,
    };
    Some(KeyEvent {
        code,
        modifiers: translate_mods(k.modifiers),
    })
}

fn translate_mods(m: CtKeyModifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    if m.contains(CtKeyModifiers::SHIFT) {
        out = out | KeyModifiers::SHIFT;
    }
    if m.contains(CtKeyModifiers::CONTROL) {
        out = out | KeyModifiers::CONTROL;
    }
    if m.contains(CtKeyModifiers::ALT) {
        out = out | KeyModifiers::ALT;
    }
    out
}

// ── Output driver ──────────────────────────────────────────────────────

/// Sync counterpart of [`TerminalInputDriver`] — the "write side" of
/// the terminal. Takes a [`Frame`] and paints it; emits a trace
/// event around the call so goldens + any future capture harness
/// sees it. Course-correct #3: paint used to be a free function
/// with no driver wrapper and the render-tick trace was emitted by
/// the runtime itself. Now it lives here like every other driver.
pub struct TerminalOutputDriver {
    trace: Arc<dyn Trace>,
}

impl TerminalOutputDriver {
    pub fn new(trace: Arc<dyn Trace>) -> Self {
        Self { trace }
    }

    /// Paint a frame to `out`, skipping regions that match
    /// `last_frame`. The `execute` name matches the shape every
    /// other driver uses: a sync entry that accepts intent and
    /// performs the I/O.
    ///
    /// Regions compared: `side_panel`, `body`, `tab_bar`,
    /// `status_bar`. Each is `Arc`-wrapped so `PartialEq` is a
    /// pointer check when the memo cache hit. Held-key scroll only
    /// mutates `body` + `status_bar`; skipping the sidebar + tab
    /// bar + border drops ~150 crossterm ops per frame, which is
    /// where the stutter came from.
    pub fn execute<W: Write>(
        &self,
        frame: &Frame,
        last: Option<&Frame>,
        out: &mut W,
    ) -> io::Result<()> {
        self.trace.render_tick();
        paint(frame, last, out)
    }
}

// ── Painter ────────────────────────────────────────────────────────────

/// Paint the regions of `frame` that differ from `last` (or all of
/// them on first paint / layout change). At 120×40 a full repaint
/// is ~4800 cells; dirty-diffing avoids that cost on tight scroll
/// loops where only the body + status line change.
pub fn paint(frame: &Frame, last: Option<&Frame>, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue};

    queue!(out, cursor::Hide)?;

    // Layout change (resize, sidebar toggle) invalidates every
    // region — repaint in full. Otherwise diff sub-components.
    let layout_same = last.is_some_and(|l| l.layout == frame.layout);
    let force = !layout_same;

    if force || last.map(|l| &l.side_panel) != Some(&frame.side_panel)
        || last.map(|l| l.layout.side_area) != Some(frame.layout.side_area)
    {
        if let (Some(panel), Some(area)) = (&frame.side_panel, frame.layout.side_area) {
            paint_side_panel(panel, area, out)?;
        }
        // Border is layout-derived; repaint whenever layout changes
        // or when we're repainting the side panel anyway.
        if let Some(x) = frame.layout.side_border_x {
            let rows = frame.layout.editor_area.rows + frame.layout.tab_bar.rows;
            paint_side_border(x, rows, out)?;
        }
    }

    if force || last.map(|l| &l.body) != Some(&frame.body) {
        paint_body(&frame.body, frame.layout.editor_area, out)?;
    }

    if force || last.map(|l| &l.tab_bar) != Some(&frame.tab_bar) {
        paint_tab_bar(&frame.tab_bar, frame.layout.tab_bar, out)?;
    }

    if force || last.map(|l| &l.status_bar) != Some(&frame.status_bar) {
        paint_status_bar(&frame.status_bar, frame.layout.status_bar, out)?;
    }

    // Cursor placement last, on top of the finished frame. The
    // per-frame `Hide` above prevents flicker while drawing; the
    // trailing `Show` + `MoveTo` puts the cursor exactly where
    // `render_frame` wants it, or leaves it hidden if the active
    // view has no cursor (no content loaded, scrolled away, etc.).
    if let Some((col, row)) = frame.cursor {
        queue!(out, cursor::MoveTo(col, row), cursor::Show)?;
    }
    out.flush()
}

fn paint_tab_bar(bar: &TabBarModel, area: Rect, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    // Tab bar at the bottom of the editor area: second-to-last row.
    // Matches legacy led's ratatui layout + the goldens.
    queue!(out, cursor::MoveTo(area.x, area.y))?;
    let mut col: u16 = 0;
    for (i, label) in bar.labels.iter().enumerate() {
        let active = bar.active == Some(i);
        if active {
            queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
        }
        // No `format!(" {label} ")` — three Prints go straight through
        // crossterm's buffered writer with zero allocation.
        queue!(
            out,
            style::Print(" "),
            style::Print(label),
            style::Print(" ")
        )?;
        if active {
            queue!(out, style::SetAttribute(style::Attribute::NoReverse))?;
        }
        col = col.saturating_add(label.chars().count().saturating_add(2) as u16);
        if col >= area.cols {
            break;
        }
    }
    queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    Ok(())
}

fn paint_status_bar(s: &StatusBarModel, area: Rect, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    queue!(out, cursor::MoveTo(area.x, area.y))?;

    // Warn styling spans the whole row — set it before the first
    // print, reset after the row is complete.
    if s.is_warn {
        queue!(
            out,
            style::SetBackgroundColor(style::Color::Red),
            style::SetForegroundColor(style::Color::White),
            style::SetAttribute(style::Attribute::Bold),
        )?;
    }

    let cols = area.cols as usize;
    let left_cols = s.left.chars().count().min(cols);
    let right_cols = s.right.chars().count().min(cols - left_cols);
    let pad = cols - left_cols - right_cols;

    queue!(out, style::Print(s.left.as_ref()))?;
    for _ in 0..pad {
        queue!(out, style::Print(" "))?;
    }
    queue!(out, style::Print(s.right.as_ref()))?;

    if s.is_warn {
        queue!(
            out,
            style::SetAttribute(style::Attribute::Reset),
            style::ResetColor,
        )?;
    }
    queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    Ok(())
}

fn paint_body(body: &BodyModel, area: Rect, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    for row in 0..area.rows {
        queue!(out, cursor::MoveTo(area.x, area.y + row))?;
        let line: Option<&str> = match body {
            BodyModel::Empty => None,
            BodyModel::Pending { path_display } => match row {
                0 => Some(path_display.as_ref()),
                1 => Some("loading..."),
                _ => None,
            },
            BodyModel::Error {
                path_display,
                message,
            } => match row {
                0 => Some(path_display.as_ref()),
                1 => Some(message.as_ref()),
                _ => None,
            },
            BodyModel::Content { lines, .. } => lines.get(row as usize).map(String::as_str),
        };
        if let Some(line) = line {
            queue!(out, style::Print(line))?;
        }
        queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    }
    Ok(())
}

fn paint_side_panel(panel: &SidePanelModel, area: Rect, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, style};

    let cols = area.cols as usize;
    // Reused across rows so empty rows don't allocate.
    let blanks: String = " ".repeat(cols);

    for row in 0..area.rows {
        queue!(out, cursor::MoveTo(area.x, area.y + row))?;
        if let Some(entry) = panel.rows.get(row as usize) {
            // Two-space indent per depth, then chevron, then name.
            let mut line = String::with_capacity(cols);
            for _ in 0..entry.depth {
                line.push_str("  ");
            }
            match entry.chevron {
                Some(true) => line.push_str("\u{25bd} "),  // ▽
                Some(false) => line.push_str("\u{25b7} "), // ▷
                None => line.push_str("  "),
            }
            line.push_str(&entry.name);
            // Pad to full width so the reverse-video background
            // covers the row end-to-end.
            let ch_count = line.chars().count();
            if ch_count < cols {
                for _ in 0..(cols - ch_count) {
                    line.push(' ');
                }
            } else {
                // Truncate to fit.
                let truncated: String = line.chars().take(cols).collect();
                line = truncated;
            }
            if entry.selected {
                queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
                queue!(out, style::Print(line))?;
                queue!(out, style::SetAttribute(style::Attribute::NoReverse))?;
            } else {
                queue!(out, style::Print(line))?;
            }
        } else {
            // Print `cols` spaces — scoped to the side-panel area.
            // NOT `Clear(UntilNewLine)`: that would wipe the body
            // columns too, and because `paint_body` is skipped on
            // cache-hit (body Arc unchanged) the blanked cells would
            // stay blank until something else forces a body repaint.
            queue!(out, style::Print(&blanks))?;
        }
    }
    Ok(())
}

fn paint_side_border(x: u16, rows: u16, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, style};
    for row in 0..rows {
        queue!(out, cursor::MoveTo(x, row), style::Print("\u{2502}"))?; // │
    }
    Ok(())
}

// ── Raw mode guard ─────────────────────────────────────────────────────

pub struct RawModeGuard;

impl RawModeGuard {
    pub fn acquire() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        // `DisableLineWrap` is essential: without it, writing the
        // rightmost column of the rightmost row makes the terminal
        // auto-scroll, shifting every row up by one — the status-bar
        // paint would then corrupt what's visible. The editor paints
        // every cell explicitly, so it never needs auto-wrap.
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::terminal::DisableLineWrap,
        )?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // `paint` emits `cursor::Hide` each frame; the Hide state
        // persists across `LeaveAlternateScreen` on most terminals, so
        // we'd leave the user's shell with an invisible cursor. Show it
        // explicitly before handing the terminal back.
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::cursor::Show,
            crossterm::terminal::EnableLineWrap,
            crossterm::terminal::LeaveAlternateScreen,
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_tab_and_shift_tab() {
        let k = translate_key(CtKeyEvent::new(CtKeyCode::Tab, CtKeyModifiers::NONE)).unwrap();
        assert_eq!(k.code, KeyCode::Tab);
        assert!(k.modifiers.is_empty());

        let k = translate_key(CtKeyEvent::new(CtKeyCode::BackTab, CtKeyModifiers::SHIFT)).unwrap();
        assert_eq!(k.code, KeyCode::BackTab);
        assert!(k.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn translate_ctrl_c() {
        let k =
            translate_key(CtKeyEvent::new(CtKeyCode::Char('c'), CtKeyModifiers::CONTROL)).unwrap();
        assert_eq!(k.code, KeyCode::Char('c'));
        assert!(k.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn paint_renders_without_panicking() {
        use std::sync::Arc;
        let frame = Frame {
            tab_bar: TabBarModel {
                labels: Arc::new(vec!["a.rs".into(), "b.rs".into()]),
                active: Some(0),
            },
            body: BodyModel::Content {
                lines: Arc::new(vec!["line 1".into(), "line 2".into()]),
                cursor: Some((0, 0)),
            },
            status_bar: StatusBarModel::default(),
            side_panel: None,
            layout: Layout::compute(Dims { cols: 40, rows: 5 }, false),
            cursor: Some((0, 0)),
            dims: Dims { cols: 40, rows: 5 },
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&frame, None, &mut out).expect("paint to Vec<u8>");
        assert!(!out.is_empty());
    }

    #[test]
    fn paint_hides_cursor_when_frame_cursor_is_none() {
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body: BodyModel::Empty,
            status_bar: StatusBarModel::default(),
            side_panel: None,
            layout: Layout::compute(Dims { cols: 40, rows: 5 }, false),
            cursor: None,
            dims: Dims { cols: 40, rows: 5 },
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&frame, None, &mut out).expect("paint to Vec<u8>");
        // Empty frames still produce clear/hide sequences — just don't panic.
        assert!(!out.is_empty());
    }

    #[test]
    fn paint_side_panel_never_emits_clear_until_newline() {
        // Regression guard: `Clear(UntilNewLine)` at col 0 wipes the
        // body columns to the right of the panel, and because
        // `paint_body` skips on cache-hit the wipe stays visible
        // until something else forces a body repaint. The fix prints
        // `cols` spaces instead — no `\x1b[K` should escape.
        use std::sync::Arc;
        let panel = SidePanelModel {
            rows: Arc::new(vec![SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from("a.rs"),
                selected: true,
            }]),
            focused: true,
        };
        let area = Rect { x: 0, y: 0, cols: 24, rows: 10 };
        let mut out: Vec<u8> = Vec::new();
        paint_side_panel(&panel, area, &mut out).expect("paint");
        assert!(
            !out.windows(3).any(|w| w == b"\x1b[K"),
            "paint_side_panel emitted Clear(UntilNewLine); bytes: {out:?}",
        );
    }

    #[test]
    fn alt_tab_cache_hit_repaint_preserves_body_cells() {
        // End-to-end guard for the reported Alt-Tab regression.
        //
        // Construct two frames that differ ONLY in `side_panel.focused`
        // — the exact diff produced by `ToggleFocus` on an already-
        // visible panel. Paint the first frame with `last = None`
        // (full paint), then the second with `last = Some(first)`
        // (dirty-diff: body Arc is identical so `paint_body` is
        // skipped). Apply both byte streams to a small grid sim and
        // assert the body cells still contain the expected text.
        use std::sync::Arc;

        let dims = Dims { cols: 60, rows: 10 };
        let layout = Layout::compute(dims, true);

        let side_rows = Arc::new(vec![
            SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from("a.rs"),
                selected: true,
            },
            SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from("b.rs"),
                selected: false,
            },
        ]);
        // Only two panel rows but editor_area.rows is 8 — six empty
        // rows exercise the bug path.

        let body_lines = Arc::new(
            (0..(layout.editor_area.rows as usize))
                .map(|i| format!("  line {i:02}"))
                .collect::<Vec<_>>(),
        );
        let body = BodyModel::Content {
            lines: body_lines.clone(),
            cursor: Some((0, 2)),
        };

        let frame1 = Frame {
            tab_bar: TabBarModel {
                labels: Arc::new(vec!["a.rs".into()]),
                active: Some(0),
            },
            body: body.clone(),
            status_bar: StatusBarModel::default(),
            side_panel: Some(SidePanelModel {
                rows: side_rows.clone(),
                focused: false,
            }),
            layout,
            cursor: Some((layout.editor_area.x + 2, 0)),
            dims,
        };
        // Frame 2: same body (same Arc → cache hit), side_panel.focused flipped.
        let frame2 = Frame {
            side_panel: Some(SidePanelModel {
                rows: side_rows,
                focused: true,
            }),
            cursor: None, // focus=Side hides editor cursor
            ..frame1.clone()
        };

        let mut grid = Grid::new(dims);
        let mut out: Vec<u8> = Vec::new();
        paint(&frame1, None, &mut out).expect("paint frame1");
        grid.apply(&out);
        out.clear();
        paint(&frame2, Some(&frame1), &mut out).expect("paint frame2");
        grid.apply(&out);

        // Body column 25 ("  line NN" starts at editor_area.x=25).
        // After the second paint (cache-hit body skip), every body
        // row must still read "  line NN" — regression would leave
        // rows 2..=7 blank.
        for row in 0..layout.editor_area.rows {
            let want = format!("  line {row:02}");
            let got: String = grid.row_text(
                layout.editor_area.y + row,
                layout.editor_area.x,
                want.chars().count() as u16,
            );
            assert_eq!(got, want, "body cells wiped at row {row}");
        }
    }

    /// Tiny ANSI sim — enough to execute what `paint` emits
    /// (`MoveTo`, `Print`, `Clear(UntilNewLine)`, cursor hide/show,
    /// SGR attributes). SGR is ignored: we care about cell contents,
    /// not styling.
    struct Grid {
        cells: Vec<Vec<char>>,
        row: u16,
        col: u16,
    }

    impl Grid {
        fn new(d: Dims) -> Self {
            Self {
                cells: vec![vec![' '; d.cols as usize]; d.rows as usize],
                row: 0,
                col: 0,
            }
        }
        fn row_text(&self, row: u16, col: u16, n: u16) -> String {
            let r = &self.cells[row as usize];
            r[col as usize..(col + n) as usize].iter().collect()
        }
        fn put(&mut self, ch: char) {
            if (self.row as usize) < self.cells.len()
                && (self.col as usize) < self.cells[self.row as usize].len()
            {
                self.cells[self.row as usize][self.col as usize] = ch;
                self.col = self.col.saturating_add(1);
            }
        }
        fn clear_until_newline(&mut self) {
            if let Some(r) = self.cells.get_mut(self.row as usize) {
                for cell in r.iter_mut().skip(self.col as usize) {
                    *cell = ' ';
                }
            }
        }
        fn apply(&mut self, bytes: &[u8]) {
            // Decode as UTF-8 to handle the ▷/▽/│ glyphs.
            let s = std::str::from_utf8(bytes).expect("UTF-8 paint output");
            let mut it = s.chars().peekable();
            while let Some(c) = it.next() {
                if c != '\x1b' {
                    self.put(c);
                    continue;
                }
                // ESC — next must be '['.
                match it.next() {
                    Some('[') => {}
                    _ => continue,
                }
                let mut params = String::new();
                let final_byte = loop {
                    match it.next() {
                        Some(ch) if ch.is_ascii_alphabetic() => break ch,
                        Some(ch) => params.push(ch),
                        None => return,
                    }
                };
                match final_byte {
                    'H' => {
                        // <row>;<col>H — 1-indexed. Empty params → 1;1.
                        let (r, c) = match params.split_once(';') {
                            Some((a, b)) => (
                                a.parse::<u16>().unwrap_or(1),
                                b.parse::<u16>().unwrap_or(1),
                            ),
                            None if params.is_empty() => (1, 1),
                            None => (params.parse::<u16>().unwrap_or(1), 1),
                        };
                        self.row = r.saturating_sub(1);
                        self.col = c.saturating_sub(1);
                    }
                    'K' => {
                        // CSI n K — 0 (default) = from cursor to EOL.
                        self.clear_until_newline();
                    }
                    _ => {
                        // Ignore SGR (`m`), cursor show/hide (`h`/`l` with `?25`), etc.
                    }
                }
            }
        }
    }
}
