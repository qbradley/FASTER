//! Integration tests for the batch operation API.
//!
//! These tests verify batch_read, batch_upsert, batch_rmw, batch_delete,
//! and batch_execute against a real FasterKv store.

use faster_core::InMemoryDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{BatchOp, CounterFunctions, FasterKv, FasterKvConfig, SimpleFunctions};

/// Small in-memory store suitable for batch tests.
fn small_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

fn counter_store() -> FasterKv<CounterFunctions<u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, CounterFunctions::default(), InMemoryDevice::new())
}

// ── batch_upsert + batch_read ───────────────────────────────────────

#[test]
fn batch_upsert_then_read() {
    let store = small_store();
    let mut session = store.new_session();

    let keys: Vec<u64> = (0..100).collect();
    let values: Vec<u64> = keys.iter().map(|k| k * 10).collect();

    let result = store.batch_upsert(&mut session, &keys, &values);
    assert_eq!(result.total(), 100);
    assert!(result.all_succeeded());

    let mut outputs = vec![None; 100];
    let result = store.batch_read(&mut session, &keys, &mut outputs);
    assert_eq!(result.total(), 100);
    assert!(result.all_succeeded());

    for (i, output) in outputs.iter().enumerate() {
        assert_eq!(*output, Some(i as u64 * 10), "key {} mismatch", i);
    }

    store.dispose_session(session);
}

// ── batch_read on missing keys ──────────────────────────────────────

#[test]
fn batch_read_not_found() {
    let store = small_store();
    let mut session = store.new_session();

    let keys: Vec<u64> = (1000..1010).collect();
    let mut outputs = vec![None; 10];
    let result = store.batch_read(&mut session, &keys, &mut outputs);

    assert_eq!(result.total(), 10);
    assert_eq!(result.not_found_count(), 10);
    assert!(!result.all_succeeded());

    store.dispose_session(session);
}

// ── empty batch ─────────────────────────────────────────────────────

#[test]
fn batch_empty() {
    let store = small_store();
    let mut session = store.new_session();

    let keys: Vec<u64> = vec![];
    let values: Vec<u64> = vec![];
    let result = store.batch_upsert(&mut session, &keys, &values);
    assert!(result.is_empty());
    assert_eq!(result.total(), 0);
    assert!(result.all_succeeded()); // vacuously true

    let mut outputs: Vec<Option<u64>> = vec![];
    let result = store.batch_read(&mut session, &keys, &mut outputs);
    assert!(result.is_empty());

    store.dispose_session(session);
}

// ── large batch ─────────────────────────────────────────────────────

#[test]
fn batch_large() {
    let store = small_store();
    let mut session = store.new_session();

    let n = 2000;
    let keys: Vec<u64> = (0..n).collect();
    let values: Vec<u64> = keys.iter().map(|k| k + 1000).collect();

    let result = store.batch_upsert(&mut session, &keys, &values);
    assert_eq!(result.total(), n as usize);
    assert!(result.all_succeeded());

    let mut outputs = vec![None; n as usize];
    let result = store.batch_read(&mut session, &keys, &mut outputs);
    assert_eq!(result.total(), n as usize);
    assert!(result.all_succeeded());

    for (i, output) in outputs.iter().enumerate() {
        assert_eq!(*output, Some(i as u64 + 1000));
    }

    store.dispose_session(session);
}

// ── batch_delete ────────────────────────────────────────────────────

#[test]
fn batch_delete_lifecycle() {
    let store = small_store();
    let mut session = store.new_session();

    let keys: Vec<u64> = (0..50).collect();
    let values: Vec<u64> = keys.iter().map(|k| k * 2).collect();
    store.batch_upsert(&mut session, &keys, &values);

    // Delete first 25 keys
    let del_keys: Vec<u64> = (0..25).collect();
    let result = store.batch_delete(&mut session, &del_keys);
    assert_eq!(result.total(), 25);

    // Read all 50 — first 25 should be not found
    let mut outputs = vec![None; 50];
    let result = store.batch_read(&mut session, &keys, &mut outputs);

    let mut not_found = 0;
    let mut found = 0;
    for s in &result.statuses {
        if *s == OperationStatus::NotFound {
            not_found += 1;
        } else if s.is_success() {
            found += 1;
        }
    }
    assert_eq!(not_found, 25);
    assert_eq!(found, 25);

    store.dispose_session(session);
}

// ── batch_rmw (counter) ────────────────────────────────────────────

#[test]
fn batch_rmw_counters() {
    let store = counter_store();
    let mut session = store.new_session();

    let keys: Vec<u64> = (0..10).collect();
    let increments = vec![1i64; 10];
    let mut outputs = vec![0i64; 10];

    // RMW on non-existent keys creates them with initial value
    let result = store.batch_rmw(&mut session, &keys, &increments, &mut outputs);
    assert_eq!(result.total(), 10);

    // RMW again to increment
    let result = store.batch_rmw(&mut session, &keys, &increments, &mut outputs);
    assert_eq!(result.total(), 10);

    store.dispose_session(session);
}

// ── batch_execute (mixed) ───────────────────────────────────────────

#[test]
fn batch_execute_mixed() {
    let store = small_store();
    let mut session = store.new_session();

    // First upsert some keys
    let keys: Vec<u64> = vec![1, 2, 3];
    let values: Vec<u64> = vec![10, 20, 30];
    store.batch_upsert(&mut session, &keys, &values);

    // Mixed batch: read key 1, upsert key 4, delete key 2, read key 3
    let ops: Vec<BatchOp<u64, u64>> = vec![
        BatchOp::Read { key: 1, input: 0 },
        BatchOp::Upsert { key: 4, input: 40 },
        BatchOp::Delete { key: 2 },
        BatchOp::Read { key: 3, input: 0 },
    ];
    let mut outputs = vec![None; 4];
    let result = store.batch_execute(&mut session, &ops, &mut outputs);
    assert_eq!(result.total(), 4);

    // Verify reads got correct values
    assert_eq!(outputs[0], Some(10)); // key 1 = 10
    assert_eq!(outputs[3], Some(30)); // key 3 = 30

    // Verify key 2 is deleted
    let mut check = vec![None; 1];
    let result = store.batch_read(&mut session, &[2u64], &mut check);
    assert_eq!(result.not_found_count(), 1);

    // Verify key 4 was upserted
    let mut check = vec![None; 1];
    let result = store.batch_read(&mut session, &[4u64], &mut check);
    assert!(result.all_succeeded());
    assert_eq!(check[0], Some(40));

    store.dispose_session(session);
}

// ── concurrent batches on separate threads ──────────────────────────

#[test]
fn batch_concurrent_threads() {
    use std::sync::Arc;
    use std::thread;

    let store = Arc::new(small_store());
    let num_threads = 4;
    let ops_per_thread = 500;

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut session = store.new_session();
                let base = t * ops_per_thread;
                let keys: Vec<u64> = (base..base + ops_per_thread).map(|k| k as u64).collect();
                let values: Vec<u64> = keys.iter().map(|k| k * 100).collect();

                let result = store.batch_upsert(&mut session, &keys, &values);
                assert_eq!(result.total(), ops_per_thread as usize);
                assert!(result.all_succeeded());

                let mut outputs = vec![None; ops_per_thread as usize];
                let result = store.batch_read(&mut session, &keys, &mut outputs);
                assert_eq!(result.total(), ops_per_thread as usize);
                assert!(result.all_succeeded());

                for (i, output) in outputs.iter().enumerate() {
                    let expected = (base as u64 + i as u64) * 100;
                    assert_eq!(*output, Some(expected));
                }

                store.dispose_session(session);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}
