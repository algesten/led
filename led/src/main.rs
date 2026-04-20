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
use led_runtime::{spawn_drivers, SharedTrace, TabIdGen};
use led_state_buffer_edits::BufferEdits;
use led_state_tabs::{Tab, Tabs};

#[derive(Parser, Debug)]
#[command(name = "led", version, about = "led rewrite — milestone 1 skeleton")]
struct Cli {
    /// File paths to open as tabs. First becomes active.
    files: Vec<PathBuf>,

    /// Append trace lines for each external event/execute.
    #[arg(long)]
    golden_trace: Option<PathBuf>,

    /// Reserved for later milestones; ignored today.
    #[arg(long, hide = true)]
    config_dir: Option<PathBuf>,

    // The goldens runner always passes these; parse-and-ignore so it
    // doesn't trip on unknown-flag errors.
    #[arg(long, hide = true)]
    test_clock: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_lsp_server: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_gh_binary: Option<PathBuf>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

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

    let drivers = spawn_drivers(trace.clone())?;

    let mut stdout = io::stdout();
    led_runtime::run(
        &mut tabs,
        &mut edits,
        &mut store,
        &mut terminal,
        &drivers,
        &mut stdout,
        &trace,
    )?;
    stdout.flush()?;

    Ok(())
}
