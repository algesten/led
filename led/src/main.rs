use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use clap::Parser;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
use led_config_file::ConfigFile;
use led_core::keys::Keys;
use led_core::theme::Theme;
use led_core::{AStream, Alert, FanoutStreamExt, Startup};
use led_input::TerminalInput;
use led_state::AppState;
use led_workspace::Workspace;
use tokio::sync;
use tokio_stream::StreamExt;

use crate::derived::Derived;
use crate::model::model;

mod derived;
mod model;

#[derive(Parser)]
#[command(name = "led", about = "A lightweight text editor")]
struct Cli {
    /// File or directory to open
    path: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let arg_path = cli.path.as_ref().map(|p| {
        let path = PathBuf::from(p);
        std::fs::canonicalize(&path).unwrap_or(path)
    });

    // Compute starting directory
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

    let state = AppState::new(startup);

    // Channel to "hoist" the state output from Model as
    // input into Derived.
    let (state_tx, _rx) = sync::broadcast::channel(10);

    // Derived makes output to Drivers
    let derived = Derived::new(&state_tx);

    // UI driver: renders latest state to the terminal.
    let _ui = led_ui::driver(state_tx.latest());

    // Seed the hoisting channel so Derived and UI have an initial state
    // to work with. Without this, the system deadlocks: Derived waits for
    // state, drivers wait for Derived, model waits for drivers.
    state_tx.send(Arc::new(state.clone())).ok();

    // Drivers is the input from the drivers
    let drivers = {
        let f = led_config_file::driver(derived.config_file_out.one_by_one());
        let t = led_config_file::driver(derived.config_file_out.one_by_one());

        Drivers {
            workspace: Box::pin(led_workspace::driver(derived.workspace)),
            config_file_keys: Box::pin(f),
            config_file_theme: Box::pin(t),
            storage: Box::pin(led_storage::driver(derived.storage)),
            input: Box::pin(led_input::driver()),
        }
    };

    // And model is a reducer that takes input from drivers to make new state.
    let mut state_s_real = model(drivers, state);

    // Hoisting loop.
    while let Some(v) = state_s_real.next().await {
        let quit = v.quit;
        if let Err(e) = state_tx.send(v) {
            panic!("State hoist error: {}", e);
        }
        if quit {
            break;
        }
    }

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

pub struct Drivers {
    workspace: Pin<Box<dyn AStream<Workspace> + Send>>,
    config_file_keys: Pin<Box<dyn AStream<Result<ConfigFile<Keys>, Alert>>>>,
    config_file_theme: Pin<Box<dyn AStream<Result<ConfigFile<Theme>, Alert>>>>,
    storage: Pin<Box<dyn AStream<Result<led_storage::StorageIn, Alert>>>>,
    input: Pin<Box<dyn AStream<TerminalInput>>>,
}
