//! Desktop-native async side of the terminal driver.
//!
//! Crossterm-specific: a background thread that polls
//! `crossterm::event::read`, translates crossterm events to the
//! `TermEvent` mirror types in `*-core`, and forwards them via mpsc.
//! Also the `paint` free function (writes cells into a [`Buffer`])
//! and the `RawModeGuard` RAII (raw mode + alternate screen). On
//! mobile platforms nothing from this crate applies; a different UI
//! driver takes over entirely.

mod buffer;
mod render;

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
    Attrs, BodyModel, Color, CompletionPopupModel, Dims, Frame, KeyCode, KeyEvent, KeyModifiers,
    PopoverModel, PopoverSeverity, Rect, SidePanelModel, StatusBarModel, Style, TabBarModel,
    TermEvent, Theme, TerminalInputDriver, Trace,
};

use buffer::Buffer;

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
    /// `LED_PAINT_LOG=<path>` — when set, every frame's emitted
    /// bytes (post-diff) are appended here with a `=== FRAME N ===`
    /// delimiter. Debug aid for diagnosing paint flicker.
    log: Option<std::sync::Mutex<PaintLog>>,
    /// Double-buffered cell grid that mirrors the terminal's visible
    /// contents. Each `execute`:
    ///
    ///   1. Resizes both buffers + clears the real terminal when
    ///      `frame.dims` changes.
    ///   2. Paints the new frame into `buffers[current]`.
    ///   3. Computes `buffer::diff(prev, current)` — the minimal
    ///      per-cell update list — and streams it through
    ///      `render::draw_diff` to the real terminal.
    ///   4. Swaps `current = 1 - current` so next frame writes into
    ///      the now-unused buffer and diffs against the one we just
    ///      painted.
    ///
    /// Paint functions write cells by `(row, col)` so there's no
    /// `Clear(UntilNewLine)` to worry about; each cell's state is
    /// always explicit. Idle frames produce an empty diff and zero
    /// output bytes.
    ///
    /// `Mutex` because the driver is `&self` in execute; only the
    /// main loop paints today, but the trait requires interior
    /// mutability for the shared reference pattern.
    state: std::sync::Mutex<RenderState>,
}

struct PaintLog {
    file: std::fs::File,
    frame_n: u64,
}

struct RenderState {
    /// Two buffers; `current` is the one paint writes into this
    /// frame, `1 - current` holds the previous frame (what the
    /// terminal currently shows). Swap is an index flip — no alloc,
    /// no clone.
    buffers: [Buffer; 2],
    current: usize,
    dims: Dims,
}

impl RenderState {
    fn new() -> Self {
        // Seed with 0x0 — the first `execute` will resize to the
        // real terminal dims before painting, which will also emit
        // `Clear(All)` so the real screen matches.
        let dims = Dims { cols: 0, rows: 0 };
        Self {
            buffers: [Buffer::new(0, 0), Buffer::new(0, 0)],
            current: 0,
            dims,
        }
    }
}

impl TerminalOutputDriver {
    pub fn new(trace: Arc<dyn Trace>) -> Self {
        let log = std::env::var_os("LED_PAINT_LOG").and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&p)
                .ok()
                .map(|file| std::sync::Mutex::new(PaintLog { file, frame_n: 0 }))
        });
        Self {
            trace,
            log,
            state: std::sync::Mutex::new(RenderState::new()),
        }
    }

    /// Paint a frame to `out` using a double-buffered cell diff.
    ///
    /// `last` still feeds the paint function's own component-level
    /// dirty-diffing (skip `paint_side_panel` when Arc ptrs match,
    /// etc.) — that keeps paint cheap on idle scroll. The cell-grid
    /// diff then guarantees the emitted byte stream contains only
    /// cells that actually changed, regardless of what paint decided
    /// to re-render. On skipped regions the cells simply retain
    /// their previous-frame values (paint only overwrites what it
    /// touches), and the diff correctly finds no changes there.
    pub fn execute<W: Write>(
        &self,
        frame: &Frame,
        last: Option<&Frame>,
        theme: &Theme,
        out: &mut W,
    ) -> io::Result<()> {
        use crossterm::{cursor, queue, terminal};

        self.trace.render_tick();

        let mut state = self.state.lock().expect("render state poisoned");

        // Resize path: blow away both buffers + clear the real
        // terminal so our mirror and the visible grid match again.
        // A resize implies every cell needs repainting, which the
        // diff against the freshly-blanked `prev` buffer gives us
        // for free.
        let resized = frame.dims != state.dims;
        if resized {
            state.buffers[0].resize(frame.dims.rows, frame.dims.cols);
            state.buffers[1].resize(frame.dims.rows, frame.dims.cols);
            state.dims = frame.dims;
            queue!(
                out,
                terminal::Clear(terminal::ClearType::All),
                cursor::MoveTo(0, 0),
            )?;
        }

        // Partial-paint bookkeeping: the paint function only
        // overwrites cells in regions that actually changed. For
        // skipped regions the cells in `current` must already hold
        // the previous frame's values — otherwise the diff would
        // see blanks where the body was last frame.
        //
        // We maintain that invariant by copying `prev` into
        // `current` before paint. A resize blanks both buffers,
        // which also falls out of the copy: the source is the
        // freshly-blanked `prev`, so `current` ends up blank too
        // and the forced full repaint writes every cell.
        //
        // On resize we force a full repaint by treating `last` as
        // `None`: the prior frame's regions may have been at
        // different coordinates, so per-region dirty-diffing can't
        // be trusted.
        let current_idx = state.current;
        let log_bytes: Option<Vec<u8>> = {
            // Split-borrow so we can copy prev → current without
            // cloning the whole Vec. `split_at_mut` yields disjoint
            // mutable refs to the two buffer slots.
            let (a, b) = state.buffers.split_at_mut(1);
            let (current_buf, prev_buf) = if current_idx == 0 {
                (&mut a[0], &b[0])
            } else {
                (&mut b[0], &a[0])
            };
            current_buf.copy_from(prev_buf);

            let effective_last = if resized { None } else { last };
            paint(frame, effective_last, theme, current_buf);

            // Compute the minimal cell update list prev → current
            // and stream it to the real terminal. When LED_PAINT_LOG
            // is set, tee the output into a Vec<u8> so we can also
            // write it to the log file after flushing.
            let d = buffer::diff(prev_buf, current_buf);

            if self.log.is_some() {
                let mut capture: Vec<u8> = Vec::with_capacity(1024);
                render::draw_diff(&d, &mut capture)?;
                out.write_all(&capture)?;
                Some(capture)
            } else {
                render::draw_diff(&d, out)?;
                None
            }
        };

        // Re-emit the frame's intended cursor placement (or a Hide
        // when there's nothing to show) so the user's caret lines
        // up with the rendered grid. `draw_diff` leaves the cursor
        // wherever the last cell write landed; we always place it
        // explicitly here.
        match frame.cursor {
            Some((col, row)) => {
                queue!(out, cursor::MoveTo(col, row), cursor::Show)?;
            }
            None => {
                queue!(out, cursor::Hide)?;
            }
        }

        out.flush()?;

        if let Some(log) = &self.log {
            if let Ok(mut g) = log.lock() {
                g.frame_n += 1;
                let header = format!("\n=== FRAME {} ===\n", g.frame_n);
                let _ = g.file.write_all(header.as_bytes());
                if let Some(bytes) = &log_bytes {
                    let _ = g.file.write_all(bytes);
                }
                let _ = g.file.flush();
            }
        }

        // Swap buffers: next frame writes into the one we just used
        // as `prev`, and diffs against what we just emitted.
        state.current = 1 - state.current;

        Ok(())
    }
}

// ── Painter ────────────────────────────────────────────────────────────

/// Paint the regions of `frame` that differ from `last` (or all of
/// them on first paint / layout change) into `buf`. At 120×40 a
/// full repaint touches ~4800 cells; dirty-diffing avoids that cost
/// on tight scroll loops where only the body + status line change.
///
/// Skipped regions retain whatever cells `buf` already carried from
/// the previous frame — the driver's double-buffer swap means `buf`
/// comes in holding the last-frame snapshot of every cell, so the
/// downstream cell diff correctly finds no changes there.
pub(crate) fn paint(
    frame: &Frame,
    last: Option<&Frame>,
    theme: &Theme,
    buf: &mut Buffer,
) {
    // Layout change (resize, sidebar toggle) invalidates every
    // region — repaint in full. Otherwise diff sub-components.
    let layout_same = last.is_some_and(|l| l.layout == frame.layout);
    let force = !layout_same;

    if force || last.map(|l| &l.side_panel) != Some(&frame.side_panel)
        || last.map(|l| l.layout.side_area) != Some(frame.layout.side_area)
    {
        if let (Some(panel), Some(area)) = (&frame.side_panel, frame.layout.side_area) {
            paint_side_panel(panel, area, theme, buf);
        }
        // Border is layout-derived; repaint whenever layout changes
        // or when we're repainting the side panel anyway.
        if let Some(x) = frame.layout.side_border_x {
            let rows = frame.layout.editor_area.rows + frame.layout.tab_bar.rows;
            paint_side_border(x, rows, theme, buf);
        }
    }

    // When the popover changes (appears / disappears / moves /
    // content shifts), we must repaint the body too — the old box
    // needs to be erased and the new one drawn on a fresh canvas.
    let popover_changed = last.map(|l| &l.popover) != Some(&frame.popover);

    if force || popover_changed || last.map(|l| &l.body) != Some(&frame.body) {
        paint_body(&frame.body, frame.layout.editor_area, theme, buf);
    }

    // Paint ORDER matters: popover is drawn AFTER body so it
    // overlays. With the Buffer model the last write wins per cell,
    // so the popover's cells correctly sit on top of the body.
    if let Some(pop) = &frame.popover {
        paint_popover(pop, frame.layout.editor_area, frame.dims, theme, buf);
    }

    // Completion popup (M17). Paints after the body and diagnostic
    // popover so it wins any cell overlap; both popups showing at
    // once is rare (user isn't typing a completion while a diag
    // underline is visible) but the paint order keeps the
    // completion list on top when they do.
    if let Some(comp) = &frame.completion {
        paint_completion_popup(comp, frame.layout.editor_area, frame.dims, theme, buf);
    }

    if force || last.map(|l| &l.tab_bar) != Some(&frame.tab_bar) {
        paint_tab_bar(&frame.tab_bar, frame.layout.tab_bar, theme, buf);
    }

    if force || last.map(|l| &l.status_bar) != Some(&frame.status_bar) {
        paint_status_bar(&frame.status_bar, frame.layout.status_bar, theme, buf);
    }
}

fn paint_tab_bar(bar: &TabBarModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    // Tab bar at the bottom of the editor area: second-to-last row.
    // Matches legacy led's ratatui layout + the goldens.
    let row = area.y;
    let mut col = area.x;
    let right_edge = area.x.saturating_add(area.cols);
    for (i, label) in bar.labels.iter().enumerate() {
        if col >= right_edge {
            break;
        }
        let active = bar.active == Some(i);
        let style = if active {
            theme.tab_active
        } else {
            theme.tab_inactive
        };
        col = buf.put_str(row, col, " ", style);
        col = buf.put_str(row, col, label, style);
        col = buf.put_str(row, col, " ", style);
        if col >= right_edge {
            break;
        }
    }
    // Blank the rest of the row at the terminal default — matches
    // the old `Clear(UntilNewLine)`.
    buf.fill_row(row, col, right_edge, Style::default());
}

fn paint_status_bar(s: &StatusBarModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    let row = area.y;
    let mut col = area.x;
    let right_edge = area.x.saturating_add(area.cols);

    // Row-wide styling — set on every painted cell. `status_normal`
    // lets themers tint the happy-path bar too; the default is
    // unstyled so unthemed goldens don't move.
    let row_style = if s.is_warn {
        theme.status_warn
    } else {
        theme.status_normal
    };

    let cols = area.cols as usize;
    let left_cols = s.left.chars().count().min(cols);
    let right_cols = s.right.chars().count().min(cols - left_cols);
    let pad = cols - left_cols - right_cols;

    col = buf.put_str(row, col, s.left.as_ref(), row_style);
    for _ in 0..pad {
        if col >= right_edge {
            break;
        }
        col = buf.put_str(row, col, " ", row_style);
    }
    col = buf.put_str(row, col, s.right.as_ref(), row_style);
    // Any trailing width gets blanked with the row's background
    // style so a short right-side string still has the bar tint
    // carry to the edge.
    buf.fill_row(row, col, right_edge, row_style);
}

fn paint_body(body: &BodyModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    let ruler = theme
        .ruler_column
        .filter(|c| *c < area.cols)
        .filter(|_| !theme.ruler.is_default());

    let match_highlight = match body {
        BodyModel::Content { match_highlight, .. } => *match_highlight,
        _ => None,
    };

    let right_edge = area.x.saturating_add(area.cols);

    for row in 0..area.rows {
        let buf_row = area.y + row;
        let mut col = area.x;
        // Resolve the row's text + (for Content) syntax spans +
        // gutter-diagnostic severity + gutter-category (merged
        // LSP/git bar) + inline underlines. Non-Content variants
        // carry none of the extras.
        let (line, spans, gutter_diag, gutter_cat, row_diags): (
            Option<&str>,
            &[led_driver_terminal_core::LineSpan],
            Option<led_state_diagnostics::DiagnosticSeverity>,
            Option<led_core::IssueCategory>,
            &[led_driver_terminal_core::BodyDiagnostic],
        ) = match body {
            BodyModel::Empty => (None, &[], None, None, &[]),
            BodyModel::Pending { path_display } => match row {
                0 => (Some(path_display.as_ref()), &[], None, None, &[]),
                1 => (Some("loading..."), &[], None, None, &[]),
                _ => (None, &[], None, None, &[]),
            },
            BodyModel::Error {
                path_display,
                message,
            } => match row {
                0 => (Some(path_display.as_ref()), &[], None, None, &[]),
                1 => (Some(message.as_ref()), &[], None, None, &[]),
                _ => (None, &[], None, None, &[]),
            },
            BodyModel::Content { lines, .. } => match lines.get(row as usize) {
                Some(bl) => (
                    Some(bl.text.as_str()),
                    bl.spans.as_slice(),
                    bl.gutter_diagnostic,
                    bl.gutter_category,
                    bl.diagnostics.as_slice(),
                ),
                None => (None, &[], None, None, &[]),
            },
        };
        if let Some(line) = line {
            if spans.is_empty() {
                col = buf.put_str(buf_row, col, line, Style::default());
            } else {
                col = paint_syntax_line(line, spans, &theme.syntax, buf_row, col, buf);
            }
        }
        // Blank the rest of the row at terminal default — matches
        // the old `Clear(UntilNewLine)`.
        buf.fill_row(buf_row, col, right_edge, Style::default());

        // Git/PR/LSP change bar in gutter col 0: a single `▎`
        // (U+258E LEFT ONE EIGHTH BLOCK) coloured via
        // `category_style`. Matches legacy display.rs's col-1
        // positioning (our col 0 is display.rs's col 1 because
        // led's tab bar doesn't reserve the same leading column).
        // Painted before the diagnostic dot so the two cells are
        // independent.
        if let Some(cat) = gutter_cat {
            let style = theme.category_style(cat);
            buf.put_char(buf_row, area.x, '\u{258E}', style);
        }

        // Diagnostic gutter marker: a single ● in gutter col 1
        // (the second of the two gutter cells — matches legacy
        // display.rs positioning, so goldens line up). Overpaint
        // after the row text so it's not clobbered by syntax
        // styling.
        if let Some(severity) = gutter_diag {
            let style = *severity_style(&theme.diagnostics, severity);
            buf.put_char(buf_row, area.x + 1, '●', style);
        }

        // Diagnostic underlines: for each row-diagnostic, overpaint
        // the ranged cells with the severity style + underline attr.
        for d in row_diags {
            if d.col_end <= d.col_start {
                continue;
            }
            let Some(line) = line else { continue };
            let base = *severity_style(&theme.diagnostics, d.severity);
            let mut underlined = base;
            underlined.attrs.underline = true;
            let start_col = area.x + d.col_start;
            let take = (d.col_end - d.col_start) as usize;
            let mut c = start_col;
            for ch in line.chars().skip(d.col_start as usize).take(take) {
                if c >= right_edge {
                    break;
                }
                buf.put_char(buf_row, c, ch, underlined);
                c = c.saturating_add(1);
            }
        }

        // File-search match highlight: a single run of cells inside
        // one row. Overpaint the matched substring with
        // `theme.search_match` so the hit stands out the way it
        // does in the sidebar. Only active when the file-search
        // overlay's selected hit lives on this visible row.
        if let Some(mh) = match_highlight
            && mh.row == row
            && let Some(line) = line
            && mh.col_end > mh.col_start
        {
            let start_col = area.x + mh.col_start;
            let take = (mh.col_end - mh.col_start) as usize;
            let mut c = start_col;
            for ch in line.chars().skip(mh.col_start as usize).take(take) {
                if c >= right_edge {
                    break;
                }
                buf.put_char(buf_row, c, ch, theme.search_match);
                c = c.saturating_add(1);
            }
        }

        // Overpaint the ruler column on top of the row. A single
        // cell, styled with `theme.ruler`. If the row's text covers
        // that column the original character keeps its slot and
        // picks up the ruler style; otherwise we print a plain
        // space so the ruler renders as a vertical stripe.
        if let Some(rc) = ruler {
            let glyph: char = line
                .and_then(|l| l.chars().nth(rc as usize))
                .unwrap_or(' ');
            // Skip zero-width / control chars — safer to fall back
            // to a plain space than emit something that might push
            // the cursor.
            let painted = if glyph.is_control() { ' ' } else { glyph };
            buf.put_char(buf_row, area.x + rc, painted, theme.ruler);
        }
    }
}

/// Draw the cursor-line diagnostic popover — a floating box anchored
/// near the cursor. Matches legacy's UX exactly: dark-gray fill, no
/// border, one inner-padding column on each side, Y prefers above the
/// anchor line, X clamps so the box stays on screen.
fn paint_popover(
    pop: &PopoverModel,
    editor_area: Rect,
    dims: Dims,
    theme: &Theme,
    buf: &mut Buffer,
) {
    if pop.lines.is_empty() {
        return;
    }

    // Max content width across all non-rule lines; rule lines take
    // the full content width implicitly.
    let content_w = pop
        .lines
        .iter()
        .filter(|l| l.severity.is_some())
        .map(|l| l.text.chars().count())
        .max()
        .unwrap_or(1);
    // Outer width = content + 1-char inner padding on each side.
    let outer_w = (content_w + 2).min(editor_area.cols as usize).max(3);
    let height = pop
        .lines
        .len()
        .min(editor_area.rows as usize / 2)
        .max(1);
    let lines = &pop.lines[..height];

    // X: clamp so the right edge doesn't leave the editor area.
    let area_right = editor_area.x.saturating_add(editor_area.cols);
    let max_x = area_right.saturating_sub(outer_w as u16);
    let x = pop.anchor.0.min(max_x).max(editor_area.x);
    // Y: prefer above the anchor row, fall back to below if there
    // isn't room. The editor area's top edge is the clamp; rows
    // above the editor (tab bar) never receive popover content.
    let y = if pop.anchor.1 >= editor_area.y.saturating_add(height as u16) {
        pop.anchor.1.saturating_sub(height as u16)
    } else {
        let below = pop.anchor.1.saturating_add(1);
        let area_bottom = editor_area.y.saturating_add(editor_area.rows);
        below
            .min(area_bottom.saturating_sub(height as u16))
            .max(editor_area.y)
    };

    // Guard: never overflow the physical terminal.
    if x >= dims.cols || y >= dims.rows {
        return;
    }
    let outer_w = outer_w.min((dims.cols.saturating_sub(x)) as usize);
    if outer_w < 3 {
        return;
    }
    let height = height.min((dims.rows.saturating_sub(y)) as usize);
    if height == 0 {
        return;
    }

    let bg = Color::Indexed(236); // dark gray, matches legacy

    for (i, line) in lines.iter().take(height).enumerate() {
        let row = y + i as u16;
        let mut col = x;
        match line.severity {
            None => {
                // Horizontal rule: fill outer width with ─.
                let fg = Color::Indexed(245);
                let rule_style = Style {
                    fg: Some(fg),
                    bg: Some(bg),
                    attrs: Attrs::default(),
                };
                for _ in 0..outer_w {
                    col = buf.put_str(row, col, "─", rule_style);
                }
            }
            Some(sev) => {
                let sev_style = match sev {
                    PopoverSeverity::Error => theme.diagnostics.error,
                    PopoverSeverity::Warning => theme.diagnostics.warning,
                    PopoverSeverity::Info => theme.diagnostics.info,
                    PopoverSeverity::Hint => theme.diagnostics.hint,
                };
                let style = Style {
                    fg: sev_style.fg,
                    bg: Some(bg),
                    attrs: sev_style.attrs,
                };
                // Clip text to inner width (outer_w - 2), then
                // right-pad with spaces so the box fills even when
                // the message is shorter than the widest line.
                let inner_w = outer_w.saturating_sub(2);
                col = buf.put_str(row, col, " ", style);
                let mut written = 0usize;
                for ch in line.text.chars().take(inner_w) {
                    buf.put_char(row, col, ch, style);
                    col = col.saturating_add(1);
                    written += 1;
                }
                for _ in written..inner_w {
                    buf.put_char(row, col, ' ', style);
                    col = col.saturating_add(1);
                }
                buf.put_str(row, col, " ", style);
            }
        }
    }
}

/// Draw the LSP completion popup as a box anchored at (or above)
/// the cursor. Matches legacy's UX: dark-gray background for
/// unselected rows, blue highlight for the selected row; label
/// left-padded to the widest label in the window, then 2-space
/// separator, then detail (dim). Clamps to the editor area on
/// both axes.
fn paint_completion_popup(
    comp: &CompletionPopupModel,
    editor_area: Rect,
    dims: Dims,
    _theme: &Theme,
    buf: &mut Buffer,
) {
    if comp.rows.is_empty() {
        return;
    }

    // Dimensions. Outer width = label col + 2 (gap) + detail
    // col (when any row has a detail) + 2 (inner padding, 1 col
    // each side). Cap at the editor area so the popup never
    // overflows the sidebar / tab-bar region.
    let label_w = comp.label_width as usize;
    let detail_w = comp.detail_width as usize;
    let gap = if detail_w > 0 { 2 } else { 0 };
    let content_w = label_w + gap + detail_w;
    let outer_w = (content_w + 2)
        .min(editor_area.cols as usize)
        .max(3);
    let height = comp.rows.len();

    // X: clamp so the right edge doesn't leave the editor area.
    let area_right = editor_area.x.saturating_add(editor_area.cols);
    let max_x = area_right.saturating_sub(outer_w as u16);
    let x = comp.anchor.0.min(max_x).max(editor_area.x);
    // Y: prefer below the anchor. If it'd overflow the bottom
    // of the editor area, flip above.
    let below = comp.anchor.1.saturating_add(1);
    let area_bottom = editor_area.y.saturating_add(editor_area.rows);
    let y_below = below.min(area_bottom.saturating_sub(height as u16));
    let y = if below.saturating_add(height as u16) <= area_bottom {
        y_below
    } else if comp.anchor.1 >= editor_area.y.saturating_add(height as u16) {
        comp.anchor.1.saturating_sub(height as u16)
    } else {
        // Neither above nor below has room — paint what we can
        // starting at the top of the editor area.
        editor_area.y
    };

    // Guard: terminal smaller than our anchor.
    if x >= dims.cols || y >= dims.rows {
        return;
    }
    let outer_w = outer_w.min((dims.cols.saturating_sub(x)) as usize);
    if outer_w < 3 {
        return;
    }
    let height = height.min((dims.rows.saturating_sub(y)) as usize);
    if height == 0 {
        return;
    }

    // Styles: dark-gray normal, blue-bg selected. Hardcoded to
    // match legacy until `theme.completion_*` lands.
    let bg_normal = Color::Indexed(236); // dark gray
    let fg_normal = Color::Indexed(253); // near-white
    let bg_selected = Color::Indexed(24); // muted blue
    let fg_selected = Color::Indexed(231); // bright white
    let fg_detail = Color::Indexed(244); // dim gray

    for (i, row) in comp.rows.iter().take(height).enumerate() {
        let row_y = y + i as u16;
        let is_selected = i == comp.selected;
        let bg = if is_selected { bg_selected } else { bg_normal };
        let fg = if is_selected { fg_selected } else { fg_normal };
        let base = Style {
            fg: Some(fg),
            bg: Some(bg),
            attrs: Attrs::default(),
        };
        // Leading inner-padding space, label, label padding,
        // gap, detail + its pad, trailing inner-padding space.
        let mut col = x;
        buf.put_char(row_y, col, ' ', base);
        col = col.saturating_add(1);
        let label_chars: String = row.label.chars().take(label_w).collect();
        col = buf.put_str(row_y, col, &label_chars, base);
        // Pad label column to `label_w`.
        let label_printed = label_chars.chars().count();
        for _ in label_printed..label_w {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
        // Gap.
        for _ in 0..gap {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
        // Detail (dim fg except on selected row, where the
        // selection foreground wins so the whole row reads as
        // one highlighted band).
        let detail_style = if is_selected {
            base
        } else {
            Style {
                fg: Some(fg_detail),
                bg: Some(bg),
                attrs: Attrs::default(),
            }
        };
        let detail_printed = if let Some(d) = row.detail.as_ref() {
            let s: String = d.chars().take(detail_w).collect();
            col = buf.put_str(row_y, col, &s, detail_style);
            s.chars().count()
        } else {
            0
        };
        for _ in detail_printed..detail_w {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
        // Trailing padding.
        let right_edge = x + outer_w as u16;
        while col < right_edge {
            buf.put_char(row_y, col, ' ', base);
            col = col.saturating_add(1);
        }
    }
}

fn paint_side_panel(panel: &SidePanelModel, area: Rect, theme: &Theme, buf: &mut Buffer) {
    use led_driver_terminal_core::SidePanelMode;

    let cols = area.cols as usize;

    for row in 0..area.rows {
        let buf_row = area.y + row;
        let row_x = area.x;
        // File-search mode: row 0 is the toggle header. Paint it
        // with per-glyph styling so users can tell which of
        // `Aa` / `.*` / `=>` are on, then skip the usual row-print
        // path for that row.
        if row == 0
            && let SidePanelMode::FileSearch {
                case_sensitive,
                use_regex,
                replace_mode,
            } = panel.mode
        {
            paint_file_search_header(
                row_x,
                buf_row,
                cols,
                case_sensitive,
                use_regex,
                replace_mode,
                theme,
                buf,
            );
            continue;
        }
        if let Some(entry) = panel.rows.get(row as usize) {
            // Two-space indent per depth, then chevron, then name.
            let mut line = String::with_capacity(cols);
            match panel.mode {
                SidePanelMode::Browser => {
                    for _ in 0..entry.depth {
                        line.push_str("  ");
                    }
                    match entry.chevron {
                        Some(true) => line.push_str("\u{25bd} "),  // ▽
                        Some(false) => line.push_str("\u{25b7} "), // ▷
                        None => line.push_str("  "),
                    }
                }
                SidePanelMode::Completions | SidePanelMode::FileSearch { .. } => {
                    // No indent + no chevron column: the leaf name
                    // starts at col 0.
                }
            }
            line.push_str(&entry.name);
            // Browser mode reserves the right-most column for the
            // status letter (legacy display.rs:1396-1417). The
            // name region fills the remaining `cols - 1`; status
            // letter is painted separately below so it keeps the
            // category style even on non-selected rows whose name
            // is uncoloured.
            let reserve_status = matches!(panel.mode, SidePanelMode::Browser);
            let name_width = if reserve_status {
                cols.saturating_sub(1)
            } else {
                cols
            };
            let ch_count = line.chars().count();
            if ch_count < name_width {
                for _ in 0..(name_width - ch_count) {
                    line.push(' ');
                }
            } else if ch_count > name_width {
                let truncated: String = line.chars().take(name_width).collect();
                line = truncated;
            }
            let name_end_col = row_x + name_width as u16;
            if entry.selected {
                // Selection + category composition (legacy
                // display.rs:1381-1389):
                //   - focused selection → pure selection style
                //     (loud, wins over marker colour).
                //   - unfocused selection → selection bg
                //     patched with marker fg so the user still
                //     sees "this errored file is selected".
                let base_sel = if panel.focused {
                    theme.browser_selected_focused
                } else {
                    theme.browser_selected_unfocused
                };
                let sel_style = if !panel.focused && let Some(status) = entry.status {
                    let marker = theme.category_style(status.category);
                    Style {
                        fg: marker.fg.or(base_sel.fg),
                        bg: base_sel.bg,
                        attrs: base_sel.attrs,
                    }
                } else {
                    base_sel
                };
                buf.put_str(buf_row, row_x, &line, sel_style);
            } else if entry.replaced {
                // Replaced hit rows stay visible so the user can
                // Left-arrow back onto them to undo. Paint them
                // with the dim `search_hit_replaced` style so the
                // distinction is obvious.
                buf.put_str(buf_row, row_x, &line, theme.search_hit_replaced);
            } else if let Some((start, end)) = entry.match_range {
                // Split into three styled runs so the matched
                // substring picks up `theme.search_match` styling
                // without disturbing the surrounding row.
                paint_row_with_match(
                    &line,
                    start as usize,
                    end as usize,
                    theme,
                    buf_row,
                    row_x,
                    buf,
                );
            } else if let Some(status) = entry.status {
                // Category colouring: the whole name is painted in
                // the category's theme style so the user spots the
                // error/warn/git/PR row even without the letter.
                // Matches legacy display.rs:1387-1391 ("marker_style
                // as the row colour when not selected").
                let marker = theme.category_style(status.category);
                buf.put_str(buf_row, row_x, &line, marker);
            } else {
                buf.put_str(buf_row, row_x, &line, Style::default());
            }

            // Status letter in the right-most column (Browser mode
            // only). When the row is selected, the letter keeps the
            // selection-row style so the highlighted bar reads
            // continuous across the whole row (legacy
            // display.rs:1420-1425). Otherwise the letter uses the
            // category style (coloured fg).
            if reserve_status {
                match entry.status {
                    Some(status) => {
                        if entry.selected {
                            let sel_style = if panel.focused {
                                theme.browser_selected_focused
                            } else {
                                theme.browser_selected_unfocused
                            };
                            buf.put_char(buf_row, name_end_col, status.letter, sel_style);
                        } else {
                            let marker = theme.category_style(status.category);
                            buf.put_char(buf_row, name_end_col, status.letter, marker);
                        }
                    }
                    None => {
                        // No category. Still honour selection bg
                        // so the highlight bar doesn't stop one
                        // col short of the panel edge.
                        if entry.selected {
                            let sel_style = if panel.focused {
                                theme.browser_selected_focused
                            } else {
                                theme.browser_selected_unfocused
                            };
                            buf.put_char(buf_row, name_end_col, ' ', sel_style);
                        } else {
                            buf.put_char(buf_row, name_end_col, ' ', Style::default());
                        }
                    }
                }
            }
        } else {
            // Fill `cols` spaces — scoped to the side-panel area.
            // NOT `Clear(UntilNewLine)`: that would wipe the body
            // columns too. With the cell-grid model we can just
            // blank the panel's cells directly.
            buf.fill_row(buf_row, row_x, row_x + cols as u16, Style::default());
        }
    }
}

/// Split-print a non-selected hit row so the matched substring
/// picks up `theme.search_match` styling. `start` / `end` are char
/// offsets inside `line` (the post-padded / post-truncated text the
/// sidebar will print). Clamps gracefully when the range is out of
/// bounds — mis-computed indices shouldn't crash the painter.
fn paint_row_with_match(
    line: &str,
    start: usize,
    end: usize,
    theme: &Theme,
    row: u16,
    col_start: u16,
    buf: &mut Buffer,
) {
    let total = line.chars().count();
    let start = start.min(total);
    let end = end.min(total).max(start);
    if end == start {
        buf.put_str(row, col_start, line, Style::default());
        return;
    }
    let prefix: String = line.chars().take(start).collect();
    let matched: String = line.chars().skip(start).take(end - start).collect();
    let suffix: String = line.chars().skip(end).collect();
    let mut col = col_start;
    if !prefix.is_empty() {
        col = buf.put_str(row, col, &prefix, Style::default());
    }
    col = buf.put_str(row, col, &matched, theme.search_match);
    if !suffix.is_empty() {
        buf.put_str(row, col, &suffix, Style::default());
    }
}

fn paint_side_border(x: u16, rows: u16, theme: &Theme, buf: &mut Buffer) {
    for row in 0..rows {
        buf.put_char(row, x, '\u{2502}', theme.browser_border); // │
    }
}

/// File-search header row. Prints `" Aa   .*   =>"` with each of
/// the three two-char glyph pairs styled via `theme.search_toggle_on`
/// when the corresponding flag is set (plain otherwise). The leading
/// space and gaps between glyphs stay unstyled so the eye can
/// separate the three toggles at a glance. Pads with spaces to the
/// full panel width.
#[allow(clippy::too_many_arguments)]
fn paint_file_search_header(
    col_start: u16,
    row: u16,
    cols: usize,
    case_sensitive: bool,
    use_regex: bool,
    replace_mode: bool,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let on = theme.search_toggle_on;
    let mut printed = 0usize;
    let mut col = col_start;

    // Matches the text query.rs builds for row 0 of the overlay
    // (`" Aa   .*   =>"`), segment-for-segment. If that text
    // changes, update both sites.
    let segments: [(&str, bool); 6] = [
        (" ", false),
        ("Aa", case_sensitive),
        ("   ", false),
        (".*", use_regex),
        ("   ", false),
        ("=>", replace_mode),
    ];
    for (text, active) in segments {
        if printed >= cols {
            break;
        }
        let budget = cols - printed;
        let slice: String = text.chars().take(budget).collect();
        let style = if active { on } else { Style::default() };
        for ch in slice.chars() {
            buf.put_char(row, col, ch, style);
            col = col.saturating_add(1);
        }
        printed += slice.chars().count();
    }
    // Pad to the right edge so the row is fully repainted.
    for _ in printed..cols {
        buf.put_char(row, col, ' ', Style::default());
        col = col.saturating_add(1);
    }
}

/// Look up the style for a diagnostic severity.
fn severity_style<'a>(
    theme: &'a led_driver_terminal_core::DiagnosticsTheme,
    severity: led_state_diagnostics::DiagnosticSeverity,
) -> &'a Style {
    use led_state_diagnostics::DiagnosticSeverity::*;
    match severity {
        Error => &theme.error,
        Warning => &theme.warning,
        Info => &theme.info,
        Hint => &theme.hint,
    }
}

/// Paint one body row into `buf` slicing it into styled runs
/// according to the syntax spans the runtime computed. Gaps between
/// spans (and any suffix after the last span) render with the
/// syntax theme's `default` style so the gutter and any un-captured
/// characters still respect user theming. Returns the column AFTER
/// the last written cell so the caller can continue filling the row.
///
/// Spans are assumed non-overlapping and ascending in `col_start`.
/// The caller guarantees `col_end <= line_char_count` (runtime
/// clamps against `content_cols`), so we never overshoot the row.
fn paint_syntax_line(
    line: &str,
    spans: &[led_driver_terminal_core::LineSpan],
    syntax: &led_driver_terminal_core::SyntaxTheme,
    row: u16,
    col_start: u16,
    buf: &mut Buffer,
) -> u16 {
    use led_state_syntax::TokenKind;

    let style_for = |kind: TokenKind| -> &Style {
        match kind {
            TokenKind::Keyword => &syntax.keyword,
            TokenKind::Type => &syntax.type_,
            TokenKind::Function => &syntax.function,
            TokenKind::String => &syntax.string,
            TokenKind::Number => &syntax.number,
            TokenKind::Boolean => &syntax.boolean,
            TokenKind::Comment => &syntax.comment,
            TokenKind::Operator => &syntax.operator,
            TokenKind::Punctuation => &syntax.punctuation,
            TokenKind::Variable => &syntax.variable,
            TokenKind::Property => &syntax.property,
            TokenKind::Attribute => &syntax.attribute,
            TokenKind::Tag => &syntax.tag,
            TokenKind::Label => &syntax.label,
            TokenKind::Constant => &syntax.constant,
            TokenKind::Escape => &syntax.escape,
            TokenKind::Default => &syntax.default,
        }
    };

    let mut cursor_col: usize = 0;
    let mut out_col = col_start;
    for span in spans {
        let span_col_start = span.col_start as usize;
        let span_col_end = span.col_end as usize;
        if span_col_end <= cursor_col {
            // Malformed / overlapping input — skip the offending span
            // so we don't go backwards.
            continue;
        }
        if span_col_start > cursor_col {
            // Gap before this span: paint it with the default syntax
            // style (catches the gutter and any unclaimed glyphs).
            let default_style = syntax.default;
            for ch in line
                .chars()
                .skip(cursor_col)
                .take(span_col_start - cursor_col)
            {
                buf.put_char(row, out_col, ch, default_style);
                out_col = out_col.saturating_add(1);
            }
            cursor_col = span_col_start;
        }
        let s = *style_for(span.kind);
        for ch in line
            .chars()
            .skip(cursor_col)
            .take(span_col_end - cursor_col)
        {
            buf.put_char(row, out_col, ch, s);
            out_col = out_col.saturating_add(1);
        }
        cursor_col = span_col_end;
    }
    // Trailing suffix past the last span.
    let default_style = syntax.default;
    for ch in line.chars().skip(cursor_col) {
        buf.put_char(row, out_col, ch, default_style);
        out_col = out_col.saturating_add(1);
    }
    out_col
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
        // The driver's final `execute` may leave the cursor hidden
        // (frame.cursor = None); the Hide state persists across
        // `LeaveAlternateScreen` on most terminals, so we'd leave the
        // user's shell with an invisible cursor. Show it explicitly
        // before handing the terminal back.
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::cursor::Show,
            crossterm::terminal::EnableLineWrap,
            crossterm::terminal::LeaveAlternateScreen,
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// SIGTSTP → `fg` resume cycle.
///
/// Temporarily reverses the setup [`RawModeGuard`] performs on
/// acquire: leaves the alternate screen, re-enables line wrap,
/// shows the cursor, and disables raw mode. Then `raise(SIGTSTP)`
/// POSIX-stops the process — the kernel parks us until the
/// shell's `fg` (SIGCONT) wakes us up. On return the reverse of
/// the reverse runs: re-enable raw mode, re-enter alt screen,
/// disable line wrap. The caller is expected to bump the
/// lifecycle `force_redraw` counter so the next paint emits
/// every cell (the terminal's content is now whatever the user's
/// shell left behind during the suspended window).
///
/// On non-Unix targets the syscall path compiles to a no-op —
/// suspend silently does nothing, matching legacy behaviour.
pub fn suspend_and_resume<W: Write>(out: &mut W) -> io::Result<()> {
    crossterm::execute!(
        out,
        crossterm::cursor::Show,
        crossterm::terminal::EnableLineWrap,
        crossterm::terminal::LeaveAlternateScreen,
    )?;
    let _ = crossterm::terminal::disable_raw_mode();
    out.flush()?;

    #[cfg(unix)]
    // SAFETY: `libc::raise` is async-signal-safe and takes a
    // single `c_int`. SIGTSTP is defined by POSIX. The call
    // either stops the process (handled by the kernel) or
    // returns 0 on SIGCONT; no invariant we uphold can be
    // violated by either path.
    unsafe {
        libc::raise(libc::SIGTSTP);
    }

    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        out,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::terminal::DisableLineWrap,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use led_driver_terminal_core::NoopTrace;

    /// Paint a frame through the full driver path and return the
    /// emitted bytes. The trace is a `NoopTrace` so tests don't need
    /// to plumb in a real capture harness.
    fn execute_frame(
        frame: &Frame,
        last: Option<&Frame>,
        theme: &Theme,
    ) -> (TerminalOutputDriver, Vec<u8>) {
        let driver = TerminalOutputDriver::new(Arc::new(NoopTrace));
        let mut out: Vec<u8> = Vec::new();
        driver.execute(frame, last, theme, &mut out).expect("execute");
        (driver, out)
    }

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
                match_highlight: None,
            },
            status_bar: StatusBarModel::default(),
            side_panel: None,
            popover: None,
            completion: None,
            layout: Layout::compute(Dims { cols: 40, rows: 5 }, false),
            cursor: Some((0, 0)),
            dims: Dims { cols: 40, rows: 5 },
        };
        let (_driver, out) = execute_frame(&frame, None, &Theme::default());
        assert!(!out.is_empty());
    }

    #[test]
    fn paint_hides_cursor_when_frame_cursor_is_none() {
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body: BodyModel::Empty,
            status_bar: StatusBarModel::default(),
            side_panel: None,
            popover: None,
            completion: None,
            layout: Layout::compute(Dims { cols: 40, rows: 5 }, false),
            cursor: None,
            dims: Dims { cols: 40, rows: 5 },
        };
        let (_driver, out) = execute_frame(&frame, None, &Theme::default());
        // Empty frames still produce clear/hide sequences — just don't panic.
        assert!(!out.is_empty());
    }

    #[test]
    fn paint_side_panel_never_emits_clear_until_newline() {
        // Regression guard: `Clear(UntilNewLine)` at col 0 wipes the
        // body columns to the right of the panel, and because
        // `paint_body` skips on cache-hit the wipe stays visible
        // until something else forces a body repaint. The cell-grid
        // renderer in `render.rs` never emits `\x1b[K`; this test
        // verifies that property end-to-end through `execute`.
        use std::sync::Arc;
        let dims = Dims { cols: 24, rows: 10 };
        let layout = Layout::compute(dims, true);
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body: BodyModel::Empty,
            status_bar: StatusBarModel::default(),
            side_panel: Some(SidePanelModel {
                rows: Arc::new(vec![SidePanelRow {
                    depth: 0,
                    chevron: None,
                    name: Arc::<str>::from("a.rs"),
                    selected: true,
                    match_range: None,
                    replaced: false,
                    status: None,
                }]),
                focused: true,
                mode: Default::default(),
            }),
            popover: None,
            completion: None,
            layout,
            cursor: None,
            dims,
        };
        let (_driver, out) = execute_frame(&frame, None, &Theme::default());
        assert!(
            !out.windows(3).any(|w| w == b"\x1b[K"),
            "execute emitted Clear(UntilNewLine); bytes: {out:?}",
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
        //
        // In the cell-grid model the body's cells in `buf` survive
        // the skipped paint (they were written the previous frame
        // and stay there), so the emitted diff touches only the
        // panel cells — the body text must still read correctly in
        // the grid sim.
        use std::sync::Arc;

        let dims = Dims { cols: 60, rows: 10 };
        let layout = Layout::compute(dims, true);

        let side_rows = Arc::new(vec![
            SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from("a.rs"),
                selected: true,
                match_range: None,
                replaced: false,
                status: None,
            },
            SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from("b.rs"),
                selected: false,
                match_range: None,
                replaced: false,
                status: None,
            },
        ]);
        // Only two panel rows but editor_area.rows is 8 — six empty
        // rows exercise the bug path.

        let body_lines: Arc<Vec<led_driver_terminal_core::BodyLine>> = Arc::new(
            (0..(layout.editor_area.rows as usize))
                .map(|i| format!("  line {i:02}").into())
                .collect::<Vec<_>>(),
        );
        let body = BodyModel::Content {
            lines: body_lines.clone(),
            cursor: Some((0, 2)),
            match_highlight: None,
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
                mode: Default::default(),
            }),
            popover: None,
            completion: None,
            layout,
            cursor: Some((layout.editor_area.x + 2, 0)),
            dims,
        };
        // Frame 2: same body (same Arc → cache hit), side_panel.focused flipped.
        let frame2 = Frame {
            side_panel: Some(SidePanelModel {
                rows: side_rows,
                focused: true,
                mode: Default::default(),
            }),
            cursor: None, // focus=Side hides editor cursor
            ..frame1.clone()
        };

        // Same driver across both frames so the internal double
        // buffer carries body cells forward — that's the whole
        // point of the regression test.
        let driver = TerminalOutputDriver::new(Arc::new(NoopTrace));
        let theme = Theme::default();
        let mut grid = Grid::new(dims);
        let mut out: Vec<u8> = Vec::new();
        driver.execute(&frame1, None, &theme, &mut out).expect("frame1");
        grid.apply(&out);
        out.clear();
        driver
            .execute(&frame2, Some(&frame1), &theme, &mut out)
            .expect("frame2");
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

    /// Tiny ANSI sim — enough to execute what the driver emits
    /// (`MoveTo`, `Print`, cursor hide/show, SGR attributes). SGR
    /// is ignored: we care about cell contents, not styling.
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
        fn char_at(&self, row: u16, col: u16) -> char {
            self.cells[row as usize][col as usize]
        }
        fn put(&mut self, ch: char) {
            if (self.row as usize) < self.cells.len()
                && (self.col as usize) < self.cells[self.row as usize].len()
            {
                self.cells[self.row as usize][self.col as usize] = ch;
                self.col = self.col.saturating_add(1);
            }
        }
        fn clear_all(&mut self) {
            for r in self.cells.iter_mut() {
                for cell in r.iter_mut() {
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
                    'J' => {
                        // CSI n J — 2 = clear whole screen. The
                        // driver emits `Clear(All)` on resize.
                        if params == "2" {
                            self.clear_all();
                        }
                    }
                    _ => {
                        // Ignore SGR (`m`), cursor show/hide (`h`/`l` with `?25`), etc.
                    }
                }
            }
        }
    }

    #[test]
    fn hit_row_match_range_emits_three_styled_segments() {
        // A non-selected hit row with match_range. The painter
        // should split the match into prefix + matched + suffix —
        // detectable via the Grid sim + SGR scan. We pipe through
        // the full driver path so we exercise the render.rs output.
        use std::sync::Arc;
        use led_driver_terminal_core::SidePanelMode;

        // Completions mode — painter doesn't prepend indent or
        // chevron, so match_range is relative to entry.name directly.
        // Dims must be wide enough for the side panel to be visible
        // (cols > 25 and remaining editor width >= 25).
        let dims = Dims { cols: 60, rows: 3 };
        let layout = Layout::compute(dims, true);
        assert!(layout.side_area.is_some(), "side panel should be visible");
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body: BodyModel::Empty,
            status_bar: StatusBarModel::default(),
            side_panel: Some(SidePanelModel {
                rows: Arc::new(vec![SidePanelRow {
                    depth: 0,
                    chevron: None,
                    name: Arc::<str>::from("   1: foo_needle_bar"),
                    selected: false,
                    match_range: Some((10, 16)),
                    replaced: false,
                    status: None,
                }]),
                focused: false,
                mode: SidePanelMode::Completions,
            }),
            popover: None,
            completion: None,
            layout,
            cursor: None,
            dims,
        };
        let (_driver, out) = execute_frame(&frame, None, &Theme::default());
        let s = std::str::from_utf8(&out).expect("utf8");

        // The default `theme.search_match` sets `bold` — a bold
        // SGR must appear somewhere in the output (no match run
        // without it).
        assert!(
            s.contains("\x1b[1m"),
            "bold SGR expected for match run; got raw = {s:?}"
        );
        // The grid content must show "needle" at cols 10..16 of
        // the panel row — verifies that the three-segment split
        // actually paints the expected cells, regardless of how
        // the cell-grid renderer orders its SGR emissions.
        let mut grid = Grid::new(dims);
        grid.apply(&out);
        let row = layout.side_area.unwrap().y;
        let got: String = grid.row_text(row, 10, 6);
        assert_eq!(got, "needle", "grid cells should read 'needle'; raw = {s:?}");
    }

    #[test]
    fn paint_body_keeps_wrap_glyph_when_full_width_syntax_span_ends_adjacent() {
        // Regression for: on a wrapped markdown URL, the last
        // visible sub's `\` glyph disappeared because a syntax
        // span covered the entire content range up to the `\`
        // position. paint_syntax_line emits the trailing suffix
        // (`\`) after the last span — that one-char skip path
        // was the bit that broke.
        use std::sync::Arc;
        let dims = Dims { cols: 12, rows: 5 };
        let layout = Layout::compute(dims, false);
        // `body_rows` = 3. Simulate a non-last sub: text is
        // gutter + 9 content chars + `\` = 12 chars. One span
        // covering the full 9 content chars (cols 2..11) — the
        // situation the markdown grammar produces for a URL run.
        let body = BodyModel::Content {
            lines: Arc::new(vec![
                led_driver_terminal_core::BodyLine {
                    text: "  abcdefghi\\".to_string(),
                    spans: vec![led_driver_terminal_core::LineSpan {
                        col_start: 2,
                        col_end: 11,
                        kind: led_state_syntax::TokenKind::String,
                    }],
                    gutter_diagnostic: None,
                    gutter_category: None,
                    diagnostics: Vec::new(),
                },
                led_driver_terminal_core::BodyLine {
                    text: "  jkl".to_string(),
                    spans: Vec::new(),
                    gutter_diagnostic: None,
                    gutter_category: None,
                    diagnostics: Vec::new(),
                },
                led_driver_terminal_core::BodyLine {
                    text: "~ ".to_string(),
                    spans: Vec::new(),
                    gutter_diagnostic: None,
                    gutter_category: None,
                    diagnostics: Vec::new(),
                },
            ]),
            cursor: None,
            match_highlight: None,
        };
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body,
            status_bar: StatusBarModel::default(),
            side_panel: None,
            popover: None,
            completion: None,
            layout,
            cursor: None,
            dims,
        };
        let (_driver, out) = execute_frame(&frame, None, &Theme::default());
        let mut grid = Grid::new(dims);
        grid.apply(&out);
        let ex = layout.editor_area.x;
        assert_eq!(
            grid.char_at(0, ex + 11),
            '\\',
            "wrap glyph missing on sub-line with syntax span covering content",
        );
    }

    #[test]
    fn paint_body_prints_soft_wrap_backslash_at_sub_line_end() {
        // Regression guard for a reported bug where the `\`
        // continuation glyph wasn't visible on wrapped rows.
        // `body_model` appends `\` to non-last sub-lines; paint_body
        // must carry that char through to the terminal output.
        use std::sync::Arc;
        let dims = Dims { cols: 12, rows: 5 };
        let layout = Layout::compute(dims, false);
        // body_rows = 3 here. A two-sub-line logical line produces
        // two BodyLines; the first carries `\`, the second doesn't.
        let body = BodyModel::Content {
            lines: Arc::new(vec![
                "  abcdefghi\\".into(),
                "  jkl".into(),
                "~ ".into(),
            ]),
            cursor: None,
            match_highlight: None,
        };
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body,
            status_bar: StatusBarModel::default(),
            side_panel: None,
            popover: None,
            completion: None,
            layout,
            cursor: None,
            dims,
        };
        let (_driver, out) = execute_frame(&frame, None, &Theme::default());

        // Scan the grid: row 0 col 11 (editor_area.x + 11) must
        // render as `\`, row 1 col 11 must NOT be `\`.
        let mut grid = Grid::new(dims);
        grid.apply(&out);
        let ex = layout.editor_area.x;
        assert_eq!(
            grid.char_at(0, ex + 11),
            '\\',
            "wrap glyph missing on the sub-line's right edge",
        );
        assert_ne!(grid.char_at(1, ex + 11), '\\');
    }

    #[test]
    fn ruler_overpaints_single_column_when_theme_sets_ruler_column() {
        // Dims account for tab bar + status bar (each 1 row) so the
        // body gets `rows - 2`. 5 total = 3 body rows.
        use std::sync::Arc;
        let dims = Dims { cols: 30, rows: 5 };
        let layout = Layout::compute(dims, false);
        assert_eq!(layout.editor_area.rows, 3);
        let body = BodyModel::Content {
            lines: Arc::new(vec![
                "01234567890123456789".into(),
                "shorter".into(),
                "".into(),
            ]),
            cursor: None,
            match_highlight: None,
        };
        let mut theme = Theme::default();
        theme.ruler_column = Some(5);
        theme.ruler = Style {
            bg: Some(Color::rgb(0x22, 0x22, 0x22)),
            ..Style::default()
        };
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body,
            status_bar: StatusBarModel::default(),
            side_panel: None,
            popover: None,
            completion: None,
            layout,
            cursor: None,
            dims,
        };
        let (_driver, out) = execute_frame(&frame, None, &theme);

        let mut grid = Grid::new(dims);
        grid.apply(&out);
        let editor_x = layout.editor_area.x;
        // Row 0 col 5 = '5' (from "01234567890...").
        assert_eq!(grid.char_at(0, editor_x + 5), '5');
        // Row 1: "shorter" → s(0) h(1) o(2) r(3) t(4) e(5) r(6);
        // col 5 = 'e'. The ruler keeps the char, just restyles it.
        assert_eq!(grid.char_at(1, editor_x + 5), 'e');
        // Row 2: empty line → ruler paints a plain space.
        assert_eq!(grid.char_at(2, editor_x + 5), ' ');
    }
}
