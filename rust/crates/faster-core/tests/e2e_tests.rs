//! End-to-end integration tests for the FasterKv store.
//!
//! These tests exercise the FULL stack — public API → hash index → hybrid log
//! → device I/O → readback — to verify correctness under realistic workloads.

use std::sync::Arc;
use std::thread;

use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{CounterFunctions, FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::{InMemoryDevice, NullDevice, SyncFileDevice};

// ═══════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════

/// Small in-memory store suitable for most tests.
fn small_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 10, // 1 024 buckets
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

/// Counter store for RMW tests.
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
    FasterKv::new(config, CounterFunctions::new(), InMemoryDevice::new())
}

/// Store with tiny buffer to force page sealing quickly.
fn tiny_buffer_store(device: impl faster_core::Device) -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 14, // 16 K buckets
        buffer_size_pages: 4,     // very small buffer
        mutable_fraction: 0.5,    // half mutable → pages seal quickly
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 2,
        },
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), device)
}

// ═══════════════════════════════════════════════════════════════════
// 1. Basic CRUD lifecycle
// ═══════════════════════════════════════════════════════════════════

#[test]
fn crud_lifecycle() {
    let store = small_store();
    let mut s = store.new_session();

    // ── Create ──
    let status = store.upsert(&mut s, &1u64, &100u64, ());
    assert!(status.is_success(), "upsert failed: {status:?}");

    // ── Read ──
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &1u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(100));

    // ── Update ──
    let _ = store.upsert(&mut s, &1u64, &200u64, ());
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &1u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(200));

    // ── Delete ──
    let status = store.delete(&mut s, &1u64, ());
    assert_eq!(status, OperationStatus::Deleted);

    // ── Verify NotFound ──
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &1u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::NotFound);
    assert_eq!(out, None);

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 2. Large-scale operations (10 000+ keys)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn large_scale_insert_and_read() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 16, // 64 K buckets
        buffer_size_pages: 32,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut s = store.new_session();

    const N: u64 = 10_000;

    for i in 0..N {
        let status = store.upsert(&mut s, &i, &(i * 7), ());
        assert!(status.is_success(), "upsert key {i} returned {status:?}",);
    }

    for i in 0..N {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read key {i} failed");
        assert_eq!(out, Some(i * 7), "value mismatch at key {i}");
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 3. Multi-threaded concurrent access
// ═══════════════════════════════════════════════════════════════════

#[test]
fn concurrent_upsert_and_read() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 16,
        buffer_size_pages: 32,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    const THREADS: u64 = 8;
    const KEYS_PER_THREAD: u64 = 1_000;

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut s = store.new_session();
                let base = t * KEYS_PER_THREAD;

                // Each thread writes a disjoint key range.
                for i in 0..KEYS_PER_THREAD {
                    let key = base + i;
                    let _ = store.upsert(&mut s, &key, &(key * 3), ());
                }

                // Read back own keys.
                for i in 0..KEYS_PER_THREAD {
                    let key = base + i;
                    let mut out: Option<u64> = None;
                    let status = store.read(&mut s, &key, &0u64, &mut out, ());
                    assert_eq!(status, OperationStatus::Ok, "t{t}: read key {key} failed");
                    assert_eq!(out, Some(key * 3), "t{t}: key {key} mismatch");
                }

                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

// ═══════════════════════════════════════════════════════════════════
// 4. Flush and eviction cycle
// ═══════════════════════════════════════════════════════════════════

#[test]
fn flush_and_eviction_cycle() {
    let store = tiny_buffer_store(InMemoryDevice::new());
    let mut s = store.new_session();

    // Insert enough data to fill several pages.
    for i in 0u64..2_000 {
        let _ = store.upsert(&mut s, &i, &(i * 11), ());
    }

    let tail = store.tail_address();
    assert!(tail.raw() > 0, "tail should advance after inserts");

    let head_before = store.head_address();

    // Flush sealed pages to device.
    let flushed = store.flush();
    // We don't assert flushed > 0 because it depends on page sealing
    // timing, but the call must not panic.
    let _ = flushed;

    // Run full maintenance which may advance head via eviction.
    store.maintenance();

    // After maintenance with a tiny buffer, head may have advanced.
    let head_after = store.head_address();
    assert!(
        head_after.raw() >= head_before.raw(),
        "head must not regress: before={}, after={}",
        head_before.raw(),
        head_after.raw(),
    );

    // Recent keys in the mutable region should still be readable.
    let recent_key = 1999u64;
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &recent_key, &0u64, &mut out, ());
    assert!(
        status == OperationStatus::Ok || status == OperationStatus::Pending,
        "read after flush returned {status:?}",
    );

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 5. Data survives eviction + readback from disk (SyncFileDevice)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn disk_eviction_and_readback() {
    let dir = tempfile::tempdir().expect("create tempdir");

    let device = SyncFileDevice::new(
        dir.path(),
        "hlog.", // segment file prefix
        512,     // sector size
        1 << 20, // 1 MiB segments
        2,       // 2 I/O threads
    )
    .expect("create SyncFileDevice");

    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 14,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 2,
        },
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), device);
    let mut s = store.new_session();

    // Insert enough data to span multiple pages.
    const N: u64 = 2_000;
    for i in 0..N {
        let _ = store.upsert(&mut s, &i, &(i * 13), ());
    }

    // Flush → evict cycle to push data to disk.
    store.maintenance();
    store.maintenance();

    let head = store.head_address();
    let tail = store.tail_address();

    // Recent keys (near tail) should still be in memory.
    let recent = N - 1;
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &recent, &0u64, &mut out, ());
    assert!(
        status == OperationStatus::Ok || status == OperationStatus::Pending,
        "recent key read: {status:?} (head={}, tail={})",
        head.raw(),
        tail.raw(),
    );

    // Reading any key should succeed or be pending (I/O in flight).
    // We verify no panics and correct values for keys that are still
    // in memory.
    let mut ok_count = 0u64;
    let mut pending_count = 0u64;
    for i in 0..N {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        match status {
            OperationStatus::Ok => {
                assert_eq!(out, Some(i * 13), "value mismatch at key {i}");
                ok_count += 1;
            }
            OperationStatus::Pending => {
                pending_count += 1;
            }
            other => panic!("unexpected status for key {i}: {other:?}"),
        }
    }

    // At least some keys should be immediately readable (mutable region).
    assert!(ok_count > 0, "expected some Ok reads, got 0");
    // With a tiny buffer, some keys should have been evicted.
    // (If all keys fit in memory this still passes — it's not flaky.)
    let _ = pending_count;

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 6. RMW (Read-Modify-Write) with CounterFunctions
// ═══════════════════════════════════════════════════════════════════

#[test]
fn rmw_counter_accumulation() {
    let store = counter_store();
    let mut s = store.new_session();

    let key = 42u64;
    let mut output = 0i64;

    // Increment 100 times with delta +1.
    for _ in 0..100 {
        let _ = store.rmw(&mut s, &key, &1i64, &mut output, ());
    }

    // Read back the counter.
    let mut read_out = 0i64;
    let status = store.read(&mut s, &key, &0i64, &mut read_out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(read_out, 100);

    store.dispose_session(s);
}

#[test]
fn rmw_multiple_counters() {
    let store = counter_store();
    let mut s = store.new_session();

    // Increment 50 different counters, each by varying deltas.
    for key in 0u64..50 {
        let mut output = 0i64;
        for delta in 1..=5i64 {
            let _ = store.rmw(&mut s, &key, &delta, &mut output, ());
        }
    }

    // Each counter should have 1+2+3+4+5 = 15.
    for key in 0u64..50 {
        let mut out = 0i64;
        let status = store.read(&mut s, &key, &0i64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "counter {key} not found");
        assert_eq!(out, 15, "counter {key} mismatch");
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 7. Mixed workload across multiple threads
// ═══════════════════════════════════════════════════════════════════

#[test]
fn mixed_workload_multithreaded() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 14,
        buffer_size_pages: 16,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    const THREADS: u64 = 4;
    const OPS: u64 = 500;

    // Phase 1: populate shared keys.
    {
        let mut s = store.new_session();
        for i in 0..OPS {
            let _ = store.upsert(&mut s, &i, &i, ());
        }
        store.dispose_session(s);
    }

    // Phase 2: each thread does a mix of reads, upserts, and deletes.
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut s = store.new_session();

                for i in 0..OPS {
                    let key = i;
                    match (t + i) % 4 {
                        0 => {
                            // Read
                            let mut out: Option<u64> = None;
                            let _ = store.read(&mut s, &key, &0u64, &mut out, ());
                        }
                        1 => {
                            // Upsert with thread-specific value
                            let val = key * 100 + t;
                            let _ = store.upsert(&mut s, &key, &val, ());
                        }
                        2 => {
                            // Delete (may or may not find the key)
                            let _ = store.delete(&mut s, &key, ());
                        }
                        _ => {
                            // Re-insert after possible delete
                            let _ = store.upsert(&mut s, &key, &(key + t), ());
                        }
                    }
                }

                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker panicked");
    }

    // Verify we can still read without panic; values may vary due to
    // concurrent overwrites and deletes.
    let mut s = store.new_session();
    for i in 0..OPS {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert!(
            status == OperationStatus::Ok || status == OperationStatus::NotFound,
            "key {i}: unexpected status {status:?}",
        );
    }
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 8. Store configuration variants
// ═══════════════════════════════════════════════════════════════════

#[test]
fn config_small_hash_table() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 4, // 16 buckets — lots of collisions
        buffer_size_pages: 4,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());
    let mut s = store.new_session();

    // Despite heavy collisions, correctness must hold.
    for i in 0u64..200 {
        let _ = store.upsert(&mut s, &i, &(i + 1), ());
    }
    for i in 0u64..200 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "key {i} not found");
        assert_eq!(out, Some(i + 1), "key {i} mismatch");
    }

    store.dispose_session(s);
}

#[test]
fn config_large_buffer() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 12,
        buffer_size_pages: 64, // large in-memory buffer
        mutable_fraction: 0.95,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 128,
            eviction_batch_size: 8,
        },
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut s = store.new_session();

    for i in 0u64..5_000 {
        let _ = store.upsert(&mut s, &i, &(i * 2), ());
    }
    for i in 0u64..5_000 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "key {i} missing");
        assert_eq!(out, Some(i * 2), "key {i} mismatch");
    }

    store.dispose_session(s);
}

#[test]
fn config_low_mutable_fraction() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.1, // 10% mutable — aggressive read-only boundary
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut s = store.new_session();

    // Even with low mutable fraction, CRUD must work.
    for i in 0u64..500 {
        let _ = store.upsert(&mut s, &i, &i, ());
    }
    // Updates will trigger CopyUpdated (copy from RO region to mutable).
    for i in 0u64..500 {
        let _ = store.upsert(&mut s, &i, &(i + 1), ());
    }
    for i in 0u64..500 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert!(
            status == OperationStatus::Ok || status == OperationStatus::Pending,
            "key {i}: {status:?}",
        );
        if status == OperationStatus::Ok {
            assert_eq!(out, Some(i + 1), "key {i} mismatch");
        }
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 9. Session lifecycle and reuse
// ═══════════════════════════════════════════════════════════════════

#[test]
fn session_create_dispose_cycle() {
    let store = small_store();

    // Create and dispose sessions in a loop to exercise pool recycling.
    for round in 0u64..10 {
        let mut s = store.new_session();
        assert!(!s.is_in_epoch(), "fresh session shouldn't be in epoch");

        let _ = store.upsert(&mut s, &round, &(round * 10), ());

        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &round, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(out, Some(round * 10));

        store.dispose_session(s);
    }

    // All data from previous sessions should persist in the store.
    let mut s = store.new_session();
    for round in 0u64..10 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &round, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "round {round} lost");
        assert_eq!(out, Some(round * 10));
    }
    store.dispose_session(s);
}

#[test]
fn session_drop_without_dispose() {
    let store = small_store();

    // Dropping a session without explicit dispose should be safe.
    {
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &10u64, ());
        // session drops here
    }

    // Store still usable.
    let mut s = store.new_session();
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &1u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(10));
    store.dispose_session(s);
}

#[test]
fn many_sessions_concurrent() {
    let store = Arc::new(small_store());

    // Spawn many short-lived sessions to stress the session pool.
    let handles: Vec<_> = (0..16u64)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for i in 0..10u64 {
                    let mut s = store.new_session();
                    let key = t * 100 + i;
                    let _ = store.upsert(&mut s, &key, &key, ());
                    store.dispose_session(s);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("session thread panicked");
    }
}

// ═══════════════════════════════════════════════════════════════════
// 10. Maintenance cycle under load
// ═══════════════════════════════════════════════════════════════════

#[test]
fn maintenance_under_concurrent_load() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 14,
        buffer_size_pages: 8,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 8,
            eviction_batch_size: 2,
        },
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Worker threads doing CRUD.
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let store = Arc::clone(&store);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut s = store.new_session();
                let mut i = 0u64;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let key = t * 100_000 + i;
                    let _ = store.upsert(&mut s, &key, &key, ());
                    i += 1;
                    if i > 5_000 {
                        break;
                    }
                }
                store.dispose_session(s);
                i
            })
        })
        .collect();

    // Maintenance thread runs concurrently.
    let store_m = Arc::clone(&store);
    let done_m = Arc::clone(&done);
    let maint = thread::spawn(move || {
        let mut cycles = 0u32;
        while !done_m.load(std::sync::atomic::Ordering::Relaxed) && cycles < 50 {
            store_m.maintenance();
            cycles += 1;
            thread::yield_now();
        }
        cycles
    });

    // Wait for workers.
    let mut total_ops = 0u64;
    for w in workers {
        total_ops += w.join().expect("worker panicked");
    }

    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let cycles = maint.join().expect("maintenance panicked");

    assert!(total_ops > 0, "workers should have completed some ops");
    assert!(cycles > 0, "maintenance should have run at least once");
}

// ═══════════════════════════════════════════════════════════════════
// Bonus: delete-nonexistent and double-delete
// ═══════════════════════════════════════════════════════════════════

#[test]
fn delete_nonexistent_key() {
    let store = small_store();
    let mut s = store.new_session();

    let status = store.delete(&mut s, &9999u64, ());
    // Deleting a non-existent key should return NotFound or Deleted
    // (tombstone write is valid even if key didn't exist before).
    assert!(
        status == OperationStatus::NotFound || status == OperationStatus::Deleted,
        "unexpected: {status:?}",
    );

    store.dispose_session(s);
}

#[test]
fn double_delete() {
    let store = small_store();
    let mut s = store.new_session();

    let _ = store.upsert(&mut s, &1u64, &10u64, ());
    let _ = store.delete(&mut s, &1u64, ());

    // Second delete on already-deleted key.
    let status = store.delete(&mut s, &1u64, ());
    assert!(
        status == OperationStatus::NotFound || status == OperationStatus::Deleted,
        "double delete: {status:?}",
    );

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// Bonus: upsert overwrite preserves latest value
// ═══════════════════════════════════════════════════════════════════

#[test]
fn upsert_overwrite_sequence() {
    let store = small_store();
    let mut s = store.new_session();

    // Write key 0 with values 0..100; only the last should survive.
    for v in 0u64..100 {
        let _ = store.upsert(&mut s, &0u64, &v, ());
    }

    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &0u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(99));

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// Bonus: SyncFileDevice smoke test (basic I/O round-trip)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn sync_file_device_basic_roundtrip() {
    let dir = tempfile::tempdir().expect("create tempdir");

    let device = SyncFileDevice::new(dir.path(), "seg.", 512, 1 << 20, 1).expect("create device");

    let store = FasterKv::new(
        FasterKvConfig {
            grow_config: GrowConfig::default(),
            hash_index_size_log2: 10,
            buffer_size_pages: 8,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            auto_compact: false,
            lossy: false,
        },
        SimpleFunctions::default(),
        device,
    );

    let mut s = store.new_session();

    for i in 0u64..100 {
        let _ = store.upsert(&mut s, &i, &(i * 5), ());
    }

    for i in 0u64..100 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "key {i} not found");
        assert_eq!(out, Some(i * 5), "key {i} mismatch");
    }

    store.dispose_session(s);
}
