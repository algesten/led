mod buffer;
mod config;
mod editor;
mod file_browser;
mod session;
mod ui;

use std::io;
use std::path::PathBuf;

use clap::Parser;
use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use buffer::Buffer;
use editor::{Editor, InputResult};

#[derive(Parser)]
#[command(name = "led", about = "A lightweight text editor")]
struct Cli {
    /// File or directory to open
    path: Option<String>,

    /// Reset keybinding config to defaults
    #[arg(long)]
    reset_config: bool,

    /// Show captured key presses in the message bar
    #[arg(long)]
    debug: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.reset_config {
        match config::reset_config() {
            Ok(()) => eprintln!("Config reset to defaults."),
            Err(e) => eprintln!("Failed to reset config: {e}"),
        }
        session::reset_db();
        eprintln!("Session database reset.");
    }

    // Load keymap before ratatui::init() so parse errors print to stderr normally
    let keymap = match config::load_or_create_config() {
        Ok(km) => km,
        Err(e) => {
            eprintln!("warning: failed to load keys.toml: {e}; using defaults");
            config::default_keymap()
        }
    };

    let arg_path = cli.path.as_ref().map(PathBuf::from);
    let arg_is_dir = arg_path.as_ref().map_or(false, |p: &PathBuf| p.is_dir());

    let buffer = if arg_is_dir {
        None
    } else {
        cli.path.as_ref().map(|path| {
            Buffer::from_file(path).unwrap_or_else(|_| {
                let mut buf = Buffer::empty();
                buf.path = Some(path.into());
                buf
            })
        })
    };

    // Compute root dir: directory arg directly, file arg's parent, else CWD
    let root: PathBuf = if arg_is_dir {
        arg_path.unwrap()
    } else {
        cli.path
            .as_ref()
            .and_then(|p| {
                let path = PathBuf::from(p);
                path.parent().map(|parent| {
                    if parent.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        parent.to_path_buf()
                    }
                })
            })
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    // Open session DB before ratatui::init() so errors print to stderr
    let db = session::open_db();

    let explicit_file = buffer.is_some();
    let mut editor = Editor::new(buffer, keymap, root.clone());
    editor.debug = cli.debug;

    // Restore session only when no explicit file was passed
    if !explicit_file {
        if let Some(ref conn) = db {
            if let Some(session) = session::load_session(conn, &root) {
                editor.restore_session(session);
            }
        }
    }

    // Install panic hook that restores terminal before printing panic info
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut editor);
    ratatui::restore();

    // Save session on exit
    if let Some(ref conn) = db {
        let session_data = editor.capture_session();
        session::save_session(conn, &root, &session_data);
    }

    result
}

fn run(terminal: &mut DefaultTerminal, editor: &mut Editor) -> io::Result<()> {
    loop {
        terminal.draw(|frame| ui::render(editor, frame))?;

        if let Event::Key(key) = event::read()? {
            if editor.debug {
                editor.message = Some(format!("{key:?}"));
            }
            match editor.handle_key_event(key) {
                InputResult::Continue => {}
                InputResult::Quit => return Ok(()),
            }
        }
    }
}
