//! Sparse (random) key distribution crash scenarios.
//!
//! Uses randomly-generated keys instead of sequential `[0, N)` to create
//! varied hash distribution patterns. Tests that recovery correctly handles
//! non-contiguous key spaces with potential bucket overflow at unpredictable
//! locations.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::runtime::SimulationRuntime;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Sparse key scenario: random key distribution with crash injection.
pub fn template(crash_point: CrashPoint, record_count: usize) -> ScenarioTemplate {
    let name = format!("sparse_key_{record_count}_{}", crash_point.label());
    ScenarioTemplate::builder(name)
        .workload(move |seed| {
            let mut rt = SimulationRuntime::new(seed);
            let records = rt.random_kv_pairs(record_count);
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

/// Sparse key scenario without crash — pure recovery integrity.
pub fn template_no_crash(record_count: usize) -> ScenarioTemplate {
    let name = format!("sparse_key_recovery_{record_count}");
    ScenarioTemplate::builder(name)
        .workload(move |seed| {
            let mut rt = SimulationRuntime::new(seed);
            let records = rt.random_kv_pairs(record_count);
            CrudWorkload::from_records(records)
        })
        .check_committed_recoverable()
        .build()
}
