mod config;
mod shell;
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
use shell::{Shell, InputResult};
use session::{BufferState, SessionData};

#[derive(Debug)]
enum ConfigFile {
    Keys,
    Theme,
}

enum AppEvent {
    Key(KeyEvent),
    ConfigChanged(ConfigFile),
    BufferNotification(String),
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

    // Compute starting directory, then walk up to find a .git root
    let start_dir: PathBuf = if arg_is_dir {
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
    let start_dir = std::fs::canonicalize(&start_dir).unwrap_or(start_dir);
    let root = find_git_root(&start_dir);

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
    let mut shell = Shell::new(keymap, the_theme, db, root.clone());
    shell.debug = cli.debug;

    let had_initial_buffer = explicit_file;
    for comp in components {
        shell.register(comp);
    }
    if had_initial_buffer {
        shell.set_focus(PanelSlot::Main);
    }

    // Restore session only when no explicit file was passed
    if !explicit_file {
        if let Some(session) = shell.db().and_then(|conn| session::load_session(conn, &root)) {
            restore_session(&mut shell, session);
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

    // Thread 2: notify directory watcher for cross-instance sync
    let notify_dir = led_buffer::notify_dir();
    if let Some(ref dir) = notify_dir {
        cleanup_notify_dir(dir);
    }
    let notify_tx = tx.clone();
    let _notify_watcher = spawn_notify_watcher(notify_tx, notify_dir.as_deref());

    // Thread 3: config file watcher
    let keys_path = config::config_path();
    let theme_p = theme::theme_path();
    let _watcher = spawn_config_watcher(tx, keys_path.as_deref(), theme_p.as_deref());

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut shell, &rx);
    ratatui::restore();

    // Save session on exit
    shell.flush_to_db();
    if let Some(conn) = shell.db() {
        let snapshot = shell.capture_session();
        let session_data = capture_session_data(&shell, &snapshot);
        session::save_session(conn, &root, &session_data);
    }

    result
}

fn run(
    terminal: &mut DefaultTerminal,
    shell: &mut Shell,
    rx: &mpsc::Receiver<AppEvent>,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| ui::render(shell, frame))?;

        let timeout = match (shell.needs_redraw_in(), shell.needs_persist_in()) {
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
                match shell.handle_key_event(key) {
                    InputResult::Continue => {}
                    InputResult::Quit => return Ok(()),
                }
            }
            Some(AppEvent::ConfigChanged(file)) => {
                match file {
                    ConfigFile::Keys => {
                        if let Some(km) = config::reload_keymap() {
                            shell.set_keymap(km);
                            shell.message = Some("Reloaded keys.toml.".into());
                        }
                    }
                    ConfigFile::Theme => {
                        shell.set_theme(theme::load_theme(shell.components()));
                        shell.message = Some("Reloaded theme.toml.".into());
                    }
                }
            }
            Some(AppEvent::BufferNotification(hash)) => {
                shell.handle_notification(&hash);
            }
            None => {}
        }

        if shell.needs_persist() {
            shell.flush_to_db();
        }
    }
}

// --- Session helpers (composition root uses concrete types) ---

fn capture_session_data(
    shell: &Shell,
    snapshot: &shell::SessionSnapshot,
) -> SessionData {
    let mut buffers = Vec::new();
    for comp in shell.components() {
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
    let (browser_selected, browser_expanded_dirs) = shell
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
    shell: &mut Shell,
    session: SessionData,
) {
    // Restore buffers — undo state is loaded by Buffer::restore_session via register()
    for bs in &session.buffers {
        let path_str = bs.file_path.to_string_lossy();
        if let Ok(buf) = Buffer::from_file(&path_str) {
            shell.register(Box::new(buf));
        }
    }

    // Restore cursor/scroll positions after undo replay
    for bs in &session.buffers {
        for comp in shell.components_mut() {
            let Some(buf) = comp.as_any_mut().downcast_mut::<Buffer>() else {
                continue;
            };
            let Some(ref path) = buf.path else { continue };
            if *path == bs.file_path {
                buf.cursor_row = bs.cursor_row.min(buf.line_count().saturating_sub(1));
                buf.cursor_col = bs.cursor_col.min(buf.line_len(buf.cursor_row));
                buf.scroll_offset = bs.scroll_offset;
                break;
            }
        }
    }

    // Set active tab
    shell.set_active_tab(session.active_tab);

    // Set show side panel
    shell.show_side_panel = session.show_side_panel;

    // Set focus
    let has_tabs = shell.has_tabs();
    if session.focus_is_editor && has_tabs {
        shell.set_focus(PanelSlot::Main);
    } else {
        shell.set_focus(PanelSlot::Side);
    }

    // Restore browser state
    for comp in shell.components_mut() {
        if let Some(fb) = comp.as_any_mut().downcast_mut::<FileBrowser>() {
            fb.set_expanded_dirs(session.browser_expanded_dirs.clone());
            fb.selected = session
                .browser_selected
                .min(fb.entries.len().saturating_sub(1));
            break;
        }
    }
}

fn find_git_root(start: &std::path::Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            break;
        }
    }
    start.to_path_buf()
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

fn spawn_notify_watcher(
    tx: mpsc::Sender<AppEvent>,
    dir: Option<&std::path::Path>,
) -> Option<notify::RecommendedWatcher> {
    let dir = dir?;
    std::fs::create_dir_all(dir).ok()?;

    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        let Ok(ev) = res else { return };
        match ev.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {}
            _ => return,
        }
        for path in &ev.paths {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let _ = tx.send(AppEvent::BufferNotification(name.to_string()));
            }
        }
    }).ok()?;

    watcher.watch(dir, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

fn cleanup_notify_dir(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60);
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

