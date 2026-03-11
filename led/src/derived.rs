use std::pin::Pin;
use std::sync::Arc;

use led_core::FanoutStreamExt;
use led_state::AppState;
use led_workspace::StartDir;
use tokio::sync::broadcast::Sender;
use tokio_stream::{Stream, StreamExt};

pub struct Derived {
    pub workspace: Pin<Box<dyn Stream<Item = StartDir> + Send>>,
}

impl Derived {
    pub fn new(state_tx: &Sender<Arc<AppState>>) -> Self {
        let workspace = state_tx
            .latest()
            .map(|s| s.startup.start_dir.clone())
            .map(StartDir);

        Derived {
            workspace: Box::pin(workspace),
        }
    }
}
