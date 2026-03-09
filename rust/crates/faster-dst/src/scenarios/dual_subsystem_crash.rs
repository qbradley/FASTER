//! Dual-subsystem crash scenarios.
//!
//! Schedules crash points from two different subsystems (checkpoint +
//! recovery, or compaction + recovery). Verifies that the system handles
//! crash points across subsystem boundaries without state corruption.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Dual checkpoint + recovery crash: both subsystems have crash points
/// scheduled. Tests cross-subsystem crash safety.
pub fn template_ckpt_recovery(ckpt_phase: usize, recovery_phase: usize) -> ScenarioTemplate {
    let name = format!("dual_ckpt{}_recovery{}", ckpt_phase, recovery_phase);
    ScenarioTemplate::builder(name)
        .workload(|seed| CrudWorkload::new(seed, 75))
        .crash_schedule(move |_seed| {
            let ckpt = CrashPoint::ALL_CHECKPOINT[ckpt_phase % 6];
            let recovery = CrashPoint::ALL_RECOVERY[recovery_phase % 6];
            let mut schedule = CrashSchedule::new();
            schedule.add(ckpt, CrashTrigger::Always);
            schedule.add(recovery, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}

/// Dual compaction + recovery crash: both subsystems have crash points.
pub fn template_compact_recovery(compact_phase: usize, recovery_phase: usize) -> ScenarioTemplate {
    let name = format!("dual_compact{}_recovery{}", compact_phase, recovery_phase);
    ScenarioTemplate::builder(name)
        .workload(|seed| CrudWorkload::new(seed, 100))
        .crash_schedule(move |_seed| {
            let compact = CrashPoint::ALL_COMPACTION[compact_phase % 6];
            let recovery = CrashPoint::ALL_RECOVERY[recovery_phase % 6];
            let mut schedule = CrashSchedule::new();
            schedule.add(compact, CrashTrigger::Always);
            schedule.add(recovery, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
