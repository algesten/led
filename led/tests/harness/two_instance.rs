use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use led_core::rx::Stream;
use led_core::{Action, CanonPath, Startup, UserPath};
use led_state::AppState;
use tokio::sync::oneshot;

use super::TestDirs;

type QueryFn = Box<dyn FnOnce(&AppState) -> Box<dyn std::any::Any + Send> + Send>;

enum Cmd {
    Action(Action),
    Query {
        f: QueryFn,
        reply: std::sync::mpsc::Sender<Box<dyn std::any::Any + Send>>,
    },
    Stop,
}

pub struct Instance {
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Instance {
    /// Spawn an editor instance on its own thread + tokio runtime.
    pub fn start(startup: Startup) -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();

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
                        let (quit_tx, _quit_rx) = oneshot::channel::<()>();

                        let (state_stream, guards) = led::run(startup, actions_in.clone(), quit_tx);

                        // Capture every state update on this thread.
                        let last_state: Rc<std::cell::RefCell<Option<Rc<AppState>>>> =
                            Rc::new(std::cell::RefCell::new(None));
                        let capture = last_state.clone();
                        state_stream.on(move |opt: Option<&Rc<AppState>>| {
                            if let Some(s) = opt {
                                *capture.borrow_mut() = Some(s.clone());
                            }
                        });

                        actions_in.push(Action::Resize(80, 24));

                        // Command loop: read from test thread, push into FRP.
                        let stream = actions_in.clone();
                        let state_for_query = last_state.clone();
                        tokio::task::spawn_local(async move {
                            loop {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                                while let Ok(cmd) = cmd_rx.try_recv() {
                                    match cmd {
                                        Cmd::Action(a) => stream.push(a),
                                        Cmd::Query { f, reply } => {
                                            if let Some(ref s) = *state_for_query.borrow() {
                                                let result = f(s);
                                                reply.send(result).ok();
                                            }
                                        }
                                        Cmd::Stop => return,
                                    }
                                }
                            }
                        })
                        .await
                        .ok();

                        drop(guards);
                    })
                    .await;
            });

            rt.shutdown_timeout(Duration::from_millis(100));
        });

        Instance {
            cmd_tx,
            handle: Some(handle),
        }
    }

    pub fn push(&self, action: Action) {
        self.cmd_tx.send(Cmd::Action(action)).ok();
    }

    /// Execute a closure on the instance thread with access to the current AppState.
    /// Returns the closure's result (which must be Send).
    pub fn with_state<R: Send + 'static>(
        &self,
        f: impl FnOnce(&AppState) -> R + Send + 'static,
    ) -> R {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.cmd_tx
            .send(Cmd::Query {
                f: Box::new(move |s| Box::new(f(s)) as Box<dyn std::any::Any + Send>),
                reply: reply_tx,
            })
            .expect("send query");
        let result = reply_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("recv state query");
        *result.downcast::<R>().expect("downcast query result")
    }

    /// Block the test thread until `pred` returns true or `timeout` elapses.
    /// Panics on timeout with the given label.
    pub fn wait_for(
        &self,
        pred: impl Fn(&AppState) -> bool + Send + Clone + 'static,
        timeout: Duration,
        label: &str,
    ) {
        let start = Instant::now();
        loop {
            let result = self.with_state(pred.clone());
            if result {
                return;
            }
            if start.elapsed() > timeout {
                let state_desc: String = self.with_state(|s| {
                    let buf_info =
                        s.active_tab
                            .as_ref()
                            .and_then(|path| s.buffers.get(path))
                            .map(|b| {
                                format!(
                                "chain_id={:?} dirty={} save={:?} persisted={} seq={} change_seq={} lines={}",
                                b.chain_id(), b.is_dirty(), b.save_state(), b.persisted_undo_len(),
                                b.last_seen_seq(), b.change_seq().0, b.doc().line_count()
                            )
                            });
                    format!(
                        "buffers={} active_buf=[{}]",
                        s.buffers.len(),
                        buf_info.unwrap_or_default()
                    )
                });
                panic!(
                    "Instance::wait_for({label}) timed out after {:?}\n  state: {state_desc}",
                    timeout
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn stop(&mut self) {
        self.cmd_tx.send(Cmd::Stop).ok();
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.cmd_tx.send(Cmd::Stop).ok();
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

/// Create a shared workspace with the standard layout:
///   root/workspace/  — project files
///   root/config/     — DB, notify dir, etc.
/// Returns (dirs, file_paths).
pub fn shared_workspace(files: &[(&str, &str)]) -> (TestDirs, Vec<PathBuf>) {
    let root = tempfile::TempDir::new().expect("create tmpdir").keep();
    let workspace_dir = root.join("workspace");
    let config_dir = root.join("config");
    std::fs::create_dir_all(&workspace_dir).expect("create workspace dir");
    std::fs::create_dir_all(&config_dir).expect("create config dir");

    // Init logging once
    use std::sync::Once;
    static INIT_LOG: Once = Once::new();
    INIT_LOG.call_once(|| {
        if let Ok(path) = std::env::var("LED_LOG_FILE") {
            led::logging::init_file_logger(std::path::Path::new(&path));
        }
    });

    let paths: Vec<PathBuf> = files
        .iter()
        .map(|(name, content)| {
            let path = workspace_dir.join(name);
            std::fs::write(&path, content).expect("write test file");
            path
        })
        .collect();

    (
        TestDirs {
            root,
            workspace: workspace_dir,
            config: config_dir,
        },
        paths,
    )
}

pub fn startup_for(dirs: &TestDirs, file_paths: &[PathBuf]) -> Startup {
    let arg_paths: Vec<CanonPath> = file_paths
        .iter()
        .map(|p| UserPath::new(p).canonicalize())
        .collect();
    let start_dir = arg_paths
        .first()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| UserPath::new(&dirs.workspace).canonicalize());

    Startup {
        headless: true,
        enable_watchers: true,
        arg_paths,
        arg_dir: None,
        start_dir: Arc::new(start_dir.clone()),
        user_start_dir: UserPath::new(start_dir.as_path()),
        config_dir: UserPath::new(&dirs.config),
        test_lsp_server: None,
    }
}
