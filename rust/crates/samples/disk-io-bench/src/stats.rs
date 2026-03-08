//! Latency histogram for percentile calculation.

use std::time::Duration;

/// Collects latency samples and computes percentiles.
pub struct LatencyHistogram {
    samples: Vec<u64>,
}

impl LatencyHistogram {
    pub fn new() -> Self {
        Self {
            samples: Vec::with_capacity(100_000),
        }
    }

    #[inline]
    pub fn record(&mut self, duration: Duration) {
        self.samples.push(duration.as_nanos() as u64);
    }

    pub fn merge(&mut self, other: &LatencyHistogram) {
        self.samples.extend_from_slice(&other.samples);
    }

    pub fn percentile(&mut self, pct: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.sort_unstable();
        let idx = ((pct / 100.0) * (self.samples.len() - 1) as f64).round() as usize;
        let idx = idx.min(self.samples.len() - 1);
        self.samples[idx] as f64
    }
}
