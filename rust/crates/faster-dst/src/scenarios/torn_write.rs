//! Partial write injection and CRC detection.
//!
//! Crashes during checkpoint to simulate torn-write conditions. The
//! `FaultConfig` records the intended partial write rate for documentation;
//! file-backed stores rely on crash injection to approximate torn pages.
//! After recovery, CRC-32C validation (which runs inside `recover()`)
//! ensures corrupt pages are detected.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::{CrashPoint, FaultConfig};
use crate::invariant::{AllCommittedRecoverable, ValidPageChecksums};
use crate::scenario::ScenarioTemplate;
use crate::workload::{CrudWorkload, Workload};

/// Standard torn write scenario: 50 records, crash at a seed-selected
/// checkpoint phase with partial write config annotated.
pub fn template() -> ScenarioTemplate {
    ScenarioTemplate::builder("torn_write")
        .workload(|seed| CrudWorkload::new(seed, 50))
        .fault_config(FaultConfig::none().with_partial_writes(0.1))
        .crash_schedule(|seed| {
            let phase_idx = (seed % 6) as usize;
            let point = CrashPoint::ALL_CHECKPOINT[phase_idx];
            let mut schedule = CrashSchedule::new();
            schedule.add(point, CrashTrigger::Always);
            schedule
        })
        .invariant(|wl| Box::new(AllCommittedRecoverable::new(wl.expected_state())))
        .invariant(|_wl| Box::new(ValidPageChecksums::new()))
        .build()
}
