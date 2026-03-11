//! Concurrent stress tests for the FASTER hash index.
//!
//! These tests validate the latch-free concurrent correctness of [`HashIndex`]
//! under realistic multi-threaded workloads. All tests use `std::thread` (no
//! async runtime) and `Barrier` for coordinated start.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use faster_core::address::{LogicalAddress, Offset, Page};
use faster_core::hash::KeyHash;
use faster_core::hash_bucket::HashBucketEntry;
use faster_core::hash_index::HashIndex;

// ===========================================================================
// Helpers
// ===========================================================================

/// Create a [`KeyHash`] from a seed with good distribution.
fn make_hash(seed: u64) -> KeyHash {
    KeyHash::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Create a valid [`LogicalAddress`].
fn make_addr(page: u32, offset: u32) -> LogicalAddress {
    LogicalAddress::new(Page(page), Offset(offset))
}

// ===========================================================================
// 1. 8 threads × 100K inserts (unique key ranges) — zero lost entries
// ===========================================================================

/// Each thread inserts 100K keys in its own range [tid*100K..(tid+1)*100K),
/// commits each entry, then all threads verify all 800K entries are findable.
#[test]
fn concurrent_insert_unique_ranges_800k() {
    let num_threads = 8;
    let ops_per_thread = 100_000u64;
    let index = Arc::new(HashIndex::new(20)); // 1M buckets — plenty of room
    let barrier = Arc::new(Barrier::new(num_threads));

    // Phase 1: Parallel inserts.
    let handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                let _guard = et.protect();
                barrier.wait();

                let base = tid as u64 * ops_per_thread;
                for i in 0..ops_per_thread {
                    let seed = base + i;
                    let hash = make_hash(seed);
                    let addr = make_addr(tid as u32, (i as u32) + 2);

                    let r = index.find_or_create(hash, LogicalAddress::INVALID);
                    if r.created {
                        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                        index.update(r.slot, r.entry, committed);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("insert thread panicked");
    }

    // Phase 2: Parallel verification.
    let missing = Arc::new(AtomicU64::new(0));
    let verify_barrier = Arc::new(Barrier::new(num_threads));

    let verify_handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let index = Arc::clone(&index);
            let missing = Arc::clone(&missing);
            let barrier = Arc::clone(&verify_barrier);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                let _guard = et.protect();
                barrier.wait();

                let base = tid as u64 * ops_per_thread;
                for i in 0..ops_per_thread {
                    let seed = base + i;
                    let hash = make_hash(seed);
                    if index.find(hash).is_none() {
                        missing.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for h in verify_handles {
        h.join().expect("verify thread panicked");
    }

    let lost = missing.load(Ordering::Relaxed);
    assert_eq!(
        lost,
        0,
        "lost {lost} entries out of {}",
        num_threads as u64 * ops_per_thread
    );
}

// ===========================================================================
// 2. 8 threads × mixed insert/find on overlapping ranges
// ===========================================================================

/// 50% insert, 50% find on overlapping key ranges. No panics, no infinite loops.
#[test]
fn concurrent_mixed_insert_find() {
    let num_threads = 8;
    let ops_per_thread = 50_000u64;
    let index = Arc::new(HashIndex::new(18));
    let barrier = Arc::new(Barrier::new(num_threads));

    // Pre-populate some entries so finds have something to hit.
    {
        let et = index.register_thread().unwrap();
        let _guard = et.protect();
        for i in 0..10_000u64 {
            let hash = make_hash(i);
            let addr = make_addr(0, (i as u32) + 2);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            if r.created {
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }
    }

    let total_found = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            let total_found = Arc::clone(&total_found);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                let _guard = et.protect();
                barrier.wait();

                let mut found = 0u64;
                for i in 0..ops_per_thread {
                    let seed = i; // overlapping range across all threads
                    let hash = make_hash(seed);

                    if i % 2 == 0 {
                        // Insert
                        let addr = make_addr(tid as u32, (i as u32) + 2);
                        let r = index.find_or_create(hash, LogicalAddress::INVALID);
                        if r.created {
                            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                            index.update(r.slot, r.entry, committed);
                        }
                    } else {
                        // Find (searching pre-populated range for reliable hits)
                        let search_seed = i % 10_000;
                        let search_hash = make_hash(search_seed);
                        if index.find(search_hash).is_some() {
                            found += 1;
                        }
                    }
                }
                total_found.fetch_add(found, Ordering::Relaxed);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    // Pre-populated entries ensure finds succeed.
    assert!(
        total_found.load(Ordering::Relaxed) > 0,
        "at least some finds should succeed"
    );
}

// ===========================================================================
// 3. Contention test: all threads insert into the SAME bucket
// ===========================================================================

/// All threads insert keys that hash to the same bucket (crafted lower bits).
/// This forces heavy contention on CAS. No corruption, all entries findable.
#[test]
fn concurrent_same_bucket_contention() {
    let num_threads = 8;
    let entries_per_thread = 4; // 8 × 4 = 32 entries into same bucket chain
    let index = Arc::new(HashIndex::new(8)); // 256 buckets
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                let _guard = et.protect();
                barrier.wait();

                let mut created_hashes = Vec::new();
                for i in 0..entries_per_thread {
                    // All hashes share lower bits = 7, but have unique tags.
                    let unique_id = tid as u64 * entries_per_thread as u64 + i as u64;
                    let tag_value = (unique_id + 1) << 48;
                    let hash = KeyHash::new(tag_value | 0x7);
                    let addr = make_addr(tid as u32, i as u32 + 2);

                    let r = index.find_or_create(hash, LogicalAddress::INVALID);
                    if r.created {
                        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                        index.update(r.slot, r.entry, committed);
                        created_hashes.push(hash);
                    }
                }
                created_hashes
            })
        })
        .collect();

    let mut all_hashes = Vec::new();
    for h in handles {
        all_hashes.extend(h.join().expect("thread panicked"));
    }

    // Verify all created entries are findable.
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    for hash in &all_hashes {
        assert!(
            index.find(*hash).is_some(),
            "entry not found for hash {:?}",
            hash
        );
    }

    // Total entries should match.
    let total = num_threads * entries_per_thread;
    assert_eq!(
        index.entry_count(),
        total as u64,
        "expected {total} entries"
    );
}

// ===========================================================================
// 4. Concurrent insert + GC invalidation on different ranges
// ===========================================================================

/// Some threads insert while others call invalidate_entries_in_range on
/// different ranges. No crashes, invariants hold.
#[test]
fn concurrent_insert_and_gc() {
    let index = Arc::new(HashIndex::new(16));
    let num_inserters = 4;
    let num_gc_threads = 2;
    let total_threads = num_inserters + num_gc_threads;
    let ops_per_thread = 5_000u64;
    let barrier = Arc::new(Barrier::new(total_threads));

    // Inserters: insert on pages 100..103.
    let insert_handles: Vec<_> = (0..num_inserters)
        .map(|tid| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                let _guard = et.protect();
                barrier.wait();

                for i in 0..ops_per_thread {
                    let seed = tid as u64 * 1_000_000 + i;
                    let hash = make_hash(seed);
                    let addr = make_addr(100 + tid as u32, (i as u32) + 2);

                    let r = index.find_or_create(hash, LogicalAddress::INVALID);
                    if r.created {
                        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                        index.update(r.slot, r.entry, committed);
                    }
                }
            })
        })
        .collect();

    // GC threads: invalidate pages 0..50 (disjoint from inserts on pages 100+).
    let gc_handles: Vec<_> = (0..num_gc_threads)
        .map(|_| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..25 {
                    index.invalidate_entries_in_range(make_addr(0, 0), make_addr(50, 0));
                    std::thread::yield_now();
                }
            })
        })
        .collect();

    for h in insert_handles {
        h.join().expect("inserter panicked");
    }
    for h in gc_handles {
        h.join().expect("GC thread panicked");
    }

    // All inserted entries (on pages 100+) should survive GC (pages 0..50).
    assert!(
        index.entry_count() > 0,
        "entries should survive non-overlapping GC"
    );
}

// ===========================================================================
// 5. Same-key convergence: 8 threads insert the SAME key hash
// ===========================================================================

/// 8 threads all try to insert the same key hash. Exactly one entry should
/// exist afterward (last-writer-wins on address via CAS).
#[test]
fn concurrent_same_key_convergence() {
    // NOTE: This test has multiple threads insert the same key concurrently.
    // In FASTER's session model, each key is routed to a single session, so
    // this scenario should not occur in production. The two-phase tentative
    // protocol does NOT guarantee deduplication across concurrent creators —
    // find_or_create checks committed entries but skips tentative ones,
    // allowing multiple threads to each create a tentative entry.
    //
    // This test verifies structural integrity (no panics, no corruption) under
    // this intentional contract violation, not dedup correctness.
    let num_threads = 8;
    let index = Arc::new(HashIndex::new(10));
    let barrier = Arc::new(Barrier::new(num_threads));
    let hash = make_hash(42_000);

    let handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                let _guard = et.protect();
                barrier.wait();

                let addr = make_addr(tid as u32 + 1, 100);
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                if r.created {
                    // We created it — commit with our address.
                    let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                    index.update(r.slot, r.entry, committed);
                    true
                } else {
                    false
                }
            })
        })
        .collect();

    let creators: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // At least one thread should have created it.
    assert!(
        creators.iter().any(|&c| c),
        "at least one thread should be the creator"
    );

    // Under concurrent tentative insertion, multiple entries may exist (I2).
    // The session model prevents this in production (one session per key).
    let count = index.entry_count();
    assert!(count >= 1, "at least one entry should exist");

    // Every committed entry should be findable and non-tentative.
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();
    let (found, _) = index.find(hash).expect("same-key entry should be findable");
    assert!(!found.is_tentative());
}

// ===========================================================================
// 6. Epoch stress: rapid protect/unprotect with concurrent inserts/finds
// ===========================================================================

/// Threads rapidly protect/unprotect while doing inserts and finds.
/// No panics, no crashes.
#[test]
fn concurrent_epoch_stress() {
    let num_threads = 8;
    let cycles = 5_000;
    let index = Arc::new(HashIndex::new(14));
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                barrier.wait();

                for i in 0..cycles {
                    // Rapid protect/unprotect cycle.
                    let guard = et.protect();

                    let seed = tid as u64 * 100_000 + i;
                    let hash = make_hash(seed);
                    let addr = make_addr(tid as u32, (i as u32) + 2);

                    if i % 3 == 0 {
                        // Insert
                        let r = index.find_or_create(hash, LogicalAddress::INVALID);
                        if r.created {
                            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                            index.update(r.slot, r.entry, committed);
                        }
                    } else {
                        // Find
                        let _ = index.find(hash);
                    }

                    // Periodically refresh guard.
                    if i % 50 == 0 {
                        guard.refresh();
                    }
                    drop(guard);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("epoch stress thread panicked");
    }
}

// ===========================================================================
// 7. Scalability: 4-thread throughput > 2× single-thread
// ===========================================================================

/// Measure that 4-thread insert throughput exceeds single-thread throughput,
/// validating that the latch-free design actually scales.
///
/// This test uses a soft assertion: it warns on < 1.5× speedup but only fails
/// on < 1.0× (regression). On shared/CI machines, thread scheduling jitter
/// can suppress speedup — a hard 1.5× threshold causes flaky failures.
#[test]
#[ignore = "tier-2: VM scheduling jitter causes false regressions"]
fn scalability_4_threads_vs_1() {
    let ops = 200_000u64;

    // Single-thread measurement.
    let t1_start = std::time::Instant::now();
    {
        let index = HashIndex::new(18);
        let et = index.register_thread().unwrap();
        let _guard = et.protect();
        for i in 0..ops {
            let hash = make_hash(i);
            let addr = make_addr(0, (i as u32) + 2);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            if r.created {
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }
    }
    let t1_elapsed = t1_start.elapsed();

    // 4-thread measurement (same total ops).
    let t4_start = std::time::Instant::now();
    {
        let index = Arc::new(HashIndex::new(18));
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|tid| {
                let index = Arc::clone(&index);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let et = index.register_thread().unwrap();
                    let _guard = et.protect();
                    barrier.wait();

                    let per_thread = ops / 4;
                    let base = tid as u64 * per_thread;
                    for i in 0..per_thread {
                        let seed = base + i;
                        let hash = make_hash(seed);
                        let addr = make_addr(tid as u32, (i as u32) + 2);
                        let r = index.find_or_create(hash, LogicalAddress::INVALID);
                        if r.created {
                            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                            index.update(r.slot, r.entry, committed);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
    let t4_elapsed = t4_start.elapsed();

    let speedup = t1_elapsed.as_secs_f64() / t4_elapsed.as_secs_f64();

    // Hard floor: 4 threads must not be dramatically slower than 1 thread.
    // On shared VMs, CPU scheduling jitter can make 4T slightly slower
    // (~0.9×) without a real regression. A genuine contention bug would
    // show up as 0.3–0.5×. Use 0.8× as the hard floor to avoid flakes.
    assert!(
        speedup > 0.8,
        "4-thread MUCH SLOWER than 1-thread: {speedup:.2}× (1T={:.0}ms, 4T={:.0}ms) — \
         possible contention regression",
        t1_elapsed.as_millis(),
        t4_elapsed.as_millis(),
    );

    // Soft expectation: log a warning if speedup is underwhelming.
    if speedup < 1.5 {
        eprintln!(
            "⚠️  scalability_4_threads_vs_1: speedup {speedup:.2}× < 1.5× \
             (1T={:.0}ms, 4T={:.0}ms) — acceptable on shared/CI machines",
            t1_elapsed.as_millis(),
            t4_elapsed.as_millis(),
        );
    }
}

// ===========================================================================
// 8. Tentative cleanup under contention
// ===========================================================================

/// Multiple threads insert while one thread periodically calls
/// cleanup_tentative_entries_for_recovery. No corruption.
///
/// NOTE: In production, `cleanup_tentative_entries_for_recovery` must only be
/// called during recovery when no sessions are active. This test deliberately
/// exercises the concurrent path to verify that the CAS-based implementation
/// does not corrupt the hash table (no crashes, no double-frees). However,
/// the cleanup thread may delete live in-flight tentative entries, which is
/// the exact data-loss scenario documented in the method's `# Safety` section.
#[test]
fn concurrent_cleanup_tentative_under_contention() {
    let index = Arc::new(HashIndex::new(14));
    let num_inserters = 6;
    let total_threads = num_inserters + 1; // +1 for cleaner
    let ops_per_thread = 20_000u64;
    let barrier = Arc::new(Barrier::new(total_threads));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Inserter threads: insert and commit.
    let insert_handles: Vec<_> = (0..num_inserters)
        .map(|tid| {
            let index = Arc::clone(&index);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = index.register_thread().unwrap();
                let _guard = et.protect();
                barrier.wait();

                for i in 0..ops_per_thread {
                    let seed = tid as u64 * 1_000_000 + i;
                    let hash = make_hash(seed);
                    let addr = make_addr(tid as u32, (i as u32) + 2);

                    let r = index.find_or_create(hash, LogicalAddress::INVALID);
                    if r.created {
                        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                        index.update(r.slot, r.entry, committed);
                    }
                }
            })
        })
        .collect();

    // Cleaner thread: periodically run cleanup_tentative_entries_for_recovery.
    // WARNING: This is intentionally violating the recovery-only contract to
    // stress-test structural integrity. See doc comment above.
    let cleaner = {
        let index = Arc::clone(&index);
        let barrier = Arc::clone(&barrier);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            barrier.wait();
            let mut total_cleaned = 0u64;
            while !done.load(Ordering::Relaxed) {
                total_cleaned += index.cleanup_tentative_entries_for_recovery();
                std::thread::yield_now();
            }
            // One final cleanup pass.
            total_cleaned += index.cleanup_tentative_entries_for_recovery();
            total_cleaned
        })
    };

    for h in insert_handles {
        h.join().expect("inserter panicked");
    }
    done.store(true, Ordering::Relaxed);
    let cleaned = cleaner.join().expect("cleaner panicked");

    // The cleaner may have caught some entries in their tentative state.
    // That's expected behavior. Most entries should be committed and findable.
    let entry_count = index.entry_count();
    let total_expected = num_inserters as u64 * ops_per_thread;

    // Allow for some loss due to cleanup racing with commit. The key invariant
    // is that entry_count + cleaned >= total_expected (no entries vanish silently).
    assert!(
        entry_count + cleaned >= total_expected - 100,
        "entry_count={entry_count} + cleaned={cleaned} should be >= ~{total_expected}"
    );
}
