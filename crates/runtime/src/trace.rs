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

use led_core::{CanonPath, PersistedContentHash};
use led_driver_find_file_core::FindFileCmd;
use led_driver_terminal_core::{Dims, KeyEvent};
use led_state_syntax::Language;
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
    fn file_save_as_start(&self, from: &CanonPath, to: &CanonPath);
    fn file_save_as_done(&self, from: &CanonPath, to: &CanonPath, result: &Result<(), String>);
    fn clipboard_read_start(&self);
    fn clipboard_read_done(&self, ok: bool, empty: bool);
    /// Outbound clipboard write. Trace receives the full payload
    /// so it can derive both `len=` (in bytes) and `preview="…"`
    /// (first 14 chars, legacy parity) without forcing every
    /// caller to compute the preview itself.
    fn clipboard_write_start(&self, text: &str);
    fn clipboard_write_done(&self, ok: bool);
    fn fs_list_start(&self, path: &CanonPath);
    fn fs_list_done(&self, path: &CanonPath, ok: bool);
    /// Project-wide file search request dispatched to the ripgrep
    /// driver. Legacy dispatched.snap name: `FileSearch`.
    fn file_search_start(
        &self,
        query: &str,
        root: &CanonPath,
        case_sensitive: bool,
        use_regex: bool,
    );
    /// Project-wide replace-all request dispatched to the driver.
    /// Legacy dispatched.snap name: `FileSearchReplace`.
    fn file_search_replace_start(
        &self,
        query: &str,
        replacement: &str,
        root: &CanonPath,
        case_sensitive: bool,
        use_regex: bool,
    );
    /// Single on-disk point-replace for a per-hit Right-arrow on
    /// an unloaded file. Legacy dispatched.snap name:
    /// `FileSearchSingleReplace`.
    fn file_search_single_replace_start(&self, path: &CanonPath, line: usize);
    /// Dispatched when the runtime ships a syntax parse request
    /// off to the tree-sitter worker. Not in legacy's
    /// dispatched.snap (M15 rewrite-only), but follows the same
    /// `<Command>\t<args>` shape.
    fn syntax_parse_start(&self, path: &CanonPath, version: u64, language: Language);
    /// Completion back from the worker. Not currently serialized
    /// to dispatched.snap — kept for symmetry + future debug traces.
    fn syntax_parse_done(&self, path: &CanonPath, version: u64, ok: bool);
    /// An LSP language server subprocess started successfully
    /// (named after its `Language`). Emitted once per language
    /// per session. Legacy golden name: `LspServerStarted`.
    fn lsp_server_started(&self, server: &str);
    /// Runtime dispatched a git workspace scan. Emits as
    /// `GitScan\troot=<p>` in `dispatched.snap`. Fires once per
    /// `GitCmd::ScanFiles` — the git driver is stateless about
    /// pending work, so this maps 1:1 with the execute-phase
    /// emission.
    fn git_scan_start(&self, root: &CanonPath);
    /// Runtime asked the session driver to open the DB +
    /// flock and load the prior session. Emits as
    /// `WorkspaceLoad\troot=<p>` in `dispatched.snap`. M21.
    fn session_init_start(&self, root: &CanonPath);
    /// Runtime dispatched a session save. Emits as
    /// `WorkspaceSaveSession` in `dispatched.snap`. Fires
    /// once on the Phase::Exiting transition for primaries.
    fn session_save_start(&self);
    /// Runtime asked the LSP manager to open a diagnostic window.
    /// Fires on every buffer/save version delta — the manager
    /// coalesces via its DiagnosticSource state machine.
    fn lsp_request_diagnostics(&self);
    /// A diagnostic delivery reached the runtime, stamped with
    /// the buffer version the server was reasoning about.
    fn lsp_diagnostics_done(&self, path: &CanonPath, n: usize, hash: PersistedContentHash);
    /// Server fell back from pull mode to push mode
    /// (`publishDiagnostics` arrived while in Pull). One-way;
    /// emitted once per server.
    fn lsp_mode_fallback(&self);
    /// Emitted when the find-file driver receives a completion
    /// command. Legacy's dispatched.snap name is `FsFindFile`.
    fn find_file_start(&self, cmd: &FindFileCmd);
    /// Completion back from the driver — not traced in legacy
    /// (dispatched.snap tracks intents, not driver results), kept
    /// here for symmetry + future debug traces.
    fn find_file_done(&self, dir: &CanonPath, prefix: &str, ok: bool);
    /// Emitted when the runtime truncates the undo history after a
    /// save (saved state becomes the new baseline). Legacy traces
    /// this immediately after `FileSave`.
    fn workspace_clear_undo(&self, path: &CanonPath);
    /// Per-buffer undo flush. Emitted by the session driver when
    /// the runtime ships newly-finalised undo groups to SQLite.
    /// Legacy dispatched.snap line: `WorkspaceFlushUndo\tpath=<p>
    /// chain=<id>`.
    fn workspace_flush_undo(&self, path: &CanonPath, chain_id: &str);
    /// Emitted after a SaveAs completes: legacy re-opens the source
    /// buffer's on-disk file to refresh its pristine baseline, with
    /// `create_if_missing=false` because the file is known to exist
    /// (we just had it loaded).
    fn file_reopen_existing(&self, path: &CanonPath);
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
    pub fn workspace_flush_undo(&self, path: &CanonPath, chain_id: &str) {
        self.0.workspace_flush_undo(path, chain_id);
    }
    pub fn file_search_start(
        &self,
        query: &str,
        root: &CanonPath,
        case_sensitive: bool,
        use_regex: bool,
    ) {
        self.0
            .file_search_start(query, root, case_sensitive, use_regex);
    }
    pub fn file_reopen_existing(&self, path: &CanonPath) {
        self.0.file_reopen_existing(path);
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
    fn file_save_as_start(&self, from: &CanonPath, to: &CanonPath) {
        self.write_line(&format!(
            "FileSaveAs\tpath={} new_path={}",
            self.format_path(from),
            self.format_path(to),
        ));
    }
    fn file_save_as_done(&self, _: &CanonPath, _: &CanonPath, _: &Result<(), String>) {}
    fn clipboard_read_start(&self) {
        self.write_line("ClipboardRead");
    }
    fn clipboard_read_done(&self, _: bool, _: bool) {}
    fn clipboard_write_start(&self, text: &str) {
        // Legacy parity: `len=<bytes>` + `preview="<first 14
        // chars>"`. `chars().take(14)` keeps multi-byte boundaries
        // intact; embedded `"` / `\` are escaped so the preview
        // round-trips through a quoted shell-style field.
        let preview: String = text.chars().take(14).collect();
        let escaped = preview.replace('\\', "\\\\").replace('"', "\\\"");
        self.write_line(&format!(
            "ClipboardWrite\tlen={} preview=\"{}\"",
            text.len(),
            escaped,
        ));
    }
    fn clipboard_write_done(&self, _: bool) {}
    fn fs_list_start(&self, path: &CanonPath) {
        self.write_line(&format!("FsListDir\tpath={}", self.format_path(path)));
    }
    fn fs_list_done(&self, _: &CanonPath, _: bool) {}
    fn file_search_start(
        &self,
        query: &str,
        root: &CanonPath,
        case_sensitive: bool,
        use_regex: bool,
    ) {
        self.write_line(&format!(
            "FileSearch\tquery=\"{}\" root={} case={} regex={}",
            query,
            self.format_path(root),
            case_sensitive,
            use_regex,
        ));
    }
    fn file_search_replace_start(
        &self,
        query: &str,
        replacement: &str,
        root: &CanonPath,
        case_sensitive: bool,
        use_regex: bool,
    ) {
        self.write_line(&format!(
            "FileSearchReplace\tquery=\"{}\" replacement=\"{}\" root={} case={} regex={}",
            query,
            replacement,
            self.format_path(root),
            case_sensitive,
            use_regex,
        ));
    }
    fn file_search_single_replace_start(&self, path: &CanonPath, line: usize) {
        self.write_line(&format!(
            "FileSearchSingleReplace\tpath={} line={}",
            self.format_path(path),
            line,
        ));
    }
    // Syntax parses aren't serialized to dispatched.snap — they'd
    // fire on every buffer load and keystroke, drowning the signal
    // of what user-level intent happened. Keeping the method as a
    // no-op preserves the trait for future debug traces / assertions.
    fn syntax_parse_start(&self, _: &CanonPath, _: u64, _: Language) {}
    fn syntax_parse_done(&self, _: &CanonPath, _: u64, _: bool) {}
    fn lsp_server_started(&self, server: &str) {
        self.write_line(&format!("LspServerStarted\tserver={server}"));
    }
    fn git_scan_start(&self, root: &CanonPath) {
        self.write_line(&format!("GitScan\troot={}", self.format_path(root)));
    }
    fn session_init_start(&self, _: &CanonPath) {
        // Legacy doesn't emit a dispatched-intent line for the
        // workspace open — only for the explicit save. We
        // match: keep the hook so future debug traces can light
        // it up, but no `dispatched.snap` line.
    }
    fn session_save_start(&self) {
        self.write_line("WorkspaceSaveSession");
    }
    // Request-diagnostics fires per version delta; too noisy for
    // the intent log.
    fn lsp_request_diagnostics(&self) {}
    fn lsp_diagnostics_done(&self, _: &CanonPath, _: usize, _: PersistedContentHash) {}
    fn lsp_mode_fallback(&self) {
        self.write_line("LspModeFallback");
    }
    fn find_file_start(&self, cmd: &FindFileCmd) {
        // Legacy format: `FsFindFile\tdir=<p> prefix="<s>" show_hidden=<bool>`.
        self.write_line(&format!(
            "FsFindFile\tdir={} prefix=\"{}\" show_hidden={}",
            self.format_path(&cmd.dir),
            cmd.prefix,
            cmd.show_hidden,
        ));
    }
    fn find_file_done(&self, _: &CanonPath, _: &str, _: bool) {}
    fn workspace_clear_undo(&self, path: &CanonPath) {
        self.write_line(&format!(
            "WorkspaceClearUndo\tpath={}",
            self.format_path(path)
        ));
    }
    fn workspace_flush_undo(&self, path: &CanonPath, chain_id: &str) {
        self.write_line(&format!(
            "WorkspaceFlushUndo\tpath={} chain={}",
            self.format_path(path),
            chain_id,
        ));
    }
    fn file_reopen_existing(&self, path: &CanonPath) {
        self.write_line(&format!(
            "FileOpen\tpath={} create_if_missing=false",
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
        if let Some(root) = &self.root
            && let Ok(rel) = p.strip_prefix(root)
        {
            return rel.display().to_string();
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
    fn file_save_as_start(&self, _: &CanonPath, _: &CanonPath) {}
    fn file_save_as_done(&self, _: &CanonPath, _: &CanonPath, _: &Result<(), String>) {}
    fn clipboard_read_start(&self) {}
    fn clipboard_read_done(&self, _ok: bool, _empty: bool) {}
    fn clipboard_write_start(&self, _text: &str) {}
    fn clipboard_write_done(&self, _ok: bool) {}
    fn fs_list_start(&self, _: &CanonPath) {}
    fn fs_list_done(&self, _: &CanonPath, _: bool) {}
    fn file_search_start(&self, _: &str, _: &CanonPath, _: bool, _: bool) {}
    fn file_search_replace_start(
        &self,
        _: &str,
        _: &str,
        _: &CanonPath,
        _: bool,
        _: bool,
    ) {
    }
    fn file_search_single_replace_start(&self, _: &CanonPath, _: usize) {}
    fn syntax_parse_start(&self, _: &CanonPath, _: u64, _: Language) {}
    fn syntax_parse_done(&self, _: &CanonPath, _: u64, _: bool) {}
    fn lsp_server_started(&self, _: &str) {}
    fn git_scan_start(&self, _: &CanonPath) {}
    fn session_init_start(&self, _: &CanonPath) {}
    fn session_save_start(&self) {}
    fn lsp_request_diagnostics(&self) {}
    fn lsp_diagnostics_done(&self, _: &CanonPath, _: usize, _: PersistedContentHash) {}
    fn lsp_mode_fallback(&self) {}
    fn find_file_start(&self, _: &FindFileCmd) {}
    fn find_file_done(&self, _: &CanonPath, _: &str, _: bool) {}
    fn workspace_clear_undo(&self, _: &CanonPath) {}
    fn workspace_flush_undo(&self, _: &CanonPath, _: &str) {}
    fn file_reopen_existing(&self, _: &CanonPath) {}
    fn render_tick(&self) {}
}

