//! `--golden-trace` output.
//!
//! One line per observable external event, pipe-separated. Paths are
//! emitted relative to `LED_TRACE_ROOT` when that environment variable is
//! set (the goldens runner sets it to the scenario tmpdir), so the lines
//! match across runs on different developer machines.
//!
//! Trace lines for milestone 1:
//!
//! - `key_in          | key=<name>`
//! - `resize          | cols=<n> rows=<n>`
//! - `file_load_start | path=<p>`
//! - `file_load_done  | path=<p> ok=<true|false> [bytes=<n>] [err=<msg>]`
//! - `render_tick`

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use led_core::CanonPath;
use led_driver_terminal_core::{Dims, KeyCode, KeyEvent, KeyModifiers};
use ropey::Rope;

/// The runtime-level trace hook. The driver-specific `Trace` traits in
/// `driver-file-read` and `driver-terminal` delegate here.
pub trait Trace: Send + Sync {
    fn key_in(&self, ev: &KeyEvent);
    fn resize(&self, dims: Dims);
    fn file_load_start(&self, path: &CanonPath);
    fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>);
    fn file_save_start(&self, path: &CanonPath, version: u64);
    fn file_save_done(&self, path: &CanonPath, version: u64, result: &Result<(), String>);
    fn render_tick(&self);
}

/// Cheap clone handle around an owning `Trace`. Kept behind an `Arc` so
/// the drivers and the main-loop path-formatter can share a single sink.
#[derive(Clone)]
pub struct SharedTrace(Arc<dyn Trace>);

impl SharedTrace {
    pub fn new(inner: Arc<dyn Trace>) -> Self {
        Self(inner)
    }

    /// Build a no-op trace — for binaries invoked without `--golden-trace`.
    pub fn noop() -> Self {
        Self(Arc::new(NoopTrace))
    }

    /// Build a trace that writes one line per event to the given file.
    /// The file is opened in append mode; the runner truncates it
    /// between scenarios.
    pub fn file(path: impl AsRef<Path>, root: Option<PathBuf>) -> io::Result<Self> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path.as_ref())?;
        Ok(Self(Arc::new(FileTrace {
            w: Mutex::new(BufWriter::new(f)),
            root,
        })))
    }

    pub(crate) fn inner(&self) -> Arc<dyn Trace> {
        self.0.clone()
    }

    // Mirror each Trace method so the main loop can call `trace.foo()`
    // without a double-deref ceremony.
    pub fn render_tick(&self) {
        self.0.render_tick();
    }
}

/// Fan-out of incoming events into pipe-formatted lines on a buffered
/// writer. Flushes after every line so test runners see output promptly.
struct FileTrace {
    w: Mutex<BufWriter<File>>,
    root: Option<PathBuf>,
}

impl Trace for FileTrace {
    fn key_in(&self, ev: &KeyEvent) {
        self.write_line(&format!("key_in          | key={}", format_key(ev)));
    }
    fn resize(&self, dims: Dims) {
        self.write_line(&format!(
            "resize          | cols={} rows={}",
            dims.cols, dims.rows
        ));
    }
    fn file_load_start(&self, path: &CanonPath) {
        self.write_line(&format!(
            "file_load_start | path={}",
            self.format_path(path)
        ));
    }
    fn file_load_done(&self, path: &CanonPath, result: &Result<Arc<Rope>, String>) {
        let tail = match result {
            Ok(rope) => format!("ok=true bytes={}", rope.len_bytes()),
            Err(msg) => format!("ok=false err={:?}", msg),
        };
        self.write_line(&format!(
            "file_load_done  | path={} {}",
            self.format_path(path),
            tail
        ));
    }
    fn file_save_start(&self, path: &CanonPath, version: u64) {
        self.write_line(&format!(
            "file_save_start | path={} version={}",
            self.format_path(path),
            version
        ));
    }
    fn file_save_done(&self, path: &CanonPath, version: u64, result: &Result<(), String>) {
        let tail = match result {
            Ok(()) => "ok=true".to_string(),
            Err(msg) => format!("ok=false err={:?}", msg),
        };
        self.write_line(&format!(
            "file_save_done  | path={} version={} {}",
            self.format_path(path),
            version,
            tail
        ));
    }
    fn render_tick(&self) {
        self.write_line("render_tick");
    }
}

impl FileTrace {
    fn write_line(&self, line: &str) {
        if let Ok(mut guard) = self.w.lock() {
            let _ = writeln!(&mut *guard, "{line}");
            let _ = guard.flush();
        }
    }

    fn format_path(&self, path: &CanonPath) -> String {
        let p = path.as_path();
        if let Some(root) = &self.root {
            if let Ok(rel) = p.strip_prefix(root) {
                return rel.display().to_string();
            }
        }
        p.display().to_string()
    }
}

struct NoopTrace;
impl Trace for NoopTrace {
    fn key_in(&self, _: &KeyEvent) {}
    fn resize(&self, _: Dims) {}
    fn file_load_start(&self, _: &CanonPath) {}
    fn file_load_done(&self, _: &CanonPath, _: &Result<Arc<Rope>, String>) {}
    fn file_save_start(&self, _: &CanonPath, _: u64) {}
    fn file_save_done(&self, _: &CanonPath, _: u64, _: &Result<(), String>) {}
    fn render_tick(&self) {}
}

// ── Formatting helpers ────────────────────────────────────────────────

fn format_key(ev: &KeyEvent) -> String {
    let mut out = String::new();
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl-");
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        out.push_str("Alt-");
    }
    if ev.modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(ev.code, KeyCode::Char(_) | KeyCode::BackTab)
    {
        // Shift is implicit for uppercase chars and BackTab; don't double it.
        out.push_str("Shift-");
    }
    out.push_str(&format_code(&ev.code));
    out
}

fn format_code(c: &KeyCode) -> String {
    match c {
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "BackTab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::F(n) => format!("F{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_key_ctrl_c() {
        let ev = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(format_key(&ev), "Ctrl-c");
    }

    #[test]
    fn format_key_tab() {
        let ev = KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(format_key(&ev), "Tab");
    }

    #[test]
    fn format_key_shift_backtab_no_double_shift() {
        let ev = KeyEvent {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::SHIFT,
        };
        assert_eq!(format_key(&ev), "BackTab");
    }
}
