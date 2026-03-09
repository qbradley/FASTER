//! High-volume sequential CRUD stress test.
//!
//! No crash injection — exercises basic data integrity at scale by writing
//! many records, checkpointing, recovering, and verifying all committed data.

use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Standard CRUD stress scenario: 200 records per seed, no crashes.
pub fn template() -> ScenarioTemplate {
    ScenarioTemplate::builder("crud_stress")
        .workload(|seed| CrudWorkload::new(seed, 200))
        .check_committed_recoverable()
        .build()
}
