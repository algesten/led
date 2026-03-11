use std::path::PathBuf;
use std::sync::Arc;

pub use config::Config;
pub use fanout::{FanoutStream, FanoutStreamExt, LatestStream};

mod config;
mod fanout;

#[derive(Clone, Default)]
pub struct State {
    pub config: Arc<Config>,
    pub workspace: Option<PathBuf>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            ..Default::default()
        }
    }
}
