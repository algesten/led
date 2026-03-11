use std::path::PathBuf;

use clap::Parser;
use led_core::{Config, State};

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

    let config = Config { arg_path };

    let state = State::new(config);
}
