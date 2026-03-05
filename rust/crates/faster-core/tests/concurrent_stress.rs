//! Concurrent stress tests for FASTER foundation modules.
//!
//! These tests exercise multi-threaded interactions between the epoch system,
//! hash bucket, and allocator — the core concurrency patterns used in the
//! real hash index. Thread counts are parameterized; high counts are `#[ignore]`
//! for CI nightly runs.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use faster_core::allocator::MallocFixedPageSize;
use faster_core::address::{LogicalAddress, Offset, Page};
use faster_core::epoch::EpochTable;
use faster_core::hash::Hashable;
use faster_core::hash_bucket::{HashBucket, BUCKET_NUM_ENTRIES};

/// 128-byte item for allocator tests.
#[repr(C)]
struct Item128([u64; 16]);

// ===========================================================================
// 1. Epoch table: concurrent protect/unprotect with drain callbacks
// ===========================================================================

fn epoch_stress_with_threads(num_threads: usize, cycles_per_thread: usize) {
    let table = Arc::new(EpochTable::new());
    let drain_count = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let table = Arc::clone(&table);
            let drain_count = Arc::clone(&drain_count);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = table.register().expect("register");
                barrier.wait();

                for i in 0..cycles_per_thread {
                    let guard = et.protect();

                    // Occasionally bump the epoch with a drain callback.
                    if i % 100 == 0 {
                        let dc = Arc::clone(&drain_count);
                        table.bump_current_epoch(move || {
                            dc.fetch_add(1, Ordering::Relaxed);
                        });
                    }

                    // Periodically refresh to advance safe_epoch.
                    if i % 10 == 0 {
                        guard.refresh();
                    }

                    drop(guard);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // After all threads are done, force drain any remaining callbacks.
    table.bump_current_epoch_no_callback();

    // Every epoch bump should have drained eventually.
    let total_bumps = (num_threads * cycles_per_thread / 100) as u64;
    let drained = drain_count.load(Ordering::Relaxed);
    assert_eq!(
        drained, total_bumps,
        "expected {total_bumps} drain callbacks, got {drained}"
    );
}

#[test]
fn epoch_stress_4_threads() {
    epoch_stress_with_threads(4, 10_000);
}

#[test]
fn epoch_stress_8_threads() {
    epoch_stress_with_threads(8, 10_000);
}

#[test]
#[ignore] // CI nightly: 16 threads × 100K cycles ≈ 2-5s
fn epoch_stress_16_threads() {
    epoch_stress_with_threads(16, 100_000);
}

// ===========================================================================
// 2. Epoch safe_epoch monotonicity
// ===========================================================================

#[test]
fn safe_epoch_monotonically_increases() {
    let table = Arc::new(EpochTable::new());
    let num_threads = 4;
    let cycles = 5_000;
    let barrier = Arc::new(Barrier::new(num_threads + 1)); // +1 for observer

    let last_safe = Arc::new(AtomicU64::new(0));
    let violation = Arc::new(AtomicBool::new(false));

    // Worker threads: protect/bump/unprotect.
    let workers: Vec<_> = (0..num_threads)
        .map(|_| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = table.register().expect("register");
                barrier.wait();
                for _ in 0..cycles {
                    let guard = et.protect();
                    table.bump_current_epoch_no_callback();
                    guard.refresh();
                    drop(guard);
                }
            })
        })
        .collect();

    // Observer thread: poll safe_epoch and verify monotonicity.
    // Note: safe_epoch() is NOT an atomic snapshot — it scans per-thread
    // entries non-atomically, so a single read may lag. We use a retry to
    // distinguish genuine violations from scan-time races.
    let observer = {
        let table = Arc::clone(&table);
        let last_safe = Arc::clone(&last_safe);
        let violation = Arc::clone(&violation);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            for _ in 0..(cycles * num_threads) {
                let s = table.safe_epoch();
                let prev = last_safe.fetch_max(s, Ordering::SeqCst);
                if s < prev {
                    // Non-atomic scan can produce stale reads. Retry to
                    // confirm: a genuine violation persists across retries.
                    std::thread::yield_now();
                    let s2 = table.safe_epoch();
                    if s2 < prev {
                        violation.store(true, Ordering::SeqCst);
                    }
                }
            }
        })
    };

    for w in workers {
        w.join().expect("worker panicked");
    }
    observer.join().expect("observer panicked");

    assert!(
        !violation.load(Ordering::SeqCst),
        "safe_epoch must be monotonically non-decreasing"
    );
}

// ===========================================================================
// 3. Hash bucket: concurrent try_insert — no lost entries
// ===========================================================================

fn concurrent_bucket_insert(num_threads: usize) {
    // Each thread tries to insert one entry into a shared bucket.
    // We use unique tags so there are no tag collisions.
    let bucket = Arc::new(HashBucket::new());
    let barrier = Arc::new(Barrier::new(num_threads));
    let success_count = Arc::new(AtomicU64::new(0));

    assert!(
        num_threads <= BUCKET_NUM_ENTRIES,
        "test requires num_threads <= 7 for single-bucket test"
    );

    let handles: Vec<_> = (0..num_threads)
        .map(|i| {
            let bucket = Arc::clone(&bucket);
            let barrier = Arc::clone(&barrier);
            let success_count = Arc::clone(&success_count);
            thread::spawn(move || {
                let tag = (i as u16) + 1;
                let addr = LogicalAddress::new(Page(0), Offset((i as u32) + 10));
                barrier.wait();

                // Retry loop — another thread may race for the same empty slot.
                loop {
                    match bucket.try_insert(tag, addr) {
                        Ok(_) => {
                            success_count.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Err(()) => {
                            // Bucket full — this means other threads got there first.
                            // For N ≤ 7 threads this shouldn't happen (7 slots).
                            break;
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let successes = success_count.load(Ordering::Relaxed);
    assert_eq!(
        successes, num_threads as u64,
        "all {num_threads} threads should insert successfully with unique tags"
    );

    // Verify each entry is findable.
    for i in 0..num_threads {
        let tag = (i as u16) + 1;
        assert!(
            bucket.find_entry(tag).is_some(),
            "tag {tag} should be findable"
        );
    }
}

#[test]
fn concurrent_bucket_insert_4_threads() {
    concurrent_bucket_insert(4);
}

#[test]
fn concurrent_bucket_insert_7_threads() {
    concurrent_bucket_insert(7);
}

// ===========================================================================
// 4. Hash bucket: many concurrent inserts into separate buckets
// ===========================================================================

#[test]
fn concurrent_multi_bucket_insert_8_threads() {
    // Each thread gets its own bucket and inserts 7 entries.
    // This tests that bucket operations don't interfere across instances.
    let num_threads = 8;
    let barrier = Arc::new(Barrier::new(num_threads));
    let total_inserted = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let barrier = Arc::clone(&barrier);
            let total_inserted = Arc::clone(&total_inserted);
            thread::spawn(move || {
                let bucket = HashBucket::new();
                barrier.wait();

                for i in 0..BUCKET_NUM_ENTRIES {
                    let key: u64 = (t * 1000 + i) as u64;
                    let tag = key.hash().tag();
                    let addr = LogicalAddress::new(Page(t as u32), Offset(i as u32 + 2));
                    if bucket.try_insert(tag, addr).is_ok() {
                        total_inserted.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // Verify all 7 entries are findable.
                for i in 0..BUCKET_NUM_ENTRIES {
                    let key: u64 = (t * 1000 + i) as u64;
                    let tag = key.hash().tag();
                    assert!(bucket.find_entry(tag).is_some());
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    assert_eq!(
        total_inserted.load(Ordering::Relaxed),
        (num_threads * BUCKET_NUM_ENTRIES) as u64
    );
}

// ===========================================================================
// 5. Allocator: concurrent alloc/free — all addresses valid
// ===========================================================================

fn concurrent_alloc_free(num_threads: usize, ops_per_thread: usize) {
    let alloc = Arc::new(MallocFixedPageSize::<Item128>::new());
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let alloc = Arc::clone(&alloc);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut live: Vec<LogicalAddress> = Vec::new();

                for i in 0..ops_per_thread {
                    if i % 3 == 0 && !live.is_empty() {
                        // Free an item.
                        let addr = live.pop().unwrap();
                        alloc.free(addr);
                    } else {
                        // Allocate.
                        let addr = alloc.allocate();
                        assert!(addr.is_valid(), "allocated address must be valid");
                        // Verify we can access the item.
                        let _ = alloc.get(addr);
                        live.push(addr);
                    }
                }

                // Clean up remaining live items.
                for addr in live {
                    alloc.free(addr);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }
}

#[test]
fn concurrent_alloc_free_4_threads() {
    concurrent_alloc_free(4, 10_000);
}

#[test]
fn concurrent_alloc_free_8_threads() {
    concurrent_alloc_free(8, 10_000);
}

#[test]
#[ignore] // CI nightly: 16 threads × 50K ops
fn concurrent_alloc_free_16_threads() {
    concurrent_alloc_free(16, 50_000);
}

// ===========================================================================
// 6. Combined: epoch + bucket + allocator — the real hash index pattern
// ===========================================================================

#[test]
fn combined_epoch_bucket_alloc_pattern() {
    let num_threads = 4;
    let ops_per_thread = 1_000;

    let table = Arc::new(EpochTable::new());
    let alloc = Arc::new(MallocFixedPageSize::<Item128>::new());
    // Use a Vec of buckets to avoid overflow complexity.
    let buckets: Arc<Vec<HashBucket>> = Arc::new((0..16).map(|_| HashBucket::new()).collect());
    let total_inserted = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let table = Arc::clone(&table);
            let alloc = Arc::clone(&alloc);
            let buckets = Arc::clone(&buckets);
            let total_inserted = Arc::clone(&total_inserted);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = table.register().expect("register");
                barrier.wait();

                for i in 0..ops_per_thread {
                    let _guard = et.protect();

                    // Generate a unique key per thread per op.
                    let key: u64 = ((t as u64) << 32) | (i as u64);
                    let kh = key.hash();
                    let tag = kh.tag();

                    // Pick a bucket.
                    let bucket_idx = kh.index(16) as usize;
                    let bucket = &buckets[bucket_idx];

                    // Allocate a slot.
                    let addr = alloc.allocate();

                    // Try to insert — may fail if bucket is full (7 slots).
                    if bucket.try_insert(tag, addr).is_ok() {
                        total_inserted.fetch_add(1, Ordering::Relaxed);
                    }

                    // Periodically bump epoch.
                    if i % 50 == 0 {
                        table.bump_current_epoch_no_callback();
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let inserted = total_inserted.load(Ordering::Relaxed);
    assert!(
        inserted > 0,
        "at least some inserts should succeed across {num_threads} threads"
    );
    // With 16 buckets × 7 slots = 112 max entries, not all will succeed,
    // but that's expected — we're testing the concurrency pattern, not capacity.
}

#[test]
#[ignore] // CI nightly: heavier combined stress test
fn combined_stress_8_threads() {
    let num_threads = 8;
    let ops_per_thread = 10_000;

    let table = Arc::new(EpochTable::new());
    let alloc = Arc::new(MallocFixedPageSize::<Item128>::new());
    let drain_count = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|_t| {
            let table = Arc::clone(&table);
            let alloc = Arc::clone(&alloc);
            let drain_count = Arc::clone(&drain_count);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = table.register().expect("register");
                barrier.wait();

                let mut allocated: Vec<LogicalAddress> = Vec::new();

                for i in 0..ops_per_thread {
                    let _guard = et.protect();

                    let addr = alloc.allocate();
                    assert!(addr.is_valid());
                    allocated.push(addr);

                    // Periodically free old allocations under epoch protection.
                    if allocated.len() > 100 {
                        let old = allocated.remove(0);
                        let dc = Arc::clone(&drain_count);
                        table.bump_current_epoch(move || {
                            dc.fetch_add(1, Ordering::Relaxed);
                        });
                        // In a real system, we'd defer the free until the
                        // epoch callback fires. Here we just test the pattern.
                        alloc.free(old);
                    }

                    if i % 20 == 0 {
                        // Refresh to help safe_epoch advance.
                        _guard.refresh();
                    }
                }

                // Cleanup.
                for addr in allocated {
                    alloc.free(addr);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Force drain remaining callbacks.
    table.bump_current_epoch_no_callback();
    let drained = drain_count.load(Ordering::Relaxed);
    assert!(drained > 0, "drain callbacks should have fired");
}

// ===========================================================================
// 7. Thread registration storm
// ===========================================================================

#[test]
fn registration_storm() {
    let table = Arc::new(EpochTable::new());
    let num_threads = 32;
    let cycles = 100;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..cycles {
                    let mut et = table.register().expect("register");
                    {
                        let _guard = et.protect();
                    }
                    et.unregister();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    assert_eq!(
        table.registered_count(),
        0,
        "all threads should have deregistered"
    );
}
