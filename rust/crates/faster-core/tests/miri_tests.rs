//! Miri-based undefined behavior tests for FASTER's unsafe code.
//!
//! These tests exercise the raw pointer arithmetic, allocation, and free-list
//! operations in `allocator.rs` and the cache-line-aligned hash bucket
//! operations in `hash_bucket.rs`. They are gated behind `#[cfg(miri)]` so
//! they only run under `cargo +nightly miri test`.
//!
//! **Constraints:** Miri is slow, so all tests use small iteration counts.
//! All tests are single-threaded (Miri has limited concurrency support).

// -----------------------------------------------------------------------
// Allocator tests — exercises resolve(), ptr::write/read (free-list),
// page allocation, and directory expansion.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_allocator {
    use faster_core::allocator::MallocFixedPageSize;

    /// A type large enough for the free-list linkage (>= 8 bytes)
    /// with proper alignment (>= 8 bytes).
    #[repr(C, align(8))]
    struct Item64([u64; 8]);

    #[test]
    fn allocate_and_read_write() {
        let alloc = MallocFixedPageSize::<Item64>::new();
        // Allocate — exercises bump_allocate and resolve() pointer arithmetic
        let addr = alloc.allocate();
        assert!(addr.is_valid());

        // Read — exercises get() → resolve() → ptr.add()
        let item = alloc.get(addr);
        // Fresh page is zero-initialized
        assert_eq!(item.0, [0u64; 8]);

        // Write — exercises unsafe get_mut()
        // SAFETY: Single-threaded, no other references to this item.
        let item_mut = unsafe { alloc.get_mut(addr) };
        item_mut.0[0] = 0xDEAD_BEEF;
        item_mut.0[7] = 0xCAFE_BABE;

        // Read back to verify write
        let item = alloc.get(addr);
        assert_eq!(item.0[0], 0xDEAD_BEEF);
        assert_eq!(item.0[7], 0xCAFE_BABE);
    }

    #[test]
    fn allocate_multiple_items_are_distinct() {
        let alloc = MallocFixedPageSize::<Item64>::new();
        let mut addrs = Vec::new();
        for _ in 0..50 {
            addrs.push(alloc.allocate());
        }
        // All addresses are valid
        for a in &addrs {
            assert!(a.is_valid());
        }
        // All addresses are distinct
        for (i, a) in addrs.iter().enumerate() {
            for b in &addrs[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn allocate_write_read_multiple() {
        let alloc = MallocFixedPageSize::<Item64>::new();
        let mut addrs = Vec::new();
        for i in 0..20u64 {
            let addr = alloc.allocate();
            // SAFETY: Single-threaded, exclusive access.
            let item = unsafe { alloc.get_mut(addr) };
            item.0[0] = i;
            item.0[3] = i * 100;
            addrs.push(addr);
        }
        // Read back all items to verify no overlap/corruption
        for (i, addr) in addrs.iter().enumerate() {
            let item = alloc.get(*addr);
            assert_eq!(item.0[0], i as u64);
            assert_eq!(item.0[3], (i as u64) * 100);
        }
    }

    #[test]
    fn free_and_reallocate() {
        let alloc = MallocFixedPageSize::<Item64>::new();
        let addr1 = alloc.allocate();
        // SAFETY: Single-threaded, exclusive access.
        unsafe { alloc.get_mut(addr1) }.0[0] = 42;

        // Free — exercises push_free_list (ptr::write of next-pointer)
        alloc.free(addr1);

        // Reallocate — exercises pop_free_list (ptr::read of next-pointer)
        // Free list is LIFO so we should get the same address back
        let addr2 = alloc.allocate();
        assert_eq!(addr1, addr2);
    }

    #[test]
    fn free_multiple_and_reallocate() {
        let alloc = MallocFixedPageSize::<Item64>::new();

        // Allocate several items
        let a1 = alloc.allocate();
        let a2 = alloc.allocate();
        let a3 = alloc.allocate();

        // Free all three (LIFO: a3 is at top of free list)
        alloc.free(a1);
        alloc.free(a2);
        alloc.free(a3);

        // Reallocate — should come back in LIFO order
        let r1 = alloc.allocate();
        let r2 = alloc.allocate();
        let r3 = alloc.allocate();

        assert_eq!(r1, a3, "LIFO: first realloc should be last freed");
        assert_eq!(r2, a2);
        assert_eq!(r3, a1);
    }

    #[test]
    fn count_tracks_bump_allocations() {
        let alloc = MallocFixedPageSize::<Item64>::new();
        assert_eq!(alloc.count(), 0);

        let _ = alloc.allocate();
        assert_eq!(alloc.count(), 1);

        let _ = alloc.allocate();
        assert_eq!(alloc.count(), 2);

        // Freeing doesn't decrease count (it tracks bump, not live)
        let a = alloc.allocate();
        alloc.free(a);
        assert_eq!(alloc.count(), 3);
    }

    #[test]
    fn get_ptr_returns_valid_pointer() {
        let alloc = MallocFixedPageSize::<Item64>::new();
        let addr = alloc.allocate();
        let ptr = alloc.get_ptr(addr);
        assert!(!ptr.is_null());

        // Write through raw pointer, read through get()
        // SAFETY: Single-threaded, valid pointer from allocator.
        unsafe { (*ptr).0[0] = 0x1234 };
        assert_eq!(alloc.get(addr).0[0], 0x1234);
    }
}

// -----------------------------------------------------------------------
// Hash bucket tests — exercises AtomicHashBucketEntry CAS operations,
// bucket scanning, and overflow chain traversal.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_hash_bucket {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash_bucket::{BUCKET_NUM_ENTRIES, HashBucket, HashBucketEntry};
    use faster_core::overflow::OverflowBucketPool;

    fn make_addr(p: u32, o: u32) -> LogicalAddress {
        LogicalAddress::new(Page(p), Offset(o))
    }

    #[test]
    fn bucket_entry_pack_unpack() {
        let addr = make_addr(1, 100);
        let entry = HashBucketEntry::new(0x1234, addr, false);
        assert_eq!(entry.tag(), 0x1234);
        assert_eq!(entry.address(), addr);
        assert!(!entry.is_tentative());
        assert!(!entry.is_empty());
    }

    #[test]
    fn bucket_entry_tentative_bit() {
        let addr = make_addr(2, 200);
        let entry = HashBucketEntry::new(42, addr, true);
        assert!(entry.is_tentative());
        let cleared = entry.without_tentative();
        assert!(!cleared.is_tentative());
        assert_eq!(cleared.tag(), 42);
        assert_eq!(cleared.address(), addr);
    }

    #[test]
    fn bucket_new_is_all_empty() {
        let bucket = HashBucket::new();
        for i in 0..BUCKET_NUM_ENTRIES {
            let entry = bucket.entry(i).load(Ordering::Relaxed);
            assert!(entry.is_empty(), "entry {i} should be empty");
        }
        assert_eq!(
            bucket.overflow_address().load(Ordering::Relaxed),
            LogicalAddress::ZERO,
        );
    }

    #[test]
    fn bucket_try_insert_and_find() {
        let bucket = HashBucket::new();

        // Insert entries with distinct tags
        for i in 0..BUCKET_NUM_ENTRIES as u16 {
            let addr = make_addr(1, i as u32 + 2);
            let result = bucket.try_insert(i, addr);
            assert!(result.is_ok(), "insert of tag {i} should succeed");
        }

        // Find them back via find_entry
        for i in 0..BUCKET_NUM_ENTRIES as u16 {
            let expected_addr = make_addr(1, i as u32 + 2);
            let found = bucket.find_entry(i);
            assert!(found.is_some(), "tag {i} should be found");
            let (_, entry) = found.unwrap();
            assert_eq!(entry.tag(), i);
            assert_eq!(entry.address(), expected_addr);
        }
    }

    #[test]
    fn bucket_full_returns_error() {
        let bucket = HashBucket::new();

        // Fill all 7 slots
        for i in 0..BUCKET_NUM_ENTRIES as u16 {
            bucket.try_insert(i, make_addr(1, i as u32 + 2)).unwrap();
        }

        // 8th insert should fail (bucket full, no overflow)
        let result = bucket.try_insert(99, make_addr(2, 2));
        assert!(result.is_err());
    }

    #[test]
    fn bucket_find_empty() {
        let bucket = HashBucket::new();
        // All empty initially
        assert_eq!(bucket.find_empty(), Some(0));

        // Fill first 3 slots
        for i in 0..3u16 {
            bucket.try_insert(i, make_addr(1, i as u32 + 2)).unwrap();
        }
        // Next empty should be slot 3
        assert_eq!(bucket.find_empty(), Some(3));

        // Fill remaining slots
        for i in 3..BUCKET_NUM_ENTRIES as u16 {
            bucket.try_insert(i, make_addr(1, i as u32 + 2)).unwrap();
        }
        // No empty slots
        assert_eq!(bucket.find_empty(), None);
    }

    #[test]
    fn overflow_pool_allocate_and_resolve() {
        let pool = OverflowBucketPool::new();
        let addr = pool.allocate();
        assert!(addr.is_valid());

        // Allocated bucket is zeroed
        let bucket = pool.get(addr);
        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(bucket.entry(i).load(Ordering::Relaxed).is_empty());
        }
        assert_eq!(
            bucket.overflow_address().load(Ordering::Relaxed),
            LogicalAddress::ZERO,
        );
    }

    #[test]
    fn overflow_pool_free_and_reuse() {
        let pool = OverflowBucketPool::new();
        let addr1 = pool.allocate();

        // Dirty the bucket
        let bucket = pool.get(addr1);
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::from_raw(42), false);
        bucket.entry(0).store(entry, Ordering::Relaxed);

        // Free and reallocate
        pool.free(addr1);
        let addr2 = pool.allocate();

        // Reused bucket should be re-zeroed
        let bucket2 = pool.get(addr2);
        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(
                bucket2.entry(i).load(Ordering::Relaxed).is_empty(),
                "entry {i} should be empty after free+realloc"
            );
        }
    }

    #[test]
    fn insert_in_chain_with_overflow() {
        let bucket = HashBucket::new();
        let pool = OverflowBucketPool::new();

        // Fill primary bucket (7 entries)
        for i in 0..BUCKET_NUM_ENTRIES as u16 {
            bucket
                .insert_in_chain(i, make_addr(1, i as u32 + 2), &pool)
                .unwrap();
        }

        // 8th insert — should allocate overflow bucket
        bucket.insert_in_chain(100, make_addr(2, 2), &pool).unwrap();

        // Verify overflow bucket exists
        let overflow = bucket.overflow_bucket(&pool);
        assert!(overflow.is_some(), "overflow bucket should exist");

        // Find entry in overflow via chain search
        let found = bucket.find_entry_in_chain(100, &pool);
        assert!(found.is_some(), "tag 100 should be findable in chain");
        let (_, _, entry) = found.unwrap();
        assert_eq!(entry.tag(), 100);
        assert_eq!(entry.address(), make_addr(2, 2));
    }

    #[test]
    fn for_each_bucket_walks_chain() {
        let bucket = HashBucket::new();
        let pool = OverflowBucketPool::new();

        // Fill primary bucket + force overflow
        for i in 0..8u16 {
            bucket
                .insert_in_chain(i, make_addr(1, i as u32 + 2), &pool)
                .unwrap();
        }

        // Walk chain and count buckets
        let mut count = 0u32;
        bucket.for_each_bucket(&pool, |_b| -> core::ops::ControlFlow<()> {
            count += 1;
            core::ops::ControlFlow::Continue(())
        });
        assert!(count >= 2, "should have primary + at least 1 overflow");
    }

    #[test]
    fn find_entry_in_chain_all_entries() {
        let bucket = HashBucket::new();
        let pool = OverflowBucketPool::new();

        // Insert 10 entries (forces overflow)
        for i in 0..10u16 {
            bucket
                .insert_in_chain(i, make_addr(1, i as u32 + 2), &pool)
                .unwrap();
        }

        // All 10 entries should be findable
        for i in 0..10u16 {
            let found = bucket.find_entry_in_chain(i, &pool);
            assert!(found.is_some(), "tag {i} should be found in chain");
            let (_, _, entry) = found.unwrap();
            assert_eq!(entry.address(), make_addr(1, i as u32 + 2));
        }
    }

    #[test]
    fn atomic_entry_cas_operations() {
        use faster_core::hash_bucket::AtomicHashBucketEntry;

        let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
        assert!(atomic.load(Ordering::Relaxed).is_empty());

        let addr = make_addr(5, 42);
        let new_entry = HashBucketEntry::new(0x0ABC, addr, false);

        // CAS from EMPTY to new_entry
        let result = atomic.compare_exchange(
            HashBucketEntry::EMPTY,
            new_entry,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert!(result.is_ok());
        assert_eq!(atomic.load(Ordering::Acquire), new_entry);

        // CAS should fail with wrong expected value
        let result2 = atomic.compare_exchange(
            HashBucketEntry::EMPTY,
            HashBucketEntry::EMPTY,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert!(result2.is_err());
    }
}

// -----------------------------------------------------------------------
// Epoch-gated allocator tests — exercises the deferred free path through
// the epoch drain callback and the push_free_list_raw unsafe function.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_epoch_gated_allocator {
    use faster_core::allocator::MallocFixedPageSize;
    use faster_core::epoch::EpochTable;
    use std::sync::Arc;

    /// A type large enough for the free-list linkage (>= 8 bytes).
    #[repr(C, align(8))]
    struct Item64([u64; 8]);

    #[test]
    fn epoch_gated_free_defers_push() {
        let epoch = Arc::new(EpochTable::new());
        let mut alloc = MallocFixedPageSize::<Item64>::new();
        alloc.set_epoch(Arc::clone(&epoch));

        let addr1 = alloc.allocate();
        // SAFETY: Single-threaded, exclusive access.
        unsafe { alloc.get_mut(addr1) }.0[0] = 42;

        // Free with epoch gating — item goes to deferred queue.
        alloc.free(addr1);

        // Next allocate should NOT reuse addr1 (it's deferred).
        let addr2 = alloc.allocate();
        assert_ne!(addr1, addr2, "epoch-gated free should not be immediate");

        // Bump epoch to drain deferred frees.
        epoch.bump_current_epoch_no_callback();

        // Now the freed item should be on the free list.
        let addr3 = alloc.allocate();
        assert_eq!(
            addr3, addr1,
            "after epoch drain, freed item should be reusable"
        );
    }

    #[test]
    fn epoch_gated_free_multiple_then_drain() {
        let epoch = Arc::new(EpochTable::new());
        let mut alloc = MallocFixedPageSize::<Item64>::new();
        alloc.set_epoch(Arc::clone(&epoch));

        let a = alloc.allocate();
        let b = alloc.allocate();
        let c = alloc.allocate();

        // Write distinct values.
        // SAFETY: Single-threaded, exclusive access.
        unsafe { alloc.get_mut(a) }.0[0] = 1;
        unsafe { alloc.get_mut(b) }.0[0] = 2;
        unsafe { alloc.get_mut(c) }.0[0] = 3;

        // Free all three (deferred).
        alloc.free(a);
        alloc.free(b);
        alloc.free(c);

        // Drain.
        epoch.bump_current_epoch_no_callback();

        // All three should be reusable.
        let mut reused = vec![alloc.allocate(), alloc.allocate(), alloc.allocate()];
        reused.sort_by_key(|addr| addr.raw());
        let mut expected = vec![a, b, c];
        expected.sort_by_key(|addr| addr.raw());
        assert_eq!(reused, expected, "all deferred frees should be reusable");
    }

    #[test]
    fn free_immediate_bypasses_epoch() {
        let epoch = Arc::new(EpochTable::new());
        let mut alloc = MallocFixedPageSize::<Item64>::new();
        alloc.set_epoch(Arc::clone(&epoch));

        let addr = alloc.allocate();
        alloc.free_immediate(addr);

        // Should be immediately reusable.
        let reused = alloc.allocate();
        assert_eq!(reused, addr, "free_immediate should bypass epoch gating");
    }

    #[test]
    fn free_without_epoch_is_immediate() {
        let alloc = MallocFixedPageSize::<Item64>::new();
        let addr = alloc.allocate();
        alloc.free(addr);

        let reused = alloc.allocate();
        assert_eq!(reused, addr, "without epoch, free should be immediate");
    }

    #[test]
    fn epoch_gated_drop_flushes_deferred() {
        let epoch = Arc::new(EpochTable::new());
        {
            let mut alloc = MallocFixedPageSize::<Item64>::new();
            alloc.set_epoch(Arc::clone(&epoch));

            let addr = alloc.allocate();
            // SAFETY: Single-threaded.
            unsafe { alloc.get_mut(addr) }.0[0] = 0xDEAD;
            alloc.free(addr);
            // Drop should flush deferred frees via epoch bump.
        }
        // If Drop didn't flush, deferred callbacks would hold dangling pointers.
        // Miri would catch any use-after-free.
    }
}
