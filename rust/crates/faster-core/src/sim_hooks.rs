//! Simulation hooks for deterministic testing.
//!
//! Provides two macro families and thread-local scheduling hooks:
//!
//! - [`sim_yield!`] — scheduling yield-point markers (Phase 3).
//! - [`crash_point!`] — crash-injection hooks (Phase 4).
//! - Yield/sleep hooks — thread-local flags consumed by the DST scheduler.
//!
//! Without the `simulation` feature both macros compile to nothing and the
//! hook functions are not available.

// ── sim_yield! ──────────────────────────────────────────────────────

/// Simulation yield point marker.
///
/// Under `simulation` feature: evaluates and discards the label expression
/// (preserved for documentation / future tracing integration).
/// Without `simulation` feature: compiles to nothing.
///
/// # Example
///
/// ```ignore
/// sim_yield!("read::after_epoch_protect");
/// ```
#[allow(unused_macros)]
macro_rules! sim_yield {
    ($label:expr) => {
        #[cfg(feature = "simulation")]
        {
            let _ = $label;
        }
    };
}

#[allow(unused_imports)]
pub(crate) use sim_yield;

// ── crash_point! ────────────────────────────────────────────────────

/// Sentinel type for simulated crashes — caught by `catch_unwind`.
#[cfg(feature = "simulation")]
pub struct SimulatedCrash {
    /// The crash-point label that triggered this panic.
    pub point: &'static str,
}

/// A function that decides whether to crash at the given label.
///
/// Installed per-thread by the DST harness via [`install_crash_check`].
#[cfg(feature = "simulation")]
type CrashCheckFn = Box<dyn Fn(&str) -> bool>;

#[cfg(feature = "simulation")]
std::thread_local! {
    static CRASH_CHECK: std::cell::RefCell<Option<CrashCheckFn>> = const { std::cell::RefCell::new(None) };
}

/// Install a crash-check closure for the current thread.
///
/// While installed, every [`crash_point!`] invocation calls `f` with its
/// label string. If `f` returns `true`, the macro panics with a
/// [`SimulatedCrash`] sentinel that the test harness catches via
/// `catch_unwind`.
#[cfg(feature = "simulation")]
pub fn install_crash_check(f: impl Fn(&str) -> bool + 'static) {
    CRASH_CHECK.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(f));
    });
}

/// Remove the crash-check closure from the current thread.
#[cfg(feature = "simulation")]
pub fn remove_crash_check() {
    CRASH_CHECK.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Query the thread-local crash oracle. Returns `true` if a crash should
/// fire at `label`.
#[cfg(feature = "simulation")]
#[inline]
pub fn should_crash(label: &str) -> bool {
    CRASH_CHECK.with(|cell| cell.borrow().as_ref().is_some_and(|f| f(label)))
}

/// Crash-point injection hook.
///
/// Under `simulation`: checks the thread-local crash oracle and, if it
/// returns `true`, panics with a [`SimulatedCrash`] sentinel.
/// Without `simulation`: compiles to nothing (zero-cost).
#[allow(unused_macros)]
macro_rules! crash_point {
    ($label:expr) => {
        #[cfg(feature = "simulation")]
        {
            if $crate::sim_hooks::should_crash($label) {
                std::panic::panic_any($crate::sim_hooks::SimulatedCrash { point: $label });
            }
        }
    };
}

#[allow(unused_imports)]
pub(crate) use crash_point;

// ── Yield / Sleep hooks ─────────────────────────────────────────────
//
// Thread-local flags that record yield/sleep requests from the sync
// abstraction layer.  The DST scheduler reads and clears them to decide
// how to advance the simulated thread.

#[cfg(feature = "simulation")]
use std::time::Duration;

#[cfg(feature = "simulation")]
std::thread_local! {
    static YIELD_REQUESTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SLEEP_REQUESTED: std::cell::RefCell<Option<Duration>> = const { std::cell::RefCell::new(None) };
}

/// Record that a yield was executed at a scheduling point.
#[cfg(feature = "simulation")]
#[inline]
pub fn on_yield() {
    YIELD_REQUESTED.with(|cell| cell.set(true));
}

/// Record that a sleep was executed at a scheduling point.
#[cfg(feature = "simulation")]
#[inline]
pub fn on_sleep(duration: Duration) {
    SLEEP_REQUESTED.with(|cell| {
        *cell.borrow_mut() = Some(duration);
    });
}

/// Check and clear the yield-request flag.  Returns `true` if a yield
/// was recorded since the last call.
#[cfg(feature = "simulation")]
pub fn take_yield_request() -> bool {
    YIELD_REQUESTED.with(|cell| cell.replace(false))
}

/// Check and clear the sleep-request.  Returns `Some(duration)` if a
/// sleep was recorded since the last call.
#[cfg(feature = "simulation")]
pub fn take_sleep_request() -> Option<Duration> {
    SLEEP_REQUESTED.with(|cell| cell.borrow_mut().take())
}

/// Public API: request that the current thread yield at the next
/// scheduling point (e.g. for buggify injection).
#[cfg(feature = "simulation")]
pub fn request_yield() {
    on_yield();
}

/// Public API: request that the current thread sleep for `duration`
/// (e.g. for buggify injection).
#[cfg(feature = "simulation")]
pub fn request_sleep(duration: Duration) {
    on_sleep(duration);
}

// ── Yield / Sleep hook tests ────────────────────────────────────────

#[cfg(all(test, feature = "simulation"))]
mod hook_tests {
    use super::*;

    #[test]
    fn yield_round_trip() {
        // Clear any prior state
        let _ = take_yield_request();

        assert!(!take_yield_request());
        on_yield();
        assert!(take_yield_request());
        // Second take should be false (cleared)
        assert!(!take_yield_request());
    }

    #[test]
    fn sleep_round_trip() {
        // Clear any prior state
        let _ = take_sleep_request();

        assert!(take_sleep_request().is_none());
        on_sleep(Duration::from_millis(100));
        assert_eq!(take_sleep_request(), Some(Duration::from_millis(100)));
        // Second take should be None (cleared)
        assert!(take_sleep_request().is_none());
    }

    #[test]
    fn request_yield_api() {
        let _ = take_yield_request();

        request_yield();
        assert!(take_yield_request());
    }

    #[test]
    fn request_sleep_api() {
        let _ = take_sleep_request();

        request_sleep(Duration::from_secs(1));
        assert_eq!(take_sleep_request(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn sleep_overwrites_previous() {
        let _ = take_sleep_request();

        on_sleep(Duration::from_millis(10));
        on_sleep(Duration::from_millis(20));
        assert_eq!(take_sleep_request(), Some(Duration::from_millis(20)));
    }
}
