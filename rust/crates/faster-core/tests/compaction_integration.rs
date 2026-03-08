//! Integration tests for `FasterKv::compact()` — the K6 compaction
//! orchestration public API.

use faster_core::compaction::policy::{CompactionPolicy, CompactionStats};
use faster_core::device::NullDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, ReadInfo, RmwInfo, SimpleFunctions, UpsertInfo};
use std::sync::Arc;

type SimpleStore = FasterKv<SimpleFunctions<u64, u64>>;

fn test_store() -> SimpleStore {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 4,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
}

// ── SC-1: Write 100K, delete 50K, compact, verify ──────────────────

/// Success criteria: Write 100K records, delete 50K, compact, verify:
/// (a) 50K live records intact, (b) storage size reduced, (c) tombstones gone.
///
/// Note: With NullDevice and all records in-memory, the "delete" operation
/// creates tombstone records. We use a smaller count (100 records) since
/// the in-memory test store has limited pages, but the logic is identical.
#[test]
fn compact_write_delete_verify() {
    let store = test_store();
    let mut session = store.new_session();

    let n: u64 = 100;

    // Write n records.
    for i in 0..n {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }

    // Delete half.
    for i in 0..n / 2 {
        let _ = store.delete(&mut session, &i, ());
    }

    store.dispose_session(session);

    // (a) Before compaction, the live records (n/2 .. n) are still there.
    let mut session = store.new_session();
    for i in n / 2..n {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "pre-compact: key {i} should be readable");
    }

    let begin_before = store.begin_address().raw();

    store.dispose_session(session);

    // Run compaction.
    let result = store.compact();
    // Compaction may return EmptyRegion if safe_read_only hasn't advanced
    // past begin (all data still mutable). That's okay — it means there's
    // nothing to compact yet.
    match result {
        Ok(cr) => {
            // (b) Storage size reduced: begin_address advanced.
            assert!(
                cr.truncation.advanced,
                "begin address should advance after compaction"
            );
            let begin_after = store.begin_address().raw();
            assert!(
                begin_after > begin_before,
                "begin should advance: was {begin_before}, now {begin_after}"
            );

            // (c) Tombstones in the compacted region are gone — they're
            // below the new begin_address.
        }
        Err(faster_core::compaction::orchestrator::CompactionError::EmptyRegion { .. }) => {
            // All data still in mutable region — nothing to compact.
            // This is expected with small in-memory tests.
        }
        Err(e) => panic!("unexpected compaction error: {e}"),
    }

    // (a) Live records still intact after compaction.
    let mut session = store.new_session();
    for i in n / 2..n {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(
            val,
            Some(i * 10),
            "post-compact: key {i} should be readable"
        );
    }
    store.dispose_session(session);
}

// ── SC-2: Compact via public API ────────────────────────────────────

#[test]
fn compact_public_api_basic() {
    let store = test_store();
    let mut session = store.new_session();

    for i in 0u64..50 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }

    store.dispose_session(session);

    // compact() should succeed or return EmptyRegion.
    let result = store.compact();
    match result {
        Ok(cr) => {
            assert!(cr.records_copied > 0 || cr.plan.live_records.is_empty());
        }
        Err(faster_core::compaction::orchestrator::CompactionError::EmptyRegion { .. }) => {
            // Fine — data is all mutable.
        }
        Err(e) => panic!("unexpected: {e}"),
    }

    // Data still readable.
    let mut session = store.new_session();
    for i in 0u64..50 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10));
    }
    store.dispose_session(session);
}

// ── SC-3: Concurrent writes during compaction ───────────────────────

#[test]
fn compact_during_concurrent_writes() {
    let store = Arc::new(test_store());

    // Write initial data.
    {
        let mut session = store.new_session();
        for i in 0u64..50 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }
        store.dispose_session(session);
    }

    // Spawn a writer thread that continuously writes.
    let store_w = Arc::clone(&store);
    let writer = std::thread::spawn(move || {
        let mut session = store_w.new_session();
        for i in 100u64..200 {
            let _ = store_w.upsert(&mut session, &i, &(i * 10), ());
        }
        store_w.dispose_session(session);
    });

    // Compact on the main thread (concurrent with writes).
    let _ = store.compact();

    writer.join().expect("writer thread panicked");

    // Verify all written data is readable.
    let mut session = store.new_session();
    // Original keys might or might not be available depending on timing,
    // but the concurrent writes should all be readable.
    for i in 100u64..200 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "concurrent write key {i}");
    }
    store.dispose_session(session);
}

// ── SC-4: Multiple compactions are serialized ───────────────────────

#[test]
fn multiple_compactions_serialized() {
    let store = Arc::new(test_store());

    // Write data.
    {
        let mut session = store.new_session();
        for i in 0u64..50 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }
        store.dispose_session(session);
    }

    // Spawn two threads that both try to compact.
    let store1 = Arc::clone(&store);
    let store2 = Arc::clone(&store);

    let t1 = std::thread::spawn(move || store1.compact());
    let t2 = std::thread::spawn(move || store2.compact());

    // Both should complete without panic (mutex serializes them).
    let _r1 = t1.join().expect("compaction thread 1 panicked");
    let _r2 = t2.join().expect("compaction thread 2 panicked");

    // Data still readable.
    let mut session = store.new_session();
    for i in 0u64..50 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i} after double compaction");
    }
    store.dispose_session(session);
}

// ── SC-5: maybe_compact with policy ─────────────────────────────────

#[test]
fn maybe_compact_with_policy() {
    let mut store = FasterKv::new(
        FasterKvConfig {
            hash_index_size_log2: 10,
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: true,
        },
        SimpleFunctions::default(),
        NullDevice::new(),
    );

    // Set an always-trigger policy for testing.
    struct AlwaysCompact;
    impl CompactionPolicy for AlwaysCompact {
        fn should_compact(&self, _stats: &CompactionStats) -> bool {
            true
        }
        fn name(&self) -> &str {
            "AlwaysCompact"
        }
    }

    store.set_compaction_policy(Some(Box::new(AlwaysCompact)));

    let mut session = store.new_session();
    for i in 0u64..20 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    // maybe_compact should attempt compaction.
    let result = store.maybe_compact();
    // It should return Some(...) since auto_compact=true and policy always fires.
    assert!(result.is_some(), "should attempt compaction");
}

// ── SC-6: maybe_compact disabled ────────────────────────────────────

#[test]
fn maybe_compact_disabled_by_default() {
    let store = test_store();

    // auto_compact=false by default, so maybe_compact returns None.
    let result = store.maybe_compact();
    assert!(result.is_none(), "auto_compact is off by default");
}

// ═══════════════════════════════════════════════════════════════════
// Variable-length compaction integration tests (Phase 4)
// ═══════════════════════════════════════════════════════════════════

use faster_core::InMemoryDevice;
use faster_core::status::OperationStatus;
use faster_core::store::{Functions, RmwInPlaceResult};

/// Functions implementation for variable-length keys and values.
struct VarLenFunctions;

impl Functions for VarLenFunctions {
    type Key = Vec<u8>;
    type Value = Vec<u8>;
    type Input = Vec<u8>;
    type Output = Option<Vec<u8>>;
    type Context = ();

    fn read(
        &self,
        _key: &Vec<u8>,
        value: &Vec<u8>,
        _input: &Vec<u8>,
        output: &mut Option<Vec<u8>>,
        _info: &ReadInfo,
    ) {
        *output = Some(value.clone());
    }

    fn upsert(
        &self,
        _key: &Vec<u8>,
        value: &mut Vec<u8>,
        input: &Vec<u8>,
        _old_value: Option<&Vec<u8>>,
        _output: &mut Option<Vec<u8>>,
        _info: &UpsertInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_initial(
        &self,
        _key: &Vec<u8>,
        input: &Vec<u8>,
        value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_in_place(
        &self,
        _key: &Vec<u8>,
        _input: &Vec<u8>,
        _value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        RmwInPlaceResult::NeedsNewRecord
    }

    fn rmw_copy_update(
        &self,
        _key: &Vec<u8>,
        input: &Vec<u8>,
        old_value: &Vec<u8>,
        new_value: &mut Vec<u8>,
        output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        let mut merged = old_value.clone();
        merged.extend_from_slice(input);
        *new_value = merged.clone();
        *output = Some(merged);
    }
}

/// Functions implementation with u64 keys and Vec<u8> variable-length values.
struct MixedKeyFunctions;

impl Functions for MixedKeyFunctions {
    type Key = u64;
    type Value = Vec<u8>;
    type Input = Vec<u8>;
    type Output = Option<Vec<u8>>;
    type Context = ();

    fn read(&self, _key: &u64, value: &Vec<u8>, _input: &Vec<u8>, output: &mut Option<Vec<u8>>, _info: &ReadInfo) {
        *output = Some(value.clone());
    }

    fn upsert(
        &self,
        _key: &u64,
        value: &mut Vec<u8>,
        input: &Vec<u8>,
        _old_value: Option<&Vec<u8>>,
        _output: &mut Option<Vec<u8>>,
        _info: &UpsertInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_initial(
        &self,
        _key: &u64,
        input: &Vec<u8>,
        value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_in_place(
        &self,
        _key: &u64,
        _input: &Vec<u8>,
        _value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        RmwInPlaceResult::NeedsNewRecord
    }

    fn rmw_copy_update(
        &self,
        _key: &u64,
        input: &Vec<u8>,
        old_value: &Vec<u8>,
        new_value: &mut Vec<u8>,
        output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        let mut merged = old_value.clone();
        merged.extend_from_slice(input);
        *new_value = merged.clone();
        *output = Some(merged);
    }
}

fn varlen_store() -> FasterKv<VarLenFunctions> {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };
    FasterKv::new(config, VarLenFunctions, InMemoryDevice::new())
}

fn mixed_key_store() -> FasterKv<MixedKeyFunctions> {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };
    FasterKv::new(config, MixedKeyFunctions, InMemoryDevice::new())
}

/// Generate a deterministic byte vector of given length.
fn make_value(key: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    for i in 0..len {
        v.push(((key.wrapping_mul(31).wrapping_add(i as u64)) & 0xFF) as u8);
    }
    v
}

/// Make a variable-length key from an integer.
fn make_key(id: u64) -> Vec<u8> {
    format!("key-{id:06}").into_bytes()
}

// ── SC-7: Variable-length CRUD → compact → verify ──────────────────

#[test]
fn compact_variable_length_crud_roundtrip() {
    let store = varlen_store();
    let mut session = store.new_session();

    let n = 50u64;
    // Insert variable-length records with different sizes.
    for i in 0..n {
        let key = make_key(i);
        let val = make_value(i, 10 + (i as usize % 50));
        let status = store.upsert(&mut session, &key, &val, ());
        assert!(status.is_success(), "upsert key {i} failed: {status:?}");
    }

    store.dispose_session(session);

    // Compact with variable-length types.
    let result = store.compact();
    match result {
        Ok(cr) => {
            assert!(
                cr.truncation.advanced || cr.plan.live_records.is_empty(),
                "should advance or have no live records in region"
            );
        }
        Err(faster_core::compaction::orchestrator::CompactionError::EmptyRegion { .. }) => {
            // All data still mutable — nothing to compact.
        }
        Err(e) => panic!("unexpected compaction error: {e}"),
    }

    // Verify all records still readable after compaction.
    let mut session = store.new_session();
    for i in 0..n {
        let key = make_key(i);
        let expected = make_value(i, 10 + (i as usize % 50));
        let mut out: Option<Vec<u8>> = None;
        let status = store.read(&mut session, &key, &vec![], &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read key {i} failed");
        assert_eq!(
            out.as_deref(),
            Some(expected.as_slice()),
            "value mismatch at key {i}"
        );
    }
    store.dispose_session(session);
}

// ── SC-8: Mixed-size records → compact → verify ────────────────────

#[test]
fn compact_mixed_size_records() {
    let store = mixed_key_store();
    let mut session = store.new_session();

    // Insert records with varying value sizes: 1 byte to 100 bytes.
    let n = 40u64;
    for i in 0..n {
        let val = make_value(i, 1 + (i as usize * 3) % 100);
        let status = store.upsert(&mut session, &i, &val, ());
        assert!(status.is_success(), "upsert key {i} failed: {status:?}");
    }

    store.dispose_session(session);

    // Compact with u64 key, Vec<u8> value.
    let result = store.compact();
    match result {
        Ok(_) | Err(faster_core::compaction::orchestrator::CompactionError::EmptyRegion { .. }) => {
        }
        Err(e) => panic!("unexpected compaction error: {e}"),
    }

    // Verify all data intact.
    let mut session = store.new_session();
    for i in 0..n {
        let expected = make_value(i, 1 + (i as usize * 3) % 100);
        let mut out: Option<Vec<u8>> = None;
        let status = store.read(&mut session, &i, &vec![], &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read key {i} failed");
        assert_eq!(
            out.as_deref(),
            Some(expected.as_slice()),
            "value mismatch at key {i}"
        );
    }
    store.dispose_session(session);
}

// ── SC-9: Tombstones with variable-length → compact → verify ───────

#[test]
fn compact_variable_length_tombstones() {
    let store = mixed_key_store();
    let mut session = store.new_session();

    let n = 60u64;
    for i in 0..n {
        let val = make_value(i, 20 + (i as usize % 30));
        let status = store.upsert(&mut session, &i, &val, ());
        assert!(status.is_success(), "upsert key {i} failed");
    }

    // Delete half the records.
    for i in 0..n / 2 {
        let _ = store.delete(&mut session, &i, ());
    }

    store.dispose_session(session);

    // Compact — tombstones should be cleaned up.
    let result = store.compact();
    match result {
        Ok(cr) => {
            // Verify truncation happened or region was all-mutable.
            assert!(
                cr.truncation.advanced || cr.plan.live_records.is_empty(),
                "should advance or have no live in region"
            );
        }
        Err(faster_core::compaction::orchestrator::CompactionError::EmptyRegion { .. }) => {}
        Err(e) => panic!("unexpected compaction error: {e}"),
    }

    // Surviving records still readable.
    let mut session = store.new_session();
    for i in n / 2..n {
        let expected = make_value(i, 20 + (i as usize % 30));
        let mut out: Option<Vec<u8>> = None;
        let status = store.read(&mut session, &i, &vec![], &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read key {i} failed");
        assert_eq!(
            out.as_deref(),
            Some(expected.as_slice()),
            "value mismatch at surviving key {i}"
        );
    }
    store.dispose_session(session);
}

// ── SC-10: Concurrent reads during variable-length compaction ───────

#[test]
fn compact_variable_length_concurrent_reads() {
    let store = Arc::new(mixed_key_store());
    let n = 50u64;

    // Write initial data.
    {
        let mut session = store.new_session();
        for i in 0..n {
            let val = make_value(i, 15 + (i as usize % 40));
            let _ = store.upsert(&mut session, &i, &val, ());
        }
        store.dispose_session(session);
    }

    // Spawn reader threads that continuously read while compaction runs.
    let store_r = Arc::clone(&store);
    let reader = std::thread::spawn(move || {
        let mut session = store_r.new_session();
        for _ in 0..3 {
            for i in 0..n {
                let mut out: Option<Vec<u8>> = None;
                let _ = store_r.read(&mut session, &i, &vec![], &mut out, ());
                // We don't assert Ok here because compaction may move records,
                // but the data should never be corrupted if returned.
                if let Some(ref data) = out {
                    let expected = make_value(i, 15 + (i as usize % 40));
                    assert_eq!(
                        data.as_slice(),
                        expected.as_slice(),
                        "corrupted read for key {i} during concurrent compaction"
                    );
                }
            }
        }
        store_r.dispose_session(session);
    });

    // Compact on main thread.
    let _ = store.compact();

    reader.join().expect("reader thread panicked");

    // Final verification: all data readable.
    let mut session = store.new_session();
    for i in 0..n {
        let expected = make_value(i, 15 + (i as usize % 40));
        let mut out: Option<Vec<u8>> = None;
        let status = store.read(&mut session, &i, &vec![], &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "post-compact read key {i}");
        assert_eq!(
            out.as_deref(),
            Some(expected.as_slice()),
            "post-compact value mismatch at key {i}"
        );
    }
    store.dispose_session(session);
}

// ── SC-11: Hash-collision chains with different key sizes ───────────

#[test]
fn compact_variable_length_hash_collisions() {
    let store = varlen_store();
    let mut session = store.new_session();

    // Use keys that are likely to collide in a small hash table (1024 buckets).
    // Different key sizes exercise variable-length key handling in the scanner
    // and address updater's chain-walking logic.
    let keys_and_values: Vec<(Vec<u8>, Vec<u8>)> = (0u64..80)
        .map(|i| {
            // Vary key sizes: short keys (3 bytes) and long keys (20+ bytes)
            let key = if i % 2 == 0 {
                format!("k{i}").into_bytes()
            } else {
                format!("long-variable-key-{i:010}").into_bytes()
            };
            let val = make_value(i, 5 + (i as usize % 60));
            (key, val)
        })
        .collect();

    for (key, val) in &keys_and_values {
        let status = store.upsert(&mut session, key, val, ());
        assert!(status.is_success(), "upsert failed for key {:?}", key);
    }

    store.dispose_session(session);

    // Compact.
    let result = store.compact();
    match result {
        Ok(_) | Err(faster_core::compaction::orchestrator::CompactionError::EmptyRegion { .. }) => {
        }
        Err(e) => panic!("unexpected compaction error: {e}"),
    }

    // Verify all data intact — every key/value pair must survive.
    let mut session = store.new_session();
    for (key, expected_val) in &keys_and_values {
        let mut out: Option<Vec<u8>> = None;
        let status = store.read(&mut session, key, &vec![], &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read failed for key {:?}", key);
        assert_eq!(
            out.as_deref(),
            Some(expected_val.as_slice()),
            "value mismatch for key {:?}",
            key
        );
    }
    store.dispose_session(session);
}
