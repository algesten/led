use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
use led_core::Startup;
use led_core::rx::Stream;
use tokio::sync::oneshot;

#[derive(Parser)]
#[command(name = "led", about = "A lightweight text editor")]
struct Cli {
    /// File or directory to open
    path: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();

    let arg_path = cli.path.as_ref().map(|p| {
        let path = PathBuf::from(p);
        std::fs::canonicalize(&path).unwrap_or(path)
    });

    let start_dir: PathBuf = if arg_path.as_ref().map_or(false, |p| p.is_dir()) {
        arg_path.clone().unwrap()
    } else {
        arg_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };

    let config_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("led");

    let startup = Startup {
        headless: false,
        arg_paths: arg_path.into_iter().collect(),
        start_dir: Arc::new(start_dir),
        config_dir,
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, rx) = oneshot::channel();

            let (_state, _guards) = led::run(startup, Stream::new(), tx);

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
