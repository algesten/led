mod config;
mod shell;
mod session;
mod theme;
mod ui;

use std::io;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use clap::Parser;
use crossterm::event::{self, Event, KeyEvent};
use notify::{EventKind, RecursiveMode, Watcher};
use ratatui::DefaultTerminal;

use led_core::PanelSlot;
use led_buffer::{Buffer, BufferFactory};
use led_file_browser::FileBrowser;
use led_file_search::FileSearch;
use shell::{Shell, InputResult};
use session::SessionData;

#[derive(Debug)]
enum ConfigFile {
    Keys,
    Theme,
}

enum AppEvent {
    Key(KeyEvent),
    Resize,
    ConfigChanged(ConfigFile),
    Wakeup,
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

    // Build event channel and waker early so buffers can use them
    let (tx, rx) = mpsc::channel::<AppEvent>();
    let waker_tx = tx.clone();
    let waker: led_core::Waker = Arc::new(move || {
        let _ = waker_tx.send(AppEvent::Wakeup);
    });

    // Build component list — the ONLY place concrete types appear
    let initial_buffer = if arg_is_dir {
        None
    } else {
        cli.path.as_ref().map(|path| {
            Buffer::from_file_with_waker(path, Some(waker.clone())).unwrap_or_else(|_| {
                let mut buf = Buffer::empty();
                buf.path = Some(path.into());
                buf
            })
        })
    };

    let mut components: Vec<Box<dyn led_core::Component>> = vec![
        Box::new(BufferFactory::new()),
        Box::new(FileSearch::new(root.clone(), Some(waker.clone()))),
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
    shell.set_waker(waker);

    let had_initial_buffer = explicit_file;
    for comp in components {
        shell.register(comp);
    }
    if had_initial_buffer {
        shell.set_focus(PanelSlot::Main);
    }
    shell.single_file_mode = explicit_file;

    // Restore session only when no explicit file was passed
    if !explicit_file {
        if let Some(session) = shell.db().and_then(|conn| session::load_session(conn, &root)) {
            restore_session(&mut shell, session);
        }
    }

    // Safety: if there are no tabs, ensure focus is on the side panel
    if !shell.has_tabs() {
        shell.set_focus(PanelSlot::Side);
    }

    // Install panic hook
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::restore();
        original_hook(info);
    }));

    // Thread 1: crossterm key events
    let key_tx = tx.clone();
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key)) => {
                    if key_tx.send(AppEvent::Key(key)).is_err() {
                        break;
                    }
                }
                Ok(Event::Resize(_, _)) => {
                    if key_tx.send(AppEvent::Resize).is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    });

    // Thread 2: config file watcher
    let keys_path = config::config_path();
    let theme_p = theme::theme_path();
    let _watcher = spawn_config_watcher(tx, keys_path.as_deref(), theme_p.as_deref());

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut shell, &rx);
    ratatui::restore();

    // Save session on exit (skip if we stayed in single-file mode)
    shell.flush_to_db();
    if !shell.single_file_mode {
        shell.save_all_sessions();
        if let Some(conn) = shell.db() {
            let snapshot = shell.capture_session();
            let buffer_paths: Vec<_> = shell.components().iter()
                .filter_map(|c| c.tab().and_then(|t| t.path))
                .collect();
            let session_data = SessionData {
                buffer_paths,
                active_tab: snapshot.active_tab,
                focus_is_editor: snapshot.focus == PanelSlot::Main,
                show_side_panel: snapshot.show_side_panel,
            };
            session::save_session(conn, &root, &session_data);
        }
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
                    InputResult::Suspend => {
                        shell.flush_to_db();
                        crossterm::terminal::disable_raw_mode()?;
                        crossterm::execute!(
                            io::stdout(),
                            crossterm::terminal::LeaveAlternateScreen,
                            crossterm::cursor::Show
                        )?;
                        // SAFETY: raise(SIGTSTP) is the standard way to suspend a process.
                        unsafe { libc::raise(libc::SIGTSTP); }
                        crossterm::terminal::enable_raw_mode()?;
                        crossterm::execute!(
                            io::stdout(),
                            crossterm::terminal::EnterAlternateScreen,
                            crossterm::cursor::Hide
                        )?;
                        shell.emit(led_core::Event::Resume);
                        terminal.clear()?;
                    }
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
            Some(AppEvent::Wakeup) => {
                shell.tick();
            }
            Some(AppEvent::Resize) => {} // just redraw on next loop iteration
            None => {}
        }

        if shell.needs_persist() {
            shell.flush_to_db();
        }
    }
}

// --- Session helpers ---

fn restore_session(
    shell: &mut Shell,
    session: SessionData,
) {
    // Restore buffers — undo + cursor state loaded by Buffer::restore_session via register()
    let waker = shell.waker().cloned();
    for path in &session.buffer_paths {
        let path_str = path.to_string_lossy();
        if let Ok(buf) = Buffer::from_file_with_waker(&path_str, waker.clone()) {
            shell.register(Box::new(buf));
        }
    }

    // Set shell state
    shell.set_active_tab(session.active_tab);
    shell.show_side_panel = session.show_side_panel;
    let has_tabs = shell.has_tabs();
    if session.focus_is_editor && has_tabs {
        shell.set_focus(PanelSlot::Main);
    } else {
        shell.set_focus(PanelSlot::Side);
    }

    // Restore side panel components (FileBrowser loads its own state from DB via kv)
    shell.restore_sidepanel_sessions();
}

fn find_git_root(start: &std::path::Path) -> PathBuf {
    let mut dir = start.to_path_buf();
    let mut root = None;
    loop {
        if dir.join(".git").exists() {
            root = Some(dir.clone());
        }
        if !dir.pop() {
            break;
        }
    }
    root.unwrap_or_else(|| start.to_path_buf())
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


