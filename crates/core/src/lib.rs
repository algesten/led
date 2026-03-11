use tokio_stream::Stream;

mod alert;
mod config;
mod ext;
mod fanout;
pub mod keys;
pub mod theme;
mod watch;

pub use alert::{Alert, AlertExt};
pub use config::Startup;
pub use ext::StreamOpsExt;
pub use ext::{Combine, Dedupe, Flatten, Merge, Reduce, SampleCombine};
pub use fanout::{FanoutStream, FanoutStreamExt, LatestStream};
pub use watch::watch;

pub trait AStream<T>: Stream<Item = T> + Send + 'static {}
impl<S, T> AStream<T> for S where S: Stream<Item = T> + Send + 'static {}
