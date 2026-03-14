use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use led_core::rx::Stream;
use led_core::{Action, Startup};
use led_state::AppState;
use tempfile::TempDir;
use tokio::sync::oneshot;

pub struct TestHarness {
    tmpdir: TempDir,
    file_content: Option<String>,
    viewport: (u16, u16),
}

impl TestHarness {
    pub fn new() -> Self {
        TestHarness {
            tmpdir: TempDir::new().expect("create tmpdir"),
            file_content: None,
            viewport: (80, 24),
        }
    }

    pub fn with_file(mut self, content: &str) -> Self {
        self.file_content = Some(content.to_string());
        self
    }

    #[allow(dead_code)]
    pub fn with_viewport(mut self, w: u16, h: u16) -> Self {
        self.viewport = (w, h);
        self
    }

    pub fn run(self, actions: Vec<Action>) -> Arc<AppState> {
        let file_content = self.file_content;
        let tmpdir = self.tmpdir.keep();
        let config_dir = tmpdir.join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");

        let arg_path = file_content.map(|content| {
            let path = tmpdir.join("test_file.txt");
            std::fs::write(&path, &content).expect("write test file");
            path
        });

        let start_dir = arg_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| tmpdir.clone());

        let startup = Startup {
            arg_path,
            start_dir: Arc::new(start_dir),
            config_dir,
        };

        let viewport = self.viewport;
        let (done_tx, done_rx) = std::sync::mpsc::channel();

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

                        let state = led::run_headless(startup, actions_in.clone(), quit_tx);

                        let last_state: Rc<RefCell<Option<Arc<AppState>>>> =
                            Rc::new(RefCell::new(None));
                        let capture = last_state.clone();
                        state.on(move |s: &Arc<AppState>| {
                            *capture.borrow_mut() = Some(s.clone());
                        });

                        actions_in.push(Action::Resize(viewport.0, viewport.1));

                        let stream = actions_in.clone();
                        let done = Rc::new(Cell::new(false));
                        let done2 = done.clone();
                        tokio::task::spawn_local(async move {
                            for action in actions {
                                match action {
                                    Action::Wait(ms) => {
                                        std::thread::sleep(std::time::Duration::from_millis(ms));
                                        for _ in 0..20 {
                                            tokio::task::yield_now().await;
                                        }
                                    }
                                    other => stream.push(other),
                                }
                            }
                            done2.set(true);
                        });

                        while !done.get() {
                            tokio::task::yield_now().await;
                        }

                        let result = last_state.borrow().clone().expect("state was never set");
                        let _ = done_tx.send(result);
                    })
                    .await;
            });
        });

        let result = done_rx.recv().expect("test thread died");
        std::mem::forget(handle);
        result
    }
}
