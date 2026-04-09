pub mod two_instance;

use std::cell::Cell;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use led_core::rx::Stream;
use led_core::{Action, CanonPath, Startup, UserPath};
use led_state::AppState;
use tempfile::TempDir;
use tokio::sync::oneshot;

/// A Send-safe wrapper that runs RunFn closures on the tokio thread.
/// The closure is extracted before the async block, so only the result
/// (nothing) crosses the boundary.

/// Paths available to RunFn callbacks and TestResult.
#[derive(Clone)]
pub struct TestDirs {
    /// Root tmpdir (pass to `TestHarness::with_dir` for session restore).
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub config: PathBuf,
}

pub enum TestStep {
    Do(Action),
    WaitFor(fn(&AppState) -> bool),
    /// Dispatch Quit and wait for the real quit signal (like the app does).
    /// This tests that session save completes before the app would exit.
    QuitAndWait,
    /// Run arbitrary code during the test.
    RunFn(Box<dyn FnOnce(&TestDirs)>),
}

impl From<Action> for TestStep {
    fn from(a: Action) -> Self {
        TestStep::Do(a)
    }
}

pub struct TestResult {
    pub state: Rc<AppState>,
    pub file_path: Option<PathBuf>,
    pub dirs: TestDirs,
}

pub struct TestHarness {
    tmpdir: Option<TempDir>,
    reuse_dir: Option<PathBuf>,
    files: Vec<(String, String)>,
    arg_paths: Vec<PathBuf>,
    arg_dir: Option<PathBuf>,
    viewport: (u16, u16),
    enable_watchers: bool,
    test_lsp_server: Option<PathBuf>,
}

impl TestHarness {
    pub fn new() -> Self {
        TestHarness {
            tmpdir: Some(TempDir::new().expect("create tmpdir")),
            reuse_dir: None,
            files: Vec::new(),
            arg_paths: Vec::new(),
            arg_dir: None,
            viewport: (80, 24),
            enable_watchers: false,
            test_lsp_server: None,
        }
    }

    /// Reuse an existing root directory (for session restore tests).
    /// Expects the `workspace/` and `config/` subdirs to already exist.
    #[allow(dead_code)]
    pub fn with_dir(dir: PathBuf) -> Self {
        TestHarness {
            tmpdir: None,
            reuse_dir: Some(dir),
            files: Vec::new(),
            arg_paths: Vec::new(),
            arg_dir: None,
            viewport: (80, 24),
            enable_watchers: false,
            test_lsp_server: None,
        }
    }

    /// Set a fake LSP server binary for this test.
    #[allow(dead_code)]
    pub fn with_lsp_server(mut self, path: PathBuf) -> Self {
        self.test_lsp_server = Some(path);
        self
    }

    /// Add an existing file (already on disk) as an arg path for the second run.
    #[allow(dead_code)]
    pub fn with_arg(mut self, path: PathBuf) -> Self {
        self.arg_paths.push(path);
        self
    }

    /// Set a directory to reveal in the file browser on startup.
    #[allow(dead_code)]
    pub fn with_arg_dir(mut self, dir: PathBuf) -> Self {
        self.arg_dir = Some(dir);
        self
    }

    pub fn with_file(mut self, content: &str) -> Self {
        let name = format!("test_file_{}.txt", self.files.len());
        self.files.push((name, content.to_string()));
        self
    }

    #[allow(dead_code)]
    pub fn with_named_file(mut self, name: &str, content: &str) -> Self {
        self.files.push((name.to_string(), content.to_string()));
        self
    }

    /// Create a test file with a specific extension (e.g. "rs" for Rust syntax).
    pub fn with_file_ext(mut self, content: &str, ext: &str) -> Self {
        let name = format!("test_file_{}.{}", self.files.len(), ext);
        self.files.push((name, content.to_string()));
        self
    }

    /// Enable file system watchers for this test (docstore + workspace).
    /// Only needed for tests that depend on external-change detection or
    /// cross-instance sync.
    #[allow(dead_code)]
    pub fn with_watchers(mut self) -> Self {
        self.enable_watchers = true;
        self
    }

    #[allow(dead_code)]
    pub fn with_viewport(mut self, w: u16, h: u16) -> Self {
        self.viewport = (w, h);
        self
    }

    pub fn run(self, steps: Vec<TestStep>) -> TestResult {
        let file_count = self.files.len() + self.arg_paths.len();
        let files = self.files;
        let extra_args = self.arg_paths;
        let root = match (self.tmpdir, self.reuse_dir) {
            (Some(td), _) => td.keep(),
            (None, Some(d)) => d,
            _ => unreachable!(),
        };
        // Initialize logging once (via RUST_LOG env or default off).
        // Tests use --log-file or RUST_LOG=trace to enable.
        use std::sync::Once;
        static INIT_LOG: Once = Once::new();
        INIT_LOG.call_once(|| {
            if let Ok(path) = std::env::var(" diagnostics requested") {
                led::logging::init_file_logger(std::path::Path::new(&path));
            }
        });

        // Layout: root/{workspace,config} — separate trees like production.
        let workspace_dir = root.join("workspace");
        let config_dir = root.join("config");
        std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");
        std::fs::create_dir_all(&config_dir).expect("create config dir");

        let mut arg_paths_raw: Vec<PathBuf> = files
            .into_iter()
            .map(|(name, content)| {
                let path = workspace_dir.join(name);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).expect("create parent dir");
                }
                std::fs::write(&path, &content).expect("write test file");
                path
            })
            .collect();
        arg_paths_raw.extend(extra_args);
        let file_path = arg_paths_raw.first().cloned();

        let arg_paths: Vec<CanonPath> = arg_paths_raw
            .iter()
            .map(|p| UserPath::new(p).canonicalize())
            .collect();

        let arg_dir = self.arg_dir.map(|d| UserPath::new(d).canonicalize());
        let start_dir = if let Some(ref dir) = arg_dir {
            dir.clone()
        } else {
            arg_paths
                .first()
                .and_then(|p| p.parent())
                .unwrap_or_else(|| UserPath::new(&workspace_dir).canonicalize())
        };

        let startup = Startup {
            headless: true,
            enable_watchers: self.enable_watchers,
            arg_paths,
            arg_dir,
            start_dir: Arc::new(start_dir.clone()),
            user_start_dir: UserPath::new(start_dir.as_path()),
            config_dir: UserPath::new(config_dir.clone()),
            test_lsp_server: self.test_lsp_server.map(UserPath::new),
        };

        let dirs = TestDirs {
            root: root.clone(),
            workspace: workspace_dir,
            config: config_dir,
        };

        let viewport = self.viewport;
        let result_dirs = dirs.clone();

        // Run everything on the current thread — AppState contains Rc and is !Send.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("create runtime");

        let state = rt.block_on(async {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async {
                    let foobars_in: Stream<Action> = Stream::new();
                    let (quit_tx, quit_rx) = oneshot::channel::<()>();

                    let (state, guards) = led::run(startup, foobars_in.clone(), quit_tx);

                    let last_state: Rc<RefCell<Option<Rc<AppState>>>> = Rc::new(RefCell::new(None));
                    let capture = last_state.clone();
                    state.on(move |opt: Option<&Rc<AppState>>| {
                        if let Some(s) = opt {
                            *capture.borrow_mut() = Some(s.clone());
                        }
                    });

                    foobars_in.push(Action::Resize(viewport.0, viewport.1));

                    let stream = foobars_in.clone();
                    let done = Rc::new(Cell::new(false));
                    let done2 = done.clone();
                    let quit_rx = Rc::new(RefCell::new(Some(quit_rx)));
                    let quit_rx2 = quit_rx.clone();
                    let last_for_wait = last_state.clone();
                    tokio::task::spawn_local(async move {
                        // Wait for session restore to complete, then for files to open
                        let init_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                        loop {
                            if let Some(ref s) = *last_for_wait.borrow() {
                                let phase_done = s.phase == led_state::Phase::Running;
                                let files_ready =
                                    s.buffers.values().filter(|b| b.is_materialized()).count()
                                        >= file_count;
                                if phase_done && files_ready {
                                    break;
                                }
                            }
                            if tokio::time::Instant::now() > init_deadline {
                                let test = std::thread::current().name().unwrap_or("?").to_string();
                                eprintln!("Init wait timed out after 30s in {test} — aborting");
                                let _ = std::fs::write("/tmp/led-test-timeout.txt", format!("{test} (init)"));
                                std::process::exit(10);
                            }
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }

                        for step in steps {
                            match step {
                                TestStep::Do(action) => stream.push(action),
                                TestStep::WaitFor(pred) => {
                                    wait_for_condition(&last_for_wait, pred).await;
                                }
                                TestStep::QuitAndWait => {
                                    stream.push(Action::Quit);
                                    // Wait for the real quit signal — same as main.rs
                                    if let Some(rx) = quit_rx2.borrow_mut().take() {
                                        let _ = rx.await;
                                    }
                                }
                                TestStep::RunFn(f) => {
                                    f(&dirs);
                                }
                            }
                        }

                        done2.set(true);
                    });

                    while !done.get() {
                        tokio::task::yield_now().await;
                    }

                    let result = last_state.borrow().clone().expect("state was never set");
                    drop(guards);
                    result
                })
                .await
        });

        // Safety net: cancel any lingering tasks (e.g. filesystem watchers).
        // Allow enough time for the workspace driver to release the primary lock.
        rt.shutdown_timeout(Duration::from_secs(2));

        TestResult {
            state,
            file_path,
            dirs: result_dirs,
        }
    }
}

async fn wait_for_condition(
    state: &Rc<RefCell<Option<Rc<AppState>>>>,
    pred: fn(&AppState) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(ref s) = *state.borrow() {
            if pred(s) {
                return;
            }
        }
        if tokio::time::Instant::now() > deadline {
            let test = std::thread::current().name().unwrap_or("?").to_string();
            let state_info = if let Some(ref s) = *state.borrow() {
                let buf_paths: Vec<_> = s.buffers.keys().map(|p| p.display().to_string()).collect();
                let tab_paths: Vec<_> = s.tabs.iter().map(|t| t.path().display().to_string()).collect();
                let resume: Vec<_> = s.session.resume.iter().map(|e| format!("{:?}={:?}", e.path.file_name(), e.state)).collect();
                format!(
                    "phase={:?} bufs={} materialized={}\n  buf_paths={:?}\n  tab_paths={:?}\n  resume={:?}\n  primary={:?}",
                    s.phase,
                    s.buffers.len(),
                    s.buffers.values().filter(|b| b.is_materialized()).count(),
                    buf_paths,
                    tab_paths,
                    resume,
                    s.workspace.as_ref().map(|w| w.primary),
                )
            } else {
                "no state".to_string()
            };
            let _ = std::fs::write(
                "/tmp/led-test-timeout.txt",
                format!("{test}\n{state_info}"),
            );
            eprintln!("WaitFor timed out after 30s in {test} — {state_info}");
            std::process::exit(10);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
