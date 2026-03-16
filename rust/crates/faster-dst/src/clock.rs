//! Deterministic simulated clock.
//!
//! [`SimulatedClock`] provides a manually-advanced time source for tests that
//! need controlled time progression without real wall-clock delays.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Default epoch offset: 2025-01-01T00:00:00Z expressed in nanoseconds since
/// the UNIX epoch.  This gives [`SimulatedClock::system_time_now`] realistic
/// wall-clock values.
const DEFAULT_EPOCH_OFFSET_NANOS: u64 = 1_735_689_600_000_000_000;

/// A deterministic clock whose time only advances when explicitly told to.
#[derive(Debug)]
pub struct SimulatedClock {
    /// Current monotonic time in nanoseconds.
    nanos: AtomicU64,
    /// Offset added to [`nanos`](Self::nanos) when computing a simulated
    /// wall-clock value.
    epoch_offset_nanos: AtomicU64,
}

impl SimulatedClock {
    /// Create a clock starting at time zero with the default epoch offset
    /// (2025-01-01T00:00:00Z).
    pub fn new() -> Self {
        Self {
            nanos: AtomicU64::new(0),
            epoch_offset_nanos: AtomicU64::new(DEFAULT_EPOCH_OFFSET_NANOS),
        }
    }

    /// Create a clock starting at the given duration.
    pub fn starting_at(time: Duration) -> Self {
        Self {
            nanos: AtomicU64::new(time.as_nanos() as u64),
            epoch_offset_nanos: AtomicU64::new(DEFAULT_EPOCH_OFFSET_NANOS),
        }
    }

    /// Current time as a [`Duration`].
    pub fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
    }

    /// Advance time by `duration`.
    pub fn advance(&self, duration: Duration) {
        self.nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Set the clock to an exact point in time.
    pub fn set(&self, time: Duration) {
        self.nanos.store(time.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Current monotonic time as raw nanoseconds.
    ///
    /// Avoids [`Duration`] construction in hot paths (e.g. I/O latency
    /// calculations inside [`SimDeviceV2`](crate::sim_device_v2::SimDeviceV2)).
    pub fn now_nanos(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }

    /// Charge virtual time for an O(n) operation.
    ///
    /// Advances the clock by `cost_ns` nanoseconds, making the operation
    /// "take time" in simulation without performing real work.
    pub fn charge(&self, cost_ns: u64) {
        if cost_ns > 0 {
            self.nanos.fetch_add(cost_ns, Ordering::Relaxed);
        }
    }

    /// Simulated monotonic time (alias for [`now`](Self::now)).
    ///
    /// In production code you would use `std::time::Instant::now()`; in DST
    /// code you call this instead.
    pub fn instant_now(&self) -> Duration {
        self.now()
    }

    /// Simulated wall-clock time as a [`Duration`] since the UNIX epoch.
    ///
    /// The value is `epoch_offset + now()` where `epoch_offset` defaults to
    /// 2025-01-01T00:00:00Z.
    pub fn system_time_now(&self) -> Duration {
        let offset = Duration::from_nanos(self.epoch_offset_nanos.load(Ordering::Relaxed));
        offset + self.now()
    }

    /// Override the epoch offset used by [`system_time_now`](Self::system_time_now).
    pub fn set_epoch_offset(&self, offset: Duration) {
        self.epoch_offset_nanos
            .store(offset.as_nanos() as u64, Ordering::Relaxed);
    }
}

impl Default for SimulatedClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_zero() {
        let clock = SimulatedClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
    }

    #[test]
    fn advance_adds_time() {
        let clock = SimulatedClock::new();
        clock.advance(Duration::from_secs(5));
        clock.advance(Duration::from_millis(500));
        assert_eq!(clock.now(), Duration::from_millis(5500));
    }

    #[test]
    fn set_overrides_time() {
        let clock = SimulatedClock::new();
        clock.advance(Duration::from_secs(100));
        clock.set(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::from_secs(1));
    }

    #[test]
    fn starting_at_custom_time() {
        let clock = SimulatedClock::starting_at(Duration::from_secs(42));
        assert_eq!(clock.now(), Duration::from_secs(42));
    }

    #[test]
    fn instant_now_equals_now() {
        let clock = SimulatedClock::new();
        clock.advance(Duration::from_secs(10));
        assert_eq!(clock.instant_now(), clock.now());
    }

    #[test]
    fn system_time_has_epoch_offset() {
        let clock = SimulatedClock::new();
        let sys = clock.system_time_now();
        // Default offset is 2025-01-01T00:00:00Z ≈ 1.735 × 10^18 ns
        assert!(sys.as_secs() >= 1_735_689_600);

        clock.advance(Duration::from_secs(5));
        assert_eq!(
            clock.system_time_now(),
            Duration::from_nanos(super::DEFAULT_EPOCH_OFFSET_NANOS) + Duration::from_secs(5),
        );
    }

    #[test]
    fn custom_epoch_offset() {
        let clock = SimulatedClock::new();
        clock.set_epoch_offset(Duration::from_secs(1_000_000));
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.system_time_now(), Duration::from_secs(1_000_001),);
    }
}
