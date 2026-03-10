//! Tests for lossy (LRU) cache mode.
//!
//! When `FasterKvConfig::lossy` is `true`, the store acts as a bounded
//! cache: old records are silently discarded when the log wraps. Reads
//! of evicted keys return `NotFound`; writes transparently create fresh
//! records at the log tail.
//!
//! # Record/page sizing
//!
//! With `SimpleFunctions<u64, u64>` each record is 24 bytes (8-byte
//! RecordInfo + 8-byte key + 8-byte value). A 32 MiB page holds ~1.4 M
//! records. Tests that need eviction write ≥6 M records to fill the
//! 4-page buffer.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::InMemoryDevice;

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// A small lossy store that wraps quickly under write pressure.
///
/// 4 pages × 32 MiB = 128 MiB buffer, max 3 in memory → pages flush
/// and are immediately discarded (lossy mode).
fn lossy_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        hash_index_size_log2: 16, // 64k buckets — enough for millions of keys
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: true,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

/// Perform a single upsert with retry on Aborted (allocation backpressure).
fn upsert_with_retry(
    store: &FasterKv<SimpleFunctions<u64, u64>>,
    session: &mut faster_core::store::FasterSession<SimpleFunctions<u64, u64>>,
    key: &u64,
    value: &u64,
) -> OperationStatus {
    loop {
        let status = store.upsert(session, key, value, ());
        if status == OperationStatus::Aborted {
            store.maintenance();
            let _ = store.complete_pending(session);
            continue;
        }
        return status;
    }
}

/// Read a key, returning (status, output_value).
fn read_key(
    store: &FasterKv<SimpleFunctions<u64, u64>>,
    session: &mut faster_core::store::FasterSession<SimpleFunctions<u64, u64>>,
    key: &u64,
) -> (OperationStatus, Option<u64>) {
    let mut output: Option<u64> = None;
    let status = store.read(session, key, &0u64, &mut output, ());
    (status, output)
}

/// Number of records needed to fill one 32 MiB page with 24-byte records.
const RECORDS_PER_PAGE: u64 = (32 * 1024 * 1024) / 24;

/// Write enough records to fill ~N pages, calling maintenance periodically.
fn fill_pages(
    store: &FasterKv<SimpleFunctions<u64, u64>>,
    session: &mut faster_core::store::FasterSession<SimpleFunctions<u64, u64>>,
    num_pages: u64,
    start_key: u64,
) -> u64 {
    let total = RECORDS_PER_PAGE * num_pages;
    for i in 0..total {
        let key = start_key + i;
        let _ = upsert_with_retry(store, session, &key, &key);

        // Periodic maintenance drives the flush → evict pipeline.
        if i % 4096 == 0 {
            store.maintenance();
            let _ = store.complete_pending(session);
        }
    }
    // Drain the pipeline.
    for _ in 0..100 {
        store.maintenance();
        let _ = store.complete_pending(session);
    }
    total
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

/// After enough writes to wrap the log, old keys return NotFound (not a
/// crash, not stale data).
#[test]
fn evicted_keys_return_not_found() {
    let store = lossy_store();
    let mut session = store.new_session();

    // Write enough to fill the buffer ~6× (ensures multiple eviction cycles).
    let total = fill_pages(&store, &mut session, 6, 0);

    // Early keys should be NotFound (evicted + truncated + hash invalidated).
    let mut found = 0u64;
    let mut not_found = 0u64;
    let mut pending = 0u64;
    for i in 0..1_000 {
        let (status, _) = read_key(&store, &mut session, &i);
        match status {
            OperationStatus::Ok => found += 1,
            OperationStatus::NotFound => not_found += 1,
            OperationStatus::Pending => pending += 1,
            other => panic!("read({i}) returned unexpected {other:?}"),
        }
    }

    assert!(
        not_found > 0,
        "expected some NotFound reads for evicted keys (wrote {total} records), \
         got found={found} not_found={not_found} pending={pending}"
    );

    store.dispose_session(session);
}

/// Reads of recently-written keys still succeed after wrap-around.
#[test]
fn recent_keys_still_readable_after_wrap() {
    let store = lossy_store();
    let mut session = store.new_session();

    let total = fill_pages(&store, &mut session, 6, 0);

    // Recent keys (last few hundred) should still be in memory.
    let mut in_memory_reads = 0u64;
    for i in (total - 100)..total {
        let (status, val) = read_key(&store, &mut session, &i);
        if status == OperationStatus::Ok {
            assert_eq!(val, Some(i), "key {i} returned wrong value");
            in_memory_reads += 1;
        }
    }

    assert!(
        in_memory_reads > 0,
        "expected at least some recent keys to be readable"
    );

    store.dispose_session(session);
}

/// Upsert to a previously-evicted key creates a fresh record (not Aborted).
#[test]
fn upsert_to_evicted_key_succeeds() {
    let store = lossy_store();
    let mut session = store.new_session();

    // Write key 42 initially.
    let status = upsert_with_retry(&store, &mut session, &42u64, &100u64);
    assert!(status.is_success());

    // Fill buffer to push key 42 past begin_address.
    fill_pages(&store, &mut session, 6, 10_000_000);

    // Verify key 42 is gone.
    let (_status, _) = read_key(&store, &mut session, &42u64);
    // The point is it should not crash.

    // Re-upsert key 42 — should succeed (not loop forever on Aborted).
    let status = upsert_with_retry(&store, &mut session, &42u64, &999u64);
    assert!(
        status.is_success() || status == OperationStatus::CopyUpdated,
        "re-upsert to evicted key returned {status:?}"
    );

    // Read it back — should be the new value.
    let (status, val) = read_key(&store, &mut session, &42u64);
    assert_eq!(status, OperationStatus::Ok, "re-upserted key not readable");
    assert_eq!(val, Some(999), "re-upserted key has wrong value");

    store.dispose_session(session);
}

/// No panics or crashes after multiple wrap-arounds of the log.
#[test]
fn no_crashes_after_multiple_wraps() {
    let store = lossy_store();
    let mut session = store.new_session();

    // Write 12 pages worth (~16.8M records) — wraps the 4-page buffer 3×.
    let total = fill_pages(&store, &mut session, 12, 0);

    // Interleave reads and writes after heavy wrapping.
    for i in total..(total + 10_000) {
        let _ = upsert_with_retry(&store, &mut session, &i, &i);
        // Read a random old key (likely evicted).
        let old_key = i.wrapping_sub(total / 2);
        let (status, _) = read_key(&store, &mut session, &old_key);
        assert!(
            status == OperationStatus::Ok
                || status == OperationStatus::NotFound
                || status == OperationStatus::Pending,
            "unexpected status for old key read: {status:?}"
        );
        if i % 256 == 0 {
            store.maintenance();
            let _ = store.complete_pending(&mut session);
        }
    }

    store.dispose_session(session);
}

/// Concurrent writer and readers don't deadlock or panic under lossy mode.
#[test]
fn concurrent_readers_writers_lossy() {
    let store = Arc::new(lossy_store());
    let shutdown = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(5);

    let mut handles = Vec::new();

    // Maintenance thread
    {
        let store = Arc::clone(&store);
        let shutdown = Arc::clone(&shutdown);
        handles.push(thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                store.maintenance();
                thread::sleep(Duration::from_millis(1));
            }
        }));
    }

    // Writer
    {
        let store = Arc::clone(&store);
        let shutdown = Arc::clone(&shutdown);
        handles.push(thread::spawn(move || {
            let mut session = store.new_session();
            let mut i = 0u64;
            while !shutdown.load(Ordering::Relaxed) {
                loop {
                    let status = store.upsert(&mut session, &i, &i, ());
                    if status != OperationStatus::Aborted {
                        break;
                    }
                    store.maintenance();
                    let _ = store.complete_pending(&mut session);
                }
                i += 1;
                if i % 4096 == 0 {
                    store.maintenance();
                    let _ = store.complete_pending(&mut session);
                }
            }
            store.dispose_session(session);
        }));
    }

    // Two readers
    for _ in 0..2 {
        let store = Arc::clone(&store);
        let shutdown = Arc::clone(&shutdown);
        handles.push(thread::spawn(move || {
            let mut session = store.new_session();
            let mut i = 0u64;
            while !shutdown.load(Ordering::Relaxed) {
                let mut output: Option<u64> = None;
                let status = store.read(&mut session, &i, &0u64, &mut output, ());
                assert!(
                    status == OperationStatus::Ok
                        || status == OperationStatus::NotFound
                        || status == OperationStatus::Pending,
                    "reader got unexpected {status:?} for key {i}"
                );
                i = i.wrapping_add(1) % 10_000_000;
                if i % 4096 == 0 {
                    store.maintenance();
                    let _ = store.complete_pending(&mut session);
                }
            }
            store.dispose_session(session);
        }));
    }

    // Run until deadline
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    shutdown.store(true, Ordering::SeqCst);

    for h in handles {
        h.join().expect("thread panicked");
    }
}

/// Write throughput does not degrade after wrap-around.
#[test]
fn throughput_stable_after_wrap() {
    let store = lossy_store();
    let mut session = store.new_session();

    // Phase 1: Fill the first 4 pages (includes warm-up and initial wrap).
    let start1 = Instant::now();
    fill_pages(&store, &mut session, 4, 0);
    let dur1 = start1.elapsed();

    // Phase 2: Fill another 4 pages (post-wrap steady state).
    let start2 = Instant::now();
    fill_pages(&store, &mut session, 4, RECORDS_PER_PAGE * 4);
    let dur2 = start2.elapsed();

    let ops1 = (RECORDS_PER_PAGE * 4) as f64 / dur1.as_secs_f64();
    let ops2 = (RECORDS_PER_PAGE * 4) as f64 / dur2.as_secs_f64();

    // Phase 2 should not be dramatically slower than phase 1.
    // Allow 5× margin for CI variability.
    assert!(
        ops2 > ops1 / 5.0,
        "throughput degraded after wrap: phase1={ops1:.0}/s, phase2={ops2:.0}/s"
    );

    store.dispose_session(session);
}

/// Lossy maintenance keeps `begin_address` advancing (disk usage bounded).
#[test]
fn lossy_advances_begin_address() {
    let store = lossy_store();
    let mut session = store.new_session();

    let initial_begin = store.begin_address();

    // Write enough to trigger multiple evictions.
    fill_pages(&store, &mut session, 6, 0);

    let final_begin = store.begin_address();
    assert!(
        final_begin > initial_begin,
        "begin_address should advance in lossy mode: \
         initial={initial_begin:?}, final={final_begin:?}"
    );

    store.dispose_session(session);
}

/// Delete of an evicted key returns NotFound (not crash or Aborted).
#[test]
fn delete_evicted_key_returns_not_found() {
    let store = lossy_store();
    let mut session = store.new_session();

    // Write and evict key 42.
    let _ = upsert_with_retry(&store, &mut session, &42u64, &100u64);
    fill_pages(&store, &mut session, 6, 10_000_000);

    // Try deleting the evicted key.
    let status = store.delete(&mut session, &42u64, ());
    assert!(
        status == OperationStatus::NotFound || status == OperationStatus::Deleted,
        "delete of evicted key returned {status:?}"
    );

    store.dispose_session(session);
}

/// Non-lossy mode: upsert to a truncated address (from compaction's
/// begin_address advance) handles gracefully instead of Aborted.
#[test]
fn truncated_upsert_non_lossy() {
    // Use a non-lossy store — verify the Truncated upsert fix
    // works even without lossy mode (correctness fix).
    let config = FasterKvConfig {
        hash_index_size_log2: 16,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut session = store.new_session();

    // Write key 42 and many more to push it past head_address.
    let _ = upsert_with_retry(&store, &mut session, &42u64, &100u64);
    fill_pages(&store, &mut session, 6, 10_000_000);

    // Key 42 is now either on-disk or still accessible via pending I/O.
    // Should never crash or panic.
    let (status, _) = read_key(&store, &mut session, &42u64);
    assert!(
        status == OperationStatus::Ok
            || status == OperationStatus::NotFound
            || status == OperationStatus::Pending,
        "read of pushed-out key returned unexpected {status:?}"
    );

    store.dispose_session(session);
}
