//! Wall-clock watchdog for the DstRunner.
//!
//! Monitors progress (via [`StatsCollector`]) and a total wall-clock
//! budget.  Sets a shared abort flag if either limit is breached so
//! that writer and scheduler threads can exit gracefully.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Outcome of a watchdog run.
#[derive(Clone, Debug)]
pub enum WatchdogOutcome {
    /// All threads completed before any limit was hit.
    AllClear,
    /// No progress for longer than `max_stall_wall`.
    Stall { info: String },
    /// Total wall-clock budget exceeded.
    Timeout { info: String },
}

/// Configuration for the watchdog thread.
pub struct Watchdog {
    /// Maximum wall-clock time without any new ops.
    max_stall_wall: Duration,
    /// Total wall-clock budget for the entire scenario run.
    max_total_wall: Duration,
    /// Shared abort flag — set to `true` to tell all threads to stop.
    abort: Arc<AtomicBool>,
    /// Poll interval — how often the watchdog checks progress.
    poll_interval: Duration,
}

impl Watchdog {
    /// Create a watchdog.
    ///
    /// * `max_stall_wall` – abort if no op completes for this wall-clock duration.
    /// * `max_total_wall` – abort if the scenario exceeds this wall-clock duration.
    /// * `abort` – shared flag; watchdog sets it on violation.
    pub fn new(
        max_stall_wall: Duration,
        max_total_wall: Duration,
        abort: Arc<AtomicBool>,
    ) -> Self {
        Self {
            max_stall_wall,
            max_total_wall,
            abort,
            poll_interval: Duration::from_millis(50),
        }
    }

    /// Override the default 50 ms poll interval (useful for tests).
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Blocking loop that monitors progress.
    ///
    /// `progress` is an `AtomicU64` that writers increment on every
    /// successful op.  The watchdog reads it periodically to detect stalls.
    ///
    /// Returns when the abort flag is already set (by another thread) or
    /// when the watchdog itself triggers an abort.
    pub fn run(&self, progress: &AtomicU64) -> WatchdogOutcome {
        let start = Instant::now();
        let mut last_progress = progress.load(Ordering::Relaxed);
        let mut last_progress_time = Instant::now();

        loop {
            std::thread::sleep(self.poll_interval);

            // Already aborted by another thread (e.g. all writers finished).
            if self.abort.load(Ordering::Relaxed) {
                return WatchdogOutcome::AllClear;
            }

            let elapsed = start.elapsed();
            if elapsed >= self.max_total_wall {
                self.abort.store(true, Ordering::Relaxed);
                return WatchdogOutcome::Timeout {
                    info: format!(
                        "wall-clock timeout after {:.2}s (budget {:.2}s)",
                        elapsed.as_secs_f64(),
                        self.max_total_wall.as_secs_f64(),
                    ),
                };
            }

            let current = progress.load(Ordering::Relaxed);
            if current != last_progress {
                last_progress = current;
                last_progress_time = Instant::now();
            } else {
                let stall = last_progress_time.elapsed();
                if stall >= self.max_stall_wall {
                    self.abort.store(true, Ordering::Relaxed);
                    return WatchdogOutcome::Stall {
                        info: format!(
                            "no progress for {:.2}s (limit {:.2}s), ops so far: {}",
                            stall.as_secs_f64(),
                            self.max_stall_wall.as_secs_f64(),
                            current,
                        ),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_detects_stall() {
        let abort = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));

        let wd = Watchdog::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            Arc::clone(&abort),
        )
        .with_poll_interval(Duration::from_millis(20));

        let outcome = wd.run(&progress);
        assert!(abort.load(Ordering::Relaxed));
        match outcome {
            WatchdogOutcome::Stall { .. } => {}
            other => panic!("expected Stall, got {:?}", other),
        }
    }

    #[test]
    fn watchdog_clears_when_aborted_externally() {
        let abort = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));

        let abort2 = Arc::clone(&abort);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            abort2.store(true, Ordering::Relaxed);
        });

        let wd = Watchdog::new(
            Duration::from_secs(10),
            Duration::from_secs(10),
            Arc::clone(&abort),
        )
        .with_poll_interval(Duration::from_millis(20));

        let outcome = wd.run(&progress);
        handle.join().unwrap();

        match outcome {
            WatchdogOutcome::AllClear => {}
            other => panic!("expected AllClear, got {:?}", other),
        }
    }

    #[test]
    fn watchdog_detects_timeout() {
        let abort = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));

        // Stall budget is generous, but total budget is very short.
        let wd = Watchdog::new(
            Duration::from_secs(60),
            Duration::from_millis(120),
            Arc::clone(&abort),
        )
        .with_poll_interval(Duration::from_millis(20));

        // Continuously increment progress so stall doesn't trigger first.
        let abort2 = Arc::clone(&abort);
        let progress2 = Arc::clone(&progress);
        let ticker = std::thread::spawn(move || {
            while !abort2.load(Ordering::Relaxed) {
                progress2.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let outcome = wd.run(&progress);
        ticker.join().unwrap();

        assert!(abort.load(Ordering::Relaxed));
        match outcome {
            WatchdogOutcome::Timeout { .. } => {}
            other => panic!("expected Timeout, got {:?}", other),
        }
    }
}
