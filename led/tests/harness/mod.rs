use std::cell::Cell;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use led_core::rx::Stream;
use led_core::{Action, Startup};
use led_state::AppState;
use tempfile::TempDir;
use tokio::sync::oneshot;

pub enum TestStep {
    Do(Action),
    WaitFor(fn(&AppState) -> bool),
}

impl From<Action> for TestStep {
    fn from(a: Action) -> Self {
        TestStep::Do(a)
    }
}

pub struct TestResult {
    pub state: Arc<AppState>,
    pub file_path: Option<PathBuf>,
    pub tmpdir: PathBuf,
}

pub struct TestHarness {
    tmpdir: Option<TempDir>,
    reuse_dir: Option<PathBuf>,
    files: Vec<(String, String)>,
    viewport: (u16, u16),
}

impl TestHarness {
    pub fn new() -> Self {
        TestHarness {
            tmpdir: Some(TempDir::new().expect("create tmpdir")),
            reuse_dir: None,
            files: Vec::new(),
            viewport: (80, 24),
        }
    }

    /// Reuse an existing directory (for session restore tests).
    /// Files are created in this directory. Config dir is `{dir}/config`.
    #[allow(dead_code)]
    pub fn with_dir(dir: PathBuf) -> Self {
        TestHarness {
            tmpdir: None,
            reuse_dir: Some(dir),
            files: Vec::new(),
            viewport: (80, 24),
        }
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

    #[allow(dead_code)]
    pub fn with_viewport(mut self, w: u16, h: u16) -> Self {
        self.viewport = (w, h);
        self
    }

    pub fn run(self, steps: Vec<TestStep>) -> TestResult {
        let file_count = self.files.len();
        let files = self.files;
        let tmpdir = match (self.tmpdir, self.reuse_dir) {
            (Some(td), _) => td.keep(),
            (None, Some(d)) => d,
            _ => unreachable!(),
        };
        let config_dir = tmpdir.join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");

        let arg_paths: Vec<PathBuf> = files
            .into_iter()
            .map(|(name, content)| {
                let path = tmpdir.join(name);
                std::fs::write(&path, &content).expect("write test file");
                path
            })
            .collect();
        let file_path = arg_paths.first().cloned();

        let start_dir = arg_paths
            .first()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| tmpdir.clone());

        let startup = Startup {
            headless: true,
            arg_paths,
            start_dir: Arc::new(start_dir),
            config_dir,
        };

        let viewport = self.viewport;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let tmpdir2 = tmpdir.clone();

        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("create runtime");

            rt.block_on(async {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async {
                        let actions_in: Stream<Action> = Stream::new();
                        let (quit_tx, _) = oneshot::channel::<()>();

                        let (state, guards) = led::run(startup, actions_in.clone(), quit_tx);

                        let last_state: Rc<RefCell<Option<Arc<AppState>>>> =
                            Rc::new(RefCell::new(None));
                        let capture = last_state.clone();
                        state.on(move |opt: Option<&Arc<AppState>>| {
                            if let Some(s) = opt {
                                *capture.borrow_mut() = Some(s.clone());
                            }
                        });

                        actions_in.push(Action::Resize(viewport.0, viewport.1));

                        let stream = actions_in.clone();
                        let done = Rc::new(Cell::new(false));
                        let done2 = done.clone();
                        let last_for_wait = last_state.clone();
                        tokio::task::spawn_local(async move {
                            // Auto-wait for all files to open
                            if file_count > 0 {
                                loop {
                                    if let Some(ref s) = *last_for_wait.borrow() {
                                        if s.buffers.len() >= file_count {
                                            break;
                                        }
                                    }
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                }
                            }

                            for step in steps {
                                match step {
                                    TestStep::Do(action) => stream.push(action),
                                    TestStep::WaitFor(pred) => {
                                        wait_for_condition(&last_for_wait, pred).await;
                                    }
                                }
                            }

                            done2.set(true);
                        });

                        while !done.get() {
                            tokio::task::yield_now().await;
                        }

                        let result = last_state.borrow().clone().expect("state was never set");
                        let _ = done_tx.send(result);
                        drop(guards);
                    })
                    .await;
            });

            // Safety net: cancel any lingering tasks (e.g. filesystem watchers).
            rt.shutdown_timeout(Duration::from_millis(100));
        });

        let state = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("test timed out");
        handle.join().ok();
        TestResult {
            state,
            file_path,
            tmpdir: tmpdir2,
        }
    }
}

async fn wait_for_condition(
    state: &Rc<RefCell<Option<Arc<AppState>>>>,
    pred: fn(&AppState) -> bool,
) {
    loop {
        if let Some(ref s) = *state.borrow() {
            if pred(s) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
