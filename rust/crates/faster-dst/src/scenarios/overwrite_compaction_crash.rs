//! Overwrite-then-compaction-crash scenarios.
//!
//! Writes keys, overwrites them all with new values, then crashes during
//! compaction. Tests that the compaction subsystem correctly handles
//! record chains containing stale versions.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::runtime::SimulationRuntime;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Overwrite + compaction crash scenario.
pub fn template(crash_point: CrashPoint, count: usize) -> ScenarioTemplate {
    let name = format!("overwrite_compact_{count}_{}", crash_point.label());
    ScenarioTemplate::builder(name)
        .workload(move |seed| {
            let mut rt = SimulationRuntime::new(seed);
            let initial = rt.sequential_kv_pairs(count);
            let overwrites: Vec<(u64, u64)> =
                (0..count).map(|i| (i as u64, rt.child_seed())).collect();
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
