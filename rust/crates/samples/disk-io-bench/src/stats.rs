//! Latency histogram and statistical aggregation utilities.

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

/// Statistical summary across multiple iterations.
#[derive(Debug, Clone)]
pub struct IterationSummary {
    pub mean: f64,
    pub stddev: f64,
    pub ci95_lo: f64,
    pub ci95_hi: f64,
    #[allow(dead_code)]
    pub values: Vec<f64>,
    pub high_variance: bool,
}

impl IterationSummary {
    /// Compute summary statistics from a set of measurements.
    /// Flags high_variance when stddev > 10% of mean.
    pub fn from_values(values: &[f64]) -> Self {
        let n = values.len() as f64;
        if values.is_empty() {
            return Self {
                mean: 0.0,
                stddev: 0.0,
                ci95_lo: 0.0,
                ci95_hi: 0.0,
                values: Vec::new(),
                high_variance: false,
            };
        }
        let mean = values.iter().sum::<f64>() / n;
        let variance = if values.len() > 1 {
            values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        let stddev = variance.sqrt();
        // 95% CI using t-distribution critical values for small n.
        let t_value = match values.len() {
            1 => 0.0,
            2 => 12.706,
            3 => 4.303,
            4 => 3.182,
            5 => 2.776,
            6 => 2.571,
            7 => 2.447,
            8 => 2.365,
            9 => 2.306,
            10 => 2.262,
            _ => 1.96,
        };
        let margin = t_value * stddev / n.sqrt();
        let high_variance = mean > 0.0 && stddev / mean > 0.10;
        Self {
            mean,
            stddev,
            ci95_lo: mean - margin,
            ci95_hi: mean + margin,
            values: values.to_vec(),
            high_variance,
        }
    }
}
