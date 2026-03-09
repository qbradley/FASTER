//! Write-error injection combined with crash scenarios.
//!
//! Configures write limits so the device fails after N writes, then
//! also schedules a crash point. Tests the interaction between I/O
//! failures and crash recovery.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::{CrashPoint, FaultConfig};
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Write error + crash scenario.
pub fn template(
    crash_point: CrashPoint,
    record_count: usize,
    write_limit: u64,
) -> ScenarioTemplate {
    let name = format!(
        "write_error_crash_{}_r{record_count}_l{write_limit}",
        crash_point.label()
    );
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, record_count))
        .fault_config(FaultConfig::none().with_write_limit(write_limit))
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
