//! Overwrite-then-crash recovery scenarios.
//!
//! Writes keys with initial values, then overwrites the same keys with
//! new values. After checkpoint and crash recovery, the latest values
//! must be present. Catches bugs where the record chain resolves to a
//! stale version after recovery.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::runtime::SimulationRuntime;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Overwrite recovery scenario: write `count` keys, overwrite them all,
/// then crash at the given checkpoint phase. Recovery must return the
/// overwritten (latest) values.
pub fn template(crash_point: CrashPoint, count: usize) -> ScenarioTemplate {
    let name = format!("overwrite_crash_{}", crash_point.label());
    ScenarioTemplate::builder(name)
        .workload(move |seed| {
            let mut rt = SimulationRuntime::new(seed);
            // First pass: sequential keys with initial values
            let initial = rt.sequential_kv_pairs(count);
            // Second pass: same keys, new values (overwrite)
            let overwrites: Vec<(u64, u64)> = (0..count)
                .map(|i| {
                    let value = rt.child_seed();
                    (i as u64, value)
                })
                .collect();
            let records: Vec<(u64, u64)> = initial.into_iter().chain(overwrites).collect();
            CrudWorkload::from_records(records)
        })
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
