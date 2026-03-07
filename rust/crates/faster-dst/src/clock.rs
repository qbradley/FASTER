//! Deterministic simulated clock.
//!
//! [`SimulatedClock`] provides a manually-advanced time source for tests that
//! need controlled time progression without real wall-clock delays.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A deterministic clock whose time only advances when explicitly told to.
#[derive(Debug)]
pub struct SimulatedClock {
    /// Current time in nanoseconds.
    nanos: AtomicU64,
}

impl SimulatedClock {
    /// Create a clock starting at time zero.
    pub fn new() -> Self {
        Self {
            nanos: AtomicU64::new(0),
        }
    }

    /// Create a clock starting at the given duration.
    pub fn starting_at(time: Duration) -> Self {
        Self {
            nanos: AtomicU64::new(time.as_nanos() as u64),
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
}
