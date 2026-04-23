//! `led` — the binary entry point.
//!
//! Thin `main`: parse CLI, acquire raw mode, construct atoms + drivers,
//! hand off to `led_runtime::run`.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Parser;
use led_core::UserPath;
use led_driver_terminal_native::RawModeGuard;
use led_runtime::{
    load_keymap, load_theme, spawn_drivers, Atoms, SharedTrace, TabIdGen, Wake, World,
};
use led_state_browser::FsTree;
use led_state_tabs::Tab;
use std::time::Instant;

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

    /// Path to a `theme.toml`. Overrides the default resolution
    /// (`<config_dir>/theme.toml` → `~/.config/led/theme.toml` →
    /// built-in). Missing file at an explicit path is an error.
    #[arg(long)]
    theme: Option<PathBuf>,

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

    // Theme resolves the same way: fatal on I/O or malformed TOML,
    // non-fatal warnings for per-region schema problems.
    let loaded_theme = match load_theme(cli.config_dir.as_deref(), cli.theme.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("led: theme error: {e}");
            std::process::exit(2);
        }
    };
    for w in &loaded_theme.warnings {
        eprintln!("led: theme warning: {w}");
    }
    let theme = loaded_theme.theme;

    let trace = match cli.golden_trace {
        Some(path) => {
            let root = std::env::var_os("LED_TRACE_ROOT").map(PathBuf::from);
            SharedTrace::file(path, root)?
        }
        None => SharedTrace::noop(),
    };

    // Build atoms as plain structs.
    let mut atoms = Atoms {
        // Workspace root = process cwd. M19 (git integration) will
        // walk up for `.git` instead; for M11 the CWD convention
        // matches the typical `cd <project> && led <file>` path.
        fs: FsTree {
            root: std::env::current_dir()
                .ok()
                .map(|p| UserPath::new(&p).canonicalize()),
            ..Default::default()
        },
        ..Default::default()
    };

    // Seed tabs from CLI args. Each open path auto-expands its
    // ancestor directories in the browser so the tree reveals the
    // file — the runtime's active-tab reveal machinery
    // (set_active_reveal, driven from the main-loop execute phase)
    // auto-expands ancestors on every tab switch, so CLI startup
    // doesn't need its own reveal pass. Walking the symlink chain
    // HERE (while we still hold the user-typed path) lets the
    // language detector route `foo.rs → bar` to Rust even though
    // the canon tail has no extension.
    let mut ids = TabIdGen::default();
    for f in &cli.files {
        let id = ids.issue();
        let user = UserPath::new(f);
        let chain = user.resolve_chain();
        let canon = chain.resolved.clone();
        atoms.path_chains.insert(canon.clone(), chain);
        atoms.tabs.open.push_back(Tab {
            id,
            path: canon,
            ..Default::default()
        });
        // Each CLI file supersedes as the active tab — the LAST path
        // on the command line wins. Matches legacy and the goldens'
        // `led a.txt b.txt` convention (b.txt becomes active).
        atoms.tabs.active = Some(id);
    }

    // Raw mode *after* CLI parse so `--help` / parse errors still go to
    // a cooked terminal. Held for the entire main loop lifetime; its
    // `Drop` restores cooked mode on normal exit and on panic unwind.
    let _raw = RawModeGuard::acquire()?;

    let wake = Wake::new();
    let lsp_override = cli
        .test_lsp_server
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned());
    let drivers = spawn_drivers(trace.clone(), &wake, lsp_override)?;

    let mut stdout = io::stdout();
    let mut world = World {
        atoms: &mut atoms,
        drivers: &drivers,
        keymap: &keymap,
        theme: &theme,
        wake: &wake,
        trace: &trace,
        stdout: &mut stdout,
    };
    led_runtime::run(&mut world)?;
    stdout.flush()?;

    Ok(())
}
