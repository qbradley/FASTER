//! Fault injection configuration for deterministic simulation testing.

/// Locations in the system where crashes can be injected.
///
/// Today these serve as documentation markers; full integration requires
/// hooks inside `faster-core` (see crate-level docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrashPoint {
    /// Crash during page flush to the device.
    DuringFlush,
    /// Crash while writing checkpoint metadata.
    DuringCheckpoint,
    /// Crash during log compaction.
    DuringCompaction,
    /// Crash during recovery.
    DuringRecovery,
}

/// Configuration for fault injection in [`SimulatedDevice`](crate::SimulatedDevice).
///
/// All probabilities are in the range `[0.0, 1.0]`. A value of `0.0` means
/// the fault never fires; `1.0` means it always fires.
#[derive(Debug, Clone)]
pub struct FaultConfig {
    /// Probability that a write operation returns an I/O error.
    pub write_error_rate: f64,
    /// Probability that a read operation returns an I/O error.
    pub read_error_rate: f64,
    /// Probability that a write completes only partially (torn page).
    pub partial_write_rate: f64,
    /// If set, **all** writes fail after this many successful writes.
    pub fail_after_n_writes: Option<u64>,
    /// If set, **all** reads fail after this many successful reads.
    pub fail_after_n_reads: Option<u64>,
}

impl FaultConfig {
    /// No faults — the device behaves perfectly.
    pub fn none() -> Self {
        Self::default()
    }

    /// Builder: set the write-error probability.
    #[must_use]
    pub fn with_write_errors(mut self, rate: f64) -> Self {
        self.write_error_rate = rate;
        self
    }

    /// Builder: set the read-error probability.
    #[must_use]
    pub fn with_read_errors(mut self, rate: f64) -> Self {
        self.read_error_rate = rate;
        self
    }

    /// Builder: set the partial-write (torn page) probability.
    #[must_use]
    pub fn with_partial_writes(mut self, rate: f64) -> Self {
        self.partial_write_rate = rate;
        self
    }

    /// Builder: fail all writes after `n` successful writes.
    #[must_use]
    pub fn with_write_limit(mut self, n: u64) -> Self {
        self.fail_after_n_writes = Some(n);
        self
    }

    /// Builder: fail all reads after `n` successful reads.
    #[must_use]
    pub fn with_read_limit(mut self, n: u64) -> Self {
        self.fail_after_n_reads = Some(n);
        self
    }
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            write_error_rate: 0.0,
            read_error_rate: 0.0,
            partial_write_rate: 0.0,
            fail_after_n_writes: None,
            fail_after_n_reads: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_faults() {
        let cfg = FaultConfig::default();
        assert_eq!(cfg.write_error_rate, 0.0);
        assert_eq!(cfg.read_error_rate, 0.0);
        assert_eq!(cfg.partial_write_rate, 0.0);
        assert!(cfg.fail_after_n_writes.is_none());
        assert!(cfg.fail_after_n_reads.is_none());
    }

    #[test]
    fn builder_chaining() {
        let cfg = FaultConfig::none()
            .with_write_errors(0.1)
            .with_read_errors(0.05)
            .with_partial_writes(0.02)
            .with_write_limit(1000)
            .with_read_limit(500);

        assert_eq!(cfg.write_error_rate, 0.1);
        assert_eq!(cfg.read_error_rate, 0.05);
        assert_eq!(cfg.partial_write_rate, 0.02);
        assert_eq!(cfg.fail_after_n_writes, Some(1000));
        assert_eq!(cfg.fail_after_n_reads, Some(500));
    }

    #[test]
    fn crash_point_variants() {
        let points = [
            CrashPoint::DuringFlush,
            CrashPoint::DuringCheckpoint,
            CrashPoint::DuringCompaction,
            CrashPoint::DuringRecovery,
        ];
        // Verify Debug and Clone
        for p in &points {
            let _ = format!("{p:?}");
            let _ = *p;
        }
    }
}
