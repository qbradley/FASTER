//! High-density hash table crash scenarios.
//!
//! Writes enough records to force hash bucket overflow chains (500+
//! records into a 1024-bucket index). Crashes at each subsystem phase
//! to verify structural integrity of the hash index after recovery.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// High-density crash scenario: `record_count` records with crash at
/// the specified point. Tests hash-bucket overflow chain integrity.
pub fn template(crash_point: CrashPoint, record_count: usize) -> ScenarioTemplate {
    let name = format!("high_density_{}_{record_count}", crash_point.label());
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
