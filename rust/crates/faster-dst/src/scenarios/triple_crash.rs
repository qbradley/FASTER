//! Triple subsystem crash scenarios.
//!
//! Schedules crash points from all three subsystems (checkpoint, compaction,
//! and recovery) simultaneously. The first crash point reached fires. Tests
//! that no combination of pending crash points corrupts state.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Triple crash: checkpoint + compaction + recovery points all armed.
pub fn template(
    ckpt_phase: usize,
    compact_phase: usize,
    recovery_phase: usize,
) -> ScenarioTemplate {
    let name = format!("triple_c{ckpt_phase}_m{compact_phase}_r{recovery_phase}");
    ScenarioTemplate::builder(name)
        .workload(|seed| CrudWorkload::new(seed, 100))
        .crash_schedule(move |_seed| {
            let ckpt = CrashPoint::ALL_CHECKPOINT[ckpt_phase % 6];
            let compact = CrashPoint::ALL_COMPACTION[compact_phase % 6];
            let recovery = CrashPoint::ALL_RECOVERY[recovery_phase % 6];
            let mut schedule = CrashSchedule::new();
            schedule.add(ckpt, CrashTrigger::Always);
            schedule.add(compact, CrashTrigger::Always);
            schedule.add(recovery, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
