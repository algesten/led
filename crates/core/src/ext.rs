use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio_stream::Stream;

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
}

impl<S: Stream> StreamOpsExt for S {}
