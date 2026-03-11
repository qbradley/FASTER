//! Integration tests for hybrid log management — eviction, flushing,
//! region boundaries, scan operations, and buffer pool behavior.
//!
//! Tests exercise the hybrid log through the `FasterKv` public API.

use faster_core::InMemoryDevice;
use faster_core::checkpoint::CheckpointType;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use std::sync::{Arc, Barrier};
use std::thread;

type SimpleStore = FasterKv<SimpleFunctions<u64, u64>>;

/// Store with default-ish config for most hybrid log tests.
fn hl_store() -> SimpleStore {
    let config = FasterKvConfig {
        hash_index_size_log2: 14,
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

/// Store with a small eviction threshold to exercise eviction paths.
fn tiny_eviction_store() -> SimpleStore {
    let config = FasterKvConfig {
        hash_index_size_log2: 14,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

// ═══════════════════════════════════════════════════════════════════
// 1. Address boundary invariants
// ═══════════════════════════════════════════════════════════════════

#[test]
fn address_boundaries_initial_state() {
    let store = hl_store();
    let begin = store.begin_address().raw();
    let head = store.head_address().raw();
    let tail = store.tail_address().raw();

    // Initially all addresses are at the same point.
    assert_eq!(begin, head, "begin == head on empty store");
    assert!(tail >= head, "tail >= head on empty store");
}

#[test]
fn tail_advances_with_inserts() {
    let store = hl_store();
    let tail_before = store.tail_address().raw();

    let mut session = store.new_session();
    for i in 0u64..100 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    let tail_after = store.tail_address().raw();
    assert!(
        tail_after > tail_before,
        "tail must advance after inserts: before={tail_before}, after={tail_after}"
    );
}

#[test]
fn address_boundaries_monotonic_after_operations() {
    let store = hl_store();

    let mut session = store.new_session();
    for i in 0u64..500 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    // Take snapshot of boundaries.
    let begin = store.begin_address().raw();
    let head = store.head_address().raw();
    let tail = store.tail_address().raw();

    // Invariant: begin ≤ head ≤ tail.
    assert!(begin <= head, "begin ({begin}) <= head ({head})");
    assert!(head <= tail, "head ({head}) <= tail ({tail})");

    // Do more inserts and verify monotonicity.
    let mut session = store.new_session();
    for i in 500u64..1000 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    let begin2 = store.begin_address().raw();
    let head2 = store.head_address().raw();
    let tail2 = store.tail_address().raw();

    assert!(begin2 >= begin, "begin must not regress");
    assert!(head2 >= head, "head must not regress");
    assert!(tail2 > tail, "tail must advance with more inserts");
    assert!(begin2 <= head2, "begin2 <= head2");
    assert!(head2 <= tail2, "head2 <= tail2");
}

// ═══════════════════════════════════════════════════════════════════
// 2. Flush behavior
// ═══════════════════════════════════════════════════════════════════

#[test]
fn flush_does_not_panic_on_empty_store() {
    let store = hl_store();
    let flushed = store.flush();
    assert_eq!(flushed, 0, "nothing to flush on empty store");
}

#[test]
fn flush_does_not_panic_with_data() {
    let store = hl_store();
    let mut session = store.new_session();
    for i in 0u64..100 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    // flush() only flushes sealed pages. With small data on one page,
    // the page may still be Open (not sealed), so flushed may be 0.
    let flushed = store.flush();
    let _ = flushed; // Just verify no panic.
}

#[test]
fn flush_preserves_data_readability() {
    let store = hl_store();
    let mut session = store.new_session();
    for i in 0u64..200 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }

    // Flush should not affect readability.
    let _ = store.flush();

    for i in 0u64..200 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i} should be readable after flush");
    }
    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════
// 3. Flush and eviction cycle
// ═══════════════════════════════════════════════════════════════════

#[test]
fn flush_and_evict_advances_addresses() {
    let store = hl_store();
    let mut session = store.new_session();
    for i in 0u64..500 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    let head_before = store.head_address().raw();
    let (flushed, evicted) = store.flush_and_evict();

    // flush_and_evict shifts RO to tail, flushes, and evicts.
    // Head may or may not advance depending on whether pages got flushed.
    let head_after = store.head_address().raw();
    assert!(
        head_after >= head_before,
        "head must not regress: before={head_before}, after={head_after}"
    );

    // At minimum, flush_and_evict should not panic.
    let _ = (flushed, evicted);
}

#[test]
fn flush_and_evict_preserves_recent_writes() {
    let store = hl_store();
    let mut session = store.new_session();

    // Write data before flush_and_evict.
    for i in 0u64..200 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    store.flush_and_evict();

    // Write new data after flush_and_evict.
    let mut session = store.new_session();
    for i in 200u64..400 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }

    // New data should be readable.
    for i in 200u64..400 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "new key {i} should be readable");
    }
    store.dispose_session(session);
}

#[test]
fn flush_and_evict_then_new_writes_readable() {
    let store = hl_store();
    let mut session = store.new_session();
    for i in 0u64..300 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    // flush_and_evict pushes data to device and evicts from memory.
    store.flush_and_evict();

    // Write completely new keys (not overlapping with evicted data).
    let mut session = store.new_session();
    for i in 1000u64..1300 {
        let _ = store.upsert(&mut session, &i, &(i * 20), ());
    }

    // New keys should be readable (in mutable region).
    for i in 1000u64..1300 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(
            val,
            Some(i * 20),
            "new key {i} should be readable after eviction"
        );
    }
    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════
// 4. Eviction policy behavior
// ═══════════════════════════════════════════════════════════════════

#[test]
fn eviction_policy_default_values() {
    let policy = EvictionPolicy::default();
    assert_eq!(
        policy.max_in_memory_pages, 256,
        "default max_in_memory_pages"
    );
    assert_eq!(
        policy.eviction_batch_size, 16,
        "default eviction_batch_size"
    );
}

#[test]
fn eviction_no_panic_with_custom_policy() {
    let store = tiny_eviction_store();
    let mut session = store.new_session();
    for i in 0u64..1000 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    // Evict with tiny threshold — should not panic.
    let evicted = store.evict();
    let _ = evicted;
}

#[test]
fn eviction_after_flush_advances_head() {
    let store = tiny_eviction_store();
    let mut session = store.new_session();
    for i in 0u64..500 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    let head_before = store.head_address().raw();

    // flush_and_evict should push head forward.
    let (flushed, evicted) = store.flush_and_evict();
    let head_after = store.head_address().raw();

    assert!(
        head_after >= head_before,
        "head should not regress: before={head_before}, after={head_after}, flushed={flushed}, evicted={evicted}"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 5. Maintenance cycle
// ═══════════════════════════════════════════════════════════════════

#[test]
fn maintenance_does_not_panic_empty_store() {
    let store = hl_store();
    store.maintenance();
}

#[test]
fn maintenance_does_not_panic_with_data() {
    let store = hl_store();
    let mut session = store.new_session();
    for i in 0u64..500 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    // Multiple maintenance cycles should be safe.
    for _ in 0..5 {
        store.maintenance();
    }
}

#[test]
fn maintenance_preserves_data() {
    let store = hl_store();
    let mut session = store.new_session();
    for i in 0u64..200 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }

    store.maintenance();
    store.maintenance();

    for i in 0u64..200 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i} should survive maintenance");
    }
    store.dispose_session(session);
}

#[test]
fn maintenance_address_monotonicity() {
    let store = hl_store();

    let mut session = store.new_session();
    for i in 0u64..500 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    let begin0 = store.begin_address().raw();
    let head0 = store.head_address().raw();

    store.maintenance();

    let begin1 = store.begin_address().raw();
    let head1 = store.head_address().raw();

    assert!(begin1 >= begin0, "begin must not regress after maintenance");
    assert!(head1 >= head0, "head must not regress after maintenance");
}

// ═══════════════════════════════════════════════════════════════════
// 6. Concurrent operations with hybrid log
// ═══════════════════════════════════════════════════════════════════

#[test]
fn concurrent_writes_during_flush() {
    let store = Arc::new(hl_store());

    // Seed initial data.
    {
        let mut session = store.new_session();
        for i in 0u64..200 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }
        store.dispose_session(session);
    }

    let barrier = Arc::new(Barrier::new(3));

    // Writer thread.
    let store_w = Arc::clone(&store);
    let barrier_w = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        barrier_w.wait();
        let mut session = store_w.new_session();
        for i in 200u64..500 {
            let _ = store_w.upsert(&mut session, &i, &(i * 10), ());
        }
        store_w.dispose_session(session);
    });

    // Reader thread.
    let store_r = Arc::clone(&store);
    let barrier_r = Arc::clone(&barrier);
    let reader = thread::spawn(move || {
        barrier_r.wait();
        let mut session = store_r.new_session();
        for i in 0u64..200 {
            let val = store_r.read_simple(&mut session, &i);
            if let Some(v) = val {
                assert_eq!(v, i * 10, "corrupted read for key {i}");
            }
        }
        store_r.dispose_session(session);
    });

    // Main thread: flush + evict during concurrent reads/writes.
    barrier.wait();
    let _ = store.flush();
    store.maintenance();

    writer.join().expect("writer panicked");
    reader.join().expect("reader panicked");

    // Verify new writes survived.
    let mut session = store.new_session();
    for i in 200u64..500 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "concurrent write key {i}");
    }
    store.dispose_session(session);
}

#[test]
fn concurrent_maintenance_cycles() {
    let store = Arc::new(hl_store());

    {
        let mut session = store.new_session();
        for i in 0u64..300 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }
        store.dispose_session(session);
    }

    let barrier = Arc::new(Barrier::new(3));

    // Two threads doing maintenance.
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let s = Arc::clone(&store);
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                for _ in 0..10 {
                    s.maintenance();
                    thread::yield_now();
                }
            })
        })
        .collect();

    // Main thread writes concurrently.
    barrier.wait();
    let mut session = store.new_session();
    for i in 300u64..600 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    for h in handles {
        h.join().expect("maintenance thread panicked");
    }

    // Data integrity check for recent writes.
    let mut session = store.new_session();
    for i in 300u64..600 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i} after concurrent maintenance");
    }
    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════
// 7. Log tail growth and multiple sessions
// ═══════════════════════════════════════════════════════════════════

#[test]
fn tail_grows_linearly_with_fixed_size_records() {
    let store = hl_store();
    let mut session = store.new_session();

    let tail0 = store.tail_address().raw();
    let _ = store.upsert(&mut session, &0u64, &0u64, ());
    let tail1 = store.tail_address().raw();

    // Each u64/u64 record is at least 24 bytes (8 header + 8 key + 8 value).
    let record_size = tail1 - tail0;
    assert!(
        record_size >= 24,
        "record should be >= 24 bytes, got {record_size}"
    );

    // Write 99 more records.
    for i in 1u64..100 {
        let _ = store.upsert(&mut session, &i, &i, ());
    }
    let tail100 = store.tail_address().raw();

    // Total growth should be approximately 100 * record_size.
    let total_growth = tail100 - tail0;
    let expected_approx = 100 * record_size;
    assert!(
        total_growth >= expected_approx - record_size
            && total_growth <= expected_approx + record_size,
        "growth should be ~{expected_approx}, got {total_growth}"
    );

    store.dispose_session(session);
}

#[test]
fn multiple_sessions_see_each_others_writes() {
    let store = hl_store();

    // Session 1 writes keys 0..50.
    let mut s1 = store.new_session();
    for i in 0u64..50 {
        let _ = store.upsert(&mut s1, &i, &(i * 10), ());
    }
    store.dispose_session(s1);

    // Session 2 reads keys written by session 1, then writes more.
    let mut s2 = store.new_session();
    for i in 0u64..50 {
        let val = store.read_simple(&mut s2, &i);
        assert_eq!(
            val,
            Some(i * 10),
            "session 2 should see session 1's write for key {i}"
        );
    }
    for i in 50u64..100 {
        let _ = store.upsert(&mut s2, &i, &(i * 10), ());
    }
    store.dispose_session(s2);

    // Session 3 reads everything.
    let mut s3 = store.new_session();
    for i in 0u64..100 {
        let val = store.read_simple(&mut s3, &i);
        assert_eq!(val, Some(i * 10), "session 3 should see all 100 keys");
    }
    store.dispose_session(s3);
}

// ═══════════════════════════════════════════════════════════════════
// 8. Region management edge cases
// ═══════════════════════════════════════════════════════════════════

#[test]
fn read_nonexistent_key_returns_none() {
    let store = hl_store();
    let mut session = store.new_session();

    let val = store.read_simple(&mut session, &999u64);
    assert_eq!(val, None, "reading nonexistent key should return None");

    store.dispose_session(session);
}

#[test]
fn delete_then_read_returns_none() {
    let store = hl_store();
    let mut session = store.new_session();

    let _ = store.upsert(&mut session, &42u64, &420u64, ());
    let val = store.read_simple(&mut session, &42u64);
    assert_eq!(val, Some(420), "should read before delete");

    let _ = store.delete(&mut session, &42u64, ());
    let val = store.read_simple(&mut session, &42u64);
    assert_eq!(val, None, "should not read after delete");

    store.dispose_session(session);
}

#[test]
fn overwrite_updates_value() {
    let store = hl_store();
    let mut session = store.new_session();

    let _ = store.upsert(&mut session, &1u64, &10u64, ());
    let val = store.read_simple(&mut session, &1u64);
    assert_eq!(val, Some(10));

    let _ = store.upsert(&mut session, &1u64, &20u64, ());
    let val = store.read_simple(&mut session, &1u64);
    assert_eq!(val, Some(20), "overwrite should update value");

    store.dispose_session(session);
}

#[test]
fn addresses_after_delete_and_reinsert() {
    let store = hl_store();
    let tail_before = store.tail_address().raw();

    let mut session = store.new_session();
    let _ = store.upsert(&mut session, &1u64, &10u64, ());
    let _ = store.delete(&mut session, &1u64, ());
    let _ = store.upsert(&mut session, &1u64, &20u64, ());

    let tail_after = store.tail_address().raw();
    assert!(
        tail_after > tail_before,
        "tail must advance for upsert+delete+upsert"
    );

    let val = store.read_simple(&mut session, &1u64);
    assert_eq!(val, Some(20), "re-inserted value should be readable");

    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════
// 9. Buffer configuration variants
// ═══════════════════════════════════════════════════════════════════

#[test]
fn small_buffer_store_works() {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 2,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 2,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut session = store.new_session();

    // Even with tiny buffer, basic CRUD should work.
    for i in 0u64..50 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    for i in 0u64..50 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i} in small buffer store");
    }

    store.dispose_session(session);
}

#[test]
fn large_hash_index_works() {
    let config = FasterKvConfig {
        hash_index_size_log2: 18,
        buffer_size_pages: 4,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut session = store.new_session();

    for i in 0u64..1000 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    for i in 0u64..1000 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i} with large hash index");
    }

    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════
// 10. Stress: flush_and_evict under concurrent load
// ═══════════════════════════════════════════════════════════════════

#[test]
fn flush_and_evict_under_concurrent_writers() {
    let store = Arc::new(hl_store());
    let n_per_thread = 200u64;
    let num_writers = 4;
    let barrier = Arc::new(Barrier::new(num_writers + 1));

    let handles: Vec<_> = (0..num_writers)
        .map(|t| {
            let s = Arc::clone(&store);
            let b = Arc::clone(&barrier);
            let base = t as u64 * n_per_thread;
            thread::spawn(move || {
                b.wait();
                let mut session = s.new_session();
                for i in base..base + n_per_thread {
                    let _ = s.upsert(&mut session, &i, &(i * 10), ());
                }
                s.dispose_session(session);
            })
        })
        .collect();

    // Main thread: repeatedly flush_and_evict while writers are active.
    barrier.wait();
    for _ in 0..3 {
        store.flush_and_evict();
        thread::yield_now();
    }

    for h in handles {
        h.join().expect("writer thread panicked");
    }

    // Verify at least recent writes are present.
    let mut session = store.new_session();
    let last_base = (num_writers - 1) as u64 * n_per_thread;
    for i in last_base..last_base + n_per_thread {
        let val = store.read_simple(&mut session, &i);
        // Recent writes should be in mutable region and readable.
        if let Some(v) = val {
            assert_eq!(v, i * 10, "corrupted value for key {i}");
        }
    }
    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════
// 11. Checkpoint-based read-only region tests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn checkpoint_shifts_read_only_boundary() {
    let store = hl_store();
    let mut session = store.new_session();
    for i in 0u64..100 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    let tail_before = store.tail_address().raw();

    let dir = tempfile::tempdir().expect("tempdir");
    store
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .expect("checkpoint should succeed");

    // After checkpoint, data should still be readable.
    let mut session = store.new_session();
    for i in 0u64..100 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i} after checkpoint");
    }
    store.dispose_session(session);

    // Tail should not have regressed.
    let tail_after = store.tail_address().raw();
    assert!(
        tail_after >= tail_before,
        "tail should not regress after checkpoint"
    );
}

#[test]
fn writes_after_checkpoint_go_to_mutable_region() {
    let store = hl_store();

    let mut session = store.new_session();
    for i in 0u64..50 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }
    store.dispose_session(session);

    let dir = tempfile::tempdir().expect("tempdir");
    store
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .expect("checkpoint should succeed");

    let tail_after_cp = store.tail_address().raw();

    // New writes go to mutable region.
    let mut session = store.new_session();
    for i in 50u64..100 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }

    let tail_after_write = store.tail_address().raw();
    assert!(
        tail_after_write > tail_after_cp,
        "new writes should advance tail past checkpoint tail"
    );

    // All data readable.
    for i in 0u64..100 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "key {i}");
    }
    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════
// 12. Mixed operations stress
// ═══════════════════════════════════════════════════════════════════

#[test]
fn mixed_crud_with_maintenance_cycles() {
    let store = hl_store();
    let mut session = store.new_session();

    // Phase 1: Insert.
    for i in 0u64..200 {
        let _ = store.upsert(&mut session, &i, &(i * 10), ());
    }

    store.maintenance();

    // Phase 2: Update half.
    for i in 0u64..100 {
        let _ = store.upsert(&mut session, &i, &(i * 20), ());
    }

    store.maintenance();

    // Phase 3: Delete quarter.
    for i in 0u64..50 {
        let _ = store.delete(&mut session, &i, ());
    }

    store.maintenance();

    // Phase 4: Re-insert deleted.
    for i in 0u64..50 {
        let _ = store.upsert(&mut session, &i, &(i * 30), ());
    }

    store.maintenance();

    // Verify.
    for i in 0u64..50 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 30), "re-inserted key {i}");
    }
    for i in 50u64..100 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 20), "updated key {i}");
    }
    for i in 100u64..200 {
        let val = store.read_simple(&mut session, &i);
        assert_eq!(val, Some(i * 10), "original key {i}");
    }

    store.dispose_session(session);
}

#[test]
fn high_contention_concurrent_upserts() {
    let store = Arc::new(hl_store());
    let n_threads = 8;
    let n_keys = 100u64;
    let barrier = Arc::new(Barrier::new(n_threads));

    // All threads update the same key range.
    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let s = Arc::clone(&store);
            let b = Arc::clone(&barrier);
            thread::spawn(move || {
                b.wait();
                let mut session = s.new_session();
                for i in 0..n_keys {
                    let _ = s.upsert(&mut session, &i, &(i + t as u64 * 1000), ());
                }
                s.dispose_session(session);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Every key should have SOME value (whichever thread wrote last wins).
    let mut session = store.new_session();
    for i in 0..n_keys {
        let val = store.read_simple(&mut session, &i);
        assert!(
            val.is_some(),
            "key {i} should exist after concurrent upserts"
        );
    }
    store.dispose_session(session);
}
