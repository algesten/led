mod buffer;
mod config;
mod editor;
mod file_browser;
mod session;
mod theme;
mod ui;

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;

use clap::Parser;
use crossterm::event::{self, Event, KeyEvent};
use notify::{EventKind, RecursiveMode, Watcher};
use ratatui::DefaultTerminal;

use buffer::Buffer;
use editor::{Editor, InputResult};

#[derive(Debug)]
enum ConfigFile {
    Keys,
    Theme,
}

enum AppEvent {
    Key(KeyEvent),
    ConfigChanged(ConfigFile),
}

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
        theme::reset_theme();
        eprintln!("Theme reset to defaults.");
        session::reset_db();
        eprintln!("Session database reset.");
    }

    // Load keymap and theme before ratatui::init() so parse errors print to stderr normally
    let keymap = match config::load_or_create_config() {
        Ok(km) => km,
        Err(e) => {
            eprintln!("warning: failed to load keys.toml: {e}; using defaults");
            config::default_keymap()
        }
    };
    let theme = theme::load_theme();

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
    let mut editor = Editor::new(buffer, keymap, root.clone(), theme);
    editor.debug = cli.debug;

    // Restore session only when no explicit file was passed
    if !explicit_file {
        if let Some(ref conn) = db {
            if let Some(session) = session::load_session(conn, &root) {
                editor.restore_session(session, Some(conn), &root);
            }
        }
    }

    // Install panic hook that restores terminal before printing panic info
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::restore();
        original_hook(info);
    }));

    // Build event channel
    let (tx, rx) = mpsc::channel::<AppEvent>();

    // Thread 1: crossterm key events
    let key_tx = tx.clone();
    std::thread::spawn(move || {
        loop {
            if let Ok(Event::Key(key)) = event::read() {
                if key_tx.send(AppEvent::Key(key)).is_err() {
                    break;
                }
            }
        }
    });

    // Thread 2: config file watcher
    let keys_path = config::config_path();
    let theme_p = theme::theme_path();
    let _watcher = spawn_config_watcher(tx, keys_path.as_deref(), theme_p.as_deref());

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut editor, &rx, db.as_ref(), &root);
    ratatui::restore();

    // Save session on exit
    if let Some(ref conn) = db {
        // Final flush of any pending undo entries
        editor.flush_to_db(conn, &root);
        let session_data = editor.capture_session();
        session::save_session(conn, &root, &session_data);
    }

    result
}

fn run(
    terminal: &mut DefaultTerminal,
    editor: &mut Editor,
    rx: &mpsc::Receiver<AppEvent>,
    db: Option<&rusqlite::Connection>,
    root: &std::path::Path,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| ui::render(editor, frame))?;

        // Combine redraw and persist timeouts — use the shorter one
        let timeout = match (editor.needs_redraw_in(), editor.needs_persist_in()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let event = if let Some(timeout) = timeout {
            match rx.recv_timeout(timeout) {
                Ok(ev) => Some(ev),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        } else {
            match rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => return Ok(()),
            }
        };

        match event {
            Some(AppEvent::Key(key)) => {
                match editor.handle_key_event(key) {
                    InputResult::Continue => {}
                    InputResult::Quit => return Ok(()),
                }
            }
            Some(AppEvent::ConfigChanged(file)) => {
                match file {
                    ConfigFile::Keys => {
                        if let Some(km) = config::reload_keymap() {
                            editor.set_keymap(km);
                            editor.message = Some("Reloaded keys.toml.".into());
                        }
                    }
                    ConfigFile::Theme => {
                        editor.set_theme(theme::load_theme());
                        editor.message = Some("Reloaded theme.toml.".into());
                    }
                }
            }
            None => {}
        }

        // Periodic undo persistence
        if let Some(conn) = db {
            if editor.needs_persist() {
                editor.flush_to_db(conn, root);
            }
        }
    }
}

fn spawn_config_watcher(
    tx: mpsc::Sender<AppEvent>,
    keys_path: Option<&std::path::Path>,
    theme_path: Option<&std::path::Path>,
) -> Option<notify::RecommendedWatcher> {
    let config_dir = keys_path.or(theme_path)?.parent()?;
    let keys_name = keys_path.and_then(|p| p.file_name()).map(|n| n.to_os_string());
    let theme_name = theme_path.and_then(|p| p.file_name()).map(|n| n.to_os_string());

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        let Ok(ev) = res else { return };
        match ev.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {}
            _ => return,
        }
        for path in &ev.paths {
            let fname = path.file_name();
            if fname == keys_name.as_deref() {
                let _ = tx.send(AppEvent::ConfigChanged(ConfigFile::Keys));
            } else if fname == theme_name.as_deref() {
                let _ = tx.send(AppEvent::ConfigChanged(ConfigFile::Theme));
            }
        }
    }).ok()?;

    watcher.watch(config_dir, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}
