use std::path::PathBuf;
use std::pin::Pin;

use clap::Parser;
use led_core::Startup;
use led_state::AppState;
use led_workspace::Workspace;
use tokio::sync;
use tokio_stream::{Stream, StreamExt};

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

mod ui;

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

    let config = Startup {
        arg_path,
        start_dir,
    };

    let state = AppState::new(config);

    // Channel to "hoist" the state output from Model as
    // input into Derived.
    let (state_tx, _rx) = sync::broadcast::channel(10);

    // Derived makes output to Drivers
    let derived = Derived::new(&state_tx);

    // Drivers is the input from the drivers
    let drivers = Drivers {
        workspace: Box::pin(led_workspace::driver(derived.workspace)),
    };

    // And model is a reducer that takes input from drivers to make new state.
    let mut state_s_real = model(drivers, state);

    // Hoisting loop.
    while let Some(v) = state_s_real.next().await {
        if let Err(e) = state_tx.send(v) {
            panic!("State hoist error: {}", e);
        }
    }
}

pub struct Drivers {
    workspace: Pin<Box<dyn Stream<Item = Workspace> + Send>>,
}
