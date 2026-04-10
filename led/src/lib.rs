use std::rc::Rc;

use led_config_file::ConfigFile;
use led_core::keys::Keys;
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{Action, Alert, FileWatcher, Startup};
use led_fs::FsIn;
use led_state::AppState;
use led_terminal_in::TerminalInput;
use led_timers::TimersIn;
use led_workspace::WorkspaceIn;
use tokio::sync::oneshot;

pub mod derived;
pub mod logging;
pub mod model;

use derived::derived;
use model::model;

pub struct Drivers {
    pub terminal_in: Stream<TerminalInput>,
    pub actions_in: Stream<Action>,
    pub workspace_in: Stream<WorkspaceIn>,
    pub docstore_in: Stream<Result<led_docstore::DocStoreIn, Alert>>,
    pub config_keys_in: Stream<Result<ConfigFile<Keys>, Alert>>,
    pub config_theme_in: Stream<Result<ConfigFile<Theme>, Alert>>,
    pub timers_in: Stream<TimersIn>,
    pub fs_in: Stream<FsIn>,
    pub clipboard_in: Stream<led_clipboard::ClipboardIn>,
    pub syntax_in: Stream<led_syntax::SyntaxIn>,
    pub git_in: Stream<led_git::GitIn>,
    pub gh_pr_in: Stream<led_gh_pr::GhPrIn>,
    pub file_search_in: Stream<led_file_search::FileSearchIn>,
    pub lsp_in: Stream<led_lsp::LspIn>,
    pub ui_in: Stream<led_ui::UiIn>,
}

pub struct RunGuards {
    pub input_guard: Option<led_terminal_in::InputGuard>,
    state: Stream<Rc<AppState>>,
}

impl Drop for RunGuards {
    fn drop(&mut self) {
        self.state.close();
    }
}

/// Set up and run the editor.
///
/// When `startup.headless` is true, skips terminal setup and UI driver.
/// `actions_in` — direct action injection stream (empty in production).
/// `quit_tx` — signalled when `state.phase` becomes `Exiting`.
pub fn run(
    startup: Startup,
    actions_in: Stream<Action>,
    quit_tx: oneshot::Sender<()>,
) -> (Stream<Rc<AppState>>, RunGuards) {
    let headless = startup.headless;
    let lsp_server_override = startup
        .test_lsp_server
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let gh_binary_override = startup
        .test_gh_binary
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());

    let file_watcher = if startup.enable_watchers {
        FileWatcher::new()
    } else {
        FileWatcher::inert()
    };

    let init = AppState::new(startup);
    let seed = Rc::new(init.clone());

    // 1. Hoisted AppState
    let state: Stream<Rc<AppState>> = Stream::new();

    // git_activity bypasses AppState — wired to workspace GitChanged below
    let git_activity: Stream<()> = Stream::new();

    // 2. Derived
    let d = derived(state.clone(), git_activity.clone());

    // 3. Drivers
    let (input_guard, terminal_in, ui_in) = if headless {
        (None, Stream::new(), Stream::new())
    } else {
        let guard = led_terminal_in::setup_terminal();
        let ui_in = led_ui::driver(d.ui);
        let terminal_in = led_terminal_in::driver();
        (Some(guard), terminal_in, ui_in)
    };

    let timers_in = led_timers::driver(d.timers_out);
    let fs_in = led_fs::driver(d.fs_out);
    let clipboard_in = if headless {
        led_clipboard::driver_headless(d.clipboard_out)
    } else {
        led_clipboard::driver(d.clipboard_out)
    };

    let syntax_in = led_syntax::driver(d.syntax_out);
    let git_in = led_git::driver(d.git_out);
    let gh_pr_in = led_gh_pr::driver(d.gh_pr_out, gh_binary_override);
    let file_search_in = led_file_search::driver(d.file_search_out);
    let lsp_in = led_lsp::driver(d.lsp_out, lsp_server_override);

    // Fork workspace GitChanged events to git_activity (bypasses AppState)
    let workspace_in = led_workspace::driver(d.workspace_out, file_watcher.clone());
    let ga = git_activity;
    workspace_in.on(move |ev: Option<&WorkspaceIn>| {
        if matches!(ev, Some(WorkspaceIn::GitChanged)) {
            ga.push(());
        }
    });

    // Open URL consumer (fire-and-forget)
    d.open_url.on(move |opt: Option<&String>| {
        if let Some(url) = opt {
            let url = url.clone();
            std::thread::spawn(move || {
                let _ = open::that(&url);
            });
        }
    });

    let drivers = Drivers {
        terminal_in,
        actions_in,
        workspace_in,
        docstore_in: led_docstore::driver(d.docstore_out, file_watcher),
        config_keys_in: led_config_file::driver::<Keys>(d.config_file_out.clone()),
        config_theme_in: led_config_file::driver::<Theme>(d.config_file_out),
        timers_in,
        fs_in,
        clipboard_in,
        syntax_in,
        git_in,
        gh_pr_in,
        file_search_in,
        lsp_in,
        ui_in,
    };

    // 4. Model
    let real_state = model(drivers, init);

    // 5. Hoist
    real_state.forward(&state);

    // 6. Seed — triggers derived → drivers → first events
    state.push(seed);

    // Signal quit — wait for session save to complete (primary only)
    let mut quit_tx = Some(quit_tx);
    state.on(move |opt: Option<&Rc<AppState>>| {
        if let Some(s) = opt {
            if s.phase == led_state::Phase::Exiting {
                let needs_save = s.workspace.as_ref().is_some_and(|w| w.primary);
                if s.session.saved || !needs_save {
                    if let Some(tx) = quit_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
        }
    });

    let guards = RunGuards {
        input_guard,
        state: state.clone(),
    };
    (state, guards)
}
