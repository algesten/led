use tokio_stream::Stream;

pub use config::Startup;
pub use ext::{Combine, Flatten, Reduce, SampleCombine, StreamOpsExt};
pub use fanout::{FanoutStream, FanoutStreamExt, LatestStream};

mod config;
mod ext;
mod fanout;

pub trait AStream<T>: Stream<Item = T> + Unpin + Send + 'static {}
impl<S, T> AStream<T> for S where S: Stream<Item = T> + Unpin + Send + 'static {}
