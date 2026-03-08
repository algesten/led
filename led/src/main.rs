mod config;
mod logger;
mod session;
mod shell;
mod theme;
mod ui;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use notify::{EventKind, RecursiveMode, Watcher};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use led_buffer::{Buffer, BufferFactory};
use led_core::PanelSlot;
use led_file_browser::FileBrowser;
use led_file_search::FileSearch;
use led_find_file::FindFilePanel;
use led_git_status::GitStatus;
use led_jump_list::JumpList;
use led_lsp::LspManager;
use session::SessionData;
use shell::{InputResult, Shell};

#[derive(Debug)]
enum ConfigFile {
    Keys,
    Theme,
}

enum AppEvent {
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let arg_path = cli.path.as_ref().map(|p| {
        let path = PathBuf::from(p);
        std::fs::canonicalize(&path).unwrap_or(path)
    });

    // Compute starting directory, then walk up to find a .git root
    let start_dir: PathBuf = if arg_path.as_ref().map_or(false, |p| p.is_dir()) {
        arg_path.clone().unwrap()
    } else {
        arg_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let start_dir = std::fs::canonicalize(&start_dir).unwrap_or(start_dir);
    let root = find_git_root(&start_dir);
    let primary_lock = try_become_primary(&root);

    // Build event channel and waker early so buffers can use them
    let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
    let waker_tx = tx.clone();
    let waker: led_core::Waker = Arc::new(move || {
        let _ = waker_tx.send(AppEvent::Wakeup);
    });

    let shared_log = logger::init(if cli.debug {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });

    // Build component list — the ONLY place concrete types appear
    let initial_buffer = arg_path.as_ref().filter(|p| p.is_file()).map(|path| {
        let path_str = path.to_string_lossy();
        Buffer::from_file_with_waker(&path_str, Some(waker.clone())).unwrap_or_else(|_| {
            let mut buf = Buffer::empty();
            buf.path = Some(path.clone());
            buf
        })
    });

    let mut components: Vec<Box<dyn led_core::Component>> = vec![
        Box::new(BufferFactory::new()),
        Box::new(FileSearch::new(root.clone(), Some(waker.clone()))),
        Box::new(FileBrowser::new(root.clone())),
        Box::new(FindFilePanel::new()),
        Box::new(GitStatus::new(root.clone(), Some(waker.clone()))),
        Box::new(JumpList::new()),
        Box::new(LspManager::new(root.clone(), Some(waker.clone()))),
        Box::new(led_messages::Messages::new(shared_log)),
    ];
    if let Some(buf) = initial_buffer {
        components.push(Box::new(buf));
    }

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

    let keymap = match config::load_or_create_config() {
        Ok(km) => km,
        Err(e) => {
            log::warn!("failed to load keys.toml: {e}; using defaults");
            config::default_keymap()
        }
    };
    let the_theme = theme::load_theme();

    let db = session::open_db();

    let explicit_file = arg_path.as_ref().map_or(false, |p| p.is_file());
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
    // Primary always restores the full workspace; secondary only shows the direct file
    if primary_lock.is_some() {
        if let Some(session) = shell
            .db()
            .and_then(|conn| session::load_session(conn, &root))
        {
            restore_session(&mut shell, session);
        }
    }

    // Safety: if there are no tabs, ensure focus is on the side panel
    if !shell.has_tabs() {
        shell.set_focus(PanelSlot::Side);
    }

    if cli.debug {
        shell.emit(led_core::Event::OpenMessages);
    }

    // Install panic hook
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::restore();
        original_hook(info);
    }));

    // Config file watcher
    let keys_path = config::config_path();
    let theme_p = theme::theme_path();
    let _watcher = spawn_config_watcher(tx, keys_path.as_deref(), theme_p.as_deref());

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut shell, rx).await;
    ratatui::restore();

    // Save session on exit (only primary editor persists workspace state)
    shell.flush_to_db();
    if primary_lock.is_some() {
        // Save session rows first (DELETE + INSERT), then update cursor data
        if let Some(conn) = shell.db() {
            let snapshot = shell.capture_session();
            let buffer_paths: Vec<_> = shell
                .components()
                .iter()
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
        shell.save_all_sessions();
    }

    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    shell: &mut Shell,
    mut rx: mpsc::UnboundedReceiver<AppEvent>,
) -> io::Result<()> {
    let mut event_stream = EventStream::new();

    loop {
        terminal.draw(|frame| ui::render(shell, frame))?;

        let timeout = match (shell.needs_redraw_in(), shell.needs_persist_in()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        enum Received {
            CrosstermEvent(Event),
            AppEvent(AppEvent),
            Timeout,
        }

        let received = if let Some(timeout) = timeout {
            tokio::select! {
                biased;
                ev = event_stream.next() => {
                    match ev {
                        Some(Ok(event)) => Received::CrosstermEvent(event),
                        Some(Err(_)) => return Ok(()),
                        None => return Ok(()),
                    }
                }
                ev = rx.recv() => {
                    match ev {
                        Some(event) => Received::AppEvent(event),
                        None => return Ok(()),
                    }
                }
                _ = tokio::time::sleep(timeout) => {
                    Received::Timeout
                }
            }
        } else {
            tokio::select! {
                biased;
                ev = event_stream.next() => {
                    match ev {
                        Some(Ok(event)) => Received::CrosstermEvent(event),
                        Some(Err(_)) => return Ok(()),
                        None => return Ok(()),
                    }
                }
                ev = rx.recv() => {
                    match ev {
                        Some(event) => Received::AppEvent(event),
                        None => return Ok(()),
                    }
                }
            }
        };

        match received {
            Received::CrosstermEvent(Event::Key(key)) => {
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
                        unsafe {
                            libc::raise(libc::SIGTSTP);
                        }
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
            Received::CrosstermEvent(Event::Resize(_, _)) => {} // redraw on next iteration
            Received::CrosstermEvent(_) => {}
            Received::AppEvent(AppEvent::ConfigChanged(file)) => match file {
                ConfigFile::Keys => {
                    if let Some(km) = config::reload_keymap() {
                        shell.set_keymap(km);
                        shell.message = Some("Reloaded keys.toml.".into());
                    }
                }
                ConfigFile::Theme => {
                    shell.set_theme(theme::load_theme());
                    shell.message = Some("Reloaded theme.toml.".into());
                }
            },
            Received::AppEvent(AppEvent::Wakeup) => {
                shell.tick();
            }
            Received::Timeout => {}
        }

        if shell.needs_persist() {
            shell.flush_to_db();
        }
    }
}

// --- Session helpers ---

fn restore_session(shell: &mut Shell, session: SessionData) {
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

    // Trigger line-diff scan for the active buffer
    shell.notify_active_buffer();
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

/// Try to acquire the primary-editor lock for this workspace.
///
/// Returns `Some(File)` if we became primary (the caller must keep the File
/// alive for the whole process lifetime — dropping it releases the lock).
/// Returns `None` if another editor already holds the lock.
fn try_become_primary(root: &std::path::Path) -> Option<std::fs::File> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::io::AsRawFd;

    let lock_dir = dirs::home_dir()?
        .join(".config")
        .join("led")
        .join("primary");
    std::fs::create_dir_all(&lock_dir).ok()?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(lock_dir.join(&hash))
        .ok()?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 { Some(file) } else { None }
}

fn spawn_config_watcher(
    tx: mpsc::UnboundedSender<AppEvent>,
    keys_path: Option<&std::path::Path>,
    theme_path: Option<&std::path::Path>,
) -> Option<notify::RecommendedWatcher> {
    let config_dir = keys_path.or(theme_path)?.parent()?;
    let keys_name = keys_path
        .and_then(|p| p.file_name())
        .map(|n| n.to_os_string());
    let theme_name = theme_path
        .and_then(|p| p.file_name())
        .map(|n| n.to_os_string());

    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
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
        })
        .ok()?;

    watcher
        .watch(config_dir, RecursiveMode::NonRecursive)
        .ok()?;
    Some(watcher)
}
