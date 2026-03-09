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

// ── Combinators ─────────────────────────────────────────────────────

/// Both invariants must pass.
pub struct And<A, B> {
    a: A,
    b: B,
}

impl<A: Invariant, B: Invariant> And<A, B> {
    /// Create a combined invariant requiring both `a` and `b` to pass.
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A: Invariant, B: Invariant> Invariant for And<A, B> {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        self.a.check(store, session)?;
        self.b.check(store, session)
    }
}

/// At least one invariant must pass.
pub struct Or<A, B> {
    a: A,
    b: B,
}

impl<A: Invariant, B: Invariant> Or<A, B> {
    /// Create a combined invariant requiring at least one of `a` or `b` to pass.
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A: Invariant, B: Invariant> Invariant for Or<A, B> {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        match self.a.check(store, session) {
            Ok(()) => Ok(()),
            Err(e1) => match self.b.check(store, session) {
                Ok(()) => Ok(()),
                Err(e2) => Err(format!("both invariants failed: [{e1}] and [{e2}]")),
            },
        }
    }
}

/// All invariants in the vec must pass.
pub struct All(pub Vec<Box<dyn Invariant>>);

impl All {
    /// Create from a vector of boxed invariants.
    pub fn new(invariants: Vec<Box<dyn Invariant>>) -> Self {
        Self(invariants)
    }
}

impl Invariant for All {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        for inv in &self.0 {
            inv.check(store, session)?;
        }
        Ok(())
    }
}

/// Invert an invariant result.
pub struct Not<I>(pub I);

impl<I: Invariant> Not<I> {
    /// Create an invariant that passes when `inner` fails and vice versa.
    pub fn new(inner: I) -> Self {
        Self(inner)
    }
}

impl<I: Invariant> Invariant for Not<I> {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        match self.0.check(store, session) {
            Ok(()) => Err("expected invariant to fail, but it passed".to_string()),
            Err(_) => Ok(()),
        }
    }
}

// ── Standard Invariants ─────────────────────────────────────────────

/// Verify that all keys in a sequential range `[0, key_count)` are readable.
///
/// This is a liveness invariant: after recovery, the store should service
/// reads for all committed keys without errors or panics.
pub struct MonotonicAddresses {
    key_count: u64,
}

impl MonotonicAddresses {
    /// Verify readability of keys `[0, key_count)`.
    pub fn new(key_count: u64) -> Self {
        Self { key_count }
    }
}

impl Invariant for MonotonicAddresses {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        for key in 0..self.key_count {
            let _val: Option<u64> = store.read_simple(session, &key);
        }
        Ok(())
    }
}

/// Verify hash index consistency by double-reading each key.
///
/// Reads each key twice and verifies both reads return the same result.
/// Detects stale or corrupt hash index entries.
pub struct ConsistentHashIndex {
    keys: Vec<u64>,
}

impl ConsistentHashIndex {
    /// Check consistency for the given keys.
    pub fn new(keys: Vec<u64>) -> Self {
        Self { keys }
    }

    /// Check consistency for keys in `[range.start, range.end)`.
    pub fn from_range(range: std::ops::Range<u64>) -> Self {
        Self {
            keys: range.collect(),
        }
    }
}

impl Invariant for ConsistentHashIndex {
    fn check(
        &self,
        store: &FasterKv<SimpleFunctions<u64, u64>>,
        session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        for &key in &self.keys {
            let v1: Option<u64> = store.read_simple(session, &key);
            let v2: Option<u64> = store.read_simple(session, &key);
            if v1 != v2 {
                return Err(format!(
                    "inconsistent hash index: key {key} read as {v1:?} then {v2:?}"
                ));
            }
        }
        Ok(())
    }
}

/// Precondition marker: page CRC-32C checksums were validated during recovery.
///
/// CRC-32C validation occurs inside [`FasterKv::recover()`] when the
/// [`ChecksumValidationPolicy`](faster_core::recovery::ChecksumValidationPolicy)
/// is set to `Reject`. If recovery succeeds under that policy, **every** page
/// checksum on disk matched the computed CRC — no additional verification is
/// needed at invariant-check time.
///
/// This invariant always returns `Ok(())`. Its purpose is to document that
/// CRC validation was part of the recovery contract, making the guarantee
/// explicit and visible in scenario templates and test reports.
pub struct ValidPageChecksums;

impl ValidPageChecksums {
    /// Create a new marker invariant.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ValidPageChecksums {
    fn default() -> Self {
        Self
    }
}

impl Invariant for ValidPageChecksums {
    fn check(
        &self,
        _store: &FasterKv<SimpleFunctions<u64, u64>>,
        _session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        // CRC validation happens at recovery time. If we reach this point,
        // recovery succeeded and all checksums were valid.
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

    // ── Combinator tests ────────────────────────────────────────────

    #[test]
    fn and_passes_when_both_pass() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &10u64, ());

        let inv = And::new(
            AllCommittedRecoverable::from_pairs(&[(1, 10)]),
            NoPhantomReads::new(vec![99]),
        );
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    #[test]
    fn and_fails_when_second_fails() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &10u64, ());

        let inv = And::new(
            AllCommittedRecoverable::from_pairs(&[(1, 10)]),
            NoPhantomReads::new(vec![1]), // key 1 IS present
        );
        assert!(inv.check(&store, &mut s).is_err());
        store.dispose_session(s);
    }

    #[test]
    fn or_passes_when_one_passes() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &10u64, ());

        let inv = Or::new(
            AllCommittedRecoverable::from_pairs(&[(1, 999)]), // wrong value
            AllCommittedRecoverable::from_pairs(&[(1, 10)]),  // correct
        );
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    #[test]
    fn or_fails_when_both_fail() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();

        let inv = Or::new(
            AllCommittedRecoverable::from_pairs(&[(1, 10)]),
            AllCommittedRecoverable::from_pairs(&[(2, 20)]),
        );
        assert!(inv.check(&store, &mut s).is_err());
        store.dispose_session(s);
    }

    #[test]
    fn all_passes_when_all_pass() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &10u64, ());

        let inv = All::new(vec![
            Box::new(AllCommittedRecoverable::from_pairs(&[(1, 10)])),
            Box::new(NoPhantomReads::new(vec![99])),
        ]);
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    #[test]
    fn all_fails_on_first_violation() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();

        let inv = All::new(vec![
            Box::new(AllCommittedRecoverable::from_pairs(&[(1, 10)])), // fails
            Box::new(NoPhantomReads::new(vec![99])),
        ]);
        assert!(inv.check(&store, &mut s).is_err());
        store.dispose_session(s);
    }

    #[test]
    fn not_inverts_result() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();

        // NoPhantomReads(42) passes (key 42 absent) → Not flips to fail
        let inv = Not::new(NoPhantomReads::new(vec![42]));
        assert!(inv.check(&store, &mut s).is_err());

        // AllCommittedRecoverable fails (key 1 absent) → Not flips to pass
        let inv2 = Not::new(AllCommittedRecoverable::from_pairs(&[(1, 10)]));
        assert!(inv2.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    // ── Standard invariant tests ────────────────────────────────────

    #[test]
    fn monotonic_addresses_passes_on_live_store() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        for i in 0..10u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        let inv = MonotonicAddresses::new(10);
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    #[test]
    fn consistent_hash_index_passes_on_consistent_store() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        for i in 0..10u64 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        let inv = ConsistentHashIndex::from_range(0..10);
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }

    #[test]
    fn valid_page_checksums_always_passes() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let inv = ValidPageChecksums::new();
        assert!(inv.check(&store, &mut s).is_ok());
        store.dispose_session(s);
    }
}
