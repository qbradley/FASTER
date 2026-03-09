//! Crash during recovery and re-recover.
//!
//! The crash point is determined by `seed % 6`, covering all six recovery
//! phases. After the crash, a clean recovery verifies data integrity.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Standard recovery stress scenario: 50 records, crash at a seed-selected
/// recovery phase.
pub fn template() -> ScenarioTemplate {
    ScenarioTemplate::builder("recovery_stress")
        .workload(|seed| CrudWorkload::new(seed, 50))
        .crash_schedule(|seed| {
            let phase_idx = (seed % 6) as usize;
            let point = CrashPoint::ALL_RECOVERY[phase_idx];
            let mut schedule = CrashSchedule::new();
            schedule.add(point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
