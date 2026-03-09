//! Simulation hooks for deterministic testing.
//!
//! Provides two macro families:
//!
//! - [`sim_yield!`] — scheduling yield-point markers (Phase 3).
//! - [`crash_point!`] — crash-injection hooks (Phase 4).
//!
//! Without the `simulation` feature both macros compile to nothing.

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
