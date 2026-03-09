//! Torn write scenarios with varied fault injection rates.
//!
//! Exercises partial-write (torn page) simulation at different
//! probabilities, combined with specific checkpoint crash points.
//! Verifies CRC-based corruption detection at varying fault densities.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::{CrashPoint, FaultConfig};
use crate::invariant::{AllCommittedRecoverable, ValidPageChecksums};
use crate::scenario::ScenarioTemplate;
use crate::workload::{CrudWorkload, Workload};

/// Torn write with a specific partial-write rate and crash point.
pub fn template(partial_rate: f64, crash_point: CrashPoint) -> ScenarioTemplate {
    let rate_pct = (partial_rate * 100.0) as u32;
    let name = format!("torn_write_rate{rate_pct}_{}", crash_point.label());
    ScenarioTemplate::builder(name)
        .workload(|seed| CrudWorkload::new(seed, 50))
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

/// Torn write with no crash injection — pure fault tolerance test.
pub fn template_no_crash(partial_rate: f64) -> ScenarioTemplate {
    let rate_pct = (partial_rate * 100.0) as u32;
    let name = format!("torn_write_no_crash_rate{rate_pct}");
    ScenarioTemplate::builder(name)
        .workload(|seed| CrudWorkload::new(seed, 50))
        .fault_config(FaultConfig::none().with_partial_writes(partial_rate))
        .invariant(|wl| Box::new(AllCommittedRecoverable::new(wl.expected_state())))
        .invariant(|_wl| Box::new(ValidPageChecksums::new()))
        .build()
}
