//! Callback→Future bridge for FASTER's completion model.
//!
//! FASTER's core is synchronous: when an operation hits disk, it returns
//! `Pending` and later fires a completion callback. This module provides a
//! runtime-agnostic bridge that converts those callbacks into standard Rust
//! [`Future`]s, using only `std` types — no async runtime dependency.
//!
//! # Architecture
//!
//! The bridge uses a single `Arc<Mutex<SharedState>>` shared between a
//! [`PendingFuture`] (the async consumer) and a [`CompletionSender`] (the
//! callback producer). This is the only allocation.
//!
//! # Usage
//!
//! ```
//! use faster_tokio::bridge::pending_pair;
//!
//! let (future, sender) = pending_pair::<u64>();
//!
//! // Give the sender to the I/O completion callback
//! std::thread::spawn(move || {
//!     sender.complete(42);
//! });
//!
//! // Await the future in an async context
//! // let result = future.await; // → 42
//! ```
//!
//! For operations that may complete synchronously, use [`MaybePending`]:
//!
//! ```
//! use faster_tokio::bridge::{pending_pair, MaybePending};
//!
//! fn do_operation(sync: bool) -> MaybePending<u64> {
//!     if sync {
//!         MaybePending::Ready(42)
//!     } else {
//!         let (future, sender) = pending_pair();
//!         // hand `sender` to I/O callback...
//!         # sender.complete(42);
//!         MaybePending::Pending(future)
//!     }
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// State shared between the [`PendingFuture`] and its [`CompletionSender`].
struct SharedState<T> {
    /// The result value, set by [`CompletionSender::complete`].
    result: Option<T>,
    /// The most recent [`Waker`] registered by [`PendingFuture::poll`].
    waker: Option<Waker>,
    /// Whether [`CompletionSender::complete`] has been called.
    completed: bool,
}

/// A [`Future`] that resolves when a pending FASTER operation completes.
///
/// Created via [`pending_pair`]. The future resolves to the value passed to
/// the corresponding [`CompletionSender::complete`].
///
/// # Cancel Safety
///
/// Dropping this future before the sender completes is safe — the sender's
/// [`complete`](CompletionSender::complete) call becomes a harmless no-op.
pub struct PendingFuture<T> {
    shared: Arc<Mutex<SharedState<T>>>,
}

/// A handle given to the I/O completion callback to resolve a [`PendingFuture`].
///
/// This is the "write" side of the bridge. When I/O completes, call
/// [`complete`](Self::complete) with the result to wake the paired future.
///
/// # Drop Behavior
///
/// Dropping the sender without calling `complete` leaves the future
/// permanently pending. This is the expected behavior for abandoned I/O.
pub struct CompletionSender<T> {
    shared: Arc<Mutex<SharedState<T>>>,
}

impl<T> CompletionSender<T> {
    /// Signal the paired [`PendingFuture`] with a completion result.
    ///
    /// This consumes the sender, ensuring at-most-once completion. If the
    /// future has already been dropped (e.g., the async task was cancelled),
    /// the result is silently discarded.
    pub fn complete(self, result: T) {
        let mut state = self.shared.lock().unwrap();
        state.result = Some(result);
        state.completed = true;
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    }
}

impl<T> Future for PendingFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.shared.lock().unwrap();
        if state.completed {
            Poll::Ready(
                state
                    .result
                    .take()
                    .expect("PendingFuture polled after completion"),
            )
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Create a linked ([`PendingFuture`], [`CompletionSender`]) pair.
///
/// The only allocation is a single `Arc<Mutex<SharedState>>` shared between
/// the future and the sender — no boxed futures, no channels.
///
/// # Examples
///
/// ```
/// use faster_tokio::bridge::pending_pair;
///
/// let (future, sender) = pending_pair::<String>();
/// sender.complete("done".into());
/// // future.await → "done"
/// ```
pub fn pending_pair<T>() -> (PendingFuture<T>, CompletionSender<T>) {
    let shared = Arc::new(Mutex::new(SharedState {
        result: None,
        waker: None,
        completed: false,
    }));
    (
        PendingFuture {
            shared: shared.clone(),
        },
        CompletionSender { shared },
    )
}

/// The result of a FASTER operation that may have completed synchronously.
///
/// When the core returns a result immediately (no disk I/O needed), use
/// [`MaybePending::Ready`] to avoid allocating a future at all. When the
/// operation is pending, use [`MaybePending::Pending`] with a [`PendingFuture`].
///
/// Implements [`IntoFuture`] so it can be `.await`ed directly:
///
/// ```
/// use faster_tokio::bridge::{pending_pair, MaybePending};
///
/// async fn example() {
///     let result: u32 = MaybePending::Ready(42).await;
///     assert_eq!(result, 42);
/// }
/// ```
pub enum MaybePending<T> {
    /// The operation completed synchronously — no future allocation needed.
    Ready(T),
    /// The operation is pending disk I/O — await the inner future.
    Pending(PendingFuture<T>),
}

/// The [`Future`] type produced by [`MaybePending::into_future`].
///
/// For the `Ready` variant, resolves immediately on first poll.
/// For the `Pending` variant, delegates to the inner [`PendingFuture`].
pub enum MaybePendingFuture<T> {
    /// Resolves immediately on first poll.
    Ready(Option<T>),
    /// Delegates to the inner [`PendingFuture`].
    Pending(PendingFuture<T>),
}

impl<T> std::future::IntoFuture for MaybePending<T> {
    type Output = T;
    type IntoFuture = MaybePendingFuture<T>;

    fn into_future(self) -> Self::IntoFuture {
        match self {
            MaybePending::Ready(v) => MaybePendingFuture::Ready(Some(v)),
            MaybePending::Pending(f) => MaybePendingFuture::Pending(f),
        }
    }
}

// `MaybePendingFuture<T>` does not expose pinned references to `T`.
// - `PendingFuture<T>` is `Unpin` (wraps only `Arc<Mutex<…>>`).
// - The `Ready` variant stores `Option<T>`, but we only `take()` the value
//   out — we never create `Pin<&mut T>`, so no structural pinning applies.
//   `T` enters via `into_future()` (by-value) and exits via `take()` (by-value).
impl<T> Unpin for MaybePendingFuture<T> {}

impl<T> Future for MaybePendingFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        match this {
            MaybePendingFuture::Ready(v) => Poll::Ready(
                v.take()
                    .expect("MaybePendingFuture polled after completion"),
            ),
            MaybePendingFuture::Pending(f) => Pin::new(f).poll(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// A test waker that counts how many times it has been woken.
    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Create a [`Waker`] that tracks wake count via the returned counter.
    fn counting_waker() -> (Waker, Arc<CountingWaker>) {
        let inner = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(inner.clone());
        (waker, inner)
    }

    #[test]
    fn basic_cross_thread_completion() {
        let (mut future, sender) = pending_pair::<i32>();

        let handle = std::thread::spawn(move || {
            sender.complete(42);
        });
        handle.join().unwrap();

        let (waker, _counter) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(v) => assert_eq!(v, 42),
            Poll::Pending => panic!("expected Ready after cross-thread completion"),
        }
    }

    #[test]
    fn immediate_complete_before_poll() {
        let (mut future, sender) = pending_pair::<String>();

        // Complete before any poll — first poll must return Ready.
        sender.complete("hello".to_string());

        let (waker, _counter) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(v) => assert_eq!(v, "hello"),
            Poll::Pending => panic!("expected Ready on first poll after immediate complete"),
        }
    }

    #[test]
    fn multiple_polls_pending_then_ready() {
        let (mut future, sender) = pending_pair::<u64>();

        let (waker, counter) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll → Pending (no completion yet)
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);

        // Complete — waker should fire
        sender.complete(99);
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);

        // Second poll → Ready
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(v) => assert_eq!(v, 99),
            Poll::Pending => panic!("expected Ready after completion"),
        }
    }

    #[test]
    fn drop_future_before_complete() {
        let (future, sender) = pending_pair::<Vec<u8>>();

        // Drop the future — simulates task cancellation
        drop(future);

        // Completing must not panic; result is silently discarded
        sender.complete(vec![1, 2, 3]);
    }

    #[test]
    fn drop_sender_without_completing() {
        let (mut future, sender) = pending_pair::<i32>();

        // Drop the sender without completing — simulates abandoned I/O
        drop(sender);

        // Future remains permanently pending (expected behavior)
        let (waker, _counter) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
    }

    #[test]
    fn waker_replacement() {
        let (mut future, sender) = pending_pair::<i32>();

        // Poll with first waker
        let (waker1, count1) = counting_waker();
        let mut cx1 = Context::from_waker(&waker1);
        assert!(Pin::new(&mut future).poll(&mut cx1).is_pending());

        // Poll with second (different) waker — must replace the first
        let (waker2, count2) = counting_waker();
        let mut cx2 = Context::from_waker(&waker2);
        assert!(Pin::new(&mut future).poll(&mut cx2).is_pending());

        // Complete — only the most recent waker (waker2) should fire
        sender.complete(7);

        assert_eq!(
            count1.0.load(Ordering::SeqCst),
            0,
            "stale waker must not be woken"
        );
        assert_eq!(
            count2.0.load(Ordering::SeqCst),
            1,
            "current waker must be woken exactly once"
        );
    }

    #[test]
    fn maybe_pending_ready_resolves_immediately() {
        let mp = MaybePending::Ready(42u32);
        let mut future = mp.into_future();

        let (waker, _counter) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(v) => assert_eq!(v, 42),
            Poll::Pending => panic!("Ready variant must resolve on first poll"),
        }
    }

    #[test]
    fn maybe_pending_pending_delegates() {
        let (pf, sender) = pending_pair::<u32>();
        let mp = MaybePending::Pending(pf);
        let mut future = mp.into_future();

        let (waker, _counter) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(Pin::new(&mut future).poll(&mut cx).is_pending());

        sender.complete(100);
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(v) => assert_eq!(v, 100),
            Poll::Pending => panic!("expected Ready after sender.complete()"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tokio_integration_100_futures() {
        let mut handles = Vec::new();
        for i in 0..100i32 {
            let (future, sender) = pending_pair::<i32>();

            // Simulate I/O completion from OS threads (like FASTER's I/O pool)
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1));
                sender.complete(i);
            });

            handles.push(tokio::spawn(async move {
                let result = future.await;
                assert_eq!(result, i);
                result
            }));
        }

        let mut results: Vec<i32> = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }
        results.sort();
        let expected: Vec<i32> = (0..100).collect();
        assert_eq!(results, expected);
    }
}
