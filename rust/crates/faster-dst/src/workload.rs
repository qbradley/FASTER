//! Workload abstractions for deterministic simulation testing.

use std::collections::HashMap;

use faster_core::store::{FasterKv, FasterSession, SimpleFunctions};

use crate::runtime::SimulationRuntime;

/// A deterministic workload whose execution is fully determined by its seed.
pub trait Workload {
    /// Run the workload against the store.
    fn execute(
        &mut self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    );

    /// Return the expected key → value mapping after execution.
    ///
    /// Duplicate keys are resolved to the last written value.
    fn expected_state(&self) -> HashMap<u64, u64>;
}

/// A simple workload that upserts a sequence of (key, value) pairs.
pub struct CrudWorkload {
    records: Vec<(u64, u64)>,
}

impl CrudWorkload {
    /// Create from a seed and a record count.
    ///
    /// Keys are sequential `[0, count)`; values are deterministic per seed.
    pub fn new(seed: u64, count: usize) -> Self {
        let mut rt = SimulationRuntime::new(seed);
        let records = rt.sequential_kv_pairs(count);
        Self { records }
    }

    /// Create from an explicit list of records.
    pub fn from_records(records: Vec<(u64, u64)>) -> Self {
        Self { records }
    }

    /// The records that will be (or were) written.
    pub fn records(&self) -> &[(u64, u64)] {
        &self.records
    }
}

impl Workload for CrudWorkload {
    fn execute(
        &mut self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) {
        for &(key, value) in &self.records {
            let status = store.upsert(session, &key, &value, ());
            assert!(
                status.is_success(),
                "upsert({key}, {value}) failed: {status:?}"
            );
        }
    }

    fn expected_state(&self) -> HashMap<u64, u64> {
        self.records.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_workload_deterministic() {
        let w1 = CrudWorkload::new(42, 10);
        let w2 = CrudWorkload::new(42, 10);
        assert_eq!(w1.records(), w2.records());
    }

    #[test]
    fn expected_state_deduplicates() {
        let wl = CrudWorkload::from_records(vec![(1, 10), (2, 20), (1, 30)]);
        let state = wl.expected_state();
        assert_eq!(state.get(&1), Some(&30));
        assert_eq!(state.get(&2), Some(&20));
    }
}
