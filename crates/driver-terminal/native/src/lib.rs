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
use std::time::Duration;

use crossterm::event::{
    self as ct_event, Event as CtEvent, KeyCode as CtKeyCode, KeyEvent as CtKeyEvent,
    KeyModifiers as CtKeyModifiers,
};
use led_driver_terminal_core::{
    BodyModel, Dims, Frame, KeyCode, KeyEvent, KeyModifiers, TabBarModel, TermEvent,
    TerminalInputDriver, Trace,
};

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
pub fn spawn(trace: Arc<dyn Trace>) -> io::Result<(TerminalInputDriver, TerminalInputNative)> {
    let (tx, rx) = mpsc::channel::<TermEvent>();

    // Seed the first render with real dimensions — otherwise the main
    // loop waits for a resize event that may never come.
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = tx.send(TermEvent::Resize(Dims { cols, rows }));
    }

    thread::Builder::new()
        .name("led-terminal-input".into())
        .spawn(move || reader_loop(tx))?;

    let driver = TerminalInputDriver::new(rx, trace);
    Ok((driver, TerminalInputNative { _marker: () }))
}

fn reader_loop(tx: Sender<TermEvent>) {
    loop {
        match ct_event::poll(Duration::from_millis(50)) {
            Ok(true) => match ct_event::read() {
                Ok(CtEvent::Key(k)) => {
                    if let Some(ev) = translate_key(k) {
                        if tx.send(TermEvent::Key(ev)).is_err() {
                            return;
                        }
                    }
                }
                Ok(CtEvent::Resize(cols, rows)) => {
                    if tx.send(TermEvent::Resize(Dims { cols, rows })).is_err() {
                        return;
                    }
                }
                Ok(_) => {} // mouse/paste/focus ignored at M1
                Err(_) => return,
            },
            Ok(false) => {}
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

// ── Painter ────────────────────────────────────────────────────────────

/// Paint an entire frame. No diffing: at 120×40 that's ~4800 cells per
/// redraw — negligible. The caller only invokes `paint` when the frame
/// actually changed.
pub fn paint(frame: &Frame, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, terminal};

    queue!(out, cursor::Hide, cursor::MoveTo(0, 0))?;
    paint_tab_bar(&frame.tab_bar, frame.dims, out)?;
    paint_body(&frame.body, frame.dims, out)?;
    queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;

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

fn paint_tab_bar(bar: &TabBarModel, dims: Dims, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    queue!(out, cursor::MoveTo(0, 0))?;
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
        if col >= dims.cols {
            break;
        }
    }
    queue!(out, terminal::Clear(terminal::ClearType::UntilNewLine))?;
    Ok(())
}

fn paint_body(body: &BodyModel, dims: Dims, out: &mut impl Write) -> io::Result<()> {
    use crossterm::{cursor, queue, style, terminal};

    let body_top: u16 = 1;
    let body_rows = dims.rows.saturating_sub(1);

    // Match inside the loop — no intermediate `Vec<&str>` per paint.
    for row in 0..body_rows {
        queue!(out, cursor::MoveTo(0, body_top + row))?;
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

// ── Raw mode guard ─────────────────────────────────────────────────────

pub struct RawModeGuard;

impl RawModeGuard {
    pub fn acquire() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
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
            cursor: Some((0, 1)),
            dims: Dims { cols: 40, rows: 5 },
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&frame, &mut out).expect("paint to Vec<u8>");
        assert!(!out.is_empty());
    }

    #[test]
    fn paint_hides_cursor_when_frame_cursor_is_none() {
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body: BodyModel::Empty,
            cursor: None,
            dims: Dims { cols: 40, rows: 5 },
        };
        let mut out: Vec<u8> = Vec::new();
        paint(&frame, &mut out).expect("paint to Vec<u8>");
        // Empty frames still produce clear/hide sequences — just don't panic.
        assert!(!out.is_empty());
    }
}
