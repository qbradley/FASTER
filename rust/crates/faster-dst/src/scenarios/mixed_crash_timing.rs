//! Mixed crash-trigger timing scenarios.
//!
//! Instead of `CrashTrigger::Always`, uses `CrashTrigger::OnVisit(N)` to
//! crash on the Nth traversal of a crash point. This tests edge cases
//! where crash points are visited multiple times during a single operation
//! (e.g., multi-page flushes visiting the same checkpoint phase repeatedly).

use crate::crash::{CrashSchedule, CrashTrigger};
use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Delayed crash: crash on the Nth visit to the given crash point.
/// Visit counts > 1 test multi-pass code paths.
pub fn template(crash_point: CrashPoint, visit: u64, record_count: usize) -> ScenarioTemplate {
    let name = format!("delayed_v{visit}_{}_r{record_count}", crash_point.label());
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, record_count))
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::OnVisit(visit));
            schedule
        })
        .check_committed_recoverable()
        .check_no_phantom_reads(100_000, record_count as u64)
        .build()
}
