//! Tests for write-path pending I/O completion (copy-to-tail on disk records).
//!
//! These tests verify that upsert, RMW, and delete operations on records that
//! have been evicted to disk are properly completed when the async I/O finishes.
//!
//! Each page is 32 MiB and a `u64+u64` record is 24 bytes, so we need
//! millions of records to span multiple pages and trigger eviction.

use faster_core::InMemoryDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{CounterFunctions, FasterKv, FasterKvConfig, SimpleFunctions};

/// Number of records needed to fill ~5 pages and force eviction.
/// Each page = 32 MiB, each record (u64, u64) = 24 bytes.
/// ~1.4M records per page × 5 pages ≈ 7M records.
const RECORD_COUNT: u64 = 7_000_000;

/// Create an in-memory-device-backed store.
///
/// Uses `InMemoryDevice` which completes I/O synchronously — no async
/// timing issues. Pages flush instantly to the device's internal buffer.
fn test_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 22, // 4M buckets
        buffer_size_pages: 8,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 4,
        },
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

/// Create an in-memory-device-backed counter store.
fn counter_test_store() -> FasterKv<CounterFunctions<u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 22,
        buffer_size_pages: 8,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 4,
        },
    };
    FasterKv::new(config, CounterFunctions::new(), InMemoryDevice::new())
}

/// Insert records then force eviction via maintenance.
fn fill_then_evict<F: faster_core::store::Functions>(
    store: &FasterKv<F>,
    session: &mut faster_core::store::FasterSession<F>,
    count: u64,
    mut insert_fn: impl FnMut(&FasterKv<F>, &mut faster_core::store::FasterSession<F>, u64),
) {
    for i in 0..count {
        insert_fn(store, session, i);
    }
    // With InMemoryDevice, flush completes synchronously. Each maintenance
    // cycle shifts RO, flushes sealed pages (instantly), and evicts flushed.
    for _ in 0..20 {
        store.maintenance();
    }
}

/// Find a key that is currently evicted (on-disk).
fn find_evicted_key<F: faster_core::store::Functions>(
    store: &FasterKv<F>,
    session: &mut faster_core::store::FasterSession<F>,
    range: std::ops::Range<u64>,
    read_fn: impl Fn(&FasterKv<F>, &mut faster_core::store::FasterSession<F>, u64) -> OperationStatus,
) -> Option<u64> {
    for i in range {
        let status = read_fn(store, session, i);
        if status == OperationStatus::Pending {
            // Drain this pending read so it doesn't interfere.
            let _ = store.try_complete_pending(session, true);
            return Some(i);
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════
// 1. Upsert on evicted (on-disk) record
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_upsert_on_disk_record() {
    let store = test_store();
    let mut s = store.new_session();

    // Fill then evict.
    fill_then_evict(&store, &mut s, RECORD_COUNT, |st, sess, i| {
        let _ = st.upsert(sess, &i, &(i * 7), ());
    });

    // Find an evicted key (scan early keys — they're the oldest).
    let target_key = find_evicted_key(&store, &mut s, 0..RECORD_COUNT, |st, sess, i| {
        let mut out: Option<u64> = None;
        st.read(sess, &i, &0u64, &mut out, ())
    })
    .expect("expected at least one evicted key");

    // Upsert a new value to the evicted key.
    let new_value = 999_999u64;
    let status = store.upsert(&mut s, &target_key, &new_value, ());
    assert_eq!(
        status,
        OperationStatus::Pending,
        "upsert on evicted key should return Pending, got {status:?}"
    );

    // Complete the pending I/O.
    let result = store.try_complete_pending(&mut s, true);
    assert!(result.is_done(), "all pending ops should be done");

    // Read back — the upserted value must be visible.
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &target_key, &0u64, &mut out, ());
    match status {
        OperationStatus::Ok => {
            assert_eq!(
                out,
                Some(new_value),
                "upsert on disk record should have written new value"
            );
        }
        OperationStatus::Pending => {
            let _ = store.try_complete_pending(&mut s, true);
            let mut out2: Option<u64> = None;
            let status2 = store.read(&mut s, &target_key, &0u64, &mut out2, ());
            assert_eq!(status2, OperationStatus::Ok);
            assert_eq!(
                out2,
                Some(new_value),
                "upsert on disk record: new value after pending read"
            );
        }
        other => panic!("unexpected read status: {other:?}"),
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 2. RMW on evicted (on-disk) record
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_rmw_on_disk_record() {
    let store = counter_test_store();
    let mut s = store.new_session();

    // Fill then evict. CounterFunctions uses rmw.
    fill_then_evict(&store, &mut s, RECORD_COUNT, |st, sess, i| {
        let mut out = 0i64;
        let _ = st.rmw(sess, &i, &(i as i64), &mut out, ());
    });

    // Find an evicted key.
    let target_key = find_evicted_key(&store, &mut s, 0..RECORD_COUNT, |st, sess, i| {
        let mut out = 0i64;
        st.read(sess, &i, &0i64, &mut out, ())
    })
    .expect("expected at least one evicted key");

    // Apply RMW (add 100) to the evicted key.
    let delta = 100i64;
    let mut rmw_out = 0i64;
    let status = store.rmw(&mut s, &target_key, &delta, &mut rmw_out, ());
    assert_eq!(
        status,
        OperationStatus::Pending,
        "RMW on evicted key should return Pending, got {status:?}"
    );

    // Complete pending.
    let result = store.try_complete_pending(&mut s, true);
    assert!(result.is_done(), "all pending ops should be done");

    // Read back — the counter should be old_value + delta.
    let expected = target_key as i64 + delta;
    let mut read_out = 0i64;
    let status = store.read(&mut s, &target_key, &0i64, &mut read_out, ());
    match status {
        OperationStatus::Ok => {
            assert_eq!(
                read_out, expected,
                "RMW on disk record: expected {expected}, got {read_out}"
            );
        }
        OperationStatus::Pending => {
            let _ = store.try_complete_pending(&mut s, true);
            let mut read_out2 = 0i64;
            let status2 = store.read(&mut s, &target_key, &0i64, &mut read_out2, ());
            assert_eq!(status2, OperationStatus::Ok);
            assert_eq!(
                read_out2, expected,
                "RMW on disk record (after pending read): expected {expected}, got {read_out2}"
            );
        }
        other => panic!("unexpected read status: {other:?}"),
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 3. Delete on evicted (on-disk) record
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_delete_on_disk_record() {
    let store = test_store();
    let mut s = store.new_session();

    // Fill then evict.
    fill_then_evict(&store, &mut s, RECORD_COUNT, |st, sess, i| {
        let _ = st.upsert(sess, &i, &(i * 7), ());
    });

    // Find an evicted key.
    let target_key = find_evicted_key(&store, &mut s, 0..RECORD_COUNT, |st, sess, i| {
        let mut out: Option<u64> = None;
        st.read(sess, &i, &0u64, &mut out, ())
    })
    .expect("expected at least one evicted key");

    // Delete the evicted key.
    let status = store.delete(&mut s, &target_key, ());
    assert_eq!(
        status,
        OperationStatus::Pending,
        "delete on evicted key should return Pending, got {status:?}"
    );

    // Complete pending.
    let result = store.try_complete_pending(&mut s, true);
    assert!(result.is_done(), "all pending ops should be done");

    // Read back — the key should be gone (NotFound) or the tombstone
    // should be visible after completing pending reads.
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &target_key, &0u64, &mut out, ());
    match status {
        OperationStatus::NotFound => { /* correct */ }
        OperationStatus::Ok => {
            panic!(
                "deleted key {target_key} should be NotFound, got Ok({:?})",
                out
            );
        }
        OperationStatus::Pending => {
            let _ = store.try_complete_pending(&mut s, true);
            let mut out2: Option<u64> = None;
            let status2 = store.read(&mut s, &target_key, &0u64, &mut out2, ());
            assert_eq!(
                status2,
                OperationStatus::NotFound,
                "deleted key should be NotFound after completion, got {status2:?} val={out2:?}"
            );
        }
        other => panic!("unexpected read status: {other:?}"),
    }

    store.dispose_session(s);
}
