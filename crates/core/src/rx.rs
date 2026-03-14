//! Push-based reactive primitives.
//!
//! Single-threaded. `Stream` is `!Send` (`Rc` inside). Thread boundaries
//! are crossed with standard channel types at the edges.
//!
//! Combinator API always works with owned values (`T -> U`). The clone
//! at the Stream→Pipe boundary is the cost of fan-out. Inside a pipe
//! chain, values are moved with no cloning.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;

// ── Stream ──

struct StreamInner<T> {
    listeners: RefCell<Vec<Box<dyn FnMut(&T)>>>,
    queue: RefCell<VecDeque<T>>,
    draining: Cell<bool>,
}

/// A node in the push-based reactive graph.
///
/// `push()` fires all listeners synchronously. Re-entrant pushes are
/// queued and drained before `push()` returns — the tree reaches a
/// fixpoint for each external event.
pub struct Stream<T: 'static> {
    inner: Rc<StreamInner<T>>,
}

impl<T: 'static> Clone for Stream<T> {
    fn clone(&self) -> Self {
        Stream {
            inner: self.inner.clone(),
        }
    }
}

impl<T: 'static> Stream<T> {
    pub fn new() -> Self {
        Stream {
            inner: Rc::new(StreamInner {
                listeners: RefCell::new(Vec::new()),
                queue: RefCell::new(VecDeque::new()),
                draining: Cell::new(false),
            }),
        }
    }

    /// Push a value. All listeners fire synchronously.
    /// Re-entrant pushes are queued and drained before returning.
    pub fn push(&self, value: T) {
        self.inner.queue.borrow_mut().push_back(value);

        if self.inner.draining.get() {
            return;
        }

        self.inner.draining.set(true);
        loop {
            let next = self.inner.queue.borrow_mut().pop_front();
            let Some(value) = next else { break };
            // queue borrow is released — listeners may re-enter push()
            let mut listeners = self.inner.listeners.borrow_mut();
            for listener in listeners.iter_mut() {
                listener(&value);
            }
        }
        self.inner.draining.set(false);
    }

    /// Low-level subscribe. Listener receives `&T` (shared across fan-out).
    /// Prefer combinators (`map`, `filter_map`) for the owned-value API.
    pub fn on(&self, f: impl FnMut(&T) + 'static) {
        self.inner.listeners.borrow_mut().push(Box::new(f));
    }

    /// Subscribe another pipe to this stream (fan-in).
    pub fn or<'a, S2: 'static, F2: FnMut(&S2) -> Option<T> + 'static>(
        &self,
        pipe: Pipe<'a, S2, T, F2>,
    ) -> Stream<T> {
        pipe.into(self);
        self.clone()
    }
}

impl<T: Clone + 'static> Stream<T> {
    /// Start a combinator chain. Clones `T` at the boundary.
    pub fn map<U: 'static>(
        &self,
        mut f: impl FnMut(T) -> U + 'static,
    ) -> Pipe<'_, T, U, impl FnMut(&T) -> Option<U> + 'static> {
        Pipe {
            source: self,
            f: move |t: &T| Some(f(t.clone())),
            _t: PhantomData,
        }
    }

    /// Start a filtering combinator chain. Clones `T` at the boundary.
    pub fn filter_map<U: 'static>(
        &self,
        mut f: impl FnMut(T) -> Option<U> + 'static,
    ) -> Pipe<'_, T, U, impl FnMut(&T) -> Option<U> + 'static> {
        Pipe {
            source: self,
            f: move |t: &T| f(t.clone()),
            _t: PhantomData,
        }
    }

    /// Start a filtering combinator chain with a predicate.
    pub fn filter(
        &self,
        mut pred: impl FnMut(&T) -> bool + 'static,
    ) -> Pipe<'_, T, T, impl FnMut(&T) -> Option<T> + 'static> {
        Pipe {
            source: self,
            f: move |t: &T| {
                if pred(t) { Some(t.clone()) } else { None }
            },
            _t: PhantomData,
        }
    }

    /// Forward all values to another stream (fan-in).
    pub fn forward(&self, target: &Stream<T>) {
        let target = target.clone();
        self.on(move |t: &T| target.push(t.clone()));
    }

    /// Accumulate values. Emits the accumulator after each input.
    pub fn fold<B: Clone + 'static>(
        &self,
        seed: B,
        f: impl FnMut(B, T) -> B + 'static,
    ) -> Stream<B> {
        let target: Stream<B> = Stream::new();
        self.fold_into(&target, seed, f);
        target
    }

    /// Accumulate values into a pre-existing stream.
    /// Use when the target must exist before the fold is set up (e.g. cycles).
    pub fn fold_into<B: Clone + 'static>(
        &self,
        target: &Stream<B>,
        seed: B,
        mut f: impl FnMut(B, T) -> B + 'static,
    ) {
        let target = target.clone();
        let mut acc = Some(seed);
        self.on(move |t: &T| {
            let a = acc.take().unwrap();
            let new_a = f(a, t.clone());
            target.push(new_a.clone());
            acc = Some(new_a);
        });
    }

    /// Suppress consecutive equal values.
    pub fn dedupe(&self) -> Stream<T>
    where
        T: PartialEq,
    {
        let target = Stream::new();
        let target2 = target.clone();
        let mut prev: Option<T> = None;
        self.on(move |t: &T| {
            let dominated = prev.as_ref() == Some(t);
            if !dominated {
                prev = Some(t.clone());
                target2.push(t.clone());
            }
        });
        target
    }

    /// Suppress consecutive values with equal extracted key.
    pub fn dedupe_by<K: PartialEq + 'static>(
        &self,
        mut key_fn: impl FnMut(&T) -> K + 'static,
    ) -> Stream<T> {
        let target = Stream::new();
        let target2 = target.clone();
        let mut prev_key: Option<K> = None;
        self.on(move |t: &T| {
            let k = key_fn(t);
            let dominated = prev_key.as_ref() == Some(&k);
            if !dominated {
                prev_key = Some(k);
                target2.push(t.clone());
            }
        });
        target
    }

    /// When this stream fires, pair the value with the latest from `sampler`.
    /// Does not emit until the sampler has produced at least one value.
    pub fn sample_combine<B: Clone + 'static>(&self, sampler: &Stream<B>) -> Stream<(T, B)> {
        let target = Stream::new();
        let target2 = target.clone();
        let latest: Rc<RefCell<Option<B>>> = Rc::new(RefCell::new(None));
        let latest2 = latest.clone();

        // Track the sampler's latest value
        sampler.on(move |b: &B| {
            *latest2.borrow_mut() = Some(b.clone());
        });

        // When source fires, pair with latest sampler value
        self.on(move |t: &T| {
            let b = latest.borrow().clone();
            if let Some(b) = b {
                target2.push((t.clone(), b));
            }
        });

        target
    }

    /// Side-effect pass-through. Calls `f` on each value without altering it.
    pub fn inspect(&self, mut f: impl FnMut(&T) + 'static) -> Stream<T> {
        let target = Stream::new();
        let target2 = target.clone();
        self.on(move |t: &T| {
            f(t);
            target2.push(t.clone());
        });
        target
    }

    /// Cache last value. New listeners added after a value has been pushed
    /// receive the cached value immediately.
    pub fn remember(&self) -> MemoryStream<T> {
        let mem = MemoryStream::new();
        let mem2 = mem.clone();
        self.on(move |t: &T| mem2.push(t.clone()));
        mem
    }

    /// Prepend an initial value. Returns a MemoryStream seeded with `value`.
    pub fn start_with(&self, value: T) -> MemoryStream<T> {
        let mem = MemoryStream::new();
        mem.inner.last.replace(Some(value));
        let mem2 = mem.clone();
        self.on(move |t: &T| mem2.push(t.clone()));
        mem
    }

    /// Flatten a stream of streams. Subscribes to the latest inner stream,
    /// dropping the previous subscription when a new inner stream arrives.
    pub fn flatten(&self) -> Stream<T>
    where
        T: Clone + 'static,
    {
        // This is defined on Stream<Stream<T>>, see below.
        // We need a separate impl block for that.
        unreachable!()
    }
}

// Flatten: defined on Stream<Stream<T>>
impl<T: Clone + 'static> Stream<Stream<T>> {
    /// Subscribe to the latest inner stream. When a new inner stream
    /// arrives, the previous subscription is dropped.
    pub fn flatten_switch(&self) -> Stream<T> {
        let target = Stream::new();
        let target2 = target.clone();
        let epoch = Rc::new(Cell::new(0u64));
        let epoch2 = epoch.clone();

        self.on(move |inner: &Stream<T>| {
            let current = epoch2.get() + 1;
            epoch2.set(current);

            let target3 = target2.clone();
            let epoch3 = epoch.clone();
            inner.on(move |t: &T| {
                // Only forward if we're still the current epoch
                if epoch3.get() == current {
                    target3.push(t.clone());
                }
            });
        });

        target
    }
}

/// Emit a tuple when any input fires, once all inputs have produced a value.
///
/// ```ignore
/// let s = combine!(stream_a, stream_b, stream_c);
/// // s: Stream<(A, B, C)>
/// ```
///
/// Supports 2–10 inputs.
#[macro_export]
macro_rules! combine {
    ($s0:expr, $s1:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!($s0, __target, __v0, [__v0, __v1]);
        $crate::_combine_sub!($s1, __target, __v1, [__v0, __v1]);
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!($s0, __target, __v0, [__v0, __v1, __v2]);
        $crate::_combine_sub!($s1, __target, __v1, [__v0, __v1, __v2]);
        $crate::_combine_sub!($s2, __target, __v2, [__v0, __v1, __v2]);
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr, $s3:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        let __v3 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!($s0, __target, __v0, [__v0, __v1, __v2, __v3]);
        $crate::_combine_sub!($s1, __target, __v1, [__v0, __v1, __v2, __v3]);
        $crate::_combine_sub!($s2, __target, __v2, [__v0, __v1, __v2, __v3]);
        $crate::_combine_sub!($s3, __target, __v3, [__v0, __v1, __v2, __v3]);
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr, $s3:expr, $s4:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        let __v3 = Rc::new(RefCell::new(None));
        let __v4 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!($s0, __target, __v0, [__v0, __v1, __v2, __v3, __v4]);
        $crate::_combine_sub!($s1, __target, __v1, [__v0, __v1, __v2, __v3, __v4]);
        $crate::_combine_sub!($s2, __target, __v2, [__v0, __v1, __v2, __v3, __v4]);
        $crate::_combine_sub!($s3, __target, __v3, [__v0, __v1, __v2, __v3, __v4]);
        $crate::_combine_sub!($s4, __target, __v4, [__v0, __v1, __v2, __v3, __v4]);
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr, $s3:expr, $s4:expr, $s5:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        let __v3 = Rc::new(RefCell::new(None));
        let __v4 = Rc::new(RefCell::new(None));
        let __v5 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!($s0, __target, __v0, [__v0, __v1, __v2, __v3, __v4, __v5]);
        $crate::_combine_sub!($s1, __target, __v1, [__v0, __v1, __v2, __v3, __v4, __v5]);
        $crate::_combine_sub!($s2, __target, __v2, [__v0, __v1, __v2, __v3, __v4, __v5]);
        $crate::_combine_sub!($s3, __target, __v3, [__v0, __v1, __v2, __v3, __v4, __v5]);
        $crate::_combine_sub!($s4, __target, __v4, [__v0, __v1, __v2, __v3, __v4, __v5]);
        $crate::_combine_sub!($s5, __target, __v5, [__v0, __v1, __v2, __v3, __v4, __v5]);
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr, $s3:expr, $s4:expr, $s5:expr, $s6:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        let __v3 = Rc::new(RefCell::new(None));
        let __v4 = Rc::new(RefCell::new(None));
        let __v5 = Rc::new(RefCell::new(None));
        let __v6 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!(
            $s0,
            __target,
            __v0,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6]
        );
        $crate::_combine_sub!(
            $s1,
            __target,
            __v1,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6]
        );
        $crate::_combine_sub!(
            $s2,
            __target,
            __v2,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6]
        );
        $crate::_combine_sub!(
            $s3,
            __target,
            __v3,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6]
        );
        $crate::_combine_sub!(
            $s4,
            __target,
            __v4,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6]
        );
        $crate::_combine_sub!(
            $s5,
            __target,
            __v5,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6]
        );
        $crate::_combine_sub!(
            $s6,
            __target,
            __v6,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6]
        );
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr, $s3:expr, $s4:expr, $s5:expr, $s6:expr, $s7:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        let __v3 = Rc::new(RefCell::new(None));
        let __v4 = Rc::new(RefCell::new(None));
        let __v5 = Rc::new(RefCell::new(None));
        let __v6 = Rc::new(RefCell::new(None));
        let __v7 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!(
            $s0,
            __target,
            __v0,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        $crate::_combine_sub!(
            $s1,
            __target,
            __v1,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        $crate::_combine_sub!(
            $s2,
            __target,
            __v2,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        $crate::_combine_sub!(
            $s3,
            __target,
            __v3,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        $crate::_combine_sub!(
            $s4,
            __target,
            __v4,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        $crate::_combine_sub!(
            $s5,
            __target,
            __v5,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        $crate::_combine_sub!(
            $s6,
            __target,
            __v6,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        $crate::_combine_sub!(
            $s7,
            __target,
            __v7,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7]
        );
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr, $s3:expr, $s4:expr, $s5:expr, $s6:expr, $s7:expr, $s8:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        let __v3 = Rc::new(RefCell::new(None));
        let __v4 = Rc::new(RefCell::new(None));
        let __v5 = Rc::new(RefCell::new(None));
        let __v6 = Rc::new(RefCell::new(None));
        let __v7 = Rc::new(RefCell::new(None));
        let __v8 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!(
            $s0,
            __target,
            __v0,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s1,
            __target,
            __v1,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s2,
            __target,
            __v2,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s3,
            __target,
            __v3,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s4,
            __target,
            __v4,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s5,
            __target,
            __v5,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s6,
            __target,
            __v6,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s7,
            __target,
            __v7,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        $crate::_combine_sub!(
            $s8,
            __target,
            __v8,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8]
        );
        __target
    }};
    ($s0:expr, $s1:expr, $s2:expr, $s3:expr, $s4:expr, $s5:expr, $s6:expr, $s7:expr, $s8:expr, $s9:expr $(,)?) => {{
        use std::cell::RefCell;
        use std::rc::Rc;
        let __target = $crate::rx::Stream::new();
        let __v0 = Rc::new(RefCell::new(None));
        let __v1 = Rc::new(RefCell::new(None));
        let __v2 = Rc::new(RefCell::new(None));
        let __v3 = Rc::new(RefCell::new(None));
        let __v4 = Rc::new(RefCell::new(None));
        let __v5 = Rc::new(RefCell::new(None));
        let __v6 = Rc::new(RefCell::new(None));
        let __v7 = Rc::new(RefCell::new(None));
        let __v8 = Rc::new(RefCell::new(None));
        let __v9 = Rc::new(RefCell::new(None));
        $crate::_combine_sub!(
            $s0,
            __target,
            __v0,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s1,
            __target,
            __v1,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s2,
            __target,
            __v2,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s3,
            __target,
            __v3,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s4,
            __target,
            __v4,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s5,
            __target,
            __v5,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s6,
            __target,
            __v6,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s7,
            __target,
            __v7,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s8,
            __target,
            __v8,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        $crate::_combine_sub!(
            $s9,
            __target,
            __v9,
            [__v0, __v1, __v2, __v3, __v4, __v5, __v6, __v7, __v8, __v9]
        );
        __target
    }};
}

/// Helper: subscribe one stream, updating its slot and emitting the full tuple.
#[doc(hidden)]
#[macro_export]
macro_rules! _combine_sub {
    ($stream:expr, $target:ident, $mine:ident, [$($slot:ident),+]) => {
        {
            $(let $slot = $slot.clone();)+
            let __target = $target.clone();
            $stream.on(move |v: &_| {
                *$mine.borrow_mut() = Some(v.clone());
                $(let $slot = $slot.borrow().clone();)+
                if let ($(Some($slot),)+) = ($($slot,)+) {
                    __target.push(($($slot,)+));
                }
            });
        }
    };
}

// ── MemoryStream ──

struct MemoryStreamInner<T: 'static> {
    stream: Stream<T>,
    last: RefCell<Option<T>>,
}

/// A stream that caches the last value. New listeners receive
/// the cached value immediately upon subscribing.
pub struct MemoryStream<T: 'static> {
    inner: Rc<MemoryStreamInner<T>>,
}

impl<T: 'static> Clone for MemoryStream<T> {
    fn clone(&self) -> Self {
        MemoryStream {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Clone + 'static> MemoryStream<T> {
    pub fn new() -> Self {
        MemoryStream {
            inner: Rc::new(MemoryStreamInner {
                stream: Stream::new(),
                last: RefCell::new(None),
            }),
        }
    }

    /// Push a value. Caches it, then pushes to the underlying stream.
    pub fn push(&self, value: T) {
        *self.inner.last.borrow_mut() = Some(value.clone());
        self.inner.stream.push(value);
    }

    /// Subscribe. If a value has been cached, the listener receives it immediately.
    pub fn on(&self, mut f: impl FnMut(&T) + 'static) {
        if let Some(v) = self.inner.last.borrow().as_ref() {
            f(v);
        }
        self.inner.stream.on(f);
    }

    /// Start a combinator chain (same as Stream::map but replays cached value).
    pub fn map<U: 'static>(
        &self,
        mut f: impl FnMut(T) -> U + 'static,
    ) -> Pipe<'_, T, U, impl FnMut(&T) -> Option<U> + 'static> {
        // Delegate to the underlying stream for the pipe source.
        // The replay happens via MemoryStream::on which is called by Pipe::into/on.
        // However, Pipe stores &Stream<S> as source. We need to expose the inner stream.
        Pipe {
            source: &self.inner.stream,
            f: move |t: &T| Some(f(t.clone())),
            _t: PhantomData,
        }
    }

    /// Access the underlying stream (for combinators that take &Stream<T>).
    pub fn stream(&self) -> &Stream<T> {
        &self.inner.stream
    }
}

// ── Pipe ──

/// Combinator chain builder. Monomorphic — each step composes into a
/// single closure. Dynamic dispatch only at the `Stream` boundary (fan-out).
///
/// All combinators take and produce owned values (`T -> U`).
pub struct Pipe<'a, S: 'static, T, F> {
    source: &'a Stream<S>,
    f: F,
    _t: PhantomData<T>,
}

impl<'a, S: 'static, T: 'static, F: FnMut(&S) -> Option<T> + 'static> Pipe<'a, S, T, F> {
    /// Chain a map. `g` receives owned `T`, produces `U`.
    pub fn map<U: 'static>(
        self,
        mut g: impl FnMut(T) -> U + 'static,
    ) -> Pipe<'a, S, U, impl FnMut(&S) -> Option<U> + 'static> {
        let mut f = self.f;
        Pipe {
            source: self.source,
            f: move |s: &S| f(s).map(|t| g(t)),
            _t: PhantomData,
        }
    }

    /// Chain a filter_map. `g` receives owned `T`, returns `Option<U>`.
    pub fn filter_map<U: 'static>(
        self,
        mut g: impl FnMut(T) -> Option<U> + 'static,
    ) -> Pipe<'a, S, U, impl FnMut(&S) -> Option<U> + 'static> {
        let mut f = self.f;
        Pipe {
            source: self.source,
            f: move |s: &S| f(s).and_then(|t| g(t)),
            _t: PhantomData,
        }
    }

    /// Chain a filter. `pred` receives `&T`, keeps values where it returns true.
    pub fn filter(
        self,
        mut pred: impl FnMut(&T) -> bool + 'static,
    ) -> Pipe<'a, S, T, impl FnMut(&S) -> Option<T> + 'static> {
        let mut f = self.f;
        Pipe {
            source: self.source,
            f: move |s: &S| f(s).filter(|t| pred(t)),
            _t: PhantomData,
        }
    }

    /// Chain an inspect. `g` sees `&T` without altering the value.
    pub fn inspect(
        self,
        mut g: impl FnMut(&T) + 'static,
    ) -> Pipe<'a, S, T, impl FnMut(&S) -> Option<T> + 'static> {
        let mut f = self.f;
        Pipe {
            source: self.source,
            f: move |s: &S| f(s).inspect(|t| g(t)),
            _t: PhantomData,
        }
    }

    /// Chain dedupe. Suppresses consecutive equal values.
    pub fn dedupe(self) -> Pipe<'a, S, T, impl FnMut(&S) -> Option<T> + 'static>
    where
        T: PartialEq + Clone,
    {
        let mut f = self.f;
        let mut prev: Option<T> = None;
        Pipe {
            source: self.source,
            f: move |s: &S| {
                let t = f(s)?;
                if prev.as_ref() == Some(&t) {
                    return None;
                }
                prev = Some(t.clone());
                Some(t)
            },
            _t: PhantomData,
        }
    }

    /// Chain dedupe by key. Suppresses consecutive values with equal extracted key.
    pub fn dedupe_by<K: PartialEq + 'static>(
        self,
        mut key_fn: impl FnMut(&T) -> K + 'static,
    ) -> Pipe<'a, S, T, impl FnMut(&S) -> Option<T> + 'static> {
        let mut f = self.f;
        let mut prev_key: Option<K> = None;
        Pipe {
            source: self.source,
            f: move |s: &S| {
                let t = f(s)?;
                let k = key_fn(&t);
                if prev_key.as_ref() == Some(&k) {
                    return None;
                }
                prev_key = Some(k);
                Some(t)
            },
            _t: PhantomData,
        }
    }

    /// Merge with another pipe. Creates a Stream that both pipes push into.
    pub fn or<'b, S2: 'static, F2: FnMut(&S2) -> Option<T> + 'static>(
        self,
        other: Pipe<'b, S2, T, F2>,
    ) -> Stream<T> {
        let stream = Stream::new();
        self.into(&stream);
        other.into(&stream);
        stream
    }

    /// Finalize: accumulate values. Emits the accumulator after each input.
    pub fn fold<B: Clone + 'static>(
        self,
        seed: B,
        mut acc_fn: impl FnMut(B, T) -> B + 'static,
    ) -> Stream<B> {
        let target = Stream::new();
        let target2 = target.clone();
        let mut acc = Some(seed);
        let mut f = self.f;
        self.source.on(move |s: &S| {
            if let Some(t) = f(s) {
                let a = acc.take().unwrap();
                let new_a = acc_fn(a, t);
                target2.push(new_a.clone());
                acc = Some(new_a);
            }
        });
        target
    }

    /// Finalize: materialize into a new Stream (fan-out point).
    pub fn stream(self) -> Stream<T> {
        let s = Stream::new();
        self.into(&s);
        s
    }

    /// Finalize: push transformed values into a target stream.
    pub fn into(self, target: &Stream<T>) {
        let target = target.clone();
        let mut f = self.f;
        self.source.on(move |s: &S| {
            if let Some(t) = f(s) {
                target.push(t);
            }
        });
    }

    /// Finalize: call a callback with each transformed value (owned).
    pub fn on(self, mut callback: impl FnMut(T) + 'static) {
        let mut f = self.f;
        self.source.on(move |s: &S| {
            if let Some(t) = f(s) {
                callback(t);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── map ──

    #[test]
    fn map_combinator() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .map(|x| x * 2)
            .map(|x| x + 1)
            .on(move |x| l.borrow_mut().push(x));

        source.push(5);
        source.push(10);

        assert_eq!(*log.borrow(), vec![11, 21]);
    }

    // ── filter_map ──

    #[test]
    fn filter_map_combinator() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .filter_map(|x| if x > 0 { Some(x) } else { None })
            .map(|x| x * 10)
            .on(move |x| l.borrow_mut().push(x));

        source.push(-1);
        source.push(3);
        source.push(-2);
        source.push(5);

        assert_eq!(*log.borrow(), vec![30, 50]);
    }

    // ── filter ──

    #[test]
    fn filter_on_stream() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .filter(|x| *x > 0)
            .on(move |x| l.borrow_mut().push(x));

        source.push(-1);
        source.push(3);
        source.push(5);

        assert_eq!(*log.borrow(), vec![3, 5]);
    }

    #[test]
    fn filter_on_pipe() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .map(|x| x * 2)
            .filter(|x| *x > 5)
            .on(move |x| l.borrow_mut().push(x));

        source.push(1); // 2, filtered
        source.push(3); // 6, passes
        source.push(5); // 10, passes

        assert_eq!(*log.borrow(), vec![6, 10]);
    }

    // ── or (fan-in) ──

    #[test]
    fn or_pipes() {
        let a: Stream<i32> = Stream::new();
        let b: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        let merged = a.map(|x| x * 10).or(b.map(|x| x * 100));
        merged.on(move |x: &i32| l.borrow_mut().push(*x));

        a.push(1);
        b.push(2);
        a.push(3);

        assert_eq!(*log.borrow(), vec![10, 200, 30]);
    }

    #[test]
    fn or_chain() {
        let a: Stream<i32> = Stream::new();
        let b: Stream<i32> = Stream::new();
        let c: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        let merged = a.map(|x| x).or(b.map(|x| x)).or(c.map(|x| x));
        merged.on(move |x: &i32| l.borrow_mut().push(*x));

        a.push(1);
        b.push(2);
        c.push(3);

        assert_eq!(*log.borrow(), vec![1, 2, 3]);
    }

    // ── fold ──

    #[test]
    fn fold_on_stream() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        let state = source.fold(0, |acc, x| acc + x);
        state.on(move |x: &i32| l.borrow_mut().push(*x));

        source.push(1);
        source.push(2);
        source.push(3);

        assert_eq!(*log.borrow(), vec![1, 3, 6]);
    }

    #[test]
    fn fold_on_pipe() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        let state = source.map(|x| x * 10).fold(0, |acc, x| acc + x);
        state.on(move |x: &i32| l.borrow_mut().push(*x));

        source.push(1);
        source.push(2);
        source.push(3);

        assert_eq!(*log.borrow(), vec![10, 30, 60]);
    }

    // ── dedupe ──

    #[test]
    fn dedupe_on_stream() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source.dedupe().on(move |x: &i32| l.borrow_mut().push(*x));

        source.push(1);
        source.push(1);
        source.push(2);
        source.push(2);
        source.push(3);

        assert_eq!(*log.borrow(), vec![1, 2, 3]);
    }

    #[test]
    fn dedupe_on_pipe() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .map(|x| x / 2) // 0, 0, 1, 1, 2
            .dedupe()
            .on(move |x| l.borrow_mut().push(x));

        source.push(0);
        source.push(1);
        source.push(2);
        source.push(3);
        source.push(4);

        assert_eq!(*log.borrow(), vec![0, 1, 2]);
    }

    // ── dedupe_by ──

    #[test]
    fn dedupe_by_on_stream() {
        let source: Stream<(i32, &str)> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .dedupe_by(|t| t.0)
            .on(move |t: &(i32, &str)| l.borrow_mut().push(t.1.to_string()));

        source.push((1, "a"));
        source.push((1, "b")); // suppressed — same key
        source.push((2, "c"));

        assert_eq!(*log.borrow(), vec!["a", "c"]);
    }

    #[test]
    fn dedupe_by_on_pipe() {
        let source: Stream<(i32, &str)> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .map(|t| t)
            .dedupe_by(|t| t.0)
            .on(move |t| l.borrow_mut().push(t.1.to_string()));

        source.push((1, "a"));
        source.push((1, "b")); // suppressed
        source.push((2, "c"));

        assert_eq!(*log.borrow(), vec!["a", "c"]);
    }

    // ── sample_combine ──

    #[test]
    fn sample_combine_basic() {
        let source: Stream<&str> = Stream::new();
        let sampler: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source
            .sample_combine(&sampler)
            .on(move |pair: &(&str, i32)| l.borrow_mut().push(format!("{}:{}", pair.0, pair.1)));

        // No sampler value yet — nothing emitted
        source.push("a");

        // Sampler gets a value
        sampler.push(10);

        // Now source fires — paired with latest sampler
        source.push("b");
        source.push("c");

        // Sampler updates
        sampler.push(20);
        source.push("d");

        assert_eq!(*log.borrow(), vec!["b:10", "c:10", "d:20"]);
    }

    // ── inspect ──

    #[test]
    fn inspect_on_stream() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let side = Rc::new(RefCell::new(Vec::new()));

        let s = side.clone();
        let l = log.clone();
        source
            .inspect(move |x: &i32| s.borrow_mut().push(format!("saw:{x}")))
            .on(move |x: &i32| l.borrow_mut().push(*x));

        source.push(1);
        source.push(2);

        assert_eq!(*log.borrow(), vec![1, 2]);
        assert_eq!(*side.borrow(), vec!["saw:1", "saw:2"]);
    }

    #[test]
    fn inspect_on_pipe() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));
        let side = Rc::new(RefCell::new(Vec::new()));

        let s = side.clone();
        let l = log.clone();
        source
            .map(|x| x * 2)
            .inspect(move |x| s.borrow_mut().push(format!("saw:{x}")))
            .on(move |x| l.borrow_mut().push(x));

        source.push(3);
        source.push(5);

        assert_eq!(*log.borrow(), vec![6, 10]);
        assert_eq!(*side.borrow(), vec!["saw:6", "saw:10"]);
    }

    // ── remember / start_with ──

    #[test]
    fn remember_replays() {
        let source: Stream<i32> = Stream::new();

        let mem = source.remember();
        source.push(42);

        // Late subscriber gets the cached value
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        mem.on(move |x: &i32| l.borrow_mut().push(*x));

        // Receives 42 (replayed) immediately
        assert_eq!(*log.borrow(), vec![42]);

        // Further values propagate normally
        source.push(99);
        assert_eq!(*log.borrow(), vec![42, 99]);
    }

    #[test]
    fn start_with() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let mem = source.start_with(0);

        let l = log.clone();
        mem.on(move |x: &i32| l.borrow_mut().push(*x));

        // Receives seed immediately
        assert_eq!(*log.borrow(), vec![0]);

        source.push(1);
        source.push(2);
        assert_eq!(*log.borrow(), vec![0, 1, 2]);
    }

    // ── combine ──

    #[test]
    fn combine_two() {
        let a: Stream<i32> = Stream::new();
        let b: Stream<&str> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        combine!(a, b).on(move |pair: &(i32, &str)| {
            l.borrow_mut().push(format!("{}:{}", pair.0, pair.1));
        });

        a.push(1); // b has no value yet — no emit
        b.push("x"); // now both have values
        a.push(2);
        b.push("y");

        assert_eq!(*log.borrow(), vec!["1:x", "2:x", "2:y"]);
    }

    #[test]
    fn combine_three() {
        let a: Stream<i32> = Stream::new();
        let b: Stream<i32> = Stream::new();
        let c: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        combine!(a, b, c).on(move |t: &(i32, i32, i32)| {
            l.borrow_mut().push(format!("{},{},{}", t.0, t.1, t.2));
        });

        a.push(1);
        b.push(2);
        // still no emit — c missing
        c.push(3);
        a.push(10);

        assert_eq!(*log.borrow(), vec!["1,2,3", "10,2,3"]);
    }

    // ── flatten ──

    #[test]
    fn flatten_switch() {
        let outer: Stream<Stream<i32>> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        outer
            .flatten_switch()
            .on(move |x: &i32| l.borrow_mut().push(*x));

        let inner1: Stream<i32> = Stream::new();
        let inner2: Stream<i32> = Stream::new();

        outer.push(inner1.clone());
        inner1.push(1);
        inner1.push(2);

        outer.push(inner2.clone()); // switch to inner2
        inner1.push(3); // ignored — inner1 is stale
        inner2.push(4);
        inner2.push(5);

        assert_eq!(*log.borrow(), vec![1, 2, 4, 5]);
    }

    // ── fan-out / fan-in / reentrant / thread boundary ──

    #[test]
    fn fan_out() {
        let source: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let l = log.clone();
        source.on(move |x: &i32| l.borrow_mut().push(format!("a:{x}")));

        let l = log.clone();
        source.on(move |x: &i32| l.borrow_mut().push(format!("b:{x}")));

        source.push(1);

        assert_eq!(*log.borrow(), vec!["a:1", "b:1"]);
    }

    #[test]
    fn fan_in() {
        let a: Stream<i32> = Stream::new();
        let b: Stream<i32> = Stream::new();
        let merged: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        a.forward(&merged);
        b.forward(&merged);

        let l = log.clone();
        merged.on(move |x: &i32| l.borrow_mut().push(*x));

        a.push(1);
        b.push(2);

        assert_eq!(*log.borrow(), vec![1, 2]);
    }

    #[test]
    fn reentrant() {
        let sink: Stream<i32> = Stream::new();
        let log = Rc::new(RefCell::new(Vec::new()));

        let s = sink.clone();
        let l = log.clone();
        sink.on(move |x: &i32| {
            l.borrow_mut().push(*x);
            if *x < 3 {
                s.push(*x + 1);
            }
        });

        sink.push(1);

        assert_eq!(*log.borrow(), vec![1, 2, 3]);
    }

    #[test]
    fn thread_boundary() {
        let (in_tx, in_rx) = std::sync::mpsc::channel::<i32>();
        let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();

        let input: Stream<i32> = Stream::new();
        let output: Stream<String> = Stream::new();

        input.map(|x| format!("result:{}", x * 2)).into(&output);

        output.on(move |s: &String| {
            out_tx.send(s.clone()).ok();
        });

        in_tx.send(21).unwrap();
        in_tx.send(50).unwrap();

        while let Ok(v) = in_rx.try_recv() {
            input.push(v);
        }

        assert_eq!(out_rx.try_recv().unwrap(), "result:42");
        assert_eq!(out_rx.try_recv().unwrap(), "result:100");
    }
}
