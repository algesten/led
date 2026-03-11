use std::pin::Pin;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::broadcast::{Receiver, Sender};
use tokio_stream::Stream;
use tokio_util::sync::ReusableBoxFuture;

use std::task::{Context, Poll, ready};

/// Lossless stream over a broadcast channel. Panics if the receiver falls behind.
pub struct FanoutStream<T> {
    inner: ReusableBoxFuture<'static, (Result<T, RecvError>, Receiver<T>)>,
}

async fn make_future<T: Clone>(mut rx: Receiver<T>) -> (Result<T, RecvError>, Receiver<T>) {
    let result = rx.recv().await;
    (result, rx)
}

impl<T: 'static + Clone + Send> FanoutStream<T> {
    pub fn new(rx: Receiver<T>) -> Self {
        Self {
            inner: ReusableBoxFuture::new(make_future(rx)),
        }
    }
}

impl<T: 'static + Clone + Send> Stream for FanoutStream<T> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (result, rx) = ready!(self.inner.poll(cx));
        self.inner.set(make_future(rx));
        match result {
            Ok(item) => Poll::Ready(Some(item)),
            Err(RecvError::Closed) => Poll::Ready(None),
            Err(RecvError::Lagged(n)) => {
                panic!("FanoutStream lagged behind by {n} messages");
            }
        }
    }
}

/// Latest-wins stream over a broadcast channel. Drains all pending values and
/// yields only the most recent one. Safe to fall behind.
pub struct LatestStream<T> {
    inner: ReusableBoxFuture<'static, (Result<T, RecvError>, Receiver<T>)>,
}

impl<T: 'static + Clone + Send> LatestStream<T> {
    pub fn new(rx: Receiver<T>) -> Self {
        Self {
            inner: ReusableBoxFuture::new(make_future(rx)),
        }
    }
}

impl<T: 'static + Clone + Send> Stream for LatestStream<T> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let (result, mut rx) = ready!(self.inner.poll(cx));
        match result {
            Ok(mut item) => {
                // Drain all pending values, keep only the last.
                loop {
                    match rx.try_recv() {
                        Ok(newer) => item = newer,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Closed) => break,
                        Err(TryRecvError::Lagged(_)) => continue,
                    }
                }
                self.inner.set(make_future(rx));
                Poll::Ready(Some(item))
            }
            Err(RecvError::Closed) => Poll::Ready(None),
            Err(RecvError::Lagged(_)) => {
                // We lagged — drain to latest and wait for next.
                self.inner.set(make_future(rx));
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

pub trait FanoutStreamExt<T> {
    /// Lossless stream. Every value is delivered. Panics if the receiver falls behind.
    fn fanout(&self) -> FanoutStream<T>;
    /// Latest-wins stream. Drains pending values, yields only the most recent.
    fn latest(&self) -> LatestStream<T>;
}

impl<T: 'static + Clone + Send> FanoutStreamExt<T> for Sender<T> {
    fn fanout(&self) -> FanoutStream<T> {
        FanoutStream::new(self.subscribe())
    }

    fn latest(&self) -> LatestStream<T> {
        LatestStream::new(self.subscribe())
    }
}
