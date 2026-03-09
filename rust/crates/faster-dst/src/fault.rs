//! Fault injection configuration for deterministic simulation testing.

/// Checkpoint phase for crash-point targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckpointPhase {
    /// After Prepare → InProgress transition.
    PrepareToInProgress,
    /// After index checkpoint written to disk.
    IndexWritten,
    /// After InProgress → WaitFlush transition.
    InProgressToWaitFlush,
    /// After WaitFlush → WaitCompletion transition.
    WaitFlushToWaitCompletion,
    /// After combined metadata persisted to disk.
    MetadataPersisted,
    /// After WaitCompletion → Completed transition.
    Completed,
}

/// Compaction phase for crash-point targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompactionPhase {
    /// After K1 scan completes.
    ScanComplete,
    /// After K2 record copy completes.
    CopyComplete,
    /// After K3 pointer swing completes.
    PointerSwingComplete,
    /// After epoch drain completes (between K3 and K4).
    EpochDrained,
    /// After begin-address advanced (K4a).
    BeginAddressAdvanced,
    /// After device truncation (K4b).
    TruncationComplete,
}

/// Recovery phase for crash-point targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryPhase {
    /// After checkpoint discovery enumeration.
    DiscoveryComplete,
    /// After checkpoint selected and metadata read.
    PlanSelected,
    /// After on-disk artifact validation.
    ValidationComplete,
    /// After index CRC32 verified.
    IndexCrcVerified,
    /// After index buckets loaded into memory.
    IndexBucketsLoaded,
    /// After log file validated.
    LogValidated,
}

/// Locations in the system where crashes can be injected.
///
/// Each variant maps to a `crash_point!()` label inside `faster-core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrashPoint {
    /// Crash during a checkpoint phase transition.
    Checkpoint(CheckpointPhase),
    /// Crash during a compaction phase.
    Compaction(CompactionPhase),
    /// Crash during a recovery phase.
    Recovery(RecoveryPhase),
}

impl CrashPoint {
    /// Returns the `crash_point!()` label string that this variant targets.
    pub fn label(&self) -> &'static str {
        match self {
            CrashPoint::Checkpoint(p) => match p {
                CheckpointPhase::PrepareToInProgress => "checkpoint_prepare_to_in_progress",
                CheckpointPhase::IndexWritten => "checkpoint_index_written",
                CheckpointPhase::InProgressToWaitFlush => "checkpoint_in_progress_to_wait_flush",
                CheckpointPhase::WaitFlushToWaitCompletion => {
                    "checkpoint_wait_flush_to_wait_completion"
                }
                CheckpointPhase::MetadataPersisted => "checkpoint_metadata_persisted",
                CheckpointPhase::Completed => "checkpoint_completed",
            },
            CrashPoint::Compaction(p) => match p {
                CompactionPhase::ScanComplete => "compaction_scan_complete",
                CompactionPhase::CopyComplete => "compaction_copy_complete",
                CompactionPhase::PointerSwingComplete => "compaction_pointer_swing_complete",
                CompactionPhase::EpochDrained => "compaction_epoch_drained",
                CompactionPhase::BeginAddressAdvanced => "compaction_begin_address_advanced",
                CompactionPhase::TruncationComplete => "compaction_truncation_complete",
            },
            CrashPoint::Recovery(p) => match p {
                RecoveryPhase::DiscoveryComplete => "recovery_discovery_complete",
                RecoveryPhase::PlanSelected => "recovery_plan_selected",
                RecoveryPhase::ValidationComplete => "recovery_validation_complete",
                RecoveryPhase::IndexCrcVerified => "recovery_index_crc_verified",
                RecoveryPhase::IndexBucketsLoaded => "recovery_index_buckets_loaded",
                RecoveryPhase::LogValidated => "recovery_log_validated",
            },
        }
    }

    /// All 6 checkpoint crash points.
    pub const ALL_CHECKPOINT: [CrashPoint; 6] = [
        CrashPoint::Checkpoint(CheckpointPhase::PrepareToInProgress),
        CrashPoint::Checkpoint(CheckpointPhase::IndexWritten),
        CrashPoint::Checkpoint(CheckpointPhase::InProgressToWaitFlush),
        CrashPoint::Checkpoint(CheckpointPhase::WaitFlushToWaitCompletion),
        CrashPoint::Checkpoint(CheckpointPhase::MetadataPersisted),
        CrashPoint::Checkpoint(CheckpointPhase::Completed),
    ];

    /// All 6 compaction crash points.
    pub const ALL_COMPACTION: [CrashPoint; 6] = [
        CrashPoint::Compaction(CompactionPhase::ScanComplete),
        CrashPoint::Compaction(CompactionPhase::CopyComplete),
        CrashPoint::Compaction(CompactionPhase::PointerSwingComplete),
        CrashPoint::Compaction(CompactionPhase::EpochDrained),
        CrashPoint::Compaction(CompactionPhase::BeginAddressAdvanced),
        CrashPoint::Compaction(CompactionPhase::TruncationComplete),
    ];

    /// All 6 recovery crash points.
    pub const ALL_RECOVERY: [CrashPoint; 6] = [
        CrashPoint::Recovery(RecoveryPhase::DiscoveryComplete),
        CrashPoint::Recovery(RecoveryPhase::PlanSelected),
        CrashPoint::Recovery(RecoveryPhase::ValidationComplete),
        CrashPoint::Recovery(RecoveryPhase::IndexCrcVerified),
        CrashPoint::Recovery(RecoveryPhase::IndexBucketsLoaded),
        CrashPoint::Recovery(RecoveryPhase::LogValidated),
    ];

    /// All 18 crash points.
    pub fn all() -> Vec<CrashPoint> {
        let mut v = Vec::with_capacity(18);
        v.extend_from_slice(&Self::ALL_CHECKPOINT);
        v.extend_from_slice(&Self::ALL_COMPACTION);
        v.extend_from_slice(&Self::ALL_RECOVERY);
        v
    }
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
        // Verify all 18 crash points have labels and implement Debug + Clone
        let all = CrashPoint::all();
        assert_eq!(all.len(), 18);
        for p in &all {
            let label = p.label();
            assert!(!label.is_empty());
            let _ = format!("{p:?}");
            let _ = *p;
        }
    }

    #[test]
    fn crash_point_label_uniqueness() {
        let all = CrashPoint::all();
        let labels: std::collections::HashSet<&str> = all.iter().map(|p| p.label()).collect();
        assert_eq!(labels.len(), 18, "all crash point labels must be unique");
    }
}
