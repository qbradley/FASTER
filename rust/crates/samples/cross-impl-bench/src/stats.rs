//! Latency histogram for percentile calculation.
//!
//! Collects sampled latency measurements and computes percentiles via sorting.
//! Designed for low overhead: only a fraction of operations are sampled.

use std::time::Duration;

/// Collects latency samples and computes percentiles.
pub struct LatencyHistogram {
    samples: Vec<u64>, // nanoseconds
}

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            samples: Vec::with_capacity(100_000),
        }
    }

    /// Record a latency measurement.
    #[inline]
    pub fn record(&mut self, duration: Duration) {
        self.samples.push(duration.as_nanos() as u64);
    }

    /// Merge another histogram's samples into this one.
    pub fn merge(&mut self, other: &LatencyHistogram) {
        self.samples.extend_from_slice(&other.samples);
    }

    /// Compute a percentile (0.0 to 100.0). Returns 0 if no samples.
    pub fn percentile(&mut self, pct: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.sort_unstable();
        let idx = ((pct / 100.0) * (self.samples.len() - 1) as f64).round() as usize;
        let idx = idx.min(self.samples.len() - 1);
        self.samples[idx] as f64
    }

    /// Number of latency samples collected.
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.samples.len()
    }
}
