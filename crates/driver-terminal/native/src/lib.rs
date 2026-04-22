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
    Attrs, BodyModel, Color, Dims, Frame, KeyCode, KeyEvent, KeyModifiers, Rect, SidePanelModel,
    StatusBarModel, Style, TabBarModel, TermEvent, Theme, TerminalInputDriver, Trace,
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
        theme: &Theme,
        out: &mut W,
    ) -> io::Result<()> {
        self.trace.render_tick();
        paint(frame, last, theme, out)
    }
}

// ── Painter ────────────────────────────────────────────────────────────

/// Paint the regions of `frame` that differ from `last` (or all of
/// them on first paint / layout change). At 120×40 a full repaint
/// is ~4800 cells; dirty-diffing avoids that cost on tight scroll
/// loops where only the body + status line change.
pub fn paint(
    frame: &Frame,
    last: Option<&Frame>,
    theme: &Theme,
    out: &mut impl Write,
) -> io::Result<()> {
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
            paint_side_panel(panel, area, theme, out)?;
        }
        // Border is layout-derived; repaint whenever layout changes
        // or when we're repainting the side panel anyway.
        if let Some(x) = frame.layout.side_border_x {
            let rows = frame.layout.editor_area.rows + frame.layout.tab_bar.rows;
            paint_side_border(x, rows, theme, out)?;
        }
    }

    if force || last.map(|l| &l.body) != Some(&frame.body) {
        paint_body(&frame.body, frame.layout.editor_area, theme, out)?;
    }

    if force || last.map(|l| &l.tab_bar) != Some(&frame.tab_bar) {
        paint_tab_bar(&frame.tab_bar, frame.layout.tab_bar, theme, out)?;
    }

    if force || last.map(|l| &l.status_bar) != Some(&frame.status_bar) {
        paint_status_bar(&frame.status_bar, frame.layout.status_bar, theme, out)?;
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

fn paint_tab_bar(
    bar: &TabBarModel,
    area: Rect,
    theme: &Theme,
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    // Tab bar at the bottom of the editor area: second-to-last row.
    // Matches legacy led's ratatui layout + the goldens.
    queue!(out, cursor::MoveTo(area.x, area.y))?;
    let mut col: u16 = 0;
    for (i, label) in bar.labels.iter().enumerate() {
        let active = bar.active == Some(i);
        let style = if active {
            &theme.tab_active
        } else {
            &theme.tab_inactive
        };
        apply_style(out, style)?;
        // No `format!(" {label} ")` — three Prints go straight through
        // crossterm's buffered writer with zero allocation.
        queue!(
            out,
            style::Print(" "),
            style::Print(label),
            style::Print(" ")
        )?;
        reset_style(out, style)?;
        col = col.saturating_add(label.chars().count().saturating_add(2) as u16);
        if col >= area.cols {
            break;
        }
    }
    queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    Ok(())
}

fn paint_status_bar(
    s: &StatusBarModel,
    area: Rect,
    theme: &Theme,
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    queue!(out, cursor::MoveTo(area.x, area.y))?;

    // Row-wide styling — set once before the first print, reset
    // after. `status_normal` lets themers tint the happy-path bar
    // too; the default is unstyled so unthemed goldens don't move.
    let row_style = if s.is_warn {
        &theme.status_warn
    } else {
        &theme.status_normal
    };
    apply_style(out, row_style)?;

    let cols = area.cols as usize;
    let left_cols = s.left.chars().count().min(cols);
    let right_cols = s.right.chars().count().min(cols - left_cols);
    let pad = cols - left_cols - right_cols;

    queue!(out, style::Print(s.left.as_ref()))?;
    for _ in 0..pad {
        queue!(out, style::Print(" "))?;
    }
    queue!(out, style::Print(s.right.as_ref()))?;

    reset_style(out, row_style)?;
    queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    Ok(())
}

fn paint_body(
    body: &BodyModel,
    area: Rect,
    theme: &Theme,
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    let ruler = theme
        .ruler_column
        .filter(|c| *c < area.cols)
        .filter(|_| !theme.ruler.is_default());

    let match_highlight = match body {
        BodyModel::Content { match_highlight, .. } => *match_highlight,
        _ => None,
    };

    for row in 0..area.rows {
        queue!(out, cursor::MoveTo(area.x, area.y + row))?;
        // Resolve the row's text + (for Content) syntax spans +
        // gutter-diagnostic severity + inline underlines. Non-
        // Content variants carry none of the extras.
        let (line, spans, gutter_diag, row_diags): (
            Option<&str>,
            &[led_driver_terminal_core::LineSpan],
            Option<led_state_diagnostics::DiagnosticSeverity>,
            &[led_driver_terminal_core::BodyDiagnostic],
        ) = match body {
            BodyModel::Empty => (None, &[], None, &[]),
            BodyModel::Pending { path_display } => match row {
                0 => (Some(path_display.as_ref()), &[], None, &[]),
                1 => (Some("loading..."), &[], None, &[]),
                _ => (None, &[], None, &[]),
            },
            BodyModel::Error {
                path_display,
                message,
            } => match row {
                0 => (Some(path_display.as_ref()), &[], None, &[]),
                1 => (Some(message.as_ref()), &[], None, &[]),
                _ => (None, &[], None, &[]),
            },
            BodyModel::Content { lines, .. } => match lines.get(row as usize) {
                Some(bl) => (
                    Some(bl.text.as_str()),
                    bl.spans.as_slice(),
                    bl.gutter_diagnostic,
                    bl.diagnostics.as_slice(),
                ),
                None => (None, &[], None, &[]),
            },
        };
        if let Some(line) = line {
            if spans.is_empty() {
                queue!(out, style::Print(line))?;
            } else {
                paint_syntax_line(line, spans, &theme.syntax, out)?;
            }
        }
        queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;

        // Diagnostic gutter marker: a single ● in gutter col 1
        // (the second of the two gutter cells — matches legacy
        // display.rs positioning, so goldens line up). Overpaint
        // after the row text so it's not clobbered by syntax
        // styling.
        if let Some(severity) = gutter_diag {
            let style = severity_style(&theme.diagnostics, severity);
            queue!(out, cursor::MoveTo(area.x + 1, area.y + row))?;
            apply_style(out, style)?;
            queue!(out, style::Print("●"))?;
            reset_style(out, style)?;
        }

        // Diagnostic underlines: for each row-diagnostic, overpaint
        // the ranged cells with the severity style + underline attr.
        for d in row_diags {
            if d.col_end <= d.col_start {
                continue;
            }
            let Some(line) = line else { continue };
            let slice: String = line
                .chars()
                .skip(d.col_start as usize)
                .take((d.col_end - d.col_start) as usize)
                .collect();
            if slice.is_empty() {
                continue;
            }
            let base = *severity_style(&theme.diagnostics, d.severity);
            let mut underlined = base;
            underlined.attrs.underline = true;
            queue!(
                out,
                cursor::MoveTo(area.x + d.col_start, area.y + row)
            )?;
            apply_style(out, &underlined)?;
            queue!(out, style::Print(slice))?;
            reset_style(out, &underlined)?;
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
            let matched: String = line
                .chars()
                .skip(mh.col_start as usize)
                .take((mh.col_end - mh.col_start) as usize)
                .collect();
            if !matched.is_empty() {
                queue!(
                    out,
                    cursor::MoveTo(area.x + mh.col_start, area.y + row)
                )?;
                apply_style(out, &theme.search_match)?;
                queue!(out, style::Print(matched))?;
                reset_style(out, &theme.search_match)?;
            }
        }

        // Overpaint the ruler column on top of the row. A single
        // cell, styled with `theme.ruler`. If the row's text covers
        // that column the original character keeps its slot and
        // picks up the ruler style; otherwise we print a plain
        // space so the ruler renders as a vertical stripe.
        if let Some(col) = ruler {
            let glyph: char = line
                .and_then(|l| l.chars().nth(col as usize))
                .unwrap_or(' ');
            queue!(out, cursor::MoveTo(area.x + col, area.y + row))?;
            apply_style(out, &theme.ruler)?;
            // Skip repainting zero-width / control chars — safer to
            // fall back to a plain space than emit something that
            // might push the cursor.
            let painted = if glyph.is_control() { ' ' } else { glyph };
            queue!(out, style::Print(painted))?;
            reset_style(out, &theme.ruler)?;
        }
    }
    Ok(())
}

fn paint_side_panel(
    panel: &SidePanelModel,
    area: Rect,
    theme: &Theme,
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{cursor, queue, style};
    use led_driver_terminal_core::SidePanelMode;

    let cols = area.cols as usize;
    // Reused across rows so empty rows don't allocate.
    let blanks: String = " ".repeat(cols);

    for row in 0..area.rows {
        queue!(out, cursor::MoveTo(area.x, area.y + row))?;
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
                cols,
                case_sensitive,
                use_regex,
                replace_mode,
                theme,
                out,
            )?;
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
                let sel_style = if panel.focused {
                    &theme.browser_selected_focused
                } else {
                    &theme.browser_selected_unfocused
                };
                apply_style(out, sel_style)?;
                queue!(out, style::Print(line))?;
                reset_style(out, sel_style)?;
            } else if entry.replaced {
                // Replaced hit rows stay visible so the user can
                // Left-arrow back onto them to undo. Paint them
                // with the dim `search_hit_replaced` style so the
                // distinction is obvious.
                apply_style(out, &theme.search_hit_replaced)?;
                queue!(out, style::Print(line))?;
                reset_style(out, &theme.search_hit_replaced)?;
            } else if let Some((start, end)) = entry.match_range {
                // Split into three prints so the matched substring
                // picks up `theme.search_match` styling without
                // disturbing the surrounding row.
                paint_row_with_match(&line, start as usize, end as usize, theme, out)?;
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
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{queue, style};
    let total = line.chars().count();
    let start = start.min(total);
    let end = end.min(total).max(start);
    if end == start {
        queue!(out, style::Print(line))?;
        return Ok(());
    }
    let prefix: String = line.chars().take(start).collect();
    let matched: String = line.chars().skip(start).take(end - start).collect();
    let suffix: String = line.chars().skip(end).collect();
    if !prefix.is_empty() {
        queue!(out, style::Print(prefix))?;
    }
    apply_style(out, &theme.search_match)?;
    queue!(out, style::Print(matched))?;
    reset_style(out, &theme.search_match)?;
    if !suffix.is_empty() {
        queue!(out, style::Print(suffix))?;
    }
    Ok(())
}

fn paint_side_border(
    x: u16,
    rows: u16,
    theme: &Theme,
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{cursor, queue, style};
    apply_style(out, &theme.browser_border)?;
    for row in 0..rows {
        queue!(out, cursor::MoveTo(x, row), style::Print("\u{2502}"))?; // │
    }
    reset_style(out, &theme.browser_border)?;
    Ok(())
}

/// File-search header row. Prints `" Aa   .*   =>"` with each of
/// the three two-char glyph pairs styled via `theme.search_toggle_on`
/// when the corresponding flag is set (plain otherwise). The leading
/// space and gaps between glyphs stay unstyled so the eye can
/// separate the three toggles at a glance. Pads with spaces to the
/// full panel width.
fn paint_file_search_header(
    cols: usize,
    case_sensitive: bool,
    use_regex: bool,
    replace_mode: bool,
    theme: &Theme,
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{queue, style};

    let on = &theme.search_toggle_on;
    let mut printed = 0usize;

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
        if active {
            apply_style(out, on)?;
            queue!(out, style::Print(&slice))?;
            reset_style(out, on)?;
        } else {
            queue!(out, style::Print(&slice))?;
        }
        printed += slice.chars().count();
    }
    // Pad to the right edge so the row is fully repainted.
    for _ in printed..cols {
        queue!(out, style::Print(" "))?;
    }
    Ok(())
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

/// Print one body row slicing it into styled runs according to the
/// syntax spans the runtime computed. Gaps between spans (and any
/// suffix after the last span) render with the syntax theme's
/// `default` style so the gutter and any un-captured characters
/// still respect user theming.
///
/// Spans are assumed non-overlapping and ascending in `col_start`.
/// The caller guarantees `col_end <= line_char_count` (runtime
/// clamps against `content_cols`), so we never overshoot the row.
fn paint_syntax_line(
    line: &str,
    spans: &[led_driver_terminal_core::LineSpan],
    syntax: &led_driver_terminal_core::SyntaxTheme,
    out: &mut impl Write,
) -> io::Result<()> {
    use crossterm::{queue, style};
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
    for span in spans {
        let col_start = span.col_start as usize;
        let col_end = span.col_end as usize;
        if col_end <= cursor_col {
            // Malformed / overlapping input — skip the offending span
            // so we don't go backwards.
            continue;
        }
        if col_start > cursor_col {
            // Gap before this span: paint it with the default syntax
            // style (catches the gutter and any unclaimed glyphs).
            let gap_text: String = line
                .chars()
                .skip(cursor_col)
                .take(col_start - cursor_col)
                .collect();
            if !gap_text.is_empty() {
                let default_style = &syntax.default;
                apply_style(out, default_style)?;
                queue!(out, style::Print(gap_text))?;
                reset_style(out, default_style)?;
            }
            cursor_col = col_start;
        }
        let span_text: String = line
            .chars()
            .skip(cursor_col)
            .take(col_end - cursor_col)
            .collect();
        if !span_text.is_empty() {
            let s = style_for(span.kind);
            apply_style(out, s)?;
            queue!(out, style::Print(span_text))?;
            reset_style(out, s)?;
        }
        cursor_col = col_end;
    }
    // Trailing suffix past the last span.
    let tail: String = line.chars().skip(cursor_col).collect();
    if !tail.is_empty() {
        let default_style = &syntax.default;
        apply_style(out, default_style)?;
        queue!(out, style::Print(tail))?;
        reset_style(out, default_style)?;
    }
    Ok(())
}

// ── Theme → ANSI helpers ───────────────────────────────────────────────

/// Emit the SetForeground / SetBackground / SetAttribute escapes for
/// a [`Style`]. No-op when the style is the default — the painter
/// won't touch terminal state, so goldens stay pixel-identical with
/// an unstyled theme.
fn apply_style(out: &mut impl Write, s: &Style) -> io::Result<()> {
    use crossterm::{queue, style};
    if s.is_default() {
        return Ok(());
    }
    if let Some(fg) = s.fg {
        queue!(out, style::SetForegroundColor(to_ct_color(fg)))?;
    }
    if let Some(bg) = s.bg {
        queue!(out, style::SetBackgroundColor(to_ct_color(bg)))?;
    }
    apply_attrs(out, s.attrs)?;
    Ok(())
}

fn apply_attrs(out: &mut impl Write, a: Attrs) -> io::Result<()> {
    use crossterm::{queue, style};
    if a.bold {
        queue!(out, style::SetAttribute(style::Attribute::Bold))?;
    }
    if a.reverse {
        queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
    }
    if a.underline {
        queue!(out, style::SetAttribute(style::Attribute::Underlined))?;
    }
    Ok(())
}

/// Undo `apply_style`. A blanket `Attribute::Reset` + `ResetColor`
/// covers every case including the mixed attr+color legacy status
/// bar; a default style is a no-op.
fn reset_style(out: &mut impl Write, s: &Style) -> io::Result<()> {
    use crossterm::{queue, style};
    if s.is_default() {
        return Ok(());
    }
    queue!(
        out,
        style::SetAttribute(style::Attribute::Reset),
        style::ResetColor,
    )?;
    Ok(())
}

fn to_ct_color(c: Color) -> crossterm::style::Color {
    match c {
        // Indexed 0-15 → crossterm's named variants, which emit
        // the short `ESC[3Nm` / `ESC[4Nm` escapes terminals honour
        // via the user's palette config. Indexed 16-255 → the
        // `ESC[38;5;Nm` / `ESC[48;5;Nm` 256-color escapes.
        Color::Indexed(0) => crossterm::style::Color::Black,
        Color::Indexed(1) => crossterm::style::Color::DarkRed,
        Color::Indexed(2) => crossterm::style::Color::DarkGreen,
        Color::Indexed(3) => crossterm::style::Color::DarkYellow,
        Color::Indexed(4) => crossterm::style::Color::DarkBlue,
        Color::Indexed(5) => crossterm::style::Color::DarkMagenta,
        Color::Indexed(6) => crossterm::style::Color::DarkCyan,
        Color::Indexed(7) => crossterm::style::Color::Grey,
        Color::Indexed(8) => crossterm::style::Color::DarkGrey,
        Color::Indexed(9) => crossterm::style::Color::Red,
        Color::Indexed(10) => crossterm::style::Color::Green,
        Color::Indexed(11) => crossterm::style::Color::Yellow,
        Color::Indexed(12) => crossterm::style::Color::Blue,
        Color::Indexed(13) => crossterm::style::Color::Magenta,
        Color::Indexed(14) => crossterm::style::Color::Cyan,
        Color::Indexed(15) => crossterm::style::Color::White,
        Color::Indexed(n) => crossterm::style::Color::AnsiValue(n),
        Color::Rgb { r, g, b } => crossterm::style::Color::Rgb { r, g, b },
    }
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
                match_highlight: None,
            },
            status_bar: StatusBarModel::default(),
            side_panel: None,
            layout: Layout::compute(Dims { cols: 40, rows: 5 }, false),
            cursor: Some((0, 0)),
            dims: Dims { cols: 40, rows: 5 },
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&frame, None, &Theme::default(), &mut out).expect("paint to Vec<u8>");
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
        paint(&frame, None, &Theme::default(), &mut out).expect("paint to Vec<u8>");
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
                match_range: None,
                replaced: false,
            }]),
            focused: true,
        mode: Default::default(),
        };
        let area = Rect { x: 0, y: 0, cols: 24, rows: 10 };
        let mut out: Vec<u8> = Vec::new();
        paint_side_panel(&panel, area, &Theme::default(), &mut out).expect("paint");
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
                match_range: None,
                replaced: false,
            },
            SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from("b.rs"),
                selected: false,
                match_range: None,
                replaced: false,
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

        let mut grid = Grid::new(dims);
        let mut out: Vec<u8> = Vec::new();
        paint(&frame1, None, &Theme::default(), &mut out).expect("paint frame1");
        grid.apply(&out);
        out.clear();
        paint(&frame2, Some(&frame1), &Theme::default(), &mut out).expect("paint frame2");
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

    #[test]
    fn hit_row_match_range_emits_three_styled_segments() {
        // A non-selected hit row with match_range. The painter
        // should split the print into prefix + matched + suffix —
        // detectable by scanning the raw ANSI output for the
        // `search_match` bold + fg SGR between the prefix and the
        // suffix text.
        use std::sync::Arc;
        use led_driver_terminal_core::SidePanelMode;
        // Completions mode — painter doesn't prepend indent or
        // chevron, so match_range is relative to entry.name directly.
        let panel = SidePanelModel {
            rows: Arc::new(vec![SidePanelRow {
                depth: 0,
                chevron: None,
                name: Arc::<str>::from("   1: foo_needle_bar"),
                selected: false,
                match_range: Some((10, 16)),
                replaced: false,
            }]),
            focused: false,
            mode: SidePanelMode::Completions,
        };
        let area = Rect { x: 0, y: 0, cols: 24, rows: 1 };
        let mut out: Vec<u8> = Vec::new();
        paint_side_panel(&panel, area, &Theme::default(), &mut out).expect("paint");
        let s = std::str::from_utf8(&out).expect("utf8");
        // "needle" substring must appear after a bold SGR (1). The
        // surrounding prefix / suffix come in on plain prints.
        let bold_pos = s.find("\x1b[1m").expect("bold SGR emitted");
        let needle_pos = s.find("needle").expect("match text printed");
        assert!(
            bold_pos < needle_pos,
            "bold should be set before printing the match; got raw = {s:?}"
        );
        // After the match we emit a Reset; check that "bar" comes
        // after a Reset SGR.
        let reset_pos = s[needle_pos..].find("\x1b[0m").map(|i| needle_pos + i);
        let bar_pos = s.find("_bar").expect("suffix printed");
        assert!(
            reset_pos.is_some_and(|r| r < bar_pos),
            "reset should fire between match and suffix; got raw = {s:?}"
        );
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

        let mut out: Vec<u8> = Vec::new();
        paint_body(&body, layout.editor_area, &theme, &mut out).expect("paint_body");

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
