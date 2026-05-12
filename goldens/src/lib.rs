//! Black-box golden-test runner for led.
//!
//! Spawns the compiled `led` binary in a pseudoterminal, drives it with raw
//! keystrokes, parses ANSI output through `vt100`, snapshots the rendered
//! grid against committed `frame.snap` files. Zero coupling to internal led
//! types.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub mod keys;
pub mod scenario;

pub use scenario::run as run_scenario;

pub struct Binaries {
    pub led: PathBuf,
    pub fake_lsp: PathBuf,
}

/// Build led + the fake binaries on first use; cache for the test process
/// lifetime. One cargo invocation builds both (cheap when up-to-date).
fn binaries() -> &'static Binaries {
    static BINS: OnceLock<Binaries> = OnceLock::new();
    BINS.get_or_init(|| {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("goldens crate has parent dir");
        let status = Command::new("cargo")
            .args(["build", "-p", "led", "-p", "fake-lsp"])
            .current_dir(workspace_root)
            .status()
            .expect("invoke cargo build");
        assert!(status.success(), "cargo build failed");
        let target = workspace_root.join("target").join("debug");
        let bins = Binaries {
            led: target.join("led"),
            fake_lsp: target.join("fake-lsp"),
        };
        for (name, p) in [("led", &bins.led), ("fake-lsp", &bins.fake_lsp)] {
            assert!(p.exists(), "{name} binary not found at {}", p.display());
        }
        bins
    })
}

pub struct GoldenRunnerBuilder {
    files: Vec<(String, String)>,
    viewport: (u16, u16),
    no_workspace: bool,
    git_init: bool,
    fake_lsp_json: Option<String>,
    config_keys: Option<String>,
    config_theme: Option<String>,
}

impl GoldenRunnerBuilder {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            viewport: (80, 24),
            no_workspace: false,
            git_init: false,
            fake_lsp_json: None,
            config_keys: None,
            config_theme: None,
        }
    }

    pub fn with_file(mut self, name: &str, contents: &str) -> Self {
        self.files.push((name.to_string(), contents.to_string()));
        self
    }

    pub fn with_viewport(mut self, cols: u16, rows: u16) -> Self {
        self.viewport = (cols, rows);
        self
    }

    pub fn with_no_workspace(mut self) -> Self {
        self.no_workspace = true;
        self
    }

    /// Create an empty `.git/` in the workspace dir so led's workspace
    /// detection treats this dir as a project root.
    pub fn with_git_init(mut self) -> Self {
        self.git_init = true;
        self
    }

    /// JSON content for `.fake-lsp.json` (the fake LSP server's config).
    /// When set, led runs with `--test-lsp-server <fake-lsp-binary>`.
    pub fn with_fake_lsp_json(mut self, json: String) -> Self {
        self.fake_lsp_json = Some(json);
        self
    }

    /// TOML content written to `<config_dir>/keys.toml` before spawn.
    /// led REPLACES defaults wholesale — include every needed binding.
    pub fn with_config_keys(mut self, toml: String) -> Self {
        self.config_keys = Some(toml);
        self
    }

    /// TOML content written to `<config_dir>/theme.toml` before spawn.
    pub fn with_config_theme(mut self, toml: String) -> Self {
        self.config_theme = Some(toml);
        self
    }

    pub fn spawn(self) -> GoldenRunner {
        let tmpdir = tempfile::tempdir().expect("create tmpdir");
        let workspace_dir = tmpdir.path().join("workspace");
        let config_dir = tmpdir.path().join("config");
        std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");
        std::fs::create_dir_all(&config_dir).expect("create config dir");

        if self.git_init {
            std::fs::create_dir_all(workspace_dir.join(".git"))
                .expect("create .git dir for workspace detection");
        }
        // Same hermetic approach as theme.toml: always write a
        // keys.toml (empty by default) so led's loader doesn't
        // fall back to `~/.config/led/keys.toml` and dump host-
        // specific warnings into the captured PTY.
        let keys_payload: &str = self.config_keys.as_deref().unwrap_or("");
        std::fs::write(config_dir.join("keys.toml"), keys_payload)
            .expect("write keys.toml");
        // Always seed config_dir/theme.toml so led's loader never
        // walks up to `~/.config/led/theme.toml`. Without this,
        // any warnings the host's real theme produces leak into
        // the test's stderr (and on quit-without-clear scenarios,
        // into the captured PTY frame).
        let theme_payload: &str = self.config_theme.as_deref().unwrap_or("");
        std::fs::write(config_dir.join("theme.toml"), theme_payload)
            .expect("write theme.toml");
        // Always seed the fake-lsp config (even if empty) so the real
        // rust-analyzer on the host can never accidentally attach to a
        // workspace scenario. Determinism > convenience.
        let lsp_json = self.fake_lsp_json.as_deref().unwrap_or("{}");
        std::fs::write(workspace_dir.join(".fake-lsp.json"), lsp_json)
            .expect("write .fake-lsp.json");

        // Trace file lives OUTSIDE the workspace dir so the file browser
        // doesn't show it as a workspace file.
        let trace_holder =
            tempfile::NamedTempFile::new().expect("create trace tempfile");
        let trace_path = trace_holder.path().to_path_buf();

        let mut file_paths = Vec::new();
        for (name, contents) in &self.files {
            let path = workspace_dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create file parent dir");
            }
            std::fs::write(&path, contents).expect("write seeded file");
            file_paths.push(path);
        }

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                cols: self.viewport.0,
                rows: self.viewport.1,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let bins = binaries();
        let mut cmd = CommandBuilder::new(&bins.led);
        cmd.arg("--golden-trace");
        cmd.arg(trace_path.as_os_str());
        cmd.arg("--config-dir");
        cmd.arg(config_dir.as_os_str());
        if self.no_workspace {
            cmd.arg("--no-workspace");
        }
        // Always pass the fake binary (config is seeded above) — see
        // the comment there.
        cmd.arg("--test-lsp-server");
        cmd.arg(bins.fake_lsp.as_os_str());
        // Hermetic clipboard: each spawned `led` keeps its yank /
        // kill-ring state in-process instead of touching the OS
        // pasteboard, so parallel goldens don't trample each
        // other.
        cmd.arg("--test-clipboard-isolated");
        for p in &file_paths {
            cmd.arg(p.as_os_str());
        }
        cmd.cwd(&workspace_dir);
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).expect("spawn led");
        drop(pair.slave);

        let writer = pair.master.take_writer().expect("take_writer");
        let reader = pair.master.try_clone_reader().expect("try_clone_reader");

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            self.viewport.1,
            self.viewport.0,
            0,
        )));
        let last_byte_time = Arc::new(Mutex::new(Instant::now()));
        let bytes_received = Arc::new(AtomicUsize::new(0));

        // Reader thread: drain PTY bytes into the vt100 parser and update
        // the quiescence trackers.
        {
            let parser = parser.clone();
            let last_byte_time = last_byte_time.clone();
            let bytes_received = bytes_received.clone();
            let mut reader = reader;
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            parser.lock().unwrap().process(&buf[..n]);
                            *last_byte_time.lock().unwrap() = Instant::now();
                            bytes_received.fetch_add(n, Ordering::Relaxed);
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        let mut runner = GoldenRunner {
            _master: pair.master,
            child,
            writer,
            parser,
            last_byte_time,
            bytes_received,
            trace_path,
            _trace_holder: trace_holder,
            tmpdir_path: workspace_dir,
            _tmpdir: tmpdir,
        };
        runner.wait_ready();
        runner
    }
}

impl Default for GoldenRunnerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct GoldenRunner {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    last_byte_time: Arc<Mutex<Instant>>,
    bytes_received: Arc<AtomicUsize>,
    trace_path: PathBuf,
    _trace_holder: tempfile::NamedTempFile,
    tmpdir_path: PathBuf,
    _tmpdir: tempfile::TempDir,
}

impl GoldenRunner {
    pub fn press(&mut self, chord: &str) {
        let bytes = keys::chord_to_bytes(chord)
            .unwrap_or_else(|| panic!("unknown chord: {chord}"));
        self.writer.write_all(&bytes).expect("write to PTY");
        self.writer.flush().expect("flush PTY");
        self.settle();
    }

    /// Send a sequence of chords as a single chord-prefix sequence
    /// (e.g. Emacs-style `Ctrl-x Ctrl-s`). A single `settle()` runs at
    /// the end so intermediate chord-prefix state is preserved.
    pub fn press_seq(&mut self, chords: &[&str]) {
        for chord in chords {
            let bytes = keys::chord_to_bytes(chord)
                .unwrap_or_else(|| panic!("unknown chord: {chord}"));
            self.writer.write_all(&bytes).expect("write to PTY");
        }
        self.writer.flush().expect("flush PTY");
        self.settle();
    }

    pub fn type_text(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .expect("write to PTY");
        self.writer.flush().expect("flush PTY");
        self.settle();
    }

    /// Sleep for the given wall-clock duration, then call settle. Used by
    /// `wait <N>ms` script steps to allow async work to begin before
    /// quiescence is checked.
    pub fn wait_then_settle(&mut self, d: Duration) {
        std::thread::sleep(d);
        self.settle();
    }

    /// Write a file in the workspace dir mid-scenario. Triggers
    /// external-change reactions when led's file watcher is on. Path is
    /// workspace-relative.
    ///
    /// Includes a baseline 1.5s wall-clock wait before settling because
    /// macOS FSEvents has propagation latency that exceeds settle's
    /// normal quiescence window, especially under parallel test load.
    pub fn fs_write(&mut self, rel_path: &str, contents: &str) {
        let path = self.tmpdir_path.join(rel_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir for fs_write");
        }
        std::fs::write(&path, contents).expect("fs_write");
        self.wait_then_settle(Duration::from_millis(1500));
    }

    /// Delete a file in the workspace dir mid-scenario. Triggers
    /// external-remove reactions when led's file watcher is on. Same
    /// 1.5s baseline wait as `fs_write` for FSEvents propagation.
    pub fn fs_delete(&mut self, rel_path: &str) {
        let path = self.tmpdir_path.join(rel_path);
        let _ = std::fs::remove_file(&path);
        self.wait_then_settle(Duration::from_millis(1500));
    }

    /// Block until BOTH the PTY output stream AND the dispatch trace
    /// have been quiet for ~80ms. Tracking the trace too is essential
    /// for async drivers (LSP, git, gh) where significant work happens
    /// without producing immediate PTY output — only after the response
    /// arrives does the frame change.
    pub fn settle(&mut self) {
        self.settle_for(Duration::from_millis(120));
    }

    fn settle_for(&mut self, quiet: Duration) {
        let min_wait = Duration::from_millis(40);
        let max_wait = Duration::from_secs(15);
        let start = Instant::now();
        std::thread::sleep(min_wait);
        loop {
            let pty_quiet =
                self.last_byte_time.lock().unwrap().elapsed() >= quiet;
            let trace_quiet = self.trace_quiet_for(quiet);
            let lsp_idle = self.lsp_in_flight() == 0;
            if pty_quiet && trace_quiet && lsp_idle {
                return;
            }
            if start.elapsed() > max_wait {
                let in_flight = self.lsp_in_flight();
                panic!(
                    "settle timed out after {:?} — pty_quiet={pty_quiet} trace_quiet={trace_quiet} lsp_in_flight={in_flight}",
                    start.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn trace_quiet_for(&self, quiet: Duration) -> bool {
        let Ok(meta) = std::fs::metadata(&self.trace_path) else {
            return true;
        };
        let Ok(mtime) = meta.modified() else {
            return true;
        };
        mtime.elapsed().map(|e| e >= quiet).unwrap_or(true)
    }

    /// Outstanding LSP requests inferred from the trace.
    ///
    /// Counts `LspSend kind=request` minus `LspRecv kind=response`.
    /// While > 0 the LSP server hasn't responded to every request
    /// the runtime has issued, so any frame snapshot taken now
    /// would be racing the next `LspRecv` → state-change → repaint
    /// cycle. Notifications (both directions) are fire-and-forget
    /// and don't count.
    ///
    /// Trace ids are unique per server, so a simple count suffices;
    /// we don't need to track ids individually. Reads the trace
    /// file each call — acceptable because settle only polls every
    /// 20 ms, and trace files stay small for golden scenarios.
    fn lsp_in_flight(&self) -> usize {
        let Ok(content) = std::fs::read_to_string(&self.trace_path) else {
            return 0;
        };
        let mut sends = 0usize;
        let mut recvs = 0usize;
        for line in content.lines() {
            if line.starts_with("LspSend\t") && line.contains("kind=request") {
                sends += 1;
            } else if line.starts_with("LspRecv\t") && line.contains("kind=response") {
                recvs += 1;
            }
        }
        sends.saturating_sub(recvs)
    }

    fn wait_ready(&mut self) {
        let max_wait = Duration::from_secs(15);
        let start = Instant::now();
        while self.bytes_received.load(Ordering::Relaxed) == 0 {
            if start.elapsed() > max_wait {
                panic!("led produced no output within 15s");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Startup-only: the dispatch trace records intents (e.g.
        // `FileOpen` at dispatch time), not completions. A buffer
        // load that finishes 50–150 ms after the first paint can
        // slip past `settle`'s 120 ms quiet window, leaving keys
        // dispatched against an empty buffer. The wider window
        // here covers the dispatch → driver-completion → ingest →
        // render pipeline for CLI-seeded buffers; subsequent
        // `settle()` calls (between presses) stay at 120 ms.
        self.settle_for(Duration::from_millis(400));
    }

    /// Render the current vt100 screen to a plain-text grid suitable for
    /// snapshot diffing. One row per line, trailing whitespace stripped.
    pub fn frame(&self) -> String {
        let parser = self.parser.lock().unwrap();
        parser.screen().contents()
    }

    pub fn assert_frame(&self, scenario_dir: &Path) {
        let normalized = normalize_paths(&self.frame(), &self.tmpdir_path);
        let golden_path = scenario_dir.join("frame.snap");
        let expected = std::fs::read_to_string(&golden_path).unwrap_or_default();
        let expected_normalized = normalize_paths(&expected, &self.tmpdir_path);
        assert_against_golden_text(
            &normalized,
            &expected_normalized,
            &golden_path,
            "frame",
        );
    }

    /// Read the trace file, normalize tempdir paths to a placeholder, and
    /// snapshot against `dispatched.snap`.
    pub fn assert_trace(&self, scenario_dir: &Path) {
        let raw = std::fs::read_to_string(&self.trace_path).unwrap_or_default();
        let normalized = normalize_trace(&raw, &self.tmpdir_path);
        let golden_path = scenario_dir.join("dispatched.snap");
        let expected = std::fs::read_to_string(&golden_path).unwrap_or_default();
        let expected_normalized = normalize_trace(&expected, &self.tmpdir_path);
        assert_against_golden_text(
            &normalized,
            &expected_normalized,
            &golden_path,
            "trace",
        );
    }
}

/// Compare `actual` to a golden file, supporting `UPDATE_GOLDENS=1`
/// for first-time creation / refresh. Used when the caller has run an
/// additional normalization (e.g. fake-binary path placeholders)
/// over the golden before comparing.
fn assert_against_golden_text(
    actual: &str,
    expected_raw: &str,
    golden_path: &Path,
    kind: &str,
) {
    if std::env::var("UPDATE_GOLDENS").is_ok() {
        if let Some(parent) = golden_path.parent() {
            std::fs::create_dir_all(parent).expect("create scenario dir");
        }
        std::fs::write(golden_path, actual).expect("write golden");
        return;
    }
    // vt100's `screen().contents()` already strips trailing whitespace
    // per row; committed goldens sometimes have it and sometimes don't.
    // Normalize both sides so the diff tests what the rendering
    // actually produces, not incidental whitespace.
    let expected = strip_trailing_ws(expected_raw);
    let actual_n = strip_trailing_ws(actual);
    if actual_n != expected {
        panic!(
            "{kind} mismatch at {}\n--- expected ---\n{expected}\n--- actual ---\n{actual_n}\n--- end ---",
            golden_path.display()
        );
    }
}

fn strip_trailing_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        out.push_str(body.trim_end());
        out.push_str(nl);
    }
    out
}

/// Replace the test tempdir prefix with `<TMPDIR>` so traces are stable
/// across runs. Also drops the `t=<ms>` wall-clock prefix until we have a
/// virtual clock; in the meantime, we only diff category + fields.
///
/// Tries both the raw tempdir path and its canonical form (on macOS,
/// `/var/folders/...` canonicalizes to `/private/var/folders/...` because
/// `/var` is a symlink — led's path canonicalization picks the latter).
/// Replace tempdir prefixes anywhere in `s` with `<TMPDIR>` (handles both
/// the raw and canonical forms — on macOS tempdirs canonicalize through
/// the `/var → /private/var` symlink). Used for both frame and trace
/// normalization since both can leak absolute paths.
pub fn normalize_paths(s: &str, tmpdir: &Path) -> String {
    let raw_tmp = tmpdir.to_string_lossy().to_string();
    let canon_tmp = tmpdir
        .canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let mut out = s.to_string();
    if let Some(ref c) = canon_tmp {
        out = out.replace(c, "<TMPDIR>");
    }
    out = out.replace(&raw_tmp, "<TMPDIR>");
    // Per-machine binary paths leak into snapshots when the
    // editor renders the fake LSP process name in its status
    // line. Replace with a stable placeholder so the goldens
    // are portable across checkouts.
    let bins = binaries();
    out = out.replace(&bins.fake_lsp.to_string_lossy().to_string(), "<FAKE-LSP>");
    // Collapse runs of 2+ spaces on lines containing these
    // placeholders. The placeholder rendering rides on a status
    // line that pads with spaces to right-align an `L<r>:C<c>`
    // marker — and the padding width depends on the original
    // path length, which differs between checkouts. Collapsing
    // makes the comparison position-independent.
    out = collapse_padding_on_placeholder_lines(&out);
    out
}

fn collapse_padding_on_placeholder_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        if body.contains("<FAKE-LSP>") {
            let mut prev_space = false;
            for ch in body.chars() {
                if ch == ' ' {
                    if !prev_space {
                        out.push(' ');
                    }
                    prev_space = true;
                } else {
                    out.push(ch);
                    prev_space = false;
                }
            }
        } else {
            out.push_str(body);
        }
        out.push_str(nl);
    }
    out
}

pub fn normalize_trace(raw: &str, tmpdir: &Path) -> String {
    let raw_tmp = tmpdir.to_string_lossy().to_string();
    let canon_tmp = tmpdir
        .canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let bins = binaries();
    let fake_lsp = bins.fake_lsp.to_string_lossy().to_string();
    let mut lines: Vec<String> = Vec::new();
    for line in raw.lines() {
        let mut s = line.to_string();
        if let Some(ref c) = canon_tmp {
            s = s.replace(c, "<TMPDIR>");
        }
        s = s.replace(&raw_tmp, "<TMPDIR>");
        s = s.replace(&fake_lsp, "<FAKE-LSP>");
        // Drop wall-clock timestamp prefix `t=NNNms\t` for now. Re-add
        // when --test-clock is in place.
        if let Some(rest) = s.strip_prefix("t=")
            && let Some(idx) = rest.find('\t')
        {
            s = rest[idx + 1..].to_string();
        }
        // Mask non-deterministic per-field IDs. Single field for now;
        // expand into a loop when a second masked field arrives.
        s = mask_field(&s, "chain=", "chain=<CHAIN>");
        lines.push(s);
    }
    // Canonicalise consecutive runs of order-independent startup
    // events. The LSP driver thread races against runtime phases:
    // `LspSend method=initialize` and `GitScan` can swap depending
    // on thread scheduling, producing flaky goldens. These events
    // have no causal dependency on each other, so we sort
    // consecutive groups alphabetically.
    sort_independent_runs(&mut lines);
    // Collapse FSEvents-burst duplicates. macOS FSEvents emits
    // multiple kernel events per logical `fs_write` (CREATE +
    // MODIFY at the inode level), and the workspace-root watch
    // registers with `debounce_ms: 0` (see
    // `crates/driver-file-watch/native/src/lib.rs:270` and
    // `crates/runtime/src/query/desired.rs:148`). Each event
    // can re-trigger a whole downstream cycle, producing 1–N
    // back-to-back identical lines (`GitScan`) or 1–N identical
    // multi-line blocks (`LspDidChangeWatchedFiles` + the
    // matching `LspSend workspace/didChangeWatchedFiles`)
    // depending on how the kernel bundled the events. The
    // duplicates carry no test signal — same scan, same
    // notification, same result — so we collapse them rather
    // than masking with a runtime debounce window (which would
    // add real input-to-rescan latency for users). Runs after
    // `sort_independent_runs` so sorted-then-duplicated lines
    // also collapse.
    dedupe_repeated_blocks(&mut lines);
    let mut out = String::new();
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Collapse adjacent repetitions of identical blocks. A block is
/// any contiguous slice of 1–4 lines: this catches the GitScan
/// case (1-line block, `Vec::dedup` would suffice) and the
/// `LspDidChangeWatchedFiles` / `LspSend workspace/didChangeWatchedFiles`
/// case (2-line block, interleaved so single-line dedup misses
/// it). Block size 4 is an empirical ceiling — bigger logical
/// units in the trace would be more likely to share lines with
/// non-duplicated context and false-positive collapse.
///
/// Greedy at each block size: if `lines[i..i+size] ==
/// lines[i+size..i+2*size]`, drop the second copy and re-test at
/// the same `i` so triples collapse to one in two steps.
fn dedupe_repeated_blocks(lines: &mut Vec<String>) {
    for size in 1..=4 {
        let mut i = 0;
        while i + 2 * size <= lines.len() {
            if lines[i..i + size] == lines[i + size..i + 2 * size] {
                lines.drain(i + size..i + 2 * size);
            } else {
                i += 1;
            }
        }
    }
}

/// Sort consecutive trace lines that have no causal dependency on
/// each other. A line is "order-independent" when its event is
/// emitted on its own driver thread or runtime phase with no
/// happens-before relationship to its neighbours — see
/// [`is_order_independent`].
fn sort_independent_runs(lines: &mut Vec<String>) {
    let mut i = 0;
    while i < lines.len() {
        if !is_order_independent(&lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && is_order_independent(&lines[i]) {
            i += 1;
        }
        lines[start..i].sort();
    }
}

/// Startup events that race against each other across the
/// runtime↔driver thread boundary. They share no causal order:
/// `GitScan` is dispatched on the runtime thread; the matching
/// `LspSend method=initialize` is written by the LSP driver
/// thread whenever it dequeues the `LspCmd::Init`. Their relative
/// position in the trace flips run-to-run.
///
/// Lines outside this set (LspRecv responses, user-action
/// dispatches like `textDocument/definition`, save / undo events)
/// have meaningful order and stay put.
fn is_order_independent(line: &str) -> bool {
    line.starts_with("FsListDir\t")
        || line.starts_with("FileOpen\t")
        || line.starts_with("GitScan\t")
        || line.contains("method=initialize ")
        || line.ends_with("method=initialize")
}

fn mask_field(s: &str, prefix: &str, placeholder: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(idx) = rest.find(prefix) {
        out.push_str(&rest[..idx]);
        out.push_str(placeholder);
        let after = &rest[idx + prefix.len()..];
        let end = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

impl Drop for GoldenRunner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Resolve a path under `goldens/scenarios/...` relative to the crate root,
/// for use with `assert_frame`.
pub fn scenario_dir(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios")
        .join(rel)
}
