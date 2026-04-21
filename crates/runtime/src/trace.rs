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
    fn clipboard_read_start(&self);
    fn clipboard_read_done(&self, ok: bool, empty: bool);
    fn clipboard_write_start(&self, bytes: usize);
    fn clipboard_write_done(&self, ok: bool);
    fn fs_list_start(&self, path: &CanonPath);
    fn fs_list_done(&self, path: &CanonPath, ok: bool);
    /// Emitted when the runtime truncates the undo history after a
    /// save (saved state becomes the new baseline). Legacy traces
    /// this immediately after `FileSave`.
    fn workspace_clear_undo(&self, path: &CanonPath);
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
    pub fn workspace_clear_undo(&self, path: &CanonPath) {
        self.0.workspace_clear_undo(path);
    }
}

/// Fan-out of incoming events into pipe-formatted lines on a buffered
/// writer. Flushes after every line so test runners see output promptly.
struct FileTrace {
    w: Mutex<BufWriter<File>>,
    root: Option<PathBuf>,
}

// ── Dispatched-intent trace format ─────────────────────────────────────
//
// The goldens' `dispatched.snap` captures one line per intent the
// runtime fires at a driver — not an event log. Format:
//
//   `<CommandName>\t<key>=<value>[ <key>=<value>]*`
//
// with tab between name and args. The set of CommandNames matches
// legacy (`FsListDir`, `FileOpen`, `FileSave`, `ClipboardRead`,
// `ClipboardWrite`, `GitScan`, `WorkspaceClearUndo`,
// `WorkspaceFlushUndo`, ...). Input-side events (`key_in`,
// `resize`) and driver-completion events (`file_load_done`,
// `render_tick`, etc.) are NOT in this log — they're not intents.

impl Trace for FileTrace {
    fn key_in(&self, _: &KeyEvent) {}
    fn resize(&self, _: Dims) {}
    fn file_load_start(&self, path: &CanonPath) {
        // Legacy named this `FileOpen`; `create_if_missing=true`
        // matches its default docstore behaviour.
        self.write_line(&format!(
            "FileOpen\tpath={} create_if_missing=true",
            self.format_path(path)
        ));
    }
    fn file_load_done(&self, _: &CanonPath, _: &Result<Arc<Rope>, String>) {}
    fn file_save_start(&self, path: &CanonPath, _version: u64) {
        self.write_line(&format!("FileSave\tpath={}", self.format_path(path)));
    }
    fn file_save_done(&self, _: &CanonPath, _: u64, _: &Result<(), String>) {}
    fn clipboard_read_start(&self) {
        self.write_line("ClipboardRead");
    }
    fn clipboard_read_done(&self, _: bool, _: bool) {}
    fn clipboard_write_start(&self, bytes: usize) {
        // Legacy emitted a `preview=` of the first 14 chars too; we
        // don't currently have the text here (the hook's signature
        // only carries `bytes`). Emit `len=` alone — callers that
        // care about preview can extend later.
        self.write_line(&format!("ClipboardWrite\tlen={bytes}"));
    }
    fn clipboard_write_done(&self, _: bool) {}
    fn fs_list_start(&self, path: &CanonPath) {
        self.write_line(&format!("FsListDir\tpath={}", self.format_path(path)));
    }
    fn fs_list_done(&self, _: &CanonPath, _: bool) {}
    fn workspace_clear_undo(&self, path: &CanonPath) {
        self.write_line(&format!(
            "WorkspaceClearUndo\tpath={}",
            self.format_path(path)
        ));
    }
    fn render_tick(&self) {}
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
    fn clipboard_read_start(&self) {}
    fn clipboard_read_done(&self, _ok: bool, _empty: bool) {}
    fn clipboard_write_start(&self, _bytes: usize) {}
    fn clipboard_write_done(&self, _ok: bool) {}
    fn fs_list_start(&self, _: &CanonPath) {}
    fn fs_list_done(&self, _: &CanonPath, _: bool) {}
    fn workspace_clear_undo(&self, _: &CanonPath) {}
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
