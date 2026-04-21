//! `led` — the binary entry point.
//!
//! Thin `main`: parse CLI, acquire raw mode, construct atoms + drivers,
//! hand off to `led_runtime::run`.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Parser;
use led_core::UserPath;
use led_driver_buffers_core::BufferStore;
use led_driver_terminal_core::Terminal;
use led_driver_terminal_native::RawModeGuard;
use led_runtime::{load_keymap, spawn_drivers, SharedTrace, TabIdGen, Wake};
use led_state_alerts::AlertState;
use led_state_browser::BrowserState;
use led_state_buffer_edits::BufferEdits;
use led_state_jumps::JumpListState;
use led_state_kill_ring::KillRing;
use led_state_tabs::{Tab, Tabs};

#[derive(Parser, Debug)]
#[command(name = "led", version, about = "led rewrite — milestone 1 skeleton")]
struct Cli {
    /// File paths to open as tabs. First becomes active.
    files: Vec<PathBuf>,

    /// Append trace lines for each external event/execute.
    #[arg(long)]
    golden_trace: Option<PathBuf>,

    /// Directory containing `config.toml`. Defaults to
    /// `~/.config/led/`. Missing file is not an error.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    // The goldens runner always passes these; parse-and-ignore so it
    // doesn't trip on unknown-flag errors. Each wires up in its own
    // later milestone (see docs/rewrite/ROADMAP.md).
    #[arg(long, hide = true)]
    test_clock: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_lsp_server: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_gh_binary: Option<PathBuf>,

    /// Skip workspace root detection; treat the process's CWD as the
    /// only directory relevant to this session. Used by the goldens
    /// harness when a scenario only cares about individual files.
    /// Currently parse-only — workspace scope lands in M11 / M21.
    #[arg(long, hide = true)]
    no_workspace: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    // Load keymap before raw mode so parse errors and per-binding
    // warnings land on a cooked terminal where the user can read
    // them. Per-binding problems are non-fatal — they surface as
    // warnings and that entry is skipped. Fatal (I/O, malformed
    // TOML) exits with code 2.
    let loaded = match load_keymap(cli.config_dir.as_deref()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("led: config error: {e}");
            std::process::exit(2);
        }
    };
    for w in &loaded.warnings {
        eprintln!("led: config warning: {w}");
    }
    let keymap = loaded.keymap;

    let trace = match cli.golden_trace {
        Some(path) => {
            let root = std::env::var_os("LED_TRACE_ROOT").map(PathBuf::from);
            SharedTrace::file(path, root)?
        }
        None => SharedTrace::noop(),
    };

    // Build atoms as plain structs — drv 0.2.0 has no wrapper type.
    let mut tabs = Tabs::default();
    let mut edits = BufferEdits::default();
    let mut kill_ring = KillRing::default();
    let mut alerts = AlertState::default();
    let mut jumps = JumpListState::default();
    // Workspace root = process cwd. M19 (git integration) will walk
    // up for `.git` instead; for M11 the CWD convention matches the
    // typical `cd <project> && led <file>` invocation.
    let mut browser = BrowserState {
        root: std::env::current_dir()
            .ok()
            .map(|p| UserPath::new(&p).canonicalize()),
        ..Default::default()
    };
    let mut store = BufferStore::default();
    let mut terminal = Terminal::default();

    // Seed tabs from CLI args.
    let mut ids = TabIdGen::default();
    for f in &cli.files {
        let id = ids.next();
        tabs.open.push_back(Tab {
            id,
            path: UserPath::new(f).canonicalize(),
            ..Default::default()
        });
        if tabs.active.is_none() {
            tabs.active = Some(id);
        }
    }

    // Raw mode *after* CLI parse so `--help` / parse errors still go to
    // a cooked terminal. Held for the entire main loop lifetime; its
    // `Drop` restores cooked mode on normal exit and on panic unwind.
    let _raw = RawModeGuard::acquire()?;

    let wake = Wake::new();
    let drivers = spawn_drivers(trace.clone(), &wake)?;

    let mut stdout = io::stdout();
    led_runtime::run(
        &mut tabs,
        &mut edits,
        &mut kill_ring,
        &mut alerts,
        &mut jumps,
        &mut browser,
        &mut store,
        &mut terminal,
        &drivers,
        &wake,
        &keymap,
        &mut stdout,
        &trace,
    )?;
    stdout.flush()?;

    Ok(())
}
