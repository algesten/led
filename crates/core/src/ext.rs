use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio::sync::broadcast;
use tokio_stream::{Stream, StreamExt};

use crate::{AStream, FanoutStreamExt};

// === Reduce ===

pin_project! {
    /// Emits the running accumulation on each input event.
    /// Equivalent to xstream's `fold` / RxJS's `scan`.
    pub struct Reduce<S, B, F> {
        #[pin]
        stream: S,
        acc: B,
        f: F,
    }
}

impl<S, B, F> Stream for Reduce<S, B, F>
where
    S: Stream,
    B: Clone,
    F: FnMut(&mut B, S::Item),
{
    type Item = B;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match this.stream.poll_next(cx) {
            Poll::Ready(Some(item)) => {
                (this.f)(this.acc, item);
                Poll::Ready(Some(this.acc.clone()))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// === Combine ===

pin_project! {
    /// Emits a tuple of the latest values from both streams whenever either emits.
    /// Only starts emitting once both streams have produced at least one value.
    /// Completes when both input streams have completed.
    pub struct Combine<S1, S2>
    where
        S1: Stream,
        S2: Stream,
    {
        #[pin]
        stream1: S1,
        #[pin]
        stream2: S2,
        latest1: Option<S1::Item>,
        latest2: Option<S2::Item>,
        done1: bool,
        done2: bool,
    }
}

impl<S1, S2> Stream for Combine<S1, S2>
where
    S1: Stream,
    S2: Stream,
    S1::Item: Clone,
    S2::Item: Clone,
{
    type Item = (S1::Item, S2::Item);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let mut changed = false;

        if !*this.done1 {
            match this.stream1.poll_next(cx) {
                Poll::Ready(Some(v)) => {
                    *this.latest1 = Some(v);
                    changed = true;
                }
                Poll::Ready(None) => *this.done1 = true,
                Poll::Pending => {}
            }
        }

        if !*this.done2 {
            match this.stream2.poll_next(cx) {
                Poll::Ready(Some(v)) => {
                    *this.latest2 = Some(v);
                    changed = true;
                }
                Poll::Ready(None) => *this.done2 = true,
                Poll::Pending => {}
            }
        }

        if *this.done1 && *this.done2 {
            return Poll::Ready(None);
        }

        if changed {
            if let (Some(a), Some(b)) = (this.latest1.as_ref(), this.latest2.as_ref()) {
                return Poll::Ready(Some((a.clone(), b.clone())));
            }
        }

        Poll::Pending
    }
}

// === Merge ===

pin_project! {
    /// Merges two streams: emits items from either stream as they arrive.
    /// Completes when both input streams have completed.
    pub struct Merge<S1, S2>
    where
        S1: Stream,
        S2: Stream<Item = S1::Item>,
    {
        #[pin]
        stream1: S1,
        #[pin]
        stream2: S2,
        done1: bool,
        done2: bool,
    }
}

impl<S1, S2> Stream for Merge<S1, S2>
where
    S1: Stream,
    S2: Stream<Item = S1::Item>,
{
    type Item = S1::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // Poll stream1
        if !*this.done1 {
            match this.stream1.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(None) => *this.done1 = true,
                Poll::Pending => {}
            }
        }

        // Poll stream2
        if !*this.done2 {
            match this.stream2.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(None) => *this.done2 = true,
                Poll::Pending => {}
            }
        }

        if *this.done1 && *this.done2 {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

// === SampleCombine ===

pin_project! {
    /// Each time the source emits, pairs it with the latest value from the sampler.
    /// Does not emit until the sampler has produced at least one value.
    /// Completes when the source completes, or when the sampler completes without
    /// ever having emitted.
    pub struct SampleCombine<S, T>
    where
        T: Stream,
    {
        #[pin]
        source: S,
        #[pin]
        sampler: T,
        latest: Option<T::Item>,
        sampler_done: bool,
    }
}

impl<S, T> Stream for SampleCombine<S, T>
where
    S: Stream,
    T: Stream,
    T::Item: Clone,
{
    type Item = (S::Item, T::Item);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // Drain sampler to keep its latest value current.
        if !*this.sampler_done {
            loop {
                match this.sampler.as_mut().poll_next(cx) {
                    Poll::Ready(Some(v)) => *this.latest = Some(v),
                    Poll::Ready(None) => {
                        *this.sampler_done = true;
                        break;
                    }
                    Poll::Pending => break,
                }
            }
        }

        // Only poll source once sampler has produced a value.
        if this.latest.is_some() {
            match this.source.poll_next(cx) {
                Poll::Ready(Some(v)) => {
                    let latest = this.latest.as_ref().unwrap().clone();
                    Poll::Ready(Some((v, latest)))
                }
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        } else if *this.sampler_done {
            // Sampler completed without ever emitting.
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

// === Flatten (switch) ===

pin_project! {
    /// Flattens a stream of streams with **switch** semantics: when the outer
    /// stream emits a new inner stream, switches to it and drops the previous one.
    /// Completes when the outer stream is done and the current inner stream (if any)
    /// is exhausted.
    pub struct Flatten<S, I> {
        #[pin]
        outer: S,
        inner: Option<Pin<Box<I>>>,
        outer_done: bool,
    }
}

impl<S, I> Stream for Flatten<S, I>
where
    S: Stream<Item = I>,
    I: Stream,
{
    type Item = I::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // Drain outer to switch to the latest inner stream.
        if !*this.outer_done {
            loop {
                match this.outer.as_mut().poll_next(cx) {
                    Poll::Ready(Some(new_inner)) => {
                        *this.inner = Some(Box::pin(new_inner));
                    }
                    Poll::Ready(None) => {
                        *this.outer_done = true;
                        break;
                    }
                    Poll::Pending => break,
                }
            }
        }

        // Poll current inner stream.
        if let Some(inner) = this.inner.as_mut() {
            match inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
                Poll::Ready(None) => {
                    *this.inner = None;
                    if *this.outer_done {
                        Poll::Ready(None)
                    } else {
                        // Inner exhausted but outer may produce more — re-poll.
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
                Poll::Pending => Poll::Pending,
            }
        } else if *this.outer_done {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

// === Dedupe ===

pin_project! {
    /// Emits items only when they differ from the previous one.
    /// Filters out consecutive duplicates.
    pub struct Dedupe<S>
    where
        S: Stream,
    {
        #[pin]
        stream: S,
        prev: Option<S::Item>,
    }
}

impl<S> Stream for Dedupe<S>
where
    S: Stream,
    S::Item: Clone + PartialEq,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if this.prev.as_ref() != Some(&item) {
                        *this.prev = Some(item.clone());
                        return Poll::Ready(Some(item));
                    }
                    // Item equals previous, skip and continue polling
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// === Inspect ===

pin_project! {
    /// Calls a closure on each item by reference, passing the item through unchanged.
    /// Useful for logging/debugging without altering the stream.
    pub struct Inspect<S, F> {
        #[pin]
        stream: S,
        f: F,
    }
}

impl<S, F> Stream for Inspect<S, F>
where
    S: Stream,
    F: FnMut(&S::Item),
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        match this.stream.poll_next(cx) {
            Poll::Ready(Some(item)) => {
                (this.f)(&item);
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// === Extension trait ===

pub trait StreamOpsExt: Stream {
    /// Running accumulation: emits the accumulated value after each input event.
    ///
    /// ```ignore
    /// stream.reduce(0, |acc, x| *acc += x)
    /// ```
    fn reduce<B, F>(self, seed: B, f: F) -> Reduce<Self, B, F>
    where
        Self: Sized,
        B: Clone,
        F: FnMut(&mut B, Self::Item),
    {
        Reduce {
            stream: self,
            acc: seed,
            f,
        }
    }

    /// Merge: emits items from either stream as they arrive.
    /// Completes when both streams have completed.
    ///
    /// ```ignore
    /// stream_a.or(stream_b) // -> Stream<Item = T> where both have Item = T
    /// ```
    fn or<S2>(self, other: S2) -> Merge<Self, S2>
    where
        Self: Sized,
        S2: Stream<Item = Self::Item>,
    {
        Merge {
            stream1: self,
            stream2: other,
            done1: false,
            done2: false,
        }
    }

    /// Combine latest: emits a tuple of the latest values from both streams
    /// whenever either stream emits. Only starts emitting once both have
    /// produced at least one value.
    ///
    /// ```ignore
    /// stream_a.combine(stream_b) // -> Stream<Item = (A, B)>
    /// ```
    fn combine<S2: Stream>(self, other: S2) -> Combine<Self, S2>
    where
        Self: Sized,
        Self::Item: Clone,
        S2::Item: Clone,
    {
        Combine {
            stream1: self,
            stream2: other,
            latest1: None,
            latest2: None,
            done1: false,
            done2: false,
        }
    }

    /// Sample-combine: each time the source emits, pairs it with the latest
    /// value from the sampler stream.
    ///
    /// ```ignore
    /// source.sample_combine(sampler) // -> Stream<Item = (S, T)>
    /// ```
    fn sample_combine<S2: Stream>(self, sampler: S2) -> SampleCombine<Self, S2>
    where
        Self: Sized,
        S2::Item: Clone,
    {
        SampleCombine {
            source: self,
            sampler,
            latest: None,
            sampler_done: false,
        }
    }

    /// Flatten with switch semantics: subscribes to the latest inner stream,
    /// dropping the previous one when a new inner stream arrives.
    ///
    /// ```ignore
    /// stream_of_streams.flatten() // -> Stream<Item = Inner::Item>
    /// ```
    fn flatten<I>(self) -> Flatten<Self, I>
    where
        Self: Stream<Item = I> + Sized,
        I: Stream,
    {
        Flatten {
            outer: self,
            inner: None,
            outer_done: false,
        }
    }

    /// Filter out consecutive duplicate items.
    /// Only emits when an item differs from the previous one.
    ///
    /// ```ignore
    /// stream.dedupe() // Stream<1, 1, 2, 2, 3> -> Stream<1, 2, 3>
    /// ```
    fn dedupe(self) -> Dedupe<Self>
    where
        Self: Sized,
        Self::Item: Clone + PartialEq,
    {
        Dedupe {
            stream: self,
            prev: None,
        }
    }

    /// Inspect each item by reference without altering it.
    ///
    /// ```ignore
    /// stream.inspect(|x| log::trace!("{:#?}", x))
    /// ```
    fn inspect<F>(self, f: F) -> Inspect<Self, F>
    where
        Self: Sized,
        F: FnMut(&Self::Item),
    {
        Inspect { stream: self, f }
    }

    /// Convert a stream to a broadcast channel sender.
    /// Spawns a background task that forwards all stream items to the channel.
    /// Multiple subscribers can then call `.fanout()` on the sender.
    ///
    /// ```ignore
    /// let tx = stream.broadcast();
    /// let rx1 = tx.fanout();
    /// let rx2 = tx.fanout();
    /// ```
    fn broadcast<T: Clone + Send + 'static>(self) -> broadcast::Sender<T>
    where
        Self: AStream<T> + Sized,
    {
        let (tx, _rx) = broadcast::channel(10);
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let _rx = _rx;
            use tokio_stream::StreamExt;
            let mut stream = Box::pin(self);
            while let Some(item) = stream.next().await {
                let _ = tx_clone.send(item);
            }
        });
        tx
    }

    fn split_result<T, E>(self) -> (impl AStream<T>, impl AStream<E>)
    where
        T: Clone + Send + 'static,
        E: Clone + Send + 'static,
        Self: AStream<Result<T, E>> + Sized,
    {
        let b = self.broadcast();

        let ok = b.one_by_one().filter_map(Result::ok);
        let err = b.one_by_one().filter_map(Result::err);

        (ok, err)
    }
}

impl<S: Stream> StreamOpsExt for S {}
