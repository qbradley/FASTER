//! Boundary-condition record count crash scenarios.
//!
//! Exercises record counts near powers of two and other structurally
//! interesting boundaries (page boundaries, bucket thresholds). Catches
//! off-by-one bugs in page allocation, index sizing, and record layout.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Boundary record count crash scenario.
pub fn template(crash_point: CrashPoint, record_count: usize) -> ScenarioTemplate {
    let name = format!("boundary_r{record_count}_{}", crash_point.label());
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, record_count))
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
