//! Crash at every checkpoint phase.
//!
//! The crash point is determined by `seed % 6`, covering all six checkpoint
//! phases across a sweep of seeds.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Standard checkpoint crash scenario: 50 records, crash at a seed-selected
/// checkpoint phase.
pub fn template() -> ScenarioTemplate {
    ScenarioTemplate::builder("checkpoint_crash")
        .workload(|seed| CrudWorkload::new(seed, 50))
        .crash_schedule(|seed| {
            let phase_idx = (seed % 6) as usize;
            let point = CrashPoint::ALL_CHECKPOINT[phase_idx];
            let mut schedule = CrashSchedule::new();
            schedule.add(point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
