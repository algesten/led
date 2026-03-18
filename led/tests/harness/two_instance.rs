use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use led_core::rx::Stream;
use led_core::{Action, Startup};
use led_state::AppState;
use tokio::sync::oneshot;

use super::TestDirs;

enum Cmd {
    Action(Action),
    Stop,
}

pub struct Instance {
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
    state: Arc<Mutex<Option<Arc<AppState>>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Instance {
    /// Spawn an editor instance on its own thread + tokio runtime.
    pub fn start(startup: Startup) -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
        let state: Arc<Mutex<Option<Arc<AppState>>>> = Arc::new(Mutex::new(None));
        let state2 = state.clone();

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

                        // Capture every state update for the test thread to read.
                        let capture = state2.clone();
                        state_stream.on(move |opt: Option<&Arc<AppState>>| {
                            if let Some(s) = opt {
                                *capture.lock().unwrap() = Some(s.clone());
                            }
                        });

                        actions_in.push(Action::Resize(80, 24));

                        // Command loop: read from test thread, push into FRP.
                        let stream = actions_in.clone();
                        tokio::task::spawn_local(async move {
                            loop {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                                while let Ok(cmd) = cmd_rx.try_recv() {
                                    match cmd {
                                        Cmd::Action(a) => stream.push(a),
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
            state,
            handle: Some(handle),
        }
    }

    pub fn push(&self, action: Action) {
        self.cmd_tx.send(Cmd::Action(action)).ok();
    }

    pub fn state(&self) -> Option<Arc<AppState>> {
        self.state.lock().unwrap().clone()
    }

    /// Block the test thread until `pred` returns true or `timeout` elapses.
    /// Panics on timeout with the given label.
    pub fn wait_for(&self, pred: impl Fn(&AppState) -> bool, timeout: Duration, label: &str) {
        let start = Instant::now();
        loop {
            if let Some(ref s) = *self.state.lock().unwrap() {
                if pred(s) {
                    return;
                }
            }
            if start.elapsed() > timeout {
                let state_desc = match self.state.lock().unwrap().as_ref() {
                    Some(s) => {
                        let buf_info = s.active_buffer.and_then(|id| s.buffers.get(&id)).map(|b| {
                            format!(
                                "chain_id={:?} dirty={} save={:?} persisted={} seq={} change_seq={} lines={}",
                                b.chain_id, b.doc.dirty(), b.save_state, b.persisted_undo_len,
                                b.last_seen_seq, b.change_seq, b.doc.line_count()
                            )
                        });
                        format!(
                            "buffers={} active_buf=[{}]",
                            s.buffers.len(),
                            buf_info.unwrap_or_default()
                        )
                    }
                    None => "no state".to_string(),
                };
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
    let start_dir = file_paths
        .first()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dirs.workspace.clone());

    Startup {
        headless: true,
        enable_watchers: true,
        arg_paths: file_paths.to_vec(),
        start_dir: Arc::new(start_dir),
        config_dir: dirs.config.clone(),
    }
}
