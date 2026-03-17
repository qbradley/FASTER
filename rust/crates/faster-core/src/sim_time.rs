//! Deterministic virtual time for simulation builds.
//!
//! [`SimInstant`] is a drop-in replacement for [`std::time::Instant`] that reads
//! from a shared [`AtomicU64`] clock instead of the OS monotonic clock.  The
//! clock is installed per-thread via [`install_sim_clock`] and shared across
//! threads through an `Arc<AtomicU64>` (wait-free reads, zero contention).
//!
//! Time only advances when the simulation harness explicitly bumps the clock,
//! making all elapsed-time checks fully deterministic.

use std::cell::RefCell;
use std::fmt;
use std::ops::{Add, Sub};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ── Thread-local clock reference ────────────────────────────────────

std::thread_local! {
    static SIM_CLOCK: RefCell<Option<Arc<AtomicU64>>> = const { RefCell::new(None) };
}

/// Install a shared simulation clock for the current thread.
///
/// All subsequent [`SimInstant::now()`] calls on this thread will read from
/// `clock`.  The clock stores nanoseconds as a plain `u64`.
pub fn install_sim_clock(clock: Arc<AtomicU64>) {
    SIM_CLOCK.with(|cell| {
        *cell.borrow_mut() = Some(clock);
    });
}

/// Remove the simulation clock from the current thread.
///
/// After removal, [`SimInstant::now()`] returns time-zero.
pub fn remove_sim_clock() {
    SIM_CLOCK.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Execute `f` with a reference to the installed clock (if any).
pub fn with_sim_clock<R>(f: impl FnOnce(&Arc<AtomicU64>) -> R) -> Option<R> {
    SIM_CLOCK.with(|cell| cell.borrow().as_ref().map(f))
}

/// Read the current virtual time in nanoseconds.
/// Returns 0 if no clock is installed.
#[inline]
fn read_clock_nanos() -> u64 {
    SIM_CLOCK.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |clock| clock.load(Ordering::Relaxed))
    })
}

// ── SimInstant ──────────────────────────────────────────────────────

/// Drop-in replacement for [`std::time::Instant`] under `simulation` builds.
///
/// Captures virtual nanoseconds at construction; arithmetic and ordering
/// behave identically to the standard `Instant`.
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct SimInstant {
    nanos: u64,
}

impl SimInstant {
    /// Capture the current virtual time.
    pub fn now() -> Self {
        Self {
            nanos: read_clock_nanos(),
        }
    }

    /// Virtual duration elapsed since this instant was created.
    pub fn elapsed(&self) -> Duration {
        let current = read_clock_nanos();
        Duration::from_nanos(current.saturating_sub(self.nanos))
    }

    /// Duration from `earlier` to `self`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `earlier` is later than `self`.
    pub fn duration_since(&self, earlier: SimInstant) -> Duration {
        self.checked_duration_since(earlier)
            .expect("SimInstant::duration_since: earlier is later than self")
    }

    /// Checked duration from `earlier` to `self`.
    pub fn checked_duration_since(&self, earlier: SimInstant) -> Option<Duration> {
        self.nanos
            .checked_sub(earlier.nanos)
            .map(Duration::from_nanos)
    }

    /// Saturating duration from `earlier` to `self`.
    pub fn saturating_duration_since(&self, earlier: SimInstant) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(earlier.nanos))
    }

    /// Checked addition.
    pub fn checked_add(&self, duration: Duration) -> Option<SimInstant> {
        let nanos = duration.as_nanos();
        if nanos > u64::MAX as u128 {
            return None;
        }
        self.nanos
            .checked_add(nanos as u64)
            .map(|n| SimInstant { nanos: n })
    }

    /// Checked subtraction.
    pub fn checked_sub(&self, duration: Duration) -> Option<SimInstant> {
        let nanos = duration.as_nanos();
        if nanos > u64::MAX as u128 {
            return None;
        }
        self.nanos
            .checked_sub(nanos as u64)
            .map(|n| SimInstant { nanos: n })
    }
}

impl Add<Duration> for SimInstant {
    type Output = SimInstant;

    fn add(self, rhs: Duration) -> SimInstant {
        self.checked_add(rhs).expect("SimInstant::add overflow")
    }
}

impl Sub<Duration> for SimInstant {
    type Output = SimInstant;

    fn sub(self, rhs: Duration) -> SimInstant {
        self.checked_sub(rhs).expect("SimInstant::sub underflow")
    }
}

impl Sub<SimInstant> for SimInstant {
    type Output = Duration;

    fn sub(self, rhs: SimInstant) -> Duration {
        self.duration_since(rhs)
    }
}

impl PartialOrd for SimInstant {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SimInstant {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.nanos.cmp(&other.nanos)
    }
}

impl fmt::Debug for SimInstant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SimInstant({}ns)", self.nanos)
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_clock(nanos: u64) -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(nanos))
    }

    #[test]
    fn now_without_clock_returns_zero() {
        remove_sim_clock();
        let t = SimInstant::now();
        assert_eq!(t.nanos, 0);
    }

    #[test]
    fn install_and_read() {
        let clock = make_clock(1_000_000); // 1ms
        install_sim_clock(clock.clone());

        let t = SimInstant::now();
        assert_eq!(t.nanos, 1_000_000);

        remove_sim_clock();
    }

    #[test]
    fn elapsed_tracks_clock_advance() {
        let clock = make_clock(0);
        install_sim_clock(clock.clone());

        let t = SimInstant::now();
        assert_eq!(t.elapsed(), Duration::ZERO);

        clock.store(5_000_000_000, Ordering::Relaxed); // 5s
        assert_eq!(t.elapsed(), Duration::from_secs(5));

        remove_sim_clock();
    }

    #[test]
    fn add_sub_duration() {
        let a = SimInstant { nanos: 1_000_000 };
        let b = a + Duration::from_millis(5);
        assert_eq!(b.nanos, 6_000_000);

        let c = b - Duration::from_millis(3);
        assert_eq!(c.nanos, 3_000_000);
    }

    #[test]
    fn sub_siminstant_returns_duration() {
        let a = SimInstant {
            nanos: 10_000_000_000,
        };
        let b = SimInstant {
            nanos: 3_000_000_000,
        };
        assert_eq!(a - b, Duration::from_secs(7));
    }

    #[test]
    fn ordering() {
        let a = SimInstant { nanos: 100 };
        let b = SimInstant { nanos: 200 };
        assert!(a < b);
        assert!(b > a);
        assert!(a <= a);
        assert_eq!(a, a);
    }

    #[test]
    fn with_sim_clock_accessor() {
        let clock = make_clock(42);
        install_sim_clock(clock.clone());

        let val = with_sim_clock(|c| c.load(Ordering::Relaxed));
        assert_eq!(val, Some(42));

        remove_sim_clock();
        let val = with_sim_clock(|c| c.load(Ordering::Relaxed));
        assert_eq!(val, None);
    }

    #[test]
    fn shared_across_threads() {
        let clock = make_clock(0);
        let clock2 = clock.clone();

        install_sim_clock(clock.clone());
        let t0 = SimInstant::now();

        let handle = std::thread::spawn(move || {
            install_sim_clock(clock2);
            let t = SimInstant::now();
            assert_eq!(t.nanos, 0);
            remove_sim_clock();
        });
        handle.join().unwrap();

        // Advance clock after thread finished — t0.elapsed() still works
        clock.store(1_000_000_000, Ordering::Relaxed);
        assert_eq!(t0.elapsed(), Duration::from_secs(1));

        remove_sim_clock();
    }

    #[test]
    fn checked_add_overflow() {
        let t = SimInstant { nanos: u64::MAX };
        assert!(t.checked_add(Duration::from_nanos(1)).is_none());
    }

    #[test]
    fn checked_sub_underflow() {
        let t = SimInstant { nanos: 0 };
        assert!(t.checked_sub(Duration::from_nanos(1)).is_none());
    }

    #[test]
    fn debug_format() {
        let t = SimInstant { nanos: 12345 };
        assert_eq!(format!("{t:?}"), "SimInstant(12345ns)");
    }
}
