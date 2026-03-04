mod config;
mod editor;
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

use led_core::PanelSlot;
use led_buffer::{Buffer, BufferFactory};
use led_file_browser::FileBrowser;
use editor::{Editor, InputResult};
use session::{BufferState, SessionData};

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

    let arg_path = cli.path.as_ref().map(PathBuf::from);
    let arg_is_dir = arg_path.as_ref().map_or(false, |p: &PathBuf| p.is_dir());

    // Compute root dir
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

    // Build component list — the ONLY place concrete types appear
    let initial_buffer = if arg_is_dir {
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

    let mut components: Vec<Box<dyn led_core::Component>> = vec![
        Box::new(BufferFactory),
        Box::new(FileBrowser::new(root.clone())),
    ];
    if let Some(buf) = initial_buffer {
        components.push(Box::new(buf));
    }

    if cli.reset_config {
        match config::reset_config() {
            Ok(()) => eprintln!("Config reset to defaults."),
            Err(e) => eprintln!("Failed to reset config: {e}"),
        }
        theme::reset_theme(&components);
        eprintln!("Theme reset to defaults.");
        session::reset_db();
        eprintln!("Session database reset.");
    }

    let keymap = match config::load_or_create_config() {
        Ok(km) => km,
        Err(e) => {
            eprintln!("warning: failed to load keys.toml: {e}; using defaults");
            config::default_keymap()
        }
    };
    let the_theme = theme::load_theme(&components);

    let db = session::open_db();

    let explicit_file = components.len() > 2; // more than BufferFactory + FileBrowser
    let mut editor = Editor::new(keymap, the_theme);
    editor.debug = cli.debug;

    let had_initial_buffer = explicit_file;
    for comp in components {
        editor.register(comp);
    }
    if had_initial_buffer {
        editor.set_focus(PanelSlot::Main);
    }

    // Restore session only when no explicit file was passed
    if !explicit_file {
        if let Some(ref conn) = db {
            if let Some(session) = session::load_session(conn, &root) {
                restore_session(&mut editor, session, Some(conn), &root);
            }
        }
    }

    // Install panic hook
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
        editor.flush_to_db(conn, &root);
        let snapshot = editor.capture_session();
        let session_data = capture_session_data(&editor, &snapshot);
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
                        editor.set_theme(theme::load_theme(editor.components()));
                        editor.message = Some("Reloaded theme.toml.".into());
                    }
                }
            }
            None => {}
        }

        if let Some(conn) = db {
            if editor.needs_persist() {
                editor.flush_to_db(conn, root);
            }
        }
    }
}

// --- Session helpers (composition root uses concrete types) ---

fn capture_session_data(
    editor: &Editor,
    snapshot: &editor::SessionSnapshot,
) -> SessionData {
    let mut buffers = Vec::new();
    for comp in editor.components() {
        if comp.tab().is_none() {
            continue;
        }
        if let Some(buf) = comp.as_any().downcast_ref::<Buffer>() {
            if let Some(ref path) = buf.path {
                buffers.push(BufferState {
                    file_path: path.clone(),
                    cursor_row: buf.cursor_row,
                    cursor_col: buf.cursor_col,
                    scroll_offset: buf.scroll_offset,
                });
            }
        }
    }

    // Get browser state
    let (browser_selected, browser_expanded_dirs) = editor
        .components()
        .iter()
        .find_map(|c| {
            c.as_any().downcast_ref::<FileBrowser>().map(|fb| {
                (fb.selected, fb.expanded_dirs().clone())
            })
        })
        .unwrap_or_default();

    SessionData {
        buffers,
        active_tab: snapshot.active_tab,
        focus_is_editor: snapshot.focus == PanelSlot::Main,
        show_side_panel: snapshot.show_side_panel,
        browser_selected,
        browser_expanded_dirs,
    }
}

fn restore_session(
    editor: &mut Editor,
    session: SessionData,
    conn: Option<&rusqlite::Connection>,
    root: &std::path::Path,
) {
    let root_str = root.to_string_lossy();

    // Restore buffers
    for bs in &session.buffers {
        let path_str = bs.file_path.to_string_lossy();
        if let Ok(mut buf) = Buffer::from_file(&path_str) {
            // Try to restore undo state
            if let Some(conn) = conn {
                if let Some((entries, undo_cursor, distance_from_save, stored_hash)) =
                    session::load_undo(conn, &root_str, &path_str)
                {
                    let current_hash = buf.content_hash();
                    if current_hash == stored_hash {
                        buf.restore_undo(entries, undo_cursor, distance_from_save);
                    }
                }
            }
            // Clamp cursor to valid ranges
            buf.cursor_row = bs.cursor_row.min(buf.line_count().saturating_sub(1));
            buf.cursor_col = bs.cursor_col.min(buf.line_len(buf.cursor_row));
            buf.scroll_offset = bs.scroll_offset;
            editor.register(Box::new(buf));
        }
    }

    // Set active tab
    editor.set_active_tab(session.active_tab);

    // Set show side panel
    editor.show_side_panel = session.show_side_panel;

    // Set focus
    let has_tabs = editor.has_tabs();
    if session.focus_is_editor && has_tabs {
        editor.set_focus(PanelSlot::Main);
    } else {
        editor.set_focus(PanelSlot::Side);
    }

    // Restore browser state
    for comp in editor.components_mut() {
        if let Some(fb) = comp.as_any_mut().downcast_mut::<FileBrowser>() {
            fb.set_expanded_dirs(session.browser_expanded_dirs.clone());
            fb.selected = session
                .browser_selected
                .min(fb.entries.len().saturating_sub(1));
            break;
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
