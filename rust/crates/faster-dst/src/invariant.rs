//! Invariant checking for post-condition verification.

use std::collections::HashMap;

use faster_core::store::{FasterKv, FasterSession, SimpleFunctions};

/// A post-condition that can be checked against a store's live state.
pub trait Invariant {
    /// Check the invariant. Returns `Ok(())` if satisfied, or an `Err`
    /// describing the first violation found.
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String>;
}

/// Verify that every key in the expected set is present with the correct value.
///
/// This is the primary recovery invariant: "all committed data is recoverable".
pub struct AllCommittedRecoverable {
    expected: HashMap<u64, u64>,
}

impl AllCommittedRecoverable {
    /// Build from a map of key → expected_value.
    pub fn new(expected: HashMap<u64, u64>) -> Self {
        Self { expected }
    }

    /// Build from a slice of (key, value) pairs.
    pub fn from_pairs(pairs: &[(u64, u64)]) -> Self {
        Self {
            expected: pairs.iter().copied().collect(),
        }
    }
}

impl Invariant for AllCommittedRecoverable {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        for (&key, &expected_value) in &self.expected {
            let actual: Option<u64> = store.read_simple(session, &key);
            match actual {
                Some(v) if v == expected_value => {}
                Some(v) => {
                    return Err(format!("key {key}: expected {expected_value}, got {v}"));
                }
                None => {
                    return Err(format!(
                        "key {key}: expected {expected_value}, got NotFound"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Verify that none of the given keys exist in the store.
///
/// Useful for checking that uncommitted writes are lost after a crash.
pub struct NoPhantomReads {
    absent_keys: Vec<u64>,
}

impl NoPhantomReads {
    /// Build from a list of keys that should NOT be present.
    pub fn new(absent_keys: Vec<u64>) -> Self {
        Self { absent_keys }
    }
}

impl Invariant for NoPhantomReads {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        for &key in &self.absent_keys {
            let actual: Option<u64> = store.read_simple(session, &key);
            if let Some(v) = actual {
                return Err(format!(
                    "phantom read: key {key} should be absent but has value {v}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::SimulationHarness;

    #[test]
    fn all_committed_passes_when_data_present() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &10u64, ());
        let _ = store.upsert(&mut s, &2u64, &20u64, ());

        let inv = AllCommittedRecoverable::from_pairs(&[(1, 10), (2, 20)]);
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    #[test]
    fn all_committed_fails_on_mismatch() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &10u64, ());

        let inv = AllCommittedRecoverable::from_pairs(&[(1, 999)]);
        assert!(inv.check(&store, &mut s).is_err());
        store.dispose_session(s);
    }

    #[test]
    fn no_phantom_reads_passes_when_absent() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();

        let inv = NoPhantomReads::new(vec![42, 99]);
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    #[test]
    fn no_phantom_reads_fails_when_present() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &42u64, &1u64, ());

        let inv = NoPhantomReads::new(vec![42]);
        assert!(inv.check(&store, &mut s).is_err());
        store.dispose_session(s);
    }
}
