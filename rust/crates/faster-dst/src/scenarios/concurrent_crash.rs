//! Concurrent checkpoint + compaction crash scenarios.
//!
//! Schedules crash points from both the checkpoint and compaction subsystems.
//! The first crash point reached fires. Tests interaction between subsystem
//! state machines when both are conceptually "in-flight" at crash time.

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Concurrent checkpoint + compaction crash: both crash points scheduled,
/// first one to fire wins.
pub fn template(ckpt_phase: usize, compact_phase: usize) -> ScenarioTemplate {
    let name = format!("concurrent_ckpt{}_compact{}", ckpt_phase, compact_phase);
    ScenarioTemplate::builder(name)
        .workload(|seed| CrudWorkload::new(seed, 100))
        .crash_schedule(move |_seed| {
            let ckpt = CrashPoint::ALL_CHECKPOINT[ckpt_phase % 6];
            let compact = CrashPoint::ALL_COMPACTION[compact_phase % 6];
            let mut schedule = CrashSchedule::new();
            schedule.add(ckpt, CrashTrigger::Always);
            schedule.add(compact, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
