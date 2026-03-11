use std::path::PathBuf;
use std::sync::Arc;
use tokio_stream::Stream;

pub use config::Config;
pub use ext::{Combine, Flatten, Reduce, SampleCombine, StreamOpsExt};
pub use fanout::{FanoutStream, FanoutStreamExt, LatestStream};

mod config;
mod ext;
mod fanout;

pub trait AStream<T>: Stream<Item = T> + Unpin + Send + 'static {}
impl<S, T> AStream<T> for S where S: Stream<Item = T> + Unpin + Send + 'static {}

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
