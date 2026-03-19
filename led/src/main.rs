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

    /// Write logs to a file (e.g. --log-file /tmp/led.log)
    #[arg(long)]
    log_file: Option<PathBuf>,

    /// After 5s, spam MoveUp for flamegraph profiling
    #[arg(long)]
    flamegraph: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();

    if let Some(ref log_path) = cli.log_file {
        led::logging::init_file_logger(log_path);
    }

    let arg_path = cli.path.as_ref().map(|p| {
        let path = PathBuf::from(p);
        std::fs::canonicalize(&path).unwrap_or_else(|_| {
            // Non-existent file: resolve relative to CWD so start_dir is valid.
            let parent = path.parent().unwrap_or(std::path::Path::new("."));
            let canonical_parent = std::fs::canonicalize(parent)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            canonical_parent.join(path.file_name().unwrap_or_default())
        })
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
        enable_watchers: true,
        arg_paths: arg_path.into_iter().collect(),
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
