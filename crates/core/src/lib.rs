mod config;
use std::sync::Arc;

pub use config::Config;

#[derive(Clone)]
pub struct State {
    pub config: Arc<Config>,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}
