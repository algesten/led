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
    Dims, Frame, KeyCode, KeyEvent, KeyModifiers, TermEvent, Theme, TerminalInputDriver, Trace,
};

use buffer::Buffer;
use render::body::paint_body;
use render::completion::paint_completion_popup;
use render::popover::paint_popover;
use render::rename::paint_rename_popup;
use render::side_panel::{paint_side_border, paint_side_panel};
use render::status_bar::paint_status_bar;
use render::tab_bar::paint_tab_bar;

#[cfg(test)]
use render::body::paint_syntax_line;

#[cfg(test)]
use led_driver_terminal_core::{
    BodyModel, Color, Layout, SidePanelModel, SidePanelRow, StatusBarModel, TabBarModel,
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

    /// Discard the painter's mirror of the on-screen grid.
    ///
    /// The cell-diff renderer compares each new frame against its
    /// internal `prev_buf` and emits only the changed cells; after
    /// a SIGTSTP round-trip the user's shell has overwritten the
    /// alt-screen while we were parked, but the mirror still
    /// thinks the old pixels are live — the next frame diffs
    /// against that stale mirror and produces an empty byte
    /// stream, which looks to the user like "suspend didn't
    /// redraw". Resetting the stored `dims` to 0x0 forces the
    /// next `execute` down the resize path: both internal buffers
    /// get blanked, `Clear(All)` is queued, and every cell is
    /// re-emitted from scratch.
    pub fn invalidate(&self) {
        let mut state = self.state.lock().expect("render state poisoned");
        state.dims = Dims { cols: 0, rows: 0 };
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
        scroll_hints: &[led_driver_terminal_core::ScrollHint],
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

        // Scroll-region fast path. The runtime emits one hint per
        // disjoint rect that scrolled cleanly this tick (editor
        // body when cursor scrolls; sidebar when file-browser
        // selection scrolls; both at once is rare but supported).
        // Each hint becomes a native CSI S/T inside DECSLRM
        // margins so the terminal does the move in its own
        // optimized path (often hardware-accelerated). The prev
        // mirror is shifted to match the post-scroll terminal
        // state — the subsequent cell diff produces the small
        // set of writes that can't be expressed as pure scroll
        // (cursor-row highlight that moved, status-bar line
        // counter, anything inside the region that's not a
        // uniform shift).
        //
        // Skipped on resize: the prior frame's cells were laid
        // out at different coordinates and a scroll op would
        // shift garbage. The forced full repaint takes over.
        if !resized {
            let prev_idx = 1 - state.current;
            for h in scroll_hints {
                if h.delta_rows == 0 {
                    continue;
                }
                let buf = &mut state.buffers[prev_idx];
                if !shift_buffer_region(buf, h) {
                    continue;
                }
                let _ = emit_scroll_op(out, h);
            }
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
                render::draw_diff(&d, current_buf, &mut capture)?;
                out.write_all(&capture)?;
                Some(capture)
            } else {
                render::draw_diff(&d, current_buf, out)?;
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

        if let Some(log) = &self.log
            && let Ok(mut g) = log.lock() {
                g.frame_n += 1;
                let header = format!("\n=== FRAME {} ===\n", g.frame_n);
                let _ = g.file.write_all(header.as_bytes());
                if let Some(bytes) = &log_bytes {
                    let _ = g.file.write_all(bytes);
                }
                let _ = g.file.flush();
            }

        // Swap buffers: next frame writes into the one we just used
        // as `prev`, and diffs against what we just emitted.
        state.current = 1 - state.current;

        Ok(())
    }
}

/// Translate the runtime's [`ScrollHint`] into an in-place buffer
/// shift on `buf`. Always shifts the full row range — the scroll
/// op below uses DECSTBM-only (no horizontal margins) since
/// Apple Terminal doesn't reliably honor DECLRMM, so the prev
/// mirror has to match the actual full-width scroll the terminal
/// does. Returns `false` when the hint is degenerate or out of
/// bounds — caller skips the scroll-op emit.
fn shift_buffer_region(
    buf: &mut Buffer,
    hint: &led_driver_terminal_core::ScrollHint,
) -> bool {
    buf.shift_region(
        hint.region_top,
        hint.region_bottom,
        0,
        buf.cols(),
        hint.delta_rows,
    )
}

/// Emit the VT100 scroll sequence for `hint`. Uses only DECSTBM
/// (`CSI Pt;Pb r`) — no horizontal margins. Apple Terminal
/// silently drops DECLRMM (`CSI ? 69 h`) and falls back to
/// full-row scroll, which corrupts cells outside the requested
/// rect. By doing full-width scroll on every terminal and shifting
/// prev_buf full-width to match, the cell diff produces the
/// residual cells (sidebar restore for editor scroll, status-bar
/// counter, cursor highlight). Net byte count for editor scroll:
/// roughly 3-4x smaller than a full body repaint.
///
/// `region_left` / `region_right` on the hint are advisory only
/// in this implementation and ignored; kept on the type so a
/// future capability-detect path can use them on terminals that
/// honor DECLRMM.
fn emit_scroll_op<W: Write>(
    out: &mut W,
    hint: &led_driver_terminal_core::ScrollHint,
) -> io::Result<()> {
    let top1 = hint.region_top + 1;
    let bot1 = hint.region_bottom;
    write!(out, "\x1b[{};{}r", top1, bot1)?;
    if hint.delta_rows > 0 {
        write!(out, "\x1b[{}S", hint.delta_rows)?;
    } else {
        write!(out, "\x1b[{}T", -hint.delta_rows)?;
    }
    // Reset scroll region so subsequent absolute cursor moves
    // don't get clipped.
    write!(out, "\x1b[r")?;
    Ok(())
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

    // When any in-body overlay changes (appears / disappears /
    // moves / content shifts), we must repaint the body too — the
    // old box needs to be erased and the new one drawn on a fresh
    // canvas.
    let popover_changed = last.map(|l| &l.popover) != Some(&frame.popover);
    let completion_changed = last.map(|l| &l.completion) != Some(&frame.completion);
    let rename_changed = last.map(|l| &l.rename_popup) != Some(&frame.rename_popup);

    if force
        || popover_changed
        || completion_changed
        || rename_changed
        || last.map(|l| &l.body) != Some(&frame.body)
    {
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

    // Rename popup (M18). Single-line in-buffer overlay anchored
    // below the cursor. Mutually exclusive with the completion
    // popup at the dispatch level — the runtime won't open both
    // at the same time — but paint after completions so a stray
    // race still leaves the rename prompt visible (it's the
    // active focus when present).
    if let Some(rp) = &frame.rename_popup {
        paint_rename_popup(rp, frame.layout.editor_area, frame.dims, theme, buf);
    }

    if force || last.map(|l| &l.tab_bar) != Some(&frame.tab_bar) {
        paint_tab_bar(&frame.tab_bar, frame.layout.tab_bar, theme, buf);
    }

    if force || last.map(|l| &l.status_bar) != Some(&frame.status_bar) {
        paint_status_bar(&frame.status_bar, frame.layout.status_bar, theme, buf);
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
    use led_driver_terminal_core::{NoopTrace, Style};

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
        driver.execute(frame, last, &[], theme, &mut out).expect("execute");
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
            rename_popup: None,
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
            rename_popup: None,
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
            rename_popup: None,
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
            rename_popup: None,
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
        driver
            .execute(&frame1, None, &[], &theme, &mut out)
            .expect("frame1");
        grid.apply(&out);
        out.clear();
        driver
            .execute(&frame2, Some(&frame1), &[], &theme, &mut out)
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

    #[test]
    fn toggling_side_panel_clears_stale_tab_bar_under_panel() {
        // Regression: hiding the browser (Ctrl-B) widens the tab bar
        // to the full terminal width, then re-showing the browser
        // narrows it again. Without an explicit blank of the tab-bar
        // row to the left of the side border, the wide bar's labels
        // remain stuck under the side panel.
        use std::sync::Arc;

        let dims = Dims { cols: 60, rows: 10 };
        let layout_visible = Layout::compute(dims, true);
        let layout_hidden = Layout::compute(dims, false);

        let many_tabs = TabBarModel {
            labels: Arc::new(
                (0..6).map(|i| format!("file_{i}.rs")).collect::<Vec<_>>(),
            ),
            active: Some(0),
        };
        let one_tab = TabBarModel {
            labels: Arc::new(vec!["a.rs".into()]),
            active: Some(0),
        };
        let panel = SidePanelModel {
            rows: Arc::new(vec![]),
            focused: false,
            mode: Default::default(),
        };

        let frame_visible = Frame {
            tab_bar: one_tab.clone(),
            body: BodyModel::Empty,
            status_bar: StatusBarModel::default(),
            side_panel: Some(panel.clone()),
            popover: None,
            completion: None,
            rename_popup: None,
            layout: layout_visible,
            cursor: None,
            dims,
        };
        let frame_hidden = Frame {
            tab_bar: many_tabs,
            side_panel: None,
            layout: layout_hidden,
            ..frame_visible.clone()
        };
        let frame_visible_again = Frame {
            tab_bar: one_tab,
            side_panel: Some(panel),
            layout: layout_visible,
            ..frame_hidden.clone()
        };

        let driver = TerminalOutputDriver::new(Arc::new(NoopTrace));
        let theme = Theme::default();
        let mut grid = Grid::new(dims);
        let mut out: Vec<u8> = Vec::new();
        driver
            .execute(&frame_visible, None, &[], &theme, &mut out)
            .expect("frame_visible");
        grid.apply(&out);
        out.clear();
        driver
            .execute(&frame_hidden, Some(&frame_visible), &[], &theme, &mut out)
            .expect("frame_hidden");
        grid.apply(&out);
        out.clear();
        driver
            .execute(
                &frame_visible_again,
                Some(&frame_hidden),
                &[],
                &theme,
                &mut out,
            )
            .expect("frame_visible_again");
        grid.apply(&out);

        let tab_row = layout_visible.tab_bar.y;
        let border_x = layout_visible.side_border_x.expect("border");
        for col in 0..border_x {
            let ch = grid.char_at(tab_row, col);
            assert_eq!(
                ch, ' ',
                "stale tab bar cell at row {tab_row} col {col}: {ch:?}",
            );
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
                    'J'
                        // CSI n J — 2 = clear whole screen. The
                        // driver emits `Clear(All)` on resize.
                        if params == "2" => {
                            self.clear_all();
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
            rename_popup: None,
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
    fn paint_syntax_line_gap_fill_does_not_borrow_syntax_default_slot() {
        // Regression: a theme that sets `[syntax].embedded` (or any
        // capture that maps to TokenKind::Default) used to bleed
        // into the gap-fill colour for un-captured glyphs because
        // the painter pulled `syntax.default` for both. Result was
        // every plain identifier in a Rust file rendering with the
        // user's "embedded code" colour. Gap-fill must use
        // Style::default() so un-captured text stays neutral.
        use led_driver_terminal_core::{
            Attrs, Color, LineSpan, Style as TermStyle, SyntaxTheme,
        };
        use led_state_syntax::TokenKind;
        let mut buf = Buffer::new(3, 16);
        let mut syntax = SyntaxTheme::plain();
        // Stand-in for the user's `embedded = "$syntax_label"`
        // theme entry: a vivid bg makes the bleed obvious.
        syntax.default = TermStyle {
            fg: Some(Color::Indexed(172)),
            bg: None,
            attrs: Attrs::default(),
        };
        let line = "ab cd";
        let spans = vec![LineSpan {
            col_start: 0,
            col_end: 2,
            kind: TokenKind::Keyword,
        }];
        paint_syntax_line(line, &spans, &syntax, 0, 0, &mut buf);
        // Cell 2 is the un-captured space, cell 3-4 the un-captured
        // `cd`. Each must read back as Style::default(), not the
        // syntax.default slot's colour.
        for col in 2..5u16 {
            let cell = buf.cell(0, col).expect("cell present");
            assert_eq!(
                cell.style,
                TermStyle::default(),
                "col {col} gap-fill must be unstyled, got {:?}",
                cell.style,
            );
        }
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
                    selection: None,
                },
                led_driver_terminal_core::BodyLine {
                    text: "  jkl".to_string(),
                    spans: Vec::new(),
                    gutter_diagnostic: None,
                    gutter_category: None,
                    diagnostics: Vec::new(),
                    selection: None,
                },
                led_driver_terminal_core::BodyLine {
                    text: "~ ".to_string(),
                    spans: Vec::new(),
                    gutter_diagnostic: None,
                    gutter_category: None,
                    diagnostics: Vec::new(),
                    selection: None,
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
            rename_popup: None,
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
            rename_popup: None,
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
        let theme = Theme {
            ruler_column: Some(5),
            ruler: Style {
                bg: Some(Color::rgb(0x22, 0x22, 0x22)),
                ..Style::default()
            },
            ..Default::default()
        };
        let frame = Frame {
            tab_bar: TabBarModel::default(),
            body,
            status_bar: StatusBarModel::default(),
            side_panel: None,
            popover: None,
            completion: None,
            rename_popup: None,
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
