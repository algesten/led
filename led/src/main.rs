mod buffer;
mod config;
mod editor;
mod ui;

use std::io;

use crossterm::event::{self, Event};
use ratatui::DefaultTerminal;

use buffer::Buffer;
use editor::{Editor, InputResult};

fn main() -> io::Result<()> {
    // Load keymap before ratatui::init() so parse errors print to stderr normally
    let keymap = match config::load_or_create_config() {
        Ok(km) => km,
        Err(e) => {
            eprintln!("warning: failed to load keys.toml: {e}; using defaults");
            config::default_keymap()
        }
    };

    let buffer = match std::env::args().nth(1) {
        Some(path) => Buffer::from_file(&path).unwrap_or_else(|_| {
            let mut buf = Buffer::empty();
            buf.path = Some(path.into());
            buf
        }),
        None => Buffer::empty(),
    };

    let mut editor = Editor::new(buffer, keymap);

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
