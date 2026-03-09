//! Crash at every compaction phase.
//!
//! The crash point is determined by `seed % 6`, covering all six compaction
//! phases. Note: compaction crash points fire only if the recovery code
//! path triggers compaction-related logic. Otherwise the scenario acts as
//! a no-crash recovery test.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Standard compaction crash scenario: 100 records, crash at a seed-selected
/// compaction phase.
pub fn template() -> ScenarioTemplate {
    ScenarioTemplate::builder("compaction_crash")
        .workload(|seed| CrudWorkload::new(seed, 100))
        .crash_schedule(|seed| {
            let phase_idx = (seed % 6) as usize;
            let point = CrashPoint::ALL_COMPACTION[phase_idx];
            let mut schedule = CrashSchedule::new();
            schedule.add(point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
