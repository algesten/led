mod buffer;
mod config;
mod editor;
mod file_browser;
mod ui;

use std::io;
use std::path::PathBuf;

use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use buffer::Buffer;
use editor::{Editor, InputResult};

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--reset-config") {
        match config::reset_config() {
            Ok(()) => eprintln!("Config reset to defaults."),
            Err(e) => eprintln!("Failed to reset config: {e}"),
        }
    }

    // Load keymap before ratatui::init() so parse errors print to stderr normally
    let keymap = match config::load_or_create_config() {
        Ok(km) => km,
        Err(e) => {
            eprintln!("warning: failed to load keys.toml: {e}; using defaults");
            config::default_keymap()
        }
    };

    let file_arg = args.iter().skip(1).find(|a| !a.starts_with("--")).cloned();

    let buffer = match &file_arg {
        Some(path) => Buffer::from_file(path).unwrap_or_else(|_| {
            let mut buf = Buffer::empty();
            buf.path = Some(path.into());
            buf
        }),
        None => Buffer::empty(),
    };

    // Compute root dir: file arg's parent dir, else CWD
    let root: PathBuf = file_arg
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
        .unwrap_or_else(|| PathBuf::from("."));
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let mut editor = Editor::new(buffer, keymap, root);

    // Install panic hook that restores terminal before printing panic info
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut editor);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, editor: &mut Editor) -> io::Result<()> {
    loop {
        terminal.draw(|frame| ui::render(editor, frame))?;

        if let Event::Key(key) = event::read()? {
            match editor.handle_key_event(key) {
                InputResult::Continue => {}
                InputResult::Quit => return Ok(()),
            }
        }
    }
}
