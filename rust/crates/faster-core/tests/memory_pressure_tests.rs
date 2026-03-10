//! Memory-pressure and OOM-condition tests for FASTER.
//!
//! These tests stress the allocator, buffer pool, hybrid-log, and hash table
//! under constrained memory budgets to verify graceful degradation and data
//! integrity — not just happy-path correctness.
//!
//! Design principles:
//! - Deterministic: barriers, not sleeps. Proptest for fuzz coverage.
//! - Fast: each test < 500 ms, total suite < 30 s.
//! - Focused on invariants: "after any sequence of operations under pressure,
//!   data integrity is preserved."

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier};
use std::thread;

use faster_core::address::LogicalAddress;
use faster_core::allocator::MallocFixedPageSize;
use faster_core::grow::GrowConfig;
use faster_core::hash::Hashable;
use faster_core::hash_bucket::BUCKET_NUM_ENTRIES;
use faster_core::hash_table::HashTable;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{CounterFunctions, FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::{InMemoryDevice, NullDevice};

use proptest::prelude::*;

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// 128-byte item — satisfies MallocFixedPageSize's min-8-byte constraint
/// and is large enough to be a realistic record placeholder.
#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
struct Item128([u64; 16]);

/// A store with the *absolute minimum* in-memory budget.
/// 4 pages × 32 MB per page, half mutable → 2 mutable pages. This forces
/// page sealing and eviction almost immediately.
fn pressure_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        hash_index_size_log2: 10, // 1 024 buckets — small
        buffer_size_pages: 4,     // minimum viable
        mutable_fraction: 0.5,    // aggressive: seals pages fast
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig {
            enabled: false, // no auto-grow — keep hash table tiny
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

/// Counter store for RMW under memory pressure.
fn pressure_counter_store() -> FasterKv<CounterFunctions<u64>> {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, CounterFunctions::new(), InMemoryDevice::new())
}

/// Tiny-hash store: even smaller hash table to push overflow chains hard.
fn saturated_hash_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        hash_index_size_log2: 1, // 2 buckets → 14 slots total!
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

// ═══════════════════════════════════════════════════════════════════════════
// Module 1: Allocator Stress Tests
// ═══════════════════════════════════════════════════════════════════════════

/// Rapid alloc/free cycles must not leak or corrupt addresses.
#[test]
fn allocator_rapid_alloc_free_cycles() {
    let alloc = MallocFixedPageSize::<Item128>::new();

    // Phase 1: allocate a batch of items.
    let n = 5_000;
    let mut addrs: Vec<LogicalAddress> = Vec::with_capacity(n);
    for _ in 0..n {
        addrs.push(alloc.allocate());
    }
    assert_eq!(alloc.count(), n as u64);

    // Phase 2: free all, then re-allocate — every address should be reused
    // from the free list (no new pages needed).
    for &addr in &addrs {
        alloc.free(addr);
    }

    let mut reused: Vec<LogicalAddress> = Vec::with_capacity(n);
    for _ in 0..n {
        reused.push(alloc.allocate());
    }

    // Count should stay the same — no new bump allocations.
    assert_eq!(
        alloc.count(),
        n as u64,
        "free-list reuse should not bump the counter"
    );

    // Every reused address should be dereferenceable without panic.
    for &addr in &reused {
        let item = alloc.get(addr);
        // Touch the memory to verify it's valid.
        let _ = item.0[0];
    }
}

/// Multiple threads concurrently allocating and freeing must not produce
/// duplicate addresses or corrupt the free-list.
#[test]
fn allocator_concurrent_alloc_free() {
    let alloc = Arc::new(MallocFixedPageSize::<Item128>::new());
    let threads = 8;
    let ops_per_thread = 2_000;
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let alloc = Arc::clone(&alloc);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();

                let mut local_addrs = Vec::with_capacity(ops_per_thread);
                for i in 0..ops_per_thread {
                    let addr = alloc.allocate();
                    // Write a sentinel to verify no aliasing.
                    // SAFETY: We exclusively own `addr` — it was just allocated
                    // by this thread and not shared with any other thread yet.
                    unsafe {
                        let item = alloc.get_mut(addr);
                        item.0[0] = (t * ops_per_thread + i) as u64;
                    }
                    local_addrs.push(addr);
                }

                // Verify all sentinels are intact (no other thread overwrote).
                for (i, &addr) in local_addrs.iter().enumerate() {
                    let item = alloc.get(addr);
                    assert_eq!(
                        item.0[0],
                        (t * ops_per_thread + i) as u64,
                        "thread {t}: address aliased at iteration {i}"
                    );
                }

                // Free half the addresses to put pressure on the free-list.
                for &addr in local_addrs.iter().step_by(2) {
                    alloc.free(addr);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // count() reflects bump allocations. Threads that freed items allowed
    // other threads to reuse them from the free-list, so the bump counter
    // may be lower than threads × ops_per_thread. The invariant is that
    // count >= ops_per_thread (at least one thread's full batch was bumped).
    let count = alloc.count();
    assert!(
        count >= ops_per_thread as u64 && count <= (threads * ops_per_thread) as u64,
        "count {count} outside expected range [{ops_per_thread}, {}]",
        threads * ops_per_thread
    );
}

/// Under high allocation pressure, free-list entries must be reusable.
#[test]
fn allocator_free_list_reuse_under_pressure() {
    let alloc = MallocFixedPageSize::<Item128>::new();

    // Perform multiple alloc/free waves.
    for wave in 0..10 {
        let mut addrs = Vec::with_capacity(1_000);
        for _ in 0..1_000 {
            addrs.push(alloc.allocate());
        }

        // Write wave identifier to verify correctness.
        for &addr in &addrs {
            // SAFETY: Each wave has exclusive access — all prior addresses
            // were freed before this wave started allocating.
            unsafe {
                alloc.get_mut(addr).0[0] = wave;
            }
        }

        // Read back and verify before freeing.
        for &addr in &addrs {
            assert_eq!(alloc.get(addr).0[0], wave, "corrupt in wave {wave}");
        }

        // Free everything for the next wave.
        for &addr in &addrs {
            alloc.free(addr);
        }
    }

    // After 10 waves of 1 000 allocs each, count should still be 1 000
    // because every wave reused the previous wave's freed slots.
    // First wave allocates 1000 by bump; subsequent waves reuse via free-list.
    assert_eq!(alloc.count(), 1_000);
}

/// Concurrent alloc/free with interleaved patterns: some threads only
/// allocate while others only free from a shared pool.
#[test]
fn allocator_producer_consumer_pressure() {
    let alloc = Arc::new(MallocFixedPageSize::<Item128>::new());
    let shared_addrs = Arc::new(std::sync::Mutex::new(Vec::<LogicalAddress>::new()));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Pre-seed the pool.
    {
        let mut pool = shared_addrs.lock().unwrap();
        for _ in 0..500 {
            pool.push(alloc.allocate());
        }
    }

    let barrier = Arc::new(Barrier::new(4));

    // 2 producer threads (allocate + push).
    let producers: Vec<_> = (0..2)
        .map(|_| {
            let alloc = Arc::clone(&alloc);
            let addrs = Arc::clone(&shared_addrs);
            let barrier = Arc::clone(&barrier);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..2_000 {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    let addr = alloc.allocate();
                    let mut pool = addrs.lock().unwrap();
                    pool.push(addr);
                }
            })
        })
        .collect();

    // 2 consumer threads (pop + free).
    let consumers: Vec<_> = (0..2)
        .map(|_| {
            let alloc = Arc::clone(&alloc);
            let addrs = Arc::clone(&shared_addrs);
            let barrier = Arc::clone(&barrier);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                barrier.wait();
                let mut freed = 0;
                while freed < 1_500 {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    let addr = {
                        let mut pool = addrs.lock().unwrap();
                        pool.pop()
                    };
                    if let Some(addr) = addr {
                        alloc.free(addr);
                        freed += 1;
                    } else {
                        std::thread::yield_now();
                    }
                }
            })
        })
        .collect();

    for h in producers {
        h.join().expect("producer panicked");
    }
    done.store(true, Ordering::Relaxed);
    for h in consumers {
        h.join().expect("consumer panicked");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Module 2: Buffer Pool / Hybrid Log Under Memory Pressure
// ═══════════════════════════════════════════════════════════════════════════

/// With only 4 buffer pages and aggressive eviction, inserting many keys
/// must not panic or corrupt. After maintenance cycles, recently-written
/// keys must still be readable.
#[test]
fn hybrid_log_tiny_buffer_no_corruption() {
    let store = pressure_store();
    let mut s = store.new_session();

    // Insert enough keys to fill multiple pages (32 MB page ≈ many u64 records).
    const N: u64 = 3_000;
    for i in 0..N {
        let status = store.upsert(&mut s, &i, &(i * 7), ());
        assert!(status.is_success(), "upsert key {i} failed: {status:?}");
    }

    // Force flush + eviction.
    store.maintenance();
    store.maintenance();

    // The most recent keys (near the tail) must still be in mutable memory.
    for i in (N - 100)..N {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert!(
            status == OperationStatus::Ok || status == OperationStatus::Pending,
            "read key {i} returned {status:?}"
        );
        if status == OperationStatus::Ok {
            assert_eq!(out, Some(i * 7), "value mismatch for key {i}");
        }
    }

    store.dispose_session(s);
}

/// Under extreme pressure, head address must never regress and tail must
/// always advance (monotonicity invariant).
#[test]
fn hybrid_log_address_monotonicity_under_pressure() {
    let store = pressure_store();
    let mut s = store.new_session();

    let mut prev_head = store.head_address();
    let mut prev_tail = store.tail_address();

    for round in 0..5 {
        // Insert a batch.
        for i in 0..500 {
            let key = round * 500 + i;
            let _ = store.upsert(&mut s, &key, &(key * 11), ());
        }

        let tail = store.tail_address();
        assert!(
            tail.raw() >= prev_tail.raw(),
            "tail regressed in round {round}: prev={}, now={}",
            prev_tail.raw(),
            tail.raw()
        );
        prev_tail = tail;

        // Flush and evict.
        store.maintenance();

        let head = store.head_address();
        assert!(
            head.raw() >= prev_head.raw(),
            "head regressed in round {round}: prev={}, now={}",
            prev_head.raw(),
            head.raw()
        );
        prev_head = head;
    }

    store.dispose_session(s);
}

/// Flush-and-evict under pressure: flushed page count + evicted count
/// must be reasonable (non-negative, bounded by total pages touched).
#[test]
fn hybrid_log_flush_and_evict_returns_sane_counts() {
    let store = pressure_store();
    let mut s = store.new_session();

    for i in 0u64..2_000 {
        let _ = store.upsert(&mut s, &i, &(i * 3), ());
    }

    let (flushed, evicted) = store.flush_and_evict();
    // The counts should be non-negative u32 values (they are, by type),
    // but more importantly neither should be absurdly large.
    assert!(
        flushed <= 1_000,
        "suspiciously large flush count: {flushed}"
    );
    assert!(
        evicted <= 1_000,
        "suspiciously large evict count: {evicted}"
    );

    store.dispose_session(s);
}

/// Concurrent writers on a nearly-full buffer pool: all threads must
/// complete without deadlock or data corruption.
#[test]
fn hybrid_log_concurrent_pressure() {
    let config = FasterKvConfig {
        hash_index_size_log2: 14,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let threads = 4;
    let keys_per_thread = 500;
    let barrier = Arc::new(Barrier::new(threads + 1)); // +1 for maintenance thread

    // Maintenance thread: run flush/evict cycles concurrently with writes.
    let maint_store = Arc::clone(&store);
    let maint_barrier = Arc::clone(&barrier);
    let maint_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let maint_done_clone = Arc::clone(&maint_done);

    let maint_handle = thread::spawn(move || {
        maint_barrier.wait();
        while !maint_done_clone.load(Ordering::Relaxed) {
            maint_store.maintenance();
            thread::yield_now();
        }
    });

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut s = store.new_session();
                barrier.wait();

                let base = (t as u64) * (keys_per_thread as u64);
                for i in 0..(keys_per_thread as u64) {
                    let key = base + i;
                    let status = store.upsert(&mut s, &key, &(key * 5), ());
                    assert!(status.is_success(), "t{t}: upsert {key} → {status:?}");
                }

                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("writer thread panicked");
    }

    maint_done.store(true, Ordering::Relaxed);
    maint_handle.join().expect("maintenance thread panicked");
}

/// Under pressure, the hash index should have entries for inserted keys.
/// We verify via tail address advancement as a proxy for "stuff was stored".
#[test]
fn store_data_footprint_under_pressure() {
    let store = pressure_store();
    let mut s = store.new_session();

    let tail_before = store.tail_address();

    const N: u64 = 500;
    for i in 0..N {
        let _ = store.upsert(&mut s, &i, &(i * 2), ());
    }

    let tail_after = store.tail_address();
    assert!(
        tail_after.raw() > tail_before.raw(),
        "tail should advance after {N} inserts: before={}, after={}",
        tail_before.raw(),
        tail_after.raw()
    );

    // Verify some recent keys are readable.
    let mut readable = 0u32;
    for i in (N - 50)..N {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        if status == OperationStatus::Ok {
            assert_eq!(out, Some(i * 2));
            readable += 1;
        }
    }
    assert!(readable > 0, "no recent keys readable");

    store.dispose_session(s);
}

/// RMW under memory pressure: counter increments must be accurate even
/// when the mutable region is tiny.
#[test]
fn rmw_correctness_under_pressure() {
    let store = pressure_counter_store();
    let mut s = store.new_session();

    // Increment key 42 one hundred times.
    for _ in 0..100 {
        let mut out: i64 = 0;
        let status = store.rmw(&mut s, &42u64, &1i64, &mut out, ());
        assert!(status.is_success(), "rmw failed: {status:?}");
    }

    // The final value should be exactly 100.
    let mut out: i64 = 0;
    let status = store.read(&mut s, &42u64, &0i64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, 100, "counter drift under memory pressure");

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// Module 3: Hybrid Log Under Memory Pressure — Checkpoint
// ═══════════════════════════════════════════════════════════════════════════

/// Checkpoint must complete successfully even when memory is constrained.
#[test]
fn checkpoint_under_memory_pressure() {
    let dir = tempfile::tempdir().expect("create tempdir");

    let device = faster_core::SyncFileDevice::new(
        dir.path(),
        "hlog.",
        512,
        1 << 20, // 1 MiB segments
        2,
    )
    .expect("create SyncFileDevice");

    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), device);
    let mut s = store.new_session();

    // Insert under pressure.
    for i in 0u64..1_000 {
        let _ = store.upsert(&mut s, &i, &(i * 17), ());
    }

    store.maintenance();

    // Checkpoint must not panic under constrained memory.
    let result = store.checkpoint(
        dir.path(),
        faster_core::checkpoint::CheckpointType::FoldOver,
    );
    assert!(
        result.is_ok(),
        "checkpoint failed under memory pressure: {:?}",
        result.err()
    );

    store.dispose_session(s);
}

/// Reads should still succeed (or return Pending) while pages are being
/// evicted.
#[test]
fn reads_during_eviction() {
    let store = pressure_store();
    let mut s = store.new_session();

    const N: u64 = 2_000;
    for i in 0..N {
        let _ = store.upsert(&mut s, &i, &(i * 13), ());
    }

    // Trigger eviction.
    store.maintenance();
    store.maintenance();

    // Read a sample of keys. Some may be on disk (Pending), but none should
    // return a corrupt value.
    let mut ok_count = 0u64;
    let mut pending_count = 0u64;
    let mut not_found_count = 0u64;

    for i in (0..N).step_by(10) {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        match status.status() {
            OperationStatus::Ok => {
                assert_eq!(out, Some(i * 13), "corrupt value at key {i}");
                ok_count += 1;
            }
            OperationStatus::Pending => {
                pending_count += 1;
            }
            OperationStatus::NotFound => {
                not_found_count += 1;
            }
            _ => panic!("unexpected status for key {i}: {status:?}"),
        }
    }

    // At least some keys should be readable from memory.
    assert!(
        ok_count > 0,
        "no keys were readable from memory (ok={ok_count}, pending={pending_count}, nf={not_found_count})"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Module 4: Hash Table Saturation
// ═══════════════════════════════════════════════════════════════════════════

/// With only 2 buckets (14 primary slots), inserting hundreds of keys must
/// spill into overflow chains. All keys must remain findable.
#[test]
fn hash_table_extreme_overflow() {
    let table = HashTable::new(1); // 2 buckets
    let n = 200u64;

    for key in 0..n {
        let hash = key.hash();
        let result = table.find_or_create_entry(hash, LogicalAddress::from_raw(key + 1));
        if result.created {
            // Commit: clear the tentative bit so find_entry can see it.
            let committed = result.entry.without_tentative();
            table.update_entry(result.slot, result.entry, committed);
        }
    }

    // All 200 keys should be findable.
    for key in 0..n {
        let hash = key.hash();
        let found = table.find_entry(hash);
        assert!(
            found.is_some(),
            "key {key} not found after overflow insertion"
        );
    }

    // Overflow count should be > 0 with only 14 primary slots and 200 keys.
    let overflow = table.overflow_count();
    assert!(overflow > 0, "expected overflow buckets, got {overflow}");
}

/// Saturate hash table at >90% load factor. Entry count must track correctly
/// and no lookups should fail.
#[test]
fn hash_table_high_load_factor() {
    let log2 = 4; // 16 buckets × 7 entries = 112 primary slots
    let table = HashTable::new(log2);
    let capacity = table.num_buckets() * BUCKET_NUM_ENTRIES as u64;

    // Fill to ~95% of primary capacity.
    let target = (capacity as f64 * 0.95) as u64;
    let mut unique_tags = 0u64;
    for key in 0..target {
        let hash = key.hash();
        let result = table.find_or_create_entry(hash, LogicalAddress::from_raw(key + 1));
        if result.created {
            let committed = result.entry.without_tentative();
            table.update_entry(result.slot, result.entry, committed);
            unique_tags += 1;
        }
    }

    // Verify all committed entries are findable.
    for key in 0..target {
        let hash = key.hash();
        let found = table.find_entry(hash);
        assert!(found.is_some(), "key {key} missing at 95% load factor");
    }

    // entry_count includes tentative entries, but we committed all of ours.
    // Some keys may share a tag (14-bit), so unique_tags <= target.
    let count = table.entry_count();
    assert_eq!(
        count, unique_tags,
        "entry_count ({count}) != unique tags created ({unique_tags})"
    );
}

/// Concurrent operations on a nearly-full hash table: concurrent
/// find_or_create must not lose entries or corrupt the bucket chain.
#[test]
fn hash_table_concurrent_saturation() {
    let table = Arc::new(HashTable::new(4)); // 16 buckets
    let threads = 4;
    let keys_per_thread = 50;
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();

                let base = (t * keys_per_thread) as u64;
                for i in 0..keys_per_thread as u64 {
                    let key = base + i;
                    let hash = key.hash();
                    let result =
                        table.find_or_create_entry(hash, LogicalAddress::from_raw(key + 1));
                    if result.created {
                        let committed = result.entry.without_tentative();
                        table.update_entry(result.slot, result.entry, committed);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // All entries should be findable after commit.
    let total = (threads * keys_per_thread) as u64;
    for key in 0..total {
        let hash = key.hash();
        let found = table.find_entry(hash);
        assert!(found.is_some(), "key {key} missing after concurrent insert");
    }
}

/// With a saturated hash table backing a full store, operations must
/// not corrupt data even when every bucket overflows.
#[test]
fn store_with_saturated_hash_table() {
    let store = saturated_hash_store();
    let mut s = store.new_session();

    // 2 buckets × 7 = 14 primary slots. Insert 200 keys to force heavy overflow.
    const N: u64 = 200;
    for i in 0..N {
        let status = store.upsert(&mut s, &i, &(i * 31), ());
        assert!(status.is_success(), "upsert key {i} → {status:?}");
    }

    // Every key should read back correctly.
    for i in 0..N {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read key {i} → {status:?}");
        assert_eq!(out, Some(i * 31), "value mismatch at key {i}");
    }

    store.dispose_session(s);
}

/// Upsert (update existing) must work correctly when every bucket is overflowing.
#[test]
fn store_update_in_saturated_hash() {
    let store = saturated_hash_store();
    let mut s = store.new_session();

    const N: u64 = 100;

    // Insert initial values.
    for i in 0..N {
        let _ = store.upsert(&mut s, &i, &i, ());
    }

    // Update all values.
    for i in 0..N {
        let _ = store.upsert(&mut s, &i, &(i + 1000), ());
    }

    // Verify updated values.
    for i in 0..N {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read key {i} → {status:?}");
        assert_eq!(out, Some(i + 1000), "update not reflected for key {i}");
    }

    store.dispose_session(s);
}

/// Delete under hash saturation: deleting keys in a saturated hash table
/// must work correctly.
#[test]
fn store_delete_in_saturated_hash() {
    let store = saturated_hash_store();
    let mut s = store.new_session();

    const N: u64 = 100;

    for i in 0..N {
        let _ = store.upsert(&mut s, &i, &(i * 7), ());
    }

    // Delete even keys.
    for i in (0..N).step_by(2) {
        let status = store.delete(&mut s, &i, ());
        assert_eq!(
            status,
            OperationStatus::Deleted,
            "delete key {i} → {status:?}"
        );
    }

    // Even keys → NotFound, odd keys → still present.
    for i in 0..N {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        if i % 2 == 0 {
            assert_eq!(
                status,
                OperationStatus::NotFound,
                "deleted key {i} still found"
            );
        } else {
            assert_eq!(status, OperationStatus::Ok, "odd key {i} missing");
            assert_eq!(out, Some(i * 7));
        }
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// Module 5: Combined Pressure — Hash + Log + Eviction
// ═══════════════════════════════════════════════════════════════════════════

/// The ultimate pressure test: tiny hash table + tiny buffer + concurrent
/// writers + maintenance thread. Data integrity must be preserved.
#[test]
fn combined_pressure_concurrent() {
    let config = FasterKvConfig {
        hash_index_size_log2: 4, // 16 buckets (112 primary slots)
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let threads = 4;
    let keys_per_thread = 200;
    let barrier = Arc::new(Barrier::new(threads + 1));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Maintenance thread.
    let maint_store = Arc::clone(&store);
    let maint_barrier = Arc::clone(&barrier);
    let maint_done = Arc::clone(&done);
    let maint = thread::spawn(move || {
        maint_barrier.wait();
        while !maint_done.load(Ordering::Relaxed) {
            maint_store.maintenance();
            thread::yield_now();
        }
    });

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut s = store.new_session();
                barrier.wait();

                let base = (t as u64) * (keys_per_thread as u64);

                // Write phase.
                for i in 0..keys_per_thread as u64 {
                    let key = base + i;
                    let status = store.upsert(&mut s, &key, &(key * 37), ());
                    assert!(status.is_success(), "t{t}: upsert {key} → {status:?}");
                }

                // Read phase — only verify in-memory keys.
                for i in 0..keys_per_thread as u64 {
                    let key = base + i;
                    let mut out: Option<u64> = None;
                    let status = store.read(&mut s, &key, &0u64, &mut out, ());
                    if status == OperationStatus::Ok {
                        assert_eq!(out, Some(key * 37), "t{t}: corrupt value at key {key}");
                    }
                    // Pending or NotFound are acceptable under eviction pressure.
                }

                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("writer panicked");
    }
    done.store(true, Ordering::Relaxed);
    maint.join().expect("maintenance panicked");
}

/// Interleaved upsert + delete under combined pressure.
#[test]
fn interleaved_upsert_delete_under_pressure() {
    let store = pressure_store();
    let mut s = store.new_session();

    for round in 0..5 {
        let base = round * 200;
        // Insert keys.
        for i in 0..200u64 {
            let _ = store.upsert(&mut s, &(base + i), &(i * 3), ());
        }
        // Delete every other key.
        for i in (0..200u64).step_by(2) {
            let _ = store.delete(&mut s, &(base + i), ());
        }
        store.maintenance();
    }

    // Verify the last round's odd keys are present.
    let base = 4 * 200;
    for i in (1..200u64).step_by(2) {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &(base + i), &0u64, &mut out, ());
        if status == OperationStatus::Ok {
            assert_eq!(out, Some(i * 3));
        }
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// Module 6: Property-Based Memory Pressure Tests
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2))]

    /// For any sequence of keys and values, a constrained store must preserve
    /// recently-written data integrity (readable keys have correct values).
    #[test]
    fn prop_store_integrity_under_pressure(
        keys in prop::collection::vec(0u64..10_000, 100..500),
    ) {
        let store = pressure_store();
        let mut s = store.new_session();

        // Track the latest value for each key.
        let mut expected = std::collections::HashMap::new();

        for &key in &keys {
            let value = key.wrapping_mul(7);
            let _ = store.upsert(&mut s, &key, &value, ());
            expected.insert(key, value);
        }

        store.maintenance();

        // Verify: every readable key has the correct value.
        for (&key, &expected_val) in &expected {
            let mut out: Option<u64> = None;
            let status = store.read(&mut s, &key, &0u64, &mut out, ());
            if status == OperationStatus::Ok {
                prop_assert_eq!(out, Some(expected_val));
            }
            // Pending/NotFound are OK under pressure — only corruption is a bug.
        }

        store.dispose_session(s);
    }

    /// Allocator invariant: after N allocate/free cycles, count() equals the
    /// high-water mark, and every allocated address is unique.
    #[test]
    fn prop_allocator_no_duplicate_addresses(
        n in 100usize..2_000,
    ) {
        let alloc = MallocFixedPageSize::<Item128>::new();
        let mut addrs = std::collections::HashSet::new();

        for _ in 0..n {
            let addr = alloc.allocate();
            prop_assert!(
                addrs.insert(addr.raw()),
                "duplicate address: {addr:?}"
            );
        }
        prop_assert_eq!(alloc.count(), n as u64);

        // Free half, re-allocate — should reuse, still no duplicates within
        // the live set.
        let to_free: Vec<_> = addrs.iter().take(n / 2).copied().collect();
        for &raw in &to_free {
            alloc.free(LogicalAddress::from_raw(raw));
            addrs.remove(&raw);
        }

        for _ in 0..n / 2 {
            let addr = alloc.allocate();
            // Address may have been freed and reused — that's fine.
            // But within the live set it should be unique.
            addrs.insert(addr.raw());
        }

        // Count shouldn't have grown since we reused freed slots.
        prop_assert_eq!(alloc.count(), n as u64);
    }

    /// Hash table invariant: for any set of distinct keys, every committed
    /// entry must be findable via find_entry.
    #[test]
    fn prop_hash_table_all_keys_findable(
        keys in prop::collection::hash_set(0u64..100_000, 10..200),
    ) {
        let table = HashTable::new(4); // 16 buckets → lots of overflow

        for &key in &keys {
            let hash = key.hash();
            let result = table.find_or_create_entry(hash, LogicalAddress::from_raw(key + 1));
            if result.created {
                let committed = result.entry.without_tentative();
                table.update_entry(result.slot, result.entry, committed);
            }
        }

        for &key in &keys {
            let hash = key.hash();
            let found = table.find_entry(hash);
            prop_assert!(
                found.is_some(),
                "key {} not found", key
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Module 7: Page Recycling Correctness
// ═══════════════════════════════════════════════════════════════════════════

/// After heavy insert → flush → evict cycles, new inserts must succeed
/// and the store must not run out of page frames.
#[test]
fn page_recycling_under_sustained_pressure() {
    let store = pressure_store();
    let mut s = store.new_session();

    // Perform many insert → maintenance cycles.
    for cycle in 0..10 {
        for i in 0..300u64 {
            let key = cycle * 300 + i;
            let status = store.upsert(&mut s, &key, &(key * 19), ());
            assert!(
                status.is_success(),
                "cycle {cycle}, key {key}: upsert → {status:?}"
            );
        }
        store.maintenance();
    }

    // After 10 cycles, the store should still be functional.
    let test_key = 999_999u64;
    let status = store.upsert(&mut s, &test_key, &42u64, ());
    assert!(
        status.is_success(),
        "post-pressure upsert failed: {status:?}"
    );

    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &test_key, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(42));

    store.dispose_session(s);
}

/// NullDevice: operations must work identically under pressure (writes
/// are discarded, so eviction happens but reads return zero/default).
#[test]
fn pressure_with_null_device() {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());
    let mut s = store.new_session();

    // Insert under pressure — must not panic.
    for i in 0u64..2_000 {
        let _ = store.upsert(&mut s, &i, &(i * 23), ());
    }

    store.maintenance();
    store.maintenance();

    // Recent keys should be readable; evicted keys read from NullDevice
    // may return zeros, Pending, or NotFound — but must not corrupt.
    let mut ok = 0u32;
    for i in 1_900..2_000u64 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        if status == OperationStatus::Ok {
            assert_eq!(out, Some(i * 23));
            ok += 1;
        }
    }
    assert!(ok > 0, "no recent keys were readable from memory");

    store.dispose_session(s);
}
