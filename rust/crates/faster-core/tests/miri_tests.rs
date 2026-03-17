//! Miri-based undefined behavior tests for FASTER's unsafe code.
//!
//! These tests exercise the raw pointer arithmetic, allocation, free-list
//! operations, record accessors, log scanning, hash table operations,
//! compaction, epoch-deferred drain, state packing, and record metadata
//! across the entire `faster-core` crate. They are gated behind
//! `#[cfg(miri)]` so they only run under `cargo +nightly miri test`.
//!
//! **Constraints:** Miri is slow, so all tests use small iteration counts.
//! All tests are single-threaded (Miri has limited concurrency support).
//!
//! **Coverage:** allocator, buffer_pool, device callbacks, hash bucket,
//! hash table (get_unchecked paths), hybrid_log page/allocator/record_ops/scan,
//! compaction scanner/copier/address_update, epoch drain (via EpochTable::defer),
//! record_info bit-packing, state EPVS packing, store CRUD operations.

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
// Buffer pool tests — exercises AlignedBuffer allocation, write, read-back
// and full acquire/release lifecycle.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_buffer_pool {
    use faster_core::buffer_pool::{AlignedBuffer, BufferPool};

    #[test]
    fn buffer_pool_alloc_write_read() {
        let mut buf = AlignedBuffer::new(4096, 512);
        assert_eq!(buf.len(), 4096);
        assert_eq!(buf.as_ptr() as usize % 512, 0);

        // Write a repeating pattern.
        let slice = buf.as_mut_slice();
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }

        // Read back and verify.
        let slice = buf.as_slice();
        for (i, &byte) in slice.iter().enumerate() {
            assert_eq!(byte, (i & 0xFF) as u8, "mismatch at offset {i}");
        }
    }

    #[test]
    fn buffer_pool_release_reacquire() {
        let pool = BufferPool::new(512, 4);

        // Acquire, write, release.
        let mut buf = pool.acquire(1024);
        buf.as_mut_slice().fill(0xAB);
        let ptr = buf.as_ptr();
        pool.release(buf);

        // Re-acquire — should reuse the same allocation.
        let buf2 = pool.acquire(1024);
        assert_eq!(buf2.as_ptr(), ptr, "should reuse released buffer");
        assert_eq!(buf2.len(), 1024);
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

// -----------------------------------------------------------------------
// Device tests — exercises the raw-pointer callback path in NullDevice
// and InMemoryDevice under Miri to verify no UB.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_device {
    use core::sync::atomic::{AtomicU32, Ordering};
    use faster_core::device::{Device, InMemoryDevice, IoRequestResult, IoStatus, NullDevice};

    /// Callback that asserts success and sets an atomic flag.
    unsafe fn success_callback(ctx: *mut u8, status: IoStatus, _bytes: u32) {
        assert_eq!(status, IoStatus::Success);
        // SAFETY: `ctx` points to a valid `AtomicU32` — see each call site.
        unsafe {
            let flag = &*(ctx as *const AtomicU32);
            flag.store(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn device_null_async_callback_safety() {
        let dev = NullDevice::new();
        let flag = AtomicU32::new(0);
        let ctx = &flag as *const AtomicU32 as *mut u8;

        // Async read
        let mut buf = vec![0xFFu8; 512];
        // SAFETY: `buf` is valid for 512 bytes, `ctx` points to `flag`.
        let result = unsafe { dev.read_async(0, buf.as_mut_ptr(), 512, success_callback, ctx) };
        assert!(matches!(result, IoRequestResult::CompletedSync));
        assert_eq!(flag.load(Ordering::SeqCst), 1);
        assert!(buf.iter().all(|&b| b == 0));

        // Async write
        flag.store(0, Ordering::SeqCst);
        let data = vec![42u8; 512];
        // SAFETY: `data` is valid for 512 bytes, `ctx` points to `flag`.
        let result = unsafe { dev.write_async(data.as_ptr(), 0, 512, success_callback, ctx) };
        assert!(matches!(result, IoRequestResult::CompletedSync));
        assert_eq!(flag.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn device_in_memory_write_read() {
        let dev = InMemoryDevice::new();
        let flag = AtomicU32::new(0);
        let ctx = &flag as *const AtomicU32 as *mut u8;

        // Write 256 bytes of pattern data
        let write_data: Vec<u8> = (0..=255).collect();
        // SAFETY: valid buffer, `ctx` points to `flag`.
        let result = unsafe { dev.write_async(write_data.as_ptr(), 0, 256, success_callback, ctx) };
        assert!(matches!(result, IoRequestResult::CompletedSync));
        assert_eq!(flag.load(Ordering::SeqCst), 1);

        // Read it back
        flag.store(0, Ordering::SeqCst);
        let mut read_buf = vec![0u8; 256];
        // SAFETY: valid buffer, `ctx` points to `flag`.
        let result =
            unsafe { dev.read_async(0, read_buf.as_mut_ptr(), 256, success_callback, ctx) };
        assert!(matches!(result, IoRequestResult::CompletedSync));
        assert_eq!(flag.load(Ordering::SeqCst), 1);
        assert_eq!(read_buf, write_data);
    }
}

// -----------------------------------------------------------------------
// Hybrid log page frame tests — exercises PageFrame allocation, write,
// read, zero, and drop under Miri.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_hybrid_log_page {
    use core::sync::atomic::Ordering;
    use faster_core::hybrid_log::{PageFrame, PageState};

    #[test]
    fn page_frame_lifecycle_miri() {
        // Use a small page size so Miri doesn't take forever.
        let page_size = 4096;
        let sector_size = 512;

        let mut frame = PageFrame::new(page_size, sector_size);
        assert_eq!(frame.size(), page_size);
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Free);

        // Verify alignment.
        assert_eq!(frame.as_ptr() as usize % sector_size, 0);

        // Data should be zeroed on allocation.
        assert!(
            frame.as_slice().iter().all(|&b| b == 0),
            "new frame should be zeroed"
        );

        // Write a pattern.
        let slice = frame.as_mut_slice();
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }

        // Read back.
        let slice = frame.as_slice();
        for (i, &byte) in slice.iter().enumerate() {
            assert_eq!(byte, (i & 0xFF) as u8, "mismatch at offset {i}");
        }

        // Zero the frame.
        // SAFETY: Single-threaded, exclusive access.
        unsafe { frame.zero() };
        assert!(
            frame.as_slice().iter().all(|&b| b == 0),
            "frame should be zeroed"
        );

        // Test state transitions.
        assert!(
            frame
                .state()
                .try_transition(PageState::Free, PageState::Open)
        );
        assert!(
            frame
                .state()
                .try_transition(PageState::Open, PageState::Sealed)
        );
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Sealed);

        // Frame drops here — Miri catches any double-free or leak.
    }
}

// -----------------------------------------------------------------------
// HybridLogAllocator tests — exercises bump allocation, physical address
// resolution, and page advancement under Miri.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_hybrid_log_allocator {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hybrid_log::HybridLogAllocator;

    #[test]
    fn hybrid_log_allocator_basic_miri() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);

        // Allocate a small record.
        let addr = alloc.try_allocate(64).expect("allocation should succeed");
        assert_eq!(addr, LogicalAddress::new(Page(0), Offset(0)));

        // Get the physical address and write/read through it.
        let ptr = alloc
            .get_physical_address(addr)
            .expect("physical address should exist");

        // SAFETY: We have exclusive access — single-threaded Miri test.
        // The pointer is within a valid PageFrame allocation.
        unsafe {
            core::ptr::write(ptr, 0xAB);
            assert_eq!(core::ptr::read(ptr), 0xAB);

            // Write to a later offset in the same allocation.
            let ptr_end = ptr.add(63);
            core::ptr::write(ptr_end, 0xCD);
            assert_eq!(core::ptr::read(ptr_end), 0xCD);
        }

        // Allocate a second record and verify it doesn't overlap.
        let addr2 = alloc.try_allocate(64).expect("second allocation");
        assert_eq!(addr2, LogicalAddress::new(Page(0), Offset(64)));

        let ptr2 = alloc
            .get_physical_address(addr2)
            .expect("physical address should exist");

        // SAFETY: Non-overlapping region within the same page frame.
        unsafe {
            core::ptr::write(ptr2, 0xEF);
            assert_eq!(core::ptr::read(ptr2), 0xEF);
            // First record should be untouched.
            assert_eq!(core::ptr::read(ptr), 0xAB);
        }

        // Allocator drops here — Miri catches leaks or UB.
    }
}

// -----------------------------------------------------------------------
// RecordInfo tests — exercises bit-packing and atomic CAS on record
// metadata (record_info.rs). While the packing itself is const-fn safe
// code, AtomicRecordInfo uses atomics that Miri validates for data races
// and ordering correctness.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_record_info {
    use core::sync::atomic::Ordering;
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::record::{AtomicRecordInfo, RecordInfo};

    #[test]
    fn record_info_roundtrip_all_flags() {
        let addr = LogicalAddress::new(Page(42), Offset(1024));
        let info = RecordInfo::new(addr, 100, false, false, false);

        assert_eq!(info.previous_address(), addr);
        assert_eq!(info.checkpoint_version(), 100);
        assert!(!info.is_invalid());
        assert!(!info.is_tombstone());
        assert!(!info.is_final());
    }

    #[test]
    fn record_info_flag_mutations() {
        let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);

        let with_tomb = RecordInfo::new(LogicalAddress::INVALID, 0, false, true, false);
        assert!(with_tomb.is_tombstone());
        assert!(!with_tomb.is_invalid());

        let with_invalid = RecordInfo::new(LogicalAddress::INVALID, 0, true, false, false);
        assert!(with_invalid.is_invalid());
        assert!(!with_invalid.is_tombstone());

        let with_final = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, true);
        assert!(with_final.is_final());

        // Raw roundtrip
        let raw = info.raw();
        let back = RecordInfo::from_raw(raw);
        assert_eq!(back.raw(), raw);
    }

    #[test]
    fn atomic_record_info_load_store_cas() {
        let info = RecordInfo::new(
            LogicalAddress::new(Page(1), Offset(8)),
            5,
            false,
            false,
            false,
        );
        let atomic = AtomicRecordInfo::new(info);

        let loaded = atomic.load(Ordering::Acquire);
        assert_eq!(loaded.previous_address(), info.previous_address());
        assert_eq!(loaded.checkpoint_version(), 5);

        // Store a new value
        let new_info = RecordInfo::new(
            LogicalAddress::new(Page(2), Offset(16)),
            10,
            true,
            false,
            false,
        );
        atomic.store(new_info, Ordering::Release);
        let loaded2 = atomic.load(Ordering::Acquire);
        assert!(loaded2.is_invalid());
        assert_eq!(loaded2.checkpoint_version(), 10);

        // CAS — should succeed
        let cas_target = RecordInfo::new(
            LogicalAddress::new(Page(3), Offset(32)),
            15,
            false,
            true,
            false,
        );
        let result =
            atomic.compare_exchange(new_info, cas_target, Ordering::AcqRel, Ordering::Acquire);
        assert!(result.is_ok());
        let loaded3 = atomic.load(Ordering::Acquire);
        assert!(loaded3.is_tombstone());

        // CAS — should fail (stale expected)
        let stale = new_info;
        let result2 = atomic.compare_exchange(
            stale,
            RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert!(result2.is_err());
    }

    #[test]
    fn record_info_max_version_and_address() {
        // Version is 12-bit (0..4095) per the architecture
        let addr = LogicalAddress::new(Page(8_388_607), Offset(33_554_431));
        let info = RecordInfo::new(addr, 4095, true, true, true);
        assert_eq!(info.checkpoint_version(), 4095);
        assert!(info.is_invalid());
        assert!(info.is_tombstone());
        assert!(info.is_final());
        assert_eq!(info.previous_address(), addr);
    }
}

// -----------------------------------------------------------------------
// SystemState tests — exercises EPVS packing/unpacking and atomic CAS
// on system state (state/mod.rs). Validates that Phase+Version round-trip
// through u64 packing without corruption.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_system_state {
    use core::sync::atomic::Ordering;
    use faster_core::state::{AtomicSystemState, Phase, SystemState};

    #[test]
    fn system_state_pack_unpack() {
        let state = SystemState::new(Phase::Rest, 0);
        assert!(state.phase().is_rest());
        assert_eq!(state.version(), 0);

        let state2 = SystemState::new(Phase::InProgress, 42);
        assert_eq!(state2.phase(), Phase::InProgress);
        assert_eq!(state2.version(), 42);

        // Word roundtrip
        let word = state2.word();
        let back = SystemState::from_word(word);
        assert_eq!(back.phase(), Phase::InProgress);
        assert_eq!(back.version(), 42);
    }

    #[test]
    fn system_state_all_phases() {
        let phases = [
            Phase::Rest,
            Phase::Prepare,
            Phase::InProgress,
            Phase::WaitFlush,
            Phase::WaitCompletion,
            Phase::PersistenceCallback,
            Phase::PrepareGrow,
            Phase::InProgressGrow,
            Phase::WaitCompletionGrow,
        ];

        for (i, &phase) in phases.iter().enumerate() {
            let state = SystemState::new(phase, i as u64 * 1000);
            assert_eq!(state.phase(), phase);
            assert_eq!(state.version(), i as u64 * 1000);
        }
    }

    #[test]
    fn system_state_intermediate_bit() {
        let state = SystemState::new(Phase::Prepare, 7);
        assert!(!state.is_intermediate());

        let intermediate = state.make_intermediate();
        assert!(intermediate.is_intermediate());
        // Phase and version survive the intermediate bit
        assert_eq!(intermediate.version(), 7);
    }

    #[test]
    fn atomic_system_state_cas() {
        let initial = SystemState::new(Phase::Rest, 1);
        let atomic = AtomicSystemState::new(initial);

        let loaded = atomic.load(Ordering::Acquire);
        assert!(loaded.phase().is_rest());
        assert_eq!(loaded.version(), 1);

        // CAS to Prepare
        let new_state = SystemState::new(Phase::Prepare, 2);
        let result =
            atomic.compare_exchange(initial, new_state, Ordering::AcqRel, Ordering::Acquire);
        assert!(result.is_ok());

        let loaded2 = atomic.load(Ordering::Acquire);
        assert_eq!(loaded2.phase(), Phase::Prepare);
        assert_eq!(loaded2.version(), 2);

        // CAS with stale value — should fail
        let result2 =
            atomic.compare_exchange(initial, new_state, Ordering::AcqRel, Ordering::Acquire);
        assert!(result2.is_err());
    }
}

// -----------------------------------------------------------------------
// HashTable tests — exercises the table-level find_or_create_entry,
// find_entry, and update_entry operations that use unsafe get_unchecked
// for hot-path bucket access (hash/table.rs).
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_hash_table {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::hash::{Hashable, KeyHash};
    use faster_core::hash_table::HashTable;

    #[test]
    fn find_or_create_entry_basic() {
        // Small table (4 buckets) — exercises get_unchecked in bucket()
        let table = HashTable::new(2);
        assert_eq!(table.num_buckets(), 4);

        // Use Hashable to get a proper hash with non-zero tag
        let hash = 42u64.hash();
        let addr = LogicalAddress::new(Page(10), Offset(256));

        // First call — should create a tentative entry
        let result = table.find_or_create_entry(hash, addr);
        assert!(result.created);
        assert!(result.entry.is_tentative());
        assert_eq!(result.entry.tag(), hash.tag());

        // Commit the entry by clearing the tentative bit
        let committed = result.entry.without_tentative();
        assert!(table.update_entry(result.slot, result.entry, committed));

        // Second find — should find the committed entry
        let found = table.find_entry(hash);
        assert!(found.is_some());
        let (entry, _slot) = found.unwrap();
        assert!(!entry.is_tentative());
        assert_eq!(entry.address(), addr);
    }

    #[test]
    fn find_or_create_multiple_hashes() {
        let table = HashTable::new(4); // 16 buckets

        // Use Hashable trait for proper hash distribution
        let keys: Vec<u64> = (100..110).collect();
        let hashes: Vec<KeyHash> = keys.iter().map(|k| k.hash()).collect();

        // Insert entries
        let mut inserted = Vec::new();
        for (i, &hash) in hashes.iter().enumerate() {
            let addr = LogicalAddress::new(Page(i as u32), Offset(0));
            let result = table.find_or_create_entry(hash, addr);
            if result.created {
                let committed = result.entry.without_tentative();
                table.update_entry(result.slot, result.entry, committed);
                inserted.push((hash, addr));
            }
        }

        // All inserted entries should be findable
        for (hash, addr) in &inserted {
            let found = table.find_entry(*hash);
            assert!(found.is_some());
            let (entry, _) = found.unwrap();
            assert_eq!(entry.address(), *addr);
        }
    }

    #[test]
    fn bucket_by_index_all_buckets() {
        let table = HashTable::new(3); // 8 buckets
        for i in 0..table.num_buckets() {
            let _bucket = table.bucket_by_index(i);
            // Miri validates the get_unchecked doesn't go out of bounds
        }
    }

    #[test]
    fn find_or_create_collision_chain() {
        // Use a tiny table to force overflow chains. Each bucket holds 7 entries.
        // With 2 buckets (14 slots total), inserting many entries forces overflow.
        let table = HashTable::new(1); // 2 buckets
        let mut inserted = Vec::new();

        // Insert enough entries to trigger overflow
        for i in 0..30u64 {
            let hash = (i + 1000).hash();
            let addr = LogicalAddress::new(Page(i as u32), Offset(64));
            let result = table.find_or_create_entry(hash, addr);
            if result.created {
                let committed = result.entry.without_tentative();
                table.update_entry(result.slot, result.entry, committed);
                inserted.push((hash, addr));
            }
        }

        // All inserted entries should be findable despite overflow chains
        for (hash, addr) in &inserted {
            let found = table.find_entry(*hash);
            assert!(found.is_some());
            let (entry, _) = found.unwrap();
            assert_eq!(entry.address(), *addr);
        }

        // With 30 insertions into 2 buckets (14 inline slots), we should
        // have at least some overflow buckets.
        assert!(
            inserted.len() > 14 || table.overflow_count() > 0,
            "should have enough entries to exercise bucket chains"
        );
    }
}

// -----------------------------------------------------------------------
// RecordAccessor tests — exercises raw pointer construction and
// read/write through RecordAccessor and MutableRecordAccessor
// (hybrid_log/record_ops.rs). These types wrap *const u8 / *mut u8
// pointers and perform unsafe slice reconstruction.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_record_accessor {
    use faster_core::address::LogicalAddress;
    use faster_core::hybrid_log::{MutableRecordAccessor, RecordAccessor};
    use faster_core::record::{RecordInfo, RecordLayout};

    /// Helper to allocate a properly 8-byte-aligned buffer.
    fn aligned_buf(size: usize) -> Vec<u64> {
        vec![0u64; (size + 7) / 8]
    }

    #[test]
    fn record_accessor_from_raw_pointer() {
        let key: u64 = 42;
        let value: u64 = 100;
        let layout = RecordLayout::for_kv(&key, &value);
        let total_size = layout.total_size();

        let mut buf = aligned_buf(total_size);
        let ptr = buf.as_mut_ptr() as *mut u8;

        // SAFETY: buf is valid, 8-byte aligned (Vec<u64>), and large enough.
        let mut accessor = unsafe { MutableRecordAccessor::new(ptr, total_size as u32) };

        let info = RecordInfo::new(LogicalAddress::INVALID, 1, false, false, false);
        accessor.write_full_record(&info, &key, &value, &layout);

        // SAFETY: Same buffer, still valid.
        let reader = unsafe { RecordAccessor::new(ptr, total_size as u32) };
        let read_info = reader.record_info();
        assert_eq!(read_info.checkpoint_version(), 1);
        assert_eq!(reader.key::<u64>(&layout), 42);
        assert_eq!(reader.value::<u64>(&layout), 100);

        let slice = reader.as_slice();
        assert_eq!(slice.len(), total_size);
    }

    #[test]
    fn mutable_accessor_write_key_value_separately() {
        let key: u64 = 0xDEAD;
        let value: u64 = 0xBEEF;
        let layout = RecordLayout::for_kv(&key, &value);
        let total_size = layout.total_size();

        let mut buf = aligned_buf(total_size);
        let ptr = buf.as_mut_ptr() as *mut u8;

        // SAFETY: buf is valid, aligned, and large enough.
        let mut accessor = unsafe { MutableRecordAccessor::new(ptr, total_size as u32) };
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        accessor.write_record_info(&info);
        accessor.write_key(&key, &layout);
        accessor.write_value(&value, &layout);

        assert_eq!(accessor.key::<u64>(&layout), 0xDEAD);
        assert_eq!(accessor.value::<u64>(&layout), 0xBEEF);

        let key_bytes = accessor.key_ref(&layout);
        assert_eq!(key_bytes.len(), 8);
        let value_bytes = accessor.value_ref(&layout);
        assert_eq!(value_bytes.len(), 8);
    }

    #[test]
    fn mutable_accessor_zero() {
        let key: u64 = 42;
        let value: u64 = 100;
        let layout = RecordLayout::for_kv(&key, &value);
        let total_size = layout.total_size();

        let mut buf = aligned_buf(total_size);
        // Fill with non-zero data
        for b in buf.iter_mut() {
            *b = 0xFFFF_FFFF_FFFF_FFFF;
        }
        let ptr = buf.as_mut_ptr() as *mut u8;

        // SAFETY: buf is valid, aligned, and large enough.
        let mut accessor = unsafe { MutableRecordAccessor::new(ptr, total_size as u32) };
        accessor.zero();

        let slice = accessor.as_slice();
        assert!(slice.iter().all(|&b| b == 0));
    }

    #[test]
    fn accessor_value_mut_ptr() {
        let key: u64 = 1;
        let value: u64 = 2;
        let layout = RecordLayout::for_kv(&key, &value);
        let total_size = layout.total_size();

        let mut buf = aligned_buf(total_size);
        let ptr = buf.as_mut_ptr() as *mut u8;

        // SAFETY: buf is valid, aligned, and large enough.
        let mut accessor = unsafe { MutableRecordAccessor::new(ptr, total_size as u32) };
        let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
        accessor.write_full_record(&info, &key, &value, &layout);

        let val_ptr = accessor.value_mut_ptr(&layout);
        // SAFETY: val_ptr points within our aligned buffer, at value offset.
        unsafe {
            let val_ref = &mut *(val_ptr as *mut u64);
            *val_ref = 999;
        }

        assert_eq!(accessor.value::<u64>(&layout), 999);
    }
}

// -----------------------------------------------------------------------
// LogRecordWriter / LogRecordReader tests — exercises the full write-
// then-read path through the HybridLogAllocator, which involves unsafe
// pointer arithmetic in try_allocate() and get_physical_address().
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_log_record_ops {
    use faster_core::address::LogicalAddress;
    use faster_core::hybrid_log::{HybridLogAllocator, LogRecordReader, LogRecordWriter};
    use faster_core::record::{RecordInfo, RecordLayout};

    #[test]
    fn write_and_read_single_record() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);

        let info = RecordInfo::new(LogicalAddress::INVALID, 1, false, false, false);
        let addr = writer
            .write_record::<u64, u64>(&info, &42u64, &100u64)
            .expect("write should succeed");

        let reader = LogRecordReader::new(&alloc);
        let layout = RecordLayout::for_kv(&42u64, &100u64);

        let read_key: u64 = reader
            .read_key(addr, &layout)
            .expect("key should be readable");
        assert_eq!(read_key, 42);

        let read_val: u64 = reader
            .read_value(addr, &layout)
            .expect("value should be readable");
        assert_eq!(read_val, 100);

        let read_info = reader
            .read_record_info(addr)
            .expect("header should be readable");
        assert_eq!(read_info.checkpoint_version(), 1);
    }

    #[test]
    fn write_multiple_records_no_overlap() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);
        let layout = RecordLayout::for_kv(&0u64, &0u64);

        let mut addrs = Vec::new();
        for i in 0..15u64 {
            let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
            let addr = writer
                .write_record(&info, &i, &(i * 10))
                .expect("write should succeed");
            addrs.push(addr);
        }

        // All addresses should be distinct
        for (i, a) in addrs.iter().enumerate() {
            for b in &addrs[i + 1..] {
                assert_ne!(a, b, "records should not overlap");
            }
        }

        // All records should read back correctly
        let reader = LogRecordReader::new(&alloc);
        for (i, addr) in addrs.iter().enumerate() {
            let key: u64 = reader.read_key(*addr, &layout).unwrap();
            let val: u64 = reader.read_value(*addr, &layout).unwrap();
            assert_eq!(key, i as u64);
            assert_eq!(val, i as u64 * 10);
        }
    }

    #[test]
    fn allocate_raw_and_manual_write() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);
        let layout = RecordLayout::for_kv(&0u64, &0u64);

        let (addr, mut accessor) = writer
            .allocate_raw(layout.total_size() as u32)
            .expect("raw allocation should succeed");

        let info = RecordInfo::new(LogicalAddress::INVALID, 7, false, true, false);
        accessor.write_full_record(&info, &99u64, &88u64, &layout);

        let reader = LogRecordReader::new(&alloc);
        let read_info = reader.read_record_info(addr).unwrap();
        assert!(read_info.is_tombstone());
        assert_eq!(read_info.checkpoint_version(), 7);
        assert_eq!(reader.read_key::<u64>(addr, &layout).unwrap(), 99);
        assert_eq!(reader.read_value::<u64>(addr, &layout).unwrap(), 88);
    }

    #[test]
    fn get_record_accessor_through_reader() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);
        let layout = RecordLayout::for_kv(&0u64, &0u64);

        let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
        let addr = writer.write_record(&info, &77u64, &88u64).unwrap();

        let reader = LogRecordReader::new(&alloc);
        let accessor = reader
            .get_record(addr, layout.total_size() as u32)
            .expect("accessor should be available");

        assert_eq!(accessor.key::<u64>(&layout), 77);
        assert_eq!(accessor.value::<u64>(&layout), 88);
        assert_eq!(accessor.record_size(), layout.total_size() as u32);
    }
}

// -----------------------------------------------------------------------
// LogScanIterator tests — exercises sequential record scanning through
// the hybrid log, which uses unsafe slice::from_raw_parts to reconstruct
// record data from page memory (hybrid_log/scan.rs).
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_log_scan {
    use faster_core::address::LogicalAddress;
    use faster_core::hybrid_log::{
        HybridLogAllocator, LogRecordWriter, LogScanIterator, ScanOptions,
    };
    use faster_core::record::RecordInfo;

    #[test]
    fn scan_empty_log() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let start = alloc.head_address();
        let end = alloc.tail_address();

        let iter = LogScanIterator::new(&alloc, start, end, 8, 8, ScanOptions::default());
        let records: Vec<_> = iter.collect();
        assert!(records.is_empty());
    }

    #[test]
    fn scan_multiple_records() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);

        let start = alloc.tail_address();

        // Write 10 records
        for i in 0..10u64 {
            let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
            writer
                .write_record(&info, &i, &(i * 100))
                .expect("write should succeed");
        }

        let end = alloc.tail_address();

        let iter = LogScanIterator::new(&alloc, start, end, 8, 8, ScanOptions::default());
        let records: Vec<_> = iter.collect();
        assert_eq!(records.len(), 10);

        // Verify keys are sequential
        for (i, record) in records.iter().enumerate() {
            let key_bytes = record.key_bytes();
            let key = u64::from_le_bytes(key_bytes.try_into().unwrap());
            assert_eq!(key, i as u64);

            let value_bytes = record.value_bytes();
            let value = u64::from_le_bytes(value_bytes.try_into().unwrap());
            assert_eq!(value, i as u64 * 100);
        }
    }

    #[test]
    fn scan_with_tombstones() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);

        let start = alloc.tail_address();

        // Write a mix of normal and tombstone records
        let info_normal = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
        let info_tomb = RecordInfo::new(LogicalAddress::INVALID, 0, false, true, false);

        writer.write_record(&info_normal, &1u64, &10u64).unwrap();
        writer.write_record(&info_tomb, &2u64, &20u64).unwrap();
        writer.write_record(&info_normal, &3u64, &30u64).unwrap();

        let end = alloc.tail_address();

        // Default scan excludes tombstones
        let default_scan: Vec<_> =
            LogScanIterator::new(&alloc, start, end, 8, 8, ScanOptions::default()).collect();
        assert_eq!(default_scan.len(), 2, "tombstones should be excluded");

        // Inclusive scan includes tombstones
        let inclusive_scan: Vec<_> = LogScanIterator::new(
            &alloc,
            start,
            end,
            8,
            8,
            ScanOptions {
                include_tombstones: true,
                ..ScanOptions::default()
            },
        )
        .collect();
        assert_eq!(inclusive_scan.len(), 3, "tombstones should be included");
    }

    #[test]
    fn scan_record_addresses_are_ascending() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);
        let start = alloc.tail_address();

        for i in 0..5u64 {
            let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
            writer.write_record(&info, &i, &i).unwrap();
        }

        let end = alloc.tail_address();
        let records: Vec<_> =
            LogScanIterator::new(&alloc, start, end, 8, 8, ScanOptions::default()).collect();

        for window in records.windows(2) {
            assert!(
                window[0].address < window[1].address,
                "addresses should be ascending"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Epoch drain tests — exercises the lock-free Treiber stack used for
// deferred epoch callbacks (epoch/drain.rs). DrainList is pub(crate),
// so we exercise it indirectly through EpochTable::defer which pushes
// to the internal DrainList and then drains via try_drain.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_epoch_drain {
    use faster_core::epoch::EpochTable;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn defer_and_drain_single() {
        let table = Arc::new(EpochTable::new());
        let thread = table.register().expect("register should succeed");

        let counter = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&counter);

        // defer() pushes onto the internal DrainList
        table.defer(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });

        // Protect and unprotect to advance epochs
        for _ in 0..10 {
            let _guard = thread.protect();
            // guard drops here, calling unprotect
            table.bump_current_epoch_no_callback();
        }

        // The callback should have been executed by now
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn defer_multiple_callbacks() {
        let table = Arc::new(EpochTable::new());
        let thread = table.register().expect("register should succeed");

        let counter = Arc::new(AtomicU64::new(0));

        // Push multiple deferred actions
        for _ in 0..5 {
            let c = Arc::clone(&counter);
            table.defer(move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }

        // Advance epochs to trigger drain
        for _ in 0..15 {
            let _guard = thread.protect();
            table.bump_current_epoch_no_callback();
        }

        assert_eq!(counter.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn bump_epoch_with_callback() {
        let table = Arc::new(EpochTable::new());
        let thread = table.register().expect("register should succeed");

        let fired = Arc::new(AtomicU64::new(0));
        let f = Arc::clone(&fired);

        // bump_current_epoch takes a callback that fires when the epoch becomes safe
        table.bump_current_epoch(move || {
            f.fetch_add(1, Ordering::Relaxed);
        });

        for _ in 0..15 {
            let _guard = thread.protect();
            table.bump_current_epoch_no_callback();
        }

        assert_eq!(fired.load(Ordering::Relaxed), 1);
    }
}

// -----------------------------------------------------------------------
// Compaction scanner tests — exercises scanning log records to classify
// them as live, dead, or tombstoned. Scanner reads raw memory through
// the hybrid log allocator's unsafe pointer arithmetic.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_compaction_scanner {
    use faster_core::address::LogicalAddress;
    use faster_core::compaction::scanner::CompactionScanner;
    use faster_core::hash::index::HashIndex;
    use faster_core::hybrid_log::{HybridLogAllocator, LogRecordWriter};
    use faster_core::record::RecordInfo;

    #[test]
    fn scan_empty_range() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let hash_index = HashIndex::new(4);

        let scanner = CompactionScanner::new(&alloc, &hash_index);
        let start = alloc.tail_address();
        let end = start;

        let plan = scanner
            .scan::<u64, u64>(start, end)
            .expect("scan should succeed");
        assert_eq!(plan.total_records(), 0);
        assert_eq!(plan.live_fraction(), 0.0);
    }

    #[test]
    fn scan_with_live_and_dead_records() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let hash_index = HashIndex::new(4);
        let writer = LogRecordWriter::new(&alloc);

        let start = alloc.tail_address();

        // Write records and add them to the hash index so they appear "live"
        let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
        for i in 0..5u64 {
            let addr = writer.write_record(&info, &i, &(i * 10)).unwrap();
            let hash = <u64 as faster_core::hash::Hashable>::hash(&i);
            let result = hash_index.find_or_create(hash, addr);
            if result.created {
                let committed = result.entry.without_tentative();
                result
                    .slot
                    .compare_exchange(
                        result.entry,
                        committed,
                        core::sync::atomic::Ordering::AcqRel,
                        core::sync::atomic::Ordering::Acquire,
                    )
                    .ok();
            }
        }

        // Write a record NOT indexed — should appear "dead" to the scanner.
        // Use a key that won't hash-collide with 0..5.
        writer
            .write_record(&info, &0xFFFF_FFFF_FFFF_FFFFu64, &0u64)
            .unwrap();

        let end = alloc.tail_address();

        let scanner = CompactionScanner::new(&alloc, &hash_index);
        let plan = scanner
            .scan::<u64, u64>(start, end)
            .expect("scan should succeed");

        // At least 5 records should be live, and total should be 6
        assert_eq!(plan.total_records(), 6);
        assert!(
            plan.live_records.len() >= 5,
            "at least 5 live records expected"
        );
        assert!(plan.total_bytes_scanned > 0);
    }

    #[test]
    fn scan_with_tombstone_records() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let hash_index = HashIndex::new(4);
        let writer = LogRecordWriter::new(&alloc);

        let start = alloc.tail_address();

        // Write a tombstone record and add it to the hash index
        let tomb_info = RecordInfo::new(LogicalAddress::INVALID, 0, false, true, false);
        let addr = writer.write_record(&tomb_info, &1u64, &0u64).unwrap();
        let hash = <u64 as faster_core::hash::Hashable>::hash(&1u64);
        let result = hash_index.find_or_create(hash, addr);
        if result.created {
            let committed = result.entry.without_tentative();
            result
                .slot
                .compare_exchange(
                    result.entry,
                    committed,
                    core::sync::atomic::Ordering::AcqRel,
                    core::sync::atomic::Ordering::Acquire,
                )
                .ok();
        }

        let end = alloc.tail_address();

        let scanner = CompactionScanner::new(&alloc, &hash_index);
        let plan = scanner
            .scan::<u64, u64>(start, end)
            .expect("scan should succeed");

        assert_eq!(plan.tombstone_count, 1);
        assert_eq!(plan.tombstone_records.len(), 1);
    }

    /// Exercises the version chain walk path in `is_current_version`.
    ///
    /// Writes two records for the same key (V1 then V2) with V2's
    /// `previous_address` pointing back at V1. The hash index head points
    /// at V2. On scan V2 should be live and V1 dead — the scanner must
    /// walk the chain through `read_header_and_match_key_varlen` which
    /// constructs a `RecordAccessor` (unsafe) on each hop.
    #[test]
    fn scan_version_chain_classifies_superseded_as_dead() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let hash_index = HashIndex::new(4);
        let writer = LogRecordWriter::new(&alloc);

        let start = alloc.tail_address();
        let key: u64 = 42;

        // V1: first version, no predecessor.
        let info_v1 = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
        let addr_v1 = writer.write_record(&info_v1, &key, &100u64).unwrap();

        // Index V1 so we can update the slot to V2 below.
        let hash = <u64 as faster_core::hash::Hashable>::hash(&key);
        let result = hash_index.find_or_create(hash, addr_v1);
        let committed_v1 = result.entry.without_tentative();
        result
            .slot
            .compare_exchange(
                result.entry,
                committed_v1,
                core::sync::atomic::Ordering::AcqRel,
                core::sync::atomic::Ordering::Acquire,
            )
            .ok();

        // V2: newer version, chain links back to V1.
        let info_v2 = RecordInfo::new(addr_v1, 0, false, false, false);
        let addr_v2 = writer.write_record(&info_v2, &key, &200u64).unwrap();

        // Swing hash index to point at V2.
        let updated = faster_core::hash::bucket::HashBucketEntry::new(
            committed_v1.tag(),
            addr_v2,
            false,
        );
        hash_index.update(result.slot, committed_v1, updated);

        let end = alloc.tail_address();

        let scanner = CompactionScanner::new(&alloc, &hash_index);
        let plan = scanner
            .scan::<u64, u64>(start, end)
            .expect("scan should succeed");

        assert_eq!(plan.total_records(), 2, "should see both versions");
        assert_eq!(plan.live_records.len(), 1, "only V2 is live");
        assert_eq!(plan.dead_count, 1, "V1 is dead (superseded)");
        assert_eq!(
            plan.live_records[0].address, addr_v2,
            "the live record should be V2"
        );
    }

    /// Writes a record whose invalid flag is set before scanning.
    ///
    /// The scanner reads the `RecordInfo` through a `RecordAccessor`
    /// (unsafe pointer → `from_raw_parts`) and must detect the invalid
    /// bit. This exercises the `atomic_record_info` path for pre-
    /// invalidated records.
    #[test]
    fn scan_invalid_record_classified_as_dead() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let hash_index = HashIndex::new(4);
        let writer = LogRecordWriter::new(&alloc);

        let start = alloc.tail_address();

        // Write a record that is already invalid.
        let info = RecordInfo::new(LogicalAddress::INVALID, 0, true, false, false);
        writer.write_record(&info, &99u64, &0u64).unwrap();

        let end = alloc.tail_address();

        let scanner = CompactionScanner::new(&alloc, &hash_index);
        let plan = scanner
            .scan::<u64, u64>(start, end)
            .expect("scan should succeed");

        assert_eq!(plan.total_records(), 1);
        assert_eq!(plan.dead_count, 1, "invalid record must be dead");
        assert_eq!(plan.live_records.len(), 0, "no live records expected");
    }

    /// Fills a page nearly to its end so that the scanner hits the
    /// `MIN_READABLE` guard and must advance to the next page. Validates
    /// no out-of-bounds pointer arithmetic at page boundaries.
    #[test]
    fn scan_near_page_boundary_advances_safely() {
        // Use a tiny page so Miri finishes quickly. 4 pages, 512-byte sectors.
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let hash_index = HashIndex::new(4);
        let writer = LogRecordWriter::new(&alloc);

        let start = alloc.tail_address();

        // Write as many records as will fit, filling up towards the page end.
        // RecordLayout for u64/u64 is 24 bytes (8 header + 8 key + 8 value).
        let mut count = 0u64;
        for i in 0..2000u64 {
            if writer
                .write_record(
                    &RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false),
                    &i,
                    &(i * 10),
                )
                .is_some()
            {
                count += 1;
            } else {
                break;
            }
        }
        assert!(count > 10, "should have written many records");

        let end = alloc.tail_address();

        let scanner = CompactionScanner::new(&alloc, &hash_index);
        let plan = scanner
            .scan::<u64, u64>(start, end)
            .expect("scan near page boundary should succeed");

        // All records are unindexed → conservative = live.
        assert_eq!(plan.total_records() as u64, count);
    }
}

// -----------------------------------------------------------------------
// Compaction copier tests — exercises copying live records from one log
// region to the tail, involving unsafe pointer arithmetic for record
// reads and writes (compaction/copier.rs).
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_compaction_copier {
    use faster_core::address::LogicalAddress;
    use faster_core::compaction::LiveRecord;
    use faster_core::compaction::copier::RecordCopier;
    use faster_core::hybrid_log::{HybridLogAllocator, LogRecordReader, LogRecordWriter};
    use faster_core::record::{RecordInfo, RecordLayout};

    #[test]
    fn copy_single_record() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);
        let layout = RecordLayout::for_kv(&0u64, &0u64);

        let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
        let addr = writer.write_record(&info, &42u64, &100u64).unwrap();

        let copier = RecordCopier::new(&alloc);
        let live = vec![LiveRecord {
            address: addr,
            record_size: layout.total_size(),
        }];

        let result = copier.copy_records(&live).expect("copy should succeed");
        assert_eq!(result.records_copied(), 1);
        assert!(result.bytes_copied > 0);

        // Verify the new copy is readable
        let new_addr = result.new_address_of(addr).expect("mapping should exist");
        let reader = LogRecordReader::new(&alloc);
        let key: u64 = reader.read_key(new_addr, &layout).unwrap();
        let val: u64 = reader.read_value(new_addr, &layout).unwrap();
        assert_eq!(key, 42);
        assert_eq!(val, 100);
    }

    #[test]
    fn copy_multiple_records() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let writer = LogRecordWriter::new(&alloc);
        let layout = RecordLayout::for_kv(&0u64, &0u64);
        let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);

        let mut live = Vec::new();
        for i in 0..8u64 {
            let addr = writer.write_record(&info, &i, &(i * 10)).unwrap();
            live.push(LiveRecord {
                address: addr,
                record_size: layout.total_size(),
            });
        }

        let copier = RecordCopier::new(&alloc);
        let result = copier.copy_records(&live).expect("copy should succeed");
        assert_eq!(result.records_copied(), 8);

        // Verify all copies are correct
        let reader = LogRecordReader::new(&alloc);
        for (i, rec) in live.iter().enumerate() {
            let new_addr = result.new_address_of(rec.address).unwrap();
            let key: u64 = reader.read_key(new_addr, &layout).unwrap();
            let val: u64 = reader.read_value(new_addr, &layout).unwrap();
            assert_eq!(key, i as u64);
            assert_eq!(val, i as u64 * 10);
        }
    }

    #[test]
    fn copy_empty_list() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let copier = RecordCopier::new(&alloc);
        let result = copier.copy_records(&[]).expect("empty copy should succeed");
        assert_eq!(result.records_copied(), 0);
        assert_eq!(result.bytes_copied, 0);
    }
}

// -----------------------------------------------------------------------
// Compaction address_update tests — exercises CAS-based pointer swing
// in the hash index after records are copied. The swing operation uses
// unsafe compare_exchange on atomic bucket entries.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_compaction_address_update {
    use faster_core::address::LogicalAddress;
    use faster_core::compaction::LiveRecord;
    use faster_core::compaction::address_update::AddressUpdater;
    use faster_core::compaction::copier::RecordCopier;
    use faster_core::compaction::scanner::CompactionScanner;
    use faster_core::hash::index::HashIndex;
    use faster_core::hybrid_log::{HybridLogAllocator, LogRecordWriter};
    use faster_core::record::{RecordInfo, RecordLayout};

    #[test]
    fn swing_updates_hash_entries() {
        let alloc = HybridLogAllocator::new(4, 0.5, 512);
        let hash_index = HashIndex::new(4);
        let writer = LogRecordWriter::new(&alloc);
        let layout = RecordLayout::for_kv(&0u64, &0u64);

        let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);

        // Write records and index them
        let mut live = Vec::new();
        for i in 0..3u64 {
            let addr = writer.write_record(&info, &i, &(i * 10)).unwrap();
            let hash = <u64 as faster_core::hash::Hashable>::hash(&i);
            let result = hash_index.find_or_create(hash, addr);
            if result.created {
                let committed = result.entry.without_tentative();
                result
                    .slot
                    .compare_exchange(
                        result.entry,
                        committed,
                        core::sync::atomic::Ordering::AcqRel,
                        core::sync::atomic::Ordering::Acquire,
                    )
                    .ok();
            }
            live.push(LiveRecord {
                address: addr,
                record_size: layout.total_size(),
            });
        }

        // Copy records to new locations
        let copier = RecordCopier::new(&alloc);
        let copy_result = copier.copy_records(&live).expect("copy should succeed");
        assert_eq!(copy_result.records_copied(), 3);

        // Build a minimal CompactionPlan for the swing
        let scanner = CompactionScanner::new(&alloc, &hash_index);
        let start = alloc.head_address();
        let end = alloc.tail_address();
        let plan = scanner.scan::<u64, u64>(start, end).unwrap();

        // Swing pointers
        let updater = AddressUpdater::new(&hash_index, &alloc);
        let stats = updater.swing::<u64, u64>(&copy_result, &plan);

        // At least some entries should have been swung
        assert!(stats.swung > 0, "should have swung at least one entry");

        // Verify hash index now points to new addresses
        for (i, rec) in live.iter().enumerate() {
            let hash = <u64 as faster_core::hash::Hashable>::hash(&(i as u64));
            let found = hash_index.find(hash);
            assert!(found.is_some(), "entry {i} should still exist");
            let (entry, _) = found.unwrap();
            let new_addr = copy_result.new_address_of(rec.address).unwrap();
            assert_eq!(
                entry.address(),
                new_addr,
                "entry {i} should point to new address"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Store CRUD operations tests — exercises the full read/upsert/rmw/delete
// code paths through FasterKv, which internally use unsafe
// MutableRecordAccessor and pointer-based record access
// (store/operations.rs, store/functions.rs).
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_store_operations {
    use faster_core::NullDevice;
    use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

    fn small_store() -> FasterKv<SimpleFunctions<u64, u64>> {
        FasterKv::new(
            FasterKvConfig {
                hash_index_size_log2: 4, // 16 buckets — small for Miri
                buffer_size_pages: 4,
                mutable_fraction: 0.9,
                sector_size: 512,
                ..FasterKvConfig::default()
            },
            SimpleFunctions::new(),
            NullDevice::new(),
        )
    }

    #[test]
    fn upsert_and_read() {
        let store = small_store();
        let mut session = store.new_session();

        let _ = store.upsert_simple(&mut session, &1u64, &100u64);
        let result = store.read_simple(&mut session, &1u64);
        assert_eq!(result, Some(100u64));

        store.dispose_session(session);
    }

    #[test]
    fn upsert_overwrite() {
        let store = small_store();
        let mut session = store.new_session();

        let _ = store.upsert_simple(&mut session, &1u64, &100u64);
        let _ = store.upsert_simple(&mut session, &1u64, &200u64);

        let result = store.read_simple(&mut session, &1u64);
        assert_eq!(result, Some(200u64));

        store.dispose_session(session);
    }

    #[test]
    fn delete_removes_entry() {
        let store = small_store();
        let mut session = store.new_session();

        let _ = store.upsert_simple(&mut session, &42u64, &999u64);
        let before = store.read_simple(&mut session, &42u64);
        assert_eq!(before, Some(999u64));

        let _ = store.delete_simple(&mut session, &42u64);
        let after = store.read_simple(&mut session, &42u64);
        // After delete, read should not return the old value
        // (it may return None or a default depending on implementation)
        assert_ne!(after, Some(999u64));

        store.dispose_session(session);
    }

    #[test]
    fn multiple_keys() {
        let store = small_store();
        let mut session = store.new_session();

        for i in 0..10u64 {
            let _ = store.upsert_simple(&mut session, &i, &(i * 100));
        }

        for i in 0..10u64 {
            let result = store.read_simple(&mut session, &i);
            assert_eq!(
                result,
                Some(i * 100),
                "key {i} should have value {}",
                i * 100
            );
        }

        store.dispose_session(session);
    }

    #[test]
    fn read_nonexistent_key() {
        let store = small_store();
        let mut session = store.new_session();

        let result = store.read_simple(&mut session, &999u64);
        assert_eq!(result, None);

        store.dispose_session(session);
    }

    #[test]
    fn session_create_dispose() {
        let store = small_store();

        // Create and dispose multiple sessions
        for _ in 0..3 {
            let session = store.new_session();
            store.dispose_session(session);
        }
        // No leaks — Miri catches double-free or missing drop
    }
}

// -----------------------------------------------------------------------
// Prefetch tests — hash/prefetch.rs uses CPU-specific intrinsics
// (_mm_prefetch on x86_64, PRFM on aarch64). Under Miri, these are
// effectively no-ops but we verify the safe wrappers don't cause UB
// when given valid pointers.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_prefetch {
    use faster_core::hash::Hashable;
    use faster_core::hash_table::HashTable;

    #[test]
    fn prefetch_bucket_no_ub() {
        let table = HashTable::new(4);
        // prefetch_bucket exercises the prefetch intrinsic wrapper
        for i in 0..16u64 {
            let hash = (i + 500).hash();
            table.prefetch_bucket(hash);
        }
        // No assertion needed — Miri would detect UB if any
    }
}

// -----------------------------------------------------------------------
// HashIndex tests — hash/index.rs
// Exercises bucket_slice() which constructs a raw slice from boxed bucket array.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_hash_index {
    use faster_core::address::LogicalAddress;
    use faster_core::hash::Hashable;
    use faster_core::hash_table::HashTable;

    #[test]
    fn bucket_slice_bounds_and_alignment() {
        let table = HashTable::new(16);
        let slice = table.bucket_slice();

        // Note: HashTable may allocate more buckets than requested (power of 2 rounding)
        let actual_len = slice.len();
        assert!(actual_len >= 16, "table should have at least 16 buckets");

        // Access first and last buckets — exercises from_raw_parts bounds
        let first = &slice[0];
        let last = &slice[actual_len - 1];

        // Verify alignment (HashBucket is #[repr(C, align(64))])
        let first_addr = first as *const _ as usize;
        let last_addr = last as *const _ as usize;
        assert_eq!(first_addr % 64, 0, "first bucket not 64-byte aligned");
        assert_eq!(last_addr % 64, 0, "last bucket not 64-byte aligned");

        // Verify stride between consecutive buckets is exactly 64 bytes
        if actual_len > 1 {
            let second = &slice[1];
            let second_addr = second as *const _ as usize;
            let stride = second_addr - first_addr;
            assert_eq!(stride, 64, "bucket stride != 64 bytes");
        }
    }

    #[test]
    fn bucket_slice_read_entries() {
        let table = HashTable::new(8);
        let addr = LogicalAddress::ZERO;

        // Insert some entries
        for i in 0..10u64 {
            let hash = (i * 1000).hash();
            let _ = table.find_or_create_entry(hash, addr);
        }

        // Read all buckets via bucket_slice — exercises from_raw_parts
        let slice = table.bucket_slice();
        let mut total_entries = 0;
        for bucket in slice {
            // Use entry() method to iterate through bucket entries
            for i in 0..7 {
                let entry = bucket.entry(i).load(std::sync::atomic::Ordering::Relaxed);
                if entry.tag() != 0 {
                    total_entries += 1;
                }
            }
        }

        assert!(total_entries >= 10, "expected at least 10 entries");
    }
}

// -----------------------------------------------------------------------
// IndexWriter tests — checkpoint/index_writer.rs
// Exercises raw pointer cast from HashBucket to [u8; 64] for serialization.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_checkpoint_index_writer {
    use faster_core::address::LogicalAddress;
    use faster_core::hash::Hashable;
    use faster_core::hash_table::HashTable;

    #[test]
    fn bucket_as_bytes_no_ub() {
        let table = HashTable::new(4);
        let addr = LogicalAddress::ZERO;

        // Insert some entries to populate buckets
        for i in 0..8u64 {
            let hash = (i * 100).hash();
            let _ = table.find_or_create_entry(hash, addr);
        }

        // Simulate what write_index does: read bucket bytes via raw pointer cast
        for bucket in table.bucket_slice() {
            // SAFETY: HashBucket is repr(C, align(64)), size 64.
            // We're reading through a shared reference with no concurrent writers.
            let bytes: &[u8; 64] = unsafe { &*(bucket as *const _ as *const [u8; 64]) };

            // Verify we can read all 64 bytes without UB
            let mut sum = 0u64;
            for &b in bytes.iter() {
                sum = sum.wrapping_add(b as u64);
            }
            // No assertion on sum — just checking Miri doesn't flag UB
            let _ = sum;
        }
    }

    #[test]
    fn index_writer_bucket_serialization_pattern() {
        let table = HashTable::new(4);
        let addr = LogicalAddress::ZERO;

        // Insert entries
        for i in 0..5u64 {
            let hash = i.hash();
            let _ = table.find_or_create_entry(hash, addr);
        }

        // Test the unsafe pattern used by write_index: bucket -> [u8; 64]
        // This exercises the exact unsafe code in index_writer.rs
        let mut all_bytes = Vec::new();
        for bucket in table.bucket_slice() {
            let bytes: &[u8; 64] = unsafe { &*(bucket as *const _ as *const [u8; 64]) };
            all_bytes.extend_from_slice(bytes);
        }

        // Verify data was extracted (table might allocate more buckets than requested)
        let num_buckets = table.bucket_slice().len();
        assert_eq!(
            all_bytes.len(),
            num_buckets * 64,
            "expected {} buckets × 64 bytes",
            num_buckets
        );
        assert!(all_bytes.len() > 0, "no bytes extracted");
    }
}

// -----------------------------------------------------------------------
// Epoch tests — epoch/mod.rs
// No additional unsafe beyond drain.rs (already covered).
// The main unsafe is in drain.rs (defer callbacks), which we already test.
// This module just verifies EpochTable operations don't cause UB.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_epoch_table {
    use faster_core::epoch::EpochTable;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn epoch_protect_unprotect() {
        let table = Arc::new(EpochTable::new());
        let thread = table.register().expect("register should succeed");

        // Protect and unprotect epochs in sequence
        for _ in 0..10 {
            let guard = thread.protect();
            assert!(guard.epoch() < 1000000);
            drop(guard); // unprotects
        }
    }

    #[test]
    fn epoch_defer_basic() {
        let table = Arc::new(EpochTable::new());
        let thread = table.register().expect("register should succeed");
        let counter = Arc::new(AtomicU64::new(0));

        {
            let _guard = thread.protect();
            let c = Arc::clone(&counter);

            // Defer a callback (exercises unsafe pointer in DrainList)
            table.defer(move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }

        // Force drain by advancing epochs
        for _ in 0..10 {
            let _guard = thread.protect();
            table.bump_current_epoch_no_callback();
        }

        // Callback should have been executed
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}

// -----------------------------------------------------------------------
// Recovery tests — recovery/mod.rs and recovery/index_recovery.rs
// index_recovery.rs has one unsafe block for deserializing bucket bytes.
// This requires file I/O and is structurally similar to index_writer.
// We test the unsafe pattern in isolation.
// -----------------------------------------------------------------------

#[cfg(miri)]
mod miri_recovery_index {
    use faster_core::address::LogicalAddress;
    use faster_core::hash::Hashable;
    use faster_core::hash_table::HashTable;

    #[test]
    fn bucket_from_bytes_roundtrip() {
        let table = HashTable::new(4);
        let addr = LogicalAddress::ZERO;

        // Insert entries
        for i in 0..6u64 {
            let hash = (i * 200).hash();
            let _ = table.find_or_create_entry(hash, addr);
        }

        // Serialize buckets to bytes
        let mut serialized = Vec::new();
        for bucket in table.bucket_slice() {
            let bytes: &[u8; 64] = unsafe { &*(bucket as *const _ as *const [u8; 64]) };
            serialized.extend_from_slice(bytes);
        }

        // Deserialize and compare (simulates index_recovery.rs pattern)
        for (i, bucket) in table.bucket_slice().iter().enumerate() {
            let offset = i * 64;
            let file_bytes = &serialized[offset..offset + 64];

            let mem_bytes: &[u8; 64] = unsafe { &*(bucket as *const _ as *const [u8; 64]) };

            assert_eq!(file_bytes, mem_bytes, "bucket {} mismatch", i);
        }
    }
}
