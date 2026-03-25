use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
use led_config_file::TomlFile;
use led_core::Startup;
use led_core::keys::Keys;
use led_core::rx::Stream;
use led_core::theme::Theme;
use tokio::sync::oneshot;

#[derive(Parser)]
#[command(name = "led", about = "A lightweight text editor")]
struct Cli {
    /// Files or directory to open
    paths: Vec<String>,

    /// Write logs to a file (e.g. --log-file /tmp/led.log)
    #[arg(long)]
    log_file: Option<PathBuf>,

    /// Reset keybinding and theme config to defaults, and clear session database
    #[arg(long)]
    reset_config: bool,

    /// After 5s, spam MoveUp for flamegraph profiling
    #[arg(long)]
    flamegraph: bool,

    /// After 10s (LSP warm-up), type chars then C-a C-k in a loop
    #[arg(long)]
    flamegraph2: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();

    if let Some(ref log_path) = cli.log_file {
        led::logging::init_file_logger(log_path);
    }

    let canonicalize_path = |p: &str| -> PathBuf {
        let path = PathBuf::from(p);
        std::fs::canonicalize(&path).unwrap_or_else(|_| {
            // Non-existent file: resolve relative to CWD so start_dir is valid.
            let parent = path.parent().unwrap_or(std::path::Path::new("."));
            let canonical_parent = std::fs::canonicalize(parent)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            canonical_parent.join(path.file_name().unwrap_or_default())
        })
    };

    let resolved: Vec<PathBuf> = cli.paths.iter().map(|p| canonicalize_path(p)).collect();

    // Single directory: open in file browser, no files.
    // Otherwise: filter out directories, open remaining files.
    let (arg_dir, arg_paths, start_dir) = if resolved.len() == 1 && resolved[0].is_dir() {
        let dir = resolved.into_iter().next().unwrap();
        let start = dir.clone();
        (Some(dir), vec![], start)
    } else {
        let files: Vec<PathBuf> = resolved.into_iter().filter(|p| !p.is_dir()).collect();
        let start = files
            .first()
            .and_then(|p| p.parent())
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        (None, files, start)
    };

    let config_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("led");

    if cli.reset_config {
        std::fs::create_dir_all(&config_dir).ok();

        match std::fs::write(config_dir.join(Keys::file_name()), Keys::default_toml()) {
            Ok(()) => eprintln!("Config reset to defaults."),
            Err(e) => eprintln!("Failed to reset config: {e}"),
        }

        match std::fs::write(config_dir.join(Theme::file_name()), Theme::default_toml()) {
            Ok(()) => eprintln!("Theme reset to defaults."),
            Err(e) => eprintln!("Failed to reset theme: {e}"),
        }

        match std::fs::remove_file(config_dir.join("db.sqlite")) {
            Ok(()) => eprintln!("Session database reset."),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Session database reset.")
            }
            Err(e) => eprintln!("Failed to reset session database: {e}"),
        }
    }

    let startup = Startup {
        headless: false,
        enable_watchers: true,
        arg_paths,
        arg_dir,
        start_dir: Arc::new(start_dir),
        config_dir,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, rx) = oneshot::channel();

            let actions_in: Stream<led_core::Action> = Stream::new();
            let (_state, _guards) = led::run(startup, actions_in.clone(), tx);

            if cli.flamegraph {
                let stream = actions_in.clone();
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    loop {
                        stream.push(led_core::Action::FileStart);
                        tokio::task::yield_now().await;
                        stream.push(led_core::Action::PageDown);
                        tokio::task::yield_now().await;
                        for _ in 0..80 {
                            stream.push(led_core::Action::MoveDown);
                            tokio::task::yield_now().await;
                        }
                    }
                });
            }

            if cli.flamegraph2 {
                let stream = actions_in.clone();
                tokio::task::spawn_local(async move {
                    // Wait for LSP to start up
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    let chars = "abcdefghihjklkjasd";
                    loop {
                        for c in chars.chars() {
                            stream.push(led_core::Action::InsertChar(c));
                            tokio::task::yield_now().await;
                        }
                        // C-a: go to line start
                        stream.push(led_core::Action::LineStart);
                        tokio::task::yield_now().await;
                        // C-k: kill line
                        stream.push(led_core::Action::KillLine);
                        tokio::task::yield_now().await;
                    }
                });
            }

            let _ = rx.await;
        })
        .await;

    // Restore terminal state on exit.
    disable_raw_mode().ok();
    crossterm::execute!(
        io::stdout(),
        crossterm::cursor::Show,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )
    .ok();
}
