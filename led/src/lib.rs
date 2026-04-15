use std::rc::Rc;

use led_config_file::ConfigFile;
use led_core::keys::Keys;
use led_core::rx::Stream;
use led_core::theme::Theme;
use led_core::{Action, Alert, FileWatcher, Startup};
use led_fs::{FsIn, FsOut};
use led_state::AppState;
use led_terminal_in::TerminalInput;
use led_timers::TimersIn;
use led_workspace::WorkspaceIn;
use tokio::sync::oneshot;

pub mod derived;
pub mod golden_trace;
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
/// `terminal_in` — terminal input injection stream. The crossterm driver
/// (when not headless) forwards into this stream; callers can also push
/// synthetic `TerminalInput::Key` events for replay/profiling.
/// `actions_in` — direct action injection stream (empty in production).
/// `quit_tx` — signalled when `state.phase` becomes `Exiting`.
pub fn run(
    startup: Startup,
    terminal_in: Stream<TerminalInput>,
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
    let golden_trace_active = startup.golden_trace.is_some();
    if let Some(path) = startup.golden_trace.as_ref() {
        golden_trace::GoldenTraceSink::install(path).expect("create golden-trace file");
    }

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

    // Golden trace: subscribe ONLY to dispatches that cause externally-
    // observable work (disk I/O, subprocess spawns, OS clipboard, browser
    // launch, SQLite writes). Internal coordination signals (Workspace
    // Init, SyntaxBufferChanged, ConfigDir, TimerSet, LspOut driver-
    // commands) are deliberately NOT traced — those are implementation
    // details of the current arch and will not exist in the rewrite. The
    // LSP protocol traffic, which IS externally observable, is hooked
    // separately at the JSON-RPC transport layer in `crates/lsp/`.
    //
    // Off in production: subscribers are no-ops when no sink installed.
    if golden_trace_active {
        d.docstore_out
            .on(|opt: Option<&led_docstore::DocStoreOut>| {
                let Some(out) = opt else { return };
                use led_core::golden_trace::emit;
                match out {
                    led_docstore::DocStoreOut::Open {
                        path,
                        create_if_missing,
                    } => emit(
                        "FileOpen",
                        &format!(
                            "path={} create_if_missing={create_if_missing}",
                            path.display()
                        ),
                    ),
                    led_docstore::DocStoreOut::Save { path, .. } => {
                        emit("FileSave", &format!("path={}", path.display()))
                    }
                    led_docstore::DocStoreOut::SaveAs { path, new_path, .. } => emit(
                        "FileSaveAs",
                        &format!("path={} new_path={}", path.display(), new_path.display()),
                    ),
                }
            });

        d.fs_out.on(|opt: Option<&FsOut>| {
            let Some(out) = opt else { return };
            use led_core::golden_trace::emit;
            match out {
                FsOut::ListDir { path } => emit("FsListDir", &format!("path={}", path.display())),
                FsOut::FindFileList {
                    dir,
                    prefix,
                    show_hidden,
                } => emit(
                    "FsFindFile",
                    &format!(
                        "dir={} prefix={prefix:?} show_hidden={show_hidden}",
                        dir.display()
                    ),
                ),
            }
        });

        d.git_out.on(|opt: Option<&led_git::GitOut>| {
            let Some(out) = opt else { return };
            match out {
                led_git::GitOut::ScanFiles { root } => led_core::golden_trace::emit(
                    "GitScan",
                    &format!("root={}", root.display()),
                ),
            }
        });

        d.workspace_out
            .on(|opt: Option<&led_workspace::WorkspaceOut>| {
                let Some(out) = opt else { return };
                use led_core::golden_trace::emit;
                use led_workspace::WorkspaceOut::*;
                match out {
                    // Init is internal coordination (driver wiring), not
                    // an observable side effect; the actual SQLite open
                    // happens inside the driver but isn't reached without
                    // a real workspace anyway. Skipped intentionally.
                    Init { .. } => {}
                    SaveSession { .. } => emit("WorkspaceSaveSession", ""),
                    FlushUndo {
                        file_path,
                        chain_id,
                        ..
                    } => emit(
                        "WorkspaceFlushUndo",
                        &format!("path={} chain={chain_id}", file_path.display()),
                    ),
                    ClearUndo { file_path } => emit(
                        "WorkspaceClearUndo",
                        &format!("path={}", file_path.display()),
                    ),
                    CheckSync { file_path, .. } => emit(
                        "WorkspaceCheckSync",
                        &format!("path={}", file_path.display()),
                    ),
                }
            });

        d.clipboard_out
            .on(|opt: Option<&led_clipboard::ClipboardOut>| {
                let Some(out) = opt else { return };
                use led_core::golden_trace::emit;
                match out {
                    led_clipboard::ClipboardOut::Read => emit("ClipboardRead", ""),
                    led_clipboard::ClipboardOut::Write(s) => {
                        let preview: String = s.chars().take(40).collect();
                        emit(
                            "ClipboardWrite",
                            &format!("len={} preview={preview:?}", s.len()),
                        )
                    }
                }
            });

        d.gh_pr_out.on(|opt: Option<&led_gh_pr::GhPrOut>| {
            let Some(out) = opt else { return };
            use led_core::golden_trace::emit;
            match out {
                led_gh_pr::GhPrOut::LoadPr { branch, root } => emit(
                    "GhLoadPr",
                    &format!("branch={branch} root={}", root.display()),
                ),
                led_gh_pr::GhPrOut::PollPr {
                    api_endpoint, root, ..
                } => emit(
                    "GhPollPr",
                    &format!("endpoint={api_endpoint} root={}", root.display()),
                ),
            }
        });

        d.file_search_out
            .on(|opt: Option<&led_file_search::FileSearchOut>| {
                let Some(out) = opt else { return };
                use led_core::golden_trace::emit;
                use led_file_search::FileSearchOut::*;
                match out {
                    Search {
                        query,
                        root,
                        case_sensitive,
                        use_regex,
                    } => emit(
                        "FileSearch",
                        &format!(
                            "query={query:?} root={} case={case_sensitive} regex={use_regex}",
                            root.display()
                        ),
                    ),
                    Replace {
                        query,
                        replacement,
                        root,
                        ..
                    } => emit(
                        "FileReplace",
                        &format!(
                            "query={query:?} replacement={replacement:?} root={}",
                            root.display()
                        ),
                    ),
                }
            });

        d.open_url.on(|opt: Option<&String>| {
            if let Some(url) = opt {
                led_core::golden_trace::emit("OpenUrl", &format!("url={url:?}"));
            }
        });
    }

    // 3. Drivers
    let (input_guard, ui_in) = if headless {
        (None, Stream::new())
    } else {
        let guard = led_terminal_in::setup_terminal();
        let ui_in = led_ui::driver(d.ui);
        led_terminal_in::driver().forward(&terminal_in);
        (Some(guard), ui_in)
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
                let needs_save = s.workspace.loaded().is_some_and(|w| w.primary);
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
