use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
use led_config_file::ConfigFile;
use led_core::keys::Keys;
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{Alert, Startup};
use led_state::{AppState, Workspace};
use led_terminal_in::InputGuard;
use led_ui::Ui;
use tokio::sync::oneshot;

use crate::derived::derived;
use crate::model::model;

mod derived;
mod model;

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

    let startup = Startup {
        arg_path,
        start_dir: Arc::new(start_dir),
    };

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, rx) = oneshot::channel();

            run(startup, tx);

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

fn run(startup: Startup, tx: oneshot::Sender<()>) {
    let init = AppState::new(startup);

    // 1. Hoisted AppState
    let state: Stream<Arc<AppState>> = Stream::new();

    // 2. Derived
    let d = derived(state.clone());

    // 3. Drivers

    let drivers = Drivers {
        input_guard: led_terminal_in::setup_terminal(),
        terminal_in: led_terminal_in::driver(),
        workspace_in: led_workspace::driver(d.workspace_out),
        storage_in: led_storage::driver(d.storage_out),
        config_keys_in: led_config_file::driver::<Keys>(d.config_file_out.clone()),
        config_theme_in: led_config_file::driver::<Theme>(d.config_file_out),
        ui: led_ui::driver(d.ui),
    };

    // 4. Model
    let real_state = model(drivers, init);

    // 5. Hoist
    real_state.forward(&state);

    // Signal quit
    let mut tx = Some(tx);
    state.on(move |s: &Arc<AppState>| {
        if s.quit {
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
        }
    });
}

pub struct Drivers {
    pub input_guard: InputGuard,
    pub terminal_in: Stream<led_terminal_in::TerminalInput>,
    pub workspace_in: Stream<Workspace>,
    pub storage_in: Stream<Result<led_storage::StorageIn, Alert>>,
    pub config_keys_in: Stream<Result<ConfigFile<Keys>, Alert>>,
    pub config_theme_in: Stream<Result<ConfigFile<Theme>, Alert>>,
    pub ui: Ui,
}
