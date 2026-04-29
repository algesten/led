//! `led` — the binary entry point.
//!
//! Thin `main`: parse CLI, acquire raw mode, construct sources + drivers,
//! hand off to `led_runtime::run`.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Parser;
use led_core::UserPath;
use led_driver_terminal_native::RawModeGuard;
use led_runtime::{
    load_keymap, load_theme, spawn_drivers, Sources, SharedTrace, TabIdGen, Wake, World,
};
use led_state_browser::FsTree;
use led_state_tabs::Tab;

#[derive(Parser, Debug)]
#[command(name = "led", version, about = "led rewrite — milestone 1 skeleton")]
struct Cli {
    /// File paths to open as tabs. First becomes active.
    files: Vec<PathBuf>,

    /// Config directory (default: `~/.config/led/`). Holds `keys.toml` and `theme.toml`.
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Standalone mode for `$EDITOR` use. Disables session, git, LSP, and file watchers.
    #[arg(long)]
    no_workspace: bool,

    // Test/goldens-only flags. Hidden from --help.
    #[arg(long, hide = true)]
    golden_trace: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_clock: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_lsp_server: Option<PathBuf>,
    #[arg(long, hide = true)]
    test_clipboard_isolated: bool,
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
    // non-fatal warnings for per-region schema problems. Always
    // discovered from `<config_dir>/theme.toml` (no explicit-path
    // override) — `--config-dir` is the only way to point led at a
    // non-default theme.
    let loaded_theme = match load_theme(cli.config_dir.as_deref(), None) {
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

    // Build sources as plain structs.
    let mut sources = Sources {
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
    if cli.no_workspace {
        // Standalone mode: skip session init / save entirely.
        // Phase::Exiting still goes through the quit gate, which
        // checks `session.saved || !session.primary` — pinning
        // both to "done" lets quit fall straight through with no
        // SaveSession dispatch. Mirrors legacy's no-workspace
        // semantics: the file may be open and editable, but no
        // workspace metadata is persisted.
        sources.session.init_done = true;
        sources.session.saved = true;
        sources.session.primary = false;
    }

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
        // Standalone mode silently skips directory args — the
        // typical `--no-workspace` invocation is a single file
        // (commit message, temp file), and opening a directory
        // as a buffer is meaningless when there's no workspace
        // to anchor it to.
        if cli.no_workspace && f.is_dir() {
            continue;
        }
        let id = ids.issue();
        let user = UserPath::new(f);
        let chain = user.resolve_chain();
        let canon = chain.resolved.clone();
        sources.path_chains.insert(canon.clone(), chain);
        sources.tabs.open.push_back(Tab {
            id,
            path: canon,
            ..Default::default()
        });
        // Each CLI file supersedes as the active tab — the LAST path
        // on the command line wins. Matches legacy and the goldens'
        // `led a.txt b.txt` convention (b.txt becomes active).
        sources.tabs.active = Some(id);
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
    let drivers = spawn_drivers(
        trace.clone(),
        &wake,
        lsp_override,
        cli.test_clipboard_isolated,
    )?;

    let mut stdout = io::stdout();
    let mut world = World {
        sources: &mut sources,
        drivers: &drivers,
        keymap: &keymap,
        theme: &theme,
        wake: &wake,
        trace: &trace,
        stdout: &mut stdout,
        cli_config_dir: cli.config_dir.as_deref(),
        no_workspace: cli.no_workspace,
    };
    led_runtime::run(&mut world)?;
    stdout.flush()?;

    Ok(())
}
