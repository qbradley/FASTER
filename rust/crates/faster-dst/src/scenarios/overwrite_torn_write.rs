//! Overwrite + torn-write + crash scenarios.
//!
//! Combines the overwrite workload pattern with partial write fault injection
//! and crash injection. Tests that CRC validation and recovery correctly
//! handle torn pages containing overwritten records.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::{CrashPoint, FaultConfig};
use crate::invariant::{AllCommittedRecoverable, ValidPageChecksums};
use crate::runtime::SimulationRuntime;
use crate::scenario::ScenarioTemplate;
use crate::workload::{CrudWorkload, Workload};

/// Overwrite + torn write + crash scenario.
pub fn template(partial_rate: f64, crash_point: CrashPoint, count: usize) -> ScenarioTemplate {
    let rate_pct = (partial_rate * 100.0) as u32;
    let name = format!("overwrite_torn_r{rate_pct}_{count}_{}", crash_point.label());
    ScenarioTemplate::builder(name)
        .workload(move |seed| {
            let mut rt = SimulationRuntime::new(seed);
            let initial = rt.sequential_kv_pairs(count);
            let overwrites: Vec<(u64, u64)> =
                (0..count).map(|i| (i as u64, rt.child_seed())).collect();
            let records: Vec<(u64, u64)> = initial.into_iter().chain(overwrites).collect();
            CrudWorkload::from_records(records)
        })
        .fault_config(FaultConfig::none().with_partial_writes(partial_rate))
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::Always);
            schedule
        })
        .invariant(|wl| Box::new(AllCommittedRecoverable::new(wl.expected_state())))
        .invariant(|_wl| Box::new(ValidPageChecksums::new()))
        .build()
}
