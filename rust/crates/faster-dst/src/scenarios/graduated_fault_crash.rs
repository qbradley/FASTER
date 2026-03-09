//! Graduated fault injection crash scenarios.
//!
//! Combines multiple fault types (write errors + partial writes) at varying
//! intensity levels with crash injection. Tests that recovery handles the
//! compound effect of multiple simultaneous fault modes.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::{CrashPoint, FaultConfig};
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Graduated fault scenario: multiple fault types + crash.
pub fn template(
    write_error_rate: f64,
    partial_write_rate: f64,
    crash_point: CrashPoint,
    record_count: usize,
) -> ScenarioTemplate {
    let we_pct = (write_error_rate * 100.0) as u32;
    let pw_pct = (partial_write_rate * 100.0) as u32;
    let name = format!(
        "graduated_we{we_pct}_pw{pw_pct}_{}_r{record_count}",
        crash_point.label()
    );
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, record_count))
        .fault_config(
            FaultConfig::none()
                .with_write_errors(write_error_rate)
                .with_partial_writes(partial_write_rate),
        )
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}

/// Graduated fault scenario without crash — pure fault tolerance.
pub fn template_no_crash(
    write_error_rate: f64,
    partial_write_rate: f64,
    record_count: usize,
) -> ScenarioTemplate {
    let we_pct = (write_error_rate * 100.0) as u32;
    let pw_pct = (partial_write_rate * 100.0) as u32;
    let name = format!("graduated_no_crash_we{we_pct}_pw{pw_pct}_r{record_count}");
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, record_count))
        .fault_config(
            FaultConfig::none()
                .with_write_errors(write_error_rate)
                .with_partial_writes(partial_write_rate),
        )
        .check_committed_recoverable()
        .build()
}
