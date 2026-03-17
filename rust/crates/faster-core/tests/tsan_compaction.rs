//! ThreadSanitizer (TSan) tests for the compaction scanner–evictor data race.
//!
//! # Background
//!
//! Under 16-thread concurrent compaction the scanner crashes with SIGSEGV.
//! The root cause is a use-after-free data race:
//!
//! 1. Thread A (scanner): `get_physical_address(addr)` → raw pointer to page frame
//! 2. Thread B (evictor): `try_evict_frame(page)` → CAS slot to null → frees memory
//! 3. Thread A (scanner): dereferences the now-dangling pointer → SIGSEGV
//!
//! MIRI cannot catch this (single-threaded). Loom cannot catch this (raw
//! pointers bypass instrumentation). TSan instruments all memory accesses
//! and can detect the data race on freed memory.
//!
//! # Page size matters
//!
//! Each page is 32 MiB (~1.4M records at 24 bytes/record). The tests need
//! writers to fill multiple pages before eviction and compaction can
//! interact. This means tests run for 10+ seconds to generate enough data.
//! Under TSan's 5-15× slowdown, that's 1-2 minutes per test.
//!
//! # Running
//!
//! ```bash
//! cd rust
//! RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test \
//!     -p faster-core --test tsan_compaction -- --ignored --nocapture
//! ```
//!
//! Or use the helper script:
//!
//! ```bash
//! ./scripts/run-tsan.sh
//! ```
//!
//! # Interpretation
//!
//! - **TSan reports a data race** → The bug is confirmed. The race report
//!   will show the two conflicting accesses (scanner read vs evictor free).
//! - **No TSan report** → Either the fix is working, or the test didn't
//!   hit the timing window. Increase `DURATION_SECS` or run multiple times.
//!
//! All tests are `#[ignore]` because TSan requires nightly and adds 5-15×
//! overhead. They compile and link under stable Rust as regular ignored tests.

use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::InMemoryDevice;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

type Store = FasterKv<SimpleFunctions<u64, u64>>;

// ---------------------------------------------------------------------------
// Store configurations
// ---------------------------------------------------------------------------

/// Config for scanner-evictor tests. Uses a small buffer with aggressive
/// eviction so that once enough data accumulates to fill multiple pages,
/// eviction and compaction compete for the same pages.
fn tsan_store_config() -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 16, // 64K buckets
        buffer_size_pages: 16,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 8,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    }
}

/// Larger config for the production-like scenario with 4+ writers.
fn tsan_production_config() -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 18, // 256K buckets
        buffer_size_pages: 32,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 16,
            eviction_batch_size: 4,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: false,
    }
}

/// Lossy (LRU cache) config where eviction advances begin_address.
fn tsan_lossy_config() -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 16,
        buffer_size_pages: 16,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 8,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        },
        auto_compact: false,
        lossy: true,
    }
}

// ---------------------------------------------------------------------------
// Helper: writer loop
// ---------------------------------------------------------------------------

/// Writes keys in a tight loop. Calls `maintenance()` periodically to
/// drive the flush/evict pipeline and prevent buffer stalls. Returns the
/// number of successful ops.
fn pump_writes(store: &Store, stop: &AtomicBool, base_key: u64) -> u64 {
    let mut session = store.new_session();
    let mut key = base_key;
    let mut ops: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        let outcome = store.upsert(&mut session, &key, &(key.wrapping_mul(7)), ());
        if outcome.is_aborted() {
            thread::yield_now();
        } else {
            ops += 1;
            key = key.wrapping_add(1);
        }

        // Drain async I/O and drive maintenance periodically
        if ops % 1024 == 0 {
            store.complete_pending(&mut session);
        }
    }

    store.complete_pending_sync(&mut session);
    store.dispose_session(session);
    ops
}

// ---------------------------------------------------------------------------
// Test 1: The primary race — scanner reads while evictor frees
// ---------------------------------------------------------------------------

/// Exercises the exact race that causes SIGSEGV under concurrent compaction.
///
/// Writers fill the 32 MiB pages continuously. Once enough pages accumulate,
/// the maintenance thread flushes and evicts while the compaction thread
/// scans. The race window opens when `compact()` captures a scan range
/// `[head, safe_read_only)` and the evictor advances head through that
/// range during the scan.
///
/// Under TSan's 5-15× slowdown, the race window widens significantly
/// because the scanner takes longer to complete its page reads.
#[test]
#[ignore = "tier-2: requires TSan: RUSTFLAGS='-Z sanitizer=thread' cargo +nightly test"]
fn tsan_compaction_scanner_vs_eviction() {
    const DURATION_SECS: u64 = 10;
    const NUM_WRITERS: usize = 2;

    let store = Arc::new(FasterKv::new(
        tsan_store_config(),
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let compact_count = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(NUM_WRITERS + 2));

    let mut handles = Vec::new();
    for writer_id in 0..NUM_WRITERS {
        let s = Arc::clone(&store);
        let st = Arc::clone(&stop);
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            pump_writes(&s, &st, (writer_id as u64 + 1) * 10_000_000)
        }));
    }

    // Maintenance thread: drives flush → evict pipeline
    let s = Arc::clone(&store);
    let st = Arc::clone(&stop);
    let b = Arc::clone(&barrier);
    let maintainer = thread::spawn(move || {
        b.wait();
        let mut cycles = 0u64;
        while !st.load(Ordering::Relaxed) {
            s.maintenance();
            cycles += 1;
            thread::sleep(Duration::from_millis(1));
        }
        cycles
    });

    // Compactor thread: tight loop calling compact()
    let s = Arc::clone(&store);
    let st = Arc::clone(&stop);
    let cc = Arc::clone(&compact_count);
    let b = Arc::clone(&barrier);
    let compactor = thread::spawn(move || {
        b.wait();
        let deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
        let mut compactions = 0u64;
        while Instant::now() < deadline && !st.load(Ordering::Relaxed) {
            if s.compact().is_ok() {
                compactions += 1;
            }
        }
        cc.store(compactions, Ordering::Relaxed);
    });

    thread::sleep(Duration::from_secs(DURATION_SECS));
    stop.store(true, Ordering::Release);

    compactor.join().expect("compactor panicked");
    let maint = maintainer.join().expect("maintainer panicked");
    let mut total_writes = 0u64;
    for h in handles {
        total_writes += h.join().expect("writer panicked");
    }

    let compactions = compact_count.load(Ordering::Relaxed);
    eprintln!(
        "tsan_compaction_scanner_vs_eviction: {} compactions, {} maintenance, {} writes",
        compactions, maint, total_writes
    );
    assert!(total_writes > 0, "writers made zero progress");
}

// ---------------------------------------------------------------------------
// Test 2: Production-like — scanner + evictor + 4 writers
// ---------------------------------------------------------------------------

/// Production-like scenario with 4 writer threads, a dedicated compaction
/// thread, and a dedicated maintenance thread. More writers means faster
/// page filling and more frequent page transitions.
#[test]
#[ignore = "tier-2: requires TSan: RUSTFLAGS='-Z sanitizer=thread' cargo +nightly test"]
fn tsan_concurrent_compaction_and_writers() {
    const DURATION_SECS: u64 = 10;
    const NUM_WRITERS: usize = 4;

    let store = Arc::new(FasterKv::new(
        tsan_production_config(),
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(NUM_WRITERS + 2));

    let mut handles = Vec::new();
    for writer_id in 0..NUM_WRITERS {
        let s = Arc::clone(&store);
        let st = Arc::clone(&stop);
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            pump_writes(&s, &st, (writer_id as u64 + 1) * 100_000_000)
        }));
    }

    let s = Arc::clone(&store);
    let st = Arc::clone(&stop);
    let b = Arc::clone(&barrier);
    let maintainer = thread::spawn(move || {
        b.wait();
        let mut cycles = 0u64;
        while !st.load(Ordering::Relaxed) {
            s.maintenance();
            cycles += 1;
            thread::sleep(Duration::from_millis(1));
        }
        cycles
    });

    let s = Arc::clone(&store);
    let st = Arc::clone(&stop);
    let b = Arc::clone(&barrier);
    let compactor = thread::spawn(move || {
        b.wait();
        let deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
        let mut compactions = 0u64;
        while Instant::now() < deadline && !st.load(Ordering::Relaxed) {
            if s.compact().is_ok() {
                compactions += 1;
            }
        }
        compactions
    });

    thread::sleep(Duration::from_secs(DURATION_SECS));
    stop.store(true, Ordering::Release);

    let compactions = compactor.join().expect("compactor panicked");
    let maint = maintainer.join().expect("maintainer panicked");
    let mut total_writes = 0u64;
    for h in handles {
        total_writes += h.join().expect("writer panicked");
    }

    eprintln!(
        "tsan_concurrent_compaction_and_writers: {} compactions, {} maintenance, {} writes",
        compactions, maint, total_writes
    );
    assert!(total_writes > 0, "writers made zero progress");
}

// ---------------------------------------------------------------------------
// Test 3: begin_address advance during scan (lossy mode)
// ---------------------------------------------------------------------------

/// In lossy (LRU cache) mode, eviction advances `begin_address` past
/// evicted pages. If the compaction scanner's scan range was computed
/// before this advance, it may try to read from addresses that are now
/// below `begin_address` — pointing to freed or recycled page frames.
#[test]
#[ignore = "tier-2: requires TSan: RUSTFLAGS='-Z sanitizer=thread' cargo +nightly test"]
fn tsan_begin_address_advance_during_scan() {
    const DURATION_SECS: u64 = 10;
    const NUM_WRITERS: usize = 2;

    let store = Arc::new(FasterKv::new(
        tsan_lossy_config(),
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(NUM_WRITERS + 2));

    let mut handles = Vec::new();
    for writer_id in 0..NUM_WRITERS {
        let s = Arc::clone(&store);
        let st = Arc::clone(&stop);
        let b = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            b.wait();
            pump_writes(&s, &st, (writer_id as u64 + 1) * 10_000_000)
        }));
    }

    // Maintenance thread: in lossy mode, also advances begin_address
    let s = Arc::clone(&store);
    let st = Arc::clone(&stop);
    let b = Arc::clone(&barrier);
    let maintainer = thread::spawn(move || {
        b.wait();
        let mut cycles = 0u64;
        while !st.load(Ordering::Relaxed) {
            s.maintenance();
            cycles += 1;
            thread::sleep(Duration::from_millis(1));
        }
        cycles
    });

    let s = Arc::clone(&store);
    let st = Arc::clone(&stop);
    let b = Arc::clone(&barrier);
    let compactor = thread::spawn(move || {
        b.wait();
        let deadline = Instant::now() + Duration::from_secs(DURATION_SECS);
        let mut compactions = 0u64;
        while Instant::now() < deadline && !st.load(Ordering::Relaxed) {
            if s.compact().is_ok() {
                compactions += 1;
            }
        }
        compactions
    });

    thread::sleep(Duration::from_secs(DURATION_SECS));
    stop.store(true, Ordering::Release);

    let compactions = compactor.join().expect("compactor panicked");
    let maint = maintainer.join().expect("maintainer panicked");
    let mut total_writes = 0u64;
    for h in handles {
        total_writes += h.join().expect("writer panicked");
    }

    eprintln!(
        "tsan_begin_address_advance_during_scan: {} compactions, {} maintenance, {} writes (lossy)",
        compactions, maint, total_writes
    );
    assert!(total_writes > 0, "writers made zero progress");
}
