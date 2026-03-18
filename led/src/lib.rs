use std::sync::Arc;

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
}

pub struct RunGuards {
    pub input_guard: Option<led_terminal_in::InputGuard>,
    pub ui: Option<led_ui::Ui>,
    state: Stream<Arc<AppState>>,
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
/// `quit_tx` — signalled when `state.quit` becomes true.
pub fn run(
    startup: Startup,
    actions_in: Stream<Action>,
    quit_tx: oneshot::Sender<()>,
) -> (Stream<Arc<AppState>>, RunGuards) {
    let headless = startup.headless;

    let file_watcher = if startup.enable_watchers {
        FileWatcher::new()
    } else {
        FileWatcher::inert()
    };

    let init = AppState::new(startup);
    let seed = Arc::new(init.clone());

    // 1. Hoisted AppState
    let state: Stream<Arc<AppState>> = Stream::new();

    // 2. Derived
    let d = derived(state.clone());

    // 3. Drivers
    let (input_guard, ui, terminal_in) = if headless {
        (None, None, Stream::new())
    } else {
        let guard = led_terminal_in::setup_terminal();
        let ui = led_ui::driver(d.ui);
        let terminal_in = led_terminal_in::driver();
        (Some(guard), Some(ui), terminal_in)
    };

    let timers_in = led_timers::driver(d.timers_out);
    let fs_in = led_fs::driver(d.fs_out);
    let clipboard_in = if headless {
        led_clipboard::driver_headless(d.clipboard_out)
    } else {
        led_clipboard::driver(d.clipboard_out)
    };

    let drivers = Drivers {
        terminal_in,
        actions_in,
        workspace_in: led_workspace::driver(d.workspace_out, file_watcher.clone()),
        docstore_in: led_docstore::driver(d.docstore_out, file_watcher),
        config_keys_in: led_config_file::driver::<Keys>(d.config_file_out.clone()),
        config_theme_in: led_config_file::driver::<Theme>(d.config_file_out),
        timers_in,
        fs_in,
        clipboard_in,
    };

    // 4. Model
    let real_state = model(drivers, init);

    // 5. Hoist
    real_state.forward(&state);

    // 6. Seed — triggers derived → drivers → first events
    state.push(seed);

    // Signal quit — wait for session save to complete (primary only)
    let mut quit_tx = Some(quit_tx);
    state.on(move |opt: Option<&Arc<AppState>>| {
        if let Some(s) = opt {
            if s.quit {
                let needs_save = s.workspace.as_ref().is_some_and(|w| w.primary);
                if s.session_saved || !needs_save {
                    if let Some(tx) = quit_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
        }
    });

    let guards = RunGuards {
        input_guard,
        ui,
        state: state.clone(),
    };
    (state, guards)
}
