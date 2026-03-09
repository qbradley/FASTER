//! Large record count recovery scenarios.
//!
//! Exercises checkpoint/recovery with high record counts (500–2000).
//! At these volumes the log spans multiple pages, stressing page-boundary
//! handling, CRC validation, and index rebuild during recovery.

use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Large record recovery: `count` records, no crash injection.
/// Pure recovery correctness at scale.
pub fn template(count: usize) -> ScenarioTemplate {
    let name = format!("large_record_recovery_{count}");
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, count))
        .check_committed_recoverable()
        .build()
}
