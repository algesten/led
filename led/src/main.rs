use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use crossterm::event::DisableBracketedPaste;
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
use led_config_file::TomlFile;
use led_core::Startup;
use led_core::keys::{KeyCombo, Keys, format_key_combo, parse_key_combo};
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{CanonPath, UserPath};
use led_terminal_in::TerminalInput;
use tokio::sync::oneshot;

#[derive(Parser)]
#[command(name = "led", about = "A lightweight text editor")]
struct Cli {
    /// Files or directory to open
    paths: Vec<String>,

    /// Write logs to FILE
    #[arg(long, value_name = "FILE")]
    log_file: Option<PathBuf>,

    /// Reset keybinds/theme to defaults and clear session db
    #[arg(long)]
    reset_config: bool,

    // Intended for `EDITOR="led --no-workspace"` single-file editing.
    /// Standalone mode: no workspace, git, LSP, session or watchers
    #[arg(long)]
    no_workspace: bool,

    /// Replay a key-combo file into terminal input (for profiling)
    #[arg(long, value_name = "FILE")]
    keys_file: Option<PathBuf>,

    /// Record key presses to FILE (replay with --keys-file)
    #[arg(long, value_name = "FILE")]
    keys_record: Option<PathBuf>,
}

enum KeyScript {
    Key(KeyCombo),
    /// Jump to the first entry whose source line number is >= target.
    Goto(usize),
}

fn parse_keys_script(content: &str) -> Vec<(usize, KeyScript)> {
    let mut script = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("goto") {
            let rest = rest.trim();
            let target: usize = rest.parse().unwrap_or_else(|e| {
                panic!("keys file line {line_no}: parse goto target '{rest}': {e}")
            });
            script.push((line_no, KeyScript::Goto(target)));
            continue;
        }
        let combo = parse_key_combo(trimmed)
            .unwrap_or_else(|e| panic!("keys file line {line_no}: parse '{trimmed}': {e}"));
        script.push((line_no, KeyScript::Key(combo)));
    }
    script
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();

    if let Some(ref log_path) = cli.log_file {
        led::logging::init_file_logger(log_path);
    }

    let resolve_path = |p: &str| -> CanonPath {
        // Build a UserPath then canonicalize. For non-existent files,
        // canonicalize falls back to the original path.
        let path = std::path::PathBuf::from(p);
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        let canonical_parent = UserPath::new(parent).canonicalize();
        let joined = UserPath::new(
            canonical_parent
                .as_path()
                .join(path.file_name().unwrap_or_default()),
        );
        joined.canonicalize()
    };

    let resolved: Vec<CanonPath> = cli.paths.iter().map(|p| resolve_path(p)).collect();

    // Single directory: open in file browser, no files.
    // Otherwise: filter out directories, open remaining files.
    // Capture the user-provided start directory before canonicalization.
    //
    // Standalone mode (`--no-workspace`) always anchors on the process
    // CWD: the file arg is typically something like `.git/COMMIT_EDITMSG`
    // or a tempfile, and rooting the browser at that file's parent would
    // show a hidden/temp dir. CWD is where the user *was* when they ran
    // the command, which is almost always the useful "here".
    let user_start_dir = if cli.no_workspace {
        UserPath::new(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
    } else if cli.paths.len() == 1 {
        UserPath::new(&cli.paths[0])
    } else {
        UserPath::new(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
    };

    let (arg_dir, arg_paths, start_dir) = if cli.no_workspace {
        let files: Vec<CanonPath> = resolved.into_iter().filter(|p| !p.is_dir()).collect();
        let start = UserPath::new(
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
        )
        .canonicalize();
        (None, files, start)
    } else if resolved.len() == 1 && resolved[0].is_dir() {
        let dir = resolved.into_iter().next().unwrap();
        let start = dir.clone();
        (Some(dir), vec![], start)
    } else {
        let files: Vec<CanonPath> = resolved.into_iter().filter(|p| !p.is_dir()).collect();
        let start = files.first().and_then(|p| p.parent()).unwrap_or_else(|| {
            UserPath::new(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")))
                .canonicalize()
        });
        (None, files, start)
    };

    let config_dir = UserPath::new(
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config")
            .join("led"),
    );

    if cli.reset_config {
        std::fs::create_dir_all(config_dir.as_path()).ok();

        match std::fs::write(
            config_dir.as_path().join(Keys::file_name()),
            Keys::default_toml(),
        ) {
            Ok(()) => eprintln!("Config reset to defaults."),
            Err(e) => eprintln!("Failed to reset config: {e}"),
        }

        match std::fs::write(
            config_dir.as_path().join(Theme::file_name()),
            Theme::default_toml(),
        ) {
            Ok(()) => eprintln!("Theme reset to defaults."),
            Err(e) => eprintln!("Failed to reset theme: {e}"),
        }

        match std::fs::remove_file(config_dir.as_path().join("db.sqlite")) {
            Ok(()) => eprintln!("Session database reset."),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Session database reset.")
            }
            Err(e) => eprintln!("Failed to reset session database: {e}"),
        }

        return;
    }

    let startup = Startup {
        headless: false,
        enable_watchers: true,
        arg_paths,
        arg_dir,
        start_dir: Arc::new(start_dir),
        user_start_dir,
        config_dir,
        test_lsp_server: None,
        test_gh_binary: None,
        no_workspace: cli.no_workspace,
    };

    let keys_script: Option<Vec<(usize, KeyScript)>> = cli.keys_file.as_ref().map(|path| {
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read keys file {}: {e}", path.display()));
        parse_keys_script(&content)
    });

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, rx) = oneshot::channel();

            let terminal_in: Stream<TerminalInput> = Stream::new();
            let actions_in: Stream<led_core::Action> = Stream::new();
            let (_state, _guards) = led::run(startup, terminal_in.clone(), actions_in.clone(), tx);

            if let Some(path) = cli.keys_record.clone() {
                use std::io::Write;
                let file = std::fs::File::create(&path)
                    .unwrap_or_else(|e| panic!("create keys record file {}: {e}", path.display()));
                let file = std::cell::RefCell::new(file);
                terminal_in.on(move |opt: Option<&TerminalInput>| {
                    let Some(TerminalInput::Key(combo)) = opt else {
                        return;
                    };
                    let Some(line) = format_key_combo(combo) else {
                        return;
                    };
                    let mut f = file.borrow_mut();
                    let _ = writeln!(f, "{line}");
                    let _ = f.flush();
                });
            }

            if let Some(script) = keys_script {
                let stream = terminal_in.clone();
                tokio::task::spawn_local(async move {
                    // Give startup (session restore, LSP warm-up) a moment to settle.
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let mut i = 0;
                    while i < script.len() {
                        match script[i].1 {
                            KeyScript::Key(combo) => {
                                stream.push(TerminalInput::Key(combo));
                                tokio::task::yield_now().await;
                                i += 1;
                            }
                            KeyScript::Goto(target) => {
                                i = script
                                    .iter()
                                    .position(|(line_no, _)| *line_no >= target)
                                    .unwrap_or(script.len());
                            }
                        }
                    }
                });
            }

            let _ = rx.await;
        })
        .await;

    // Restore terminal state on exit.
    disable_raw_mode().ok();
    crossterm::execute!(
        io::stdout(),
        crossterm::cursor::Show,
        LeaveAlternateScreen,
        DisableBracketedPaste
    )
    .ok();

    // Force exit. Everything that had to be persisted (session, undo) was
    // already flushed before `rx.await` returned. Don't wait politely for
    // background `spawn_blocking` work (git scans, `gh` CLI, LSP shutdown
    // handshakes) or the native file-watcher thread — otherwise the tokio
    // current-thread runtime drop stalls until those finish, which is
    // especially visible when quitting mid-startup.
    std::process::exit(0);
}
