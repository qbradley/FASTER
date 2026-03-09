//! Write-error injection recovery scenarios.
//!
//! Configures the fault injector to fail writes after a set number of
//! successful operations, then verifies recovery of data written before
//! the fault threshold. Tests the boundary between committed and
//! uncommitted data under I/O failures.

use crate::fault::FaultConfig;
use crate::scenario::ScenarioTemplate;
use crate::workload::CrudWorkload;

/// Write-error scenario: writes `record_count` records with the device
/// configured to fail after `write_limit` writes. Data committed before
/// the limit must be recoverable.
pub fn template(record_count: usize, write_limit: u64) -> ScenarioTemplate {
    let name = format!("write_error_r{record_count}_limit{write_limit}");
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, record_count))
        .fault_config(FaultConfig::none().with_write_limit(write_limit))
        .check_committed_recoverable()
        .build()
}
