# FASTER Memory Pressure Test Examples

## Quick Setup Template

```rust
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::{InMemoryDevice, NullDevice};
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;

// === TINY STORE (memory pressure testing) ===
let config = FasterKvConfig {
    hash_index_size_log2: 10,          // 1,024 buckets only
    buffer_size_pages: 4,              // 4 × 32MB = 128MB total
    mutable_fraction: 0.5,             // Only 2 pages mutable → seals quickly
    sector_size: 512,
    eviction_policy: EvictionPolicy {
        max_in_memory_pages: 3,        // Keep only 3 pages in memory
        eviction_batch_size: 1,        // Evict one page at a time
    },
    grow_config: GrowConfig::default(),
    auto_compact: false,
};

let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
let mut session = store.new_session();
```

## Test 1: Allocator Free-List Reuse

```rust
#[test]
fn allocator_free_list_reuse() {
    use faster_core::allocator::MallocFixedPageSize;

    #[repr(C)]
    struct Item(u64, u64);  // 16 bytes, 8-byte aligned

    let alloc = MallocFixedPageSize::<Item>::new();

    // Phase 1: Allocate many items
    let mut addrs = Vec::new();
    for i in 0..100_000 {
        let addr = alloc.allocate();
        assert!(addr.is_valid());
        addrs.push(addr);
    }

    let count_after_alloc = alloc.count();
    println!("Allocated {} items", count_after_alloc);

    // Phase 2: Free every other item → should populate free list
    for (i, &addr) in addrs.iter().enumerate() {
        if i % 2 == 0 {
            alloc.free_immediate(addr);
        }
    }

    // Phase 3: Allocate again → should reuse from free list
    let initial_count = alloc.count();
    for _ in 0..50_000 {
        let addr = alloc.allocate();
        assert!(addr.is_valid());
    }
    let count_after_reuse = alloc.count();

    // Verify reuse: count should grow much less than 50K more items
    let growth = count_after_reuse - initial_count;
    assert!(
        growth < 10_000,  // Free list reuse should cap growth
        "Expected reuse, but count grew by {growth} (should be < 10K)"
    );
}
```

## Test 2: Page Lifecycle & State Transitions

```rust
#[test]
fn page_state_transitions() {
    use faster_core::hybrid_log::page::{PageState, AtomicPageState};

    let state = AtomicPageState::new(PageState::Free);

    // Free → Open
    assert!(state.try_transition(PageState::Free, PageState::Open));
    assert_eq!(state.load(), PageState::Open);

    // Open → Sealed
    assert!(state.try_transition(PageState::Open, PageState::Sealed));
    assert_eq!(state.load(), PageState::Sealed);

    // Sealed → Flushing
    assert!(state.try_transition(PageState::Sealed, PageState::Flushing));

    // Flushing → Flushed
    assert!(state.try_transition(PageState::Flushing, PageState::Flushed));

    // Flushed → Evicted
    assert!(state.try_transition(PageState::Flushed, PageState::Evicted));

    // Can't go backwards
    assert!(!state.try_transition(PageState::Evicted, PageState::Sealed));
}
```

## Test 3: Hybrid Log Allocation Under Pressure

```rust
#[test]
fn hybrid_log_page_sealing_under_pressure() {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 4,           // Only 4 pages
        mutable_fraction: 0.5,          // 2 pages mutable
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut session = store.new_session();

    // Write enough data to seal multiple pages
    let mut sealed_count = 0;
    for i in 0..50_000 {
        let key = i as u64;
        let value = key * 2;
        let status = store.upsert(&mut session, &key, &value, ());
        
        if status.is_success() {
            // On the log side, we can observe page sealing indirectly
            // by monitoring allocator state
        }
    }

    println!("Successfully upserted 50K items under buffer pressure");
    store.dispose_session(session);
}
```

## Test 4: Concurrent Memory Pressure

```rust
#[test]
fn concurrent_memory_pressure() {
    use std::sync::Arc;
    use std::thread;
    use std::sync::atomic::{AtomicU64, Ordering};

    let config = FasterKvConfig {
        hash_index_size_log2: 12,
        buffer_size_pages: 8,
        mutable_fraction: 0.8,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 6,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    let store = Arc::new(FasterKv::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    ));

    let success_count = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));

    let num_threads = 8;
    let ops_per_thread = 10_000;

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let store = Arc::clone(&store);
            let success = Arc::clone(&success_count);
            let errors = Arc::clone(&error_count);

            thread::spawn(move || {
                let mut session = store.new_session();

                for i in 0..ops_per_thread {
                    let key = (thread_id as u64) * (ops_per_thread as u64) + (i as u64);
                    let value = key * 7;

                    let status = store.upsert(&mut session, &key, &value, ());
                    if status.is_success() {
                        success.fetch_add(1, Ordering::Relaxed);
                    } else {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }

                store.dispose_session(session);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let total_success = success_count.load(Ordering::SeqCst);
    let total_errors = error_count.load(Ordering::SeqCst);
    let expected = num_threads as u64 * ops_per_thread as u64;

    println!("Concurrent ops: {} success, {} errors (expected {})", 
             total_success, total_errors, expected);
    assert_eq!(total_success, expected, "Some operations failed under memory pressure");
}
```

## Test 5: Allocator Exhaustion Stress

```rust
#[test]
fn allocator_stress_many_pages() {
    use faster_core::allocator::{MallocFixedPageSize, ITEMS_PER_PAGE};

    #[repr(C, align(64))]
    struct LargeItem([u8; 512]);  // 512-byte items

    let alloc = MallocFixedPageSize::<LargeItem>::new();

    // Allocate across many pages
    let mut addrs = Vec::new();
    let target_items = 1_000_000;

    for i in 0..target_items {
        let addr = alloc.allocate();
        assert!(addr.is_valid(), "allocation failed at item {}", i);
        addrs.push(addr);

        if i % 100_000 == 0 {
            println!("Allocated {} items (~{} pages)", i, i / ITEMS_PER_PAGE);
        }
    }

    let final_count = alloc.count();
    println!("Total items allocated: {} (~{} pages)", 
             final_count, final_count / ITEMS_PER_PAGE as u64);

    // Verify all addresses are unique and valid
    let unique: std::collections::HashSet<_> = addrs.iter().cloned().collect();
    assert_eq!(unique.len(), addrs.len(), "Duplicate allocations detected!");
}
```

## Test 6: Hash Table Collision Stress

```rust
#[test]
fn hash_collision_stress() {
    use faster_core::hash::{KeyHash, Hashable};
    use faster_core::address::LogicalAddress;

    // Small hash table to force collisions
    let store_config = FasterKvConfig {
        hash_index_size_log2: 8,        // Only 256 buckets → many collisions
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    let store = FasterKv::new(
        store_config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),
    );
    let mut session = store.new_session();

    // Insert many keys that will collide
    for i in 0..10_000 {
        let key = i as u64;
        let value = key * 13;
        let status = store.upsert(&mut session, &key, &value, ());
        assert!(status.is_success(), "upsert failed for key {}", i);
    }

    // Verify all reads succeed despite collisions
    for i in 0..10_000 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut session, &(i as u64), &0u64, &mut out, ());
        assert_eq!(out, Some((i as u64) * 13), "value mismatch for key {}", i);
    }

    store.dispose_session(session);
}
```

## Test 7: Page Flush & Eviction Cycle

```rust
#[test]
fn page_flush_eviction_cycle() {
    let config = FasterKvConfig {
        hash_index_size_log2: 12,
        buffer_size_pages: 2,           // Tiny: 2 pages = 64MB total
        mutable_fraction: 0.5,          // 1 page mutable
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 1,     // Keep ≤ 1 page in memory
            eviction_batch_size: 1,     // Evict immediately
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    let store = FasterKv::new(
        config,
        SimpleFunctions::default(),
        InMemoryDevice::new(),  // Writes go to in-memory buffer
    );
    let mut session = store.new_session();

    // Write data in bursts to trigger flushing
    for burst in 0..10 {
        for i in 0..1000 {
            let key = (burst * 1000 + i) as u64;
            let value = key * 3;
            store.upsert(&mut session, &key, &value, ());
        }

        // Trigger maintenance (flushing, eviction)
        store.maintenance();

        // Verify earlier data is still readable (was flushed)
        if burst > 0 {
            let earlier_key = ((burst - 1) * 1000) as u64;
            let mut out: Option<u64> = None;
            let status = store.read(&mut session, &earlier_key, &0u64, &mut out, ());
            assert_eq!(
                out,
                Some(earlier_key * 3),
                "lost data from earlier burst"
            );
        }
    }

    println!("Successfully cycled through flush/eviction 10 times");
    store.dispose_session(session);
}
```

## Test 8: RMW Under Memory Pressure

```rust
#[test]
fn rmw_under_memory_pressure() {
    use faster_core::store::CounterFunctions;

    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    let store = FasterKv::new(config, CounterFunctions::new(), InMemoryDevice::new());
    let mut session = store.new_session();

    // Pre-populate with keys
    for i in 0..1000 {
        let key = i as u64;
        store.upsert(&mut session, &key, &0u64, ());
    }

    // RMW (increment) each key many times
    for _iteration in 0..100 {
        for i in 0..1000 {
            let key = i as u64;
            let mut out: Option<u64> = None;
            let status = store.rmw(&mut session, &key, &1u64, &mut out, ());
            assert_eq!(status.is_success(), true, "rmw failed for key {}", i);
        }
    }

    // Verify final counts
    for i in 0..1000 {
        let mut out: Option<u64> = None;
        store.read(&mut session, &(i as u64), &0u64, &mut out, ());
        assert_eq!(out, Some(100), "counter mismatch for key {}", i);
    }

    store.dispose_session(session);
}
```

## Test 9: Pending I/O Completion

```rust
#[test]
fn pending_io_completion_under_pressure() {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.8,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 6,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut session = store.new_session();

    // Issue many operations (some may become pending on I/O)
    for i in 0..10_000 {
        let key = i as u64;
        let value = key * 2;
        store.upsert(&mut session, &key, &value, ());
    }

    // Complete all pending operations
    loop {
        let result = session.complete_pending();
        println!(
            "Completed: {}, Remaining: {}",
            result.completed, result.remaining
        );
        if result.is_done() {
            break;
        }
    }

    // Verify all data
    for i in 0..10_000 {
        let mut out: Option<u64> = None;
        store.read(&mut session, &(i as u64), &0u64, &mut out, ());
        assert_eq!(out, Some((i as u64) * 2));
    }

    store.dispose_session(session);
}
```

---

## Key Macros/Helpers for Test Writing

```rust
// Import core types
use faster_core::{
    store::{FasterKv, FasterKvConfig, SimpleFunctions, CounterFunctions},
    hybrid_log::EvictionPolicy,
    grow::GrowConfig,
    status::OperationStatus,
    address::LogicalAddress,
    allocator::MallocFixedPageSize,
    NullDevice, InMemoryDevice,
};

// Memory pressure config builder
fn pressure_config(buffer_pages: usize, max_in_memory: u32) -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: buffer_pages,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: max_in_memory,
            eviction_batch_size: 1,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
    }
}

// Quick status check
fn assert_ok(status: OperationStatus, msg: &str) {
    assert!(status.is_success(), "{}: {:?}", msg, status);
}
```

