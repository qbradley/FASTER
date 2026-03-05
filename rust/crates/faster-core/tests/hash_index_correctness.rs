//! Hash index correctness tests — single-threaded end-to-end validation.
//!
//! This is the capstone correctness suite for the FASTER hash index (Phase 2f).
//! Each test validates a specific behavioral property of [`HashIndex`] using
//! deterministic, single-threaded execution. Multi-threaded stress tests live
//! in `hash_index_concurrent.rs`.

mod common;

use faster_core::address::{LogicalAddress, Offset, Page};
use faster_core::hash::KeyHash;
use faster_core::hash_bucket::HashBucketEntry;
use faster_core::hash_index::HashIndex;

// ===========================================================================
// Helpers
// ===========================================================================

/// Create a small hash index for testing (2^8 = 256 buckets).
fn test_index() -> HashIndex {
    HashIndex::new(8)
}

/// Create a [`KeyHash`] from a seed with good distribution across buckets and tags.
fn make_hash(seed: u64) -> KeyHash {
    KeyHash::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Create a valid [`LogicalAddress`] from simple numeric inputs.
fn make_addr(page: u32, offset: u32) -> LogicalAddress {
    LogicalAddress::new(Page(page), Offset(offset))
}

/// Insert a key (by seed) into the index, commit it, and return its address.
fn insert_committed(index: &HashIndex, seed: u64, page: u32, offset: u32) -> LogicalAddress {
    let hash = make_hash(seed);
    let addr = make_addr(page, offset);
    let r = index.find_or_create(hash, LogicalAddress::INVALID);
    assert!(r.created, "seed {seed} should create a new entry");
    let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
    assert!(index.update(r.slot, r.entry, committed));
    addr
}

// ===========================================================================
// 1. Insert N keys → find all N → correct addresses
// ===========================================================================

/// Insert 200 unique keys, then verify each is findable with the correct address.
#[test]
fn insert_n_then_find_all() {
    let index = HashIndex::new(14); // larger table to minimize collisions
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let n = 200u64;
    let mut entries = Vec::new();

    for i in 0..n {
        let hash = make_hash(i);
        let addr = make_addr((i / 100 + 1) as u32, (i % 100 + 2) as u32);
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        if r.created {
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            assert!(index.update(r.slot, r.entry, committed));
            entries.push((hash, addr));
        }
    }

    // All created entries must be findable with correct addresses.
    for (hash, expected_addr) in &entries {
        let (found, _slot) = index.find(*hash).expect("entry should be findable");
        assert_eq!(found.address(), *expected_addr);
    }

    assert_eq!(index.entry_count(), entries.len() as u64);
}

// ===========================================================================
// 2. Insert → update address → find → returns new address
// ===========================================================================

/// Insert a key, commit it at address A, then update to address B. Find must
/// return address B.
#[test]
fn insert_update_address_then_find() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let hash = make_hash(42);
    let addr1 = make_addr(1, 100);
    let addr2 = make_addr(2, 200);

    // Create and commit with addr1.
    let r = index.find_or_create(hash, LogicalAddress::INVALID);
    assert!(r.created);
    let committed1 = HashBucketEntry::new(r.entry.tag(), addr1, false);
    assert!(index.update(r.slot, r.entry, committed1));

    // Update to addr2.
    let committed2 = HashBucketEntry::new(r.entry.tag(), addr2, false);
    assert!(index.update(r.slot, committed1, committed2));

    // Find must return addr2.
    let (found, _) = index.find(hash).unwrap();
    assert_eq!(found.address(), addr2);
}

// ===========================================================================
// 3. Insert → GC invalidate range → partial invalidation
// ===========================================================================

/// Insert entries on pages 1, 3, and 5. Invalidate [page 2, page 4).
/// Only page-3 entries should be gone; pages 1 and 5 intact.
#[test]
fn gc_invalidate_range_partial() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    // 10 entries on page 1, 10 on page 3, 10 on page 5.
    for i in 0..10u64 {
        insert_committed(&index, 1000 + i, 1, i as u32 + 2);
    }
    for i in 0..10u64 {
        insert_committed(&index, 2000 + i, 3, i as u32 + 2);
    }
    for i in 0..10u64 {
        insert_committed(&index, 3000 + i, 5, i as u32 + 2);
    }
    assert_eq!(index.entry_count(), 30);

    // Invalidate pages 2..4 (includes page 3).
    let invalidated = index.invalidate_entries_in_range(make_addr(2, 0), make_addr(4, 0));
    assert_eq!(invalidated, 10);
    assert_eq!(index.entry_count(), 20);

    // Page 1 and 5 entries still findable.
    for i in 0..10u64 {
        assert!(index.find(make_hash(1000 + i)).is_some(), "page 1 entry {i} missing");
        assert!(index.find(make_hash(3000 + i)).is_some(), "page 5 entry {i} missing");
    }
    // Page 3 entries gone.
    for i in 0..10u64 {
        assert!(index.find(make_hash(2000 + i)).is_none(), "page 3 entry {i} should be gone");
    }
}

// ===========================================================================
// 4. Tentative entry: find returns None until committed
// ===========================================================================

/// Create a tentative entry → find returns None → commit → find returns entry.
#[test]
fn tentative_entry_invisible_until_committed() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let hash = make_hash(999);
    let addr = make_addr(7, 512);

    // Create tentative entry.
    let r = index.find_or_create(hash, LogicalAddress::INVALID);
    assert!(r.created);
    assert!(r.entry.is_tentative());

    // find should return None — tentative entries are invisible.
    assert!(index.find(hash).is_none());

    // Commit the entry.
    let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
    assert!(index.update(r.slot, r.entry, committed));

    // Now find should return the entry.
    let (found, _) = index.find(hash).unwrap();
    assert_eq!(found.address(), addr);
    assert!(!found.is_tentative());
}

// ===========================================================================
// 5. Tentative abort: CAS back to EMPTY → find returns None
// ===========================================================================

/// Create a tentative entry → CAS back to EMPTY → find returns None, entry_count == 0.
#[test]
fn tentative_abort_via_cas_to_empty() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let hash = make_hash(888);

    let r = index.find_or_create(hash, LogicalAddress::INVALID);
    assert!(r.created);
    assert_eq!(index.entry_count(), 1);

    // Abort: CAS tentative entry back to EMPTY.
    assert!(index.update(r.slot, r.entry, HashBucketEntry::EMPTY));

    assert!(index.find(hash).is_none());
    assert_eq!(index.entry_count(), 0);
}

// ===========================================================================
// 6. Overflow chains: 20+ entries into one bucket via crafted hashes
// ===========================================================================

/// Force 20+ entries into the same primary bucket using crafted hashes that
/// share lower bits (same bucket index) but differ in tag bits. This triggers
/// overflow bucket allocation. All entries must be findable.
#[test]
fn overflow_chain_forced_same_bucket() {
    // Use log2_size = 4 → 16 buckets, so bucket index = hash & 0xF.
    let index = HashIndex::new(4);
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let num_entries = 22;
    let mut hashes = Vec::with_capacity(num_entries);

    for i in 0..num_entries as u64 {
        // Craft hash: lower bits = 3 (same bucket), tag bits vary.
        // Tag occupies bits [61:48], bucket index from lower bits.
        let tag_value = (i + 1) << 48; // unique tags
        let hash = KeyHash::new(tag_value | 0x3); // all map to bucket 3
        let addr = make_addr(0, (i as u32) + 10);

        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r.created, "entry {i} should be created");
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        assert!(index.update(r.slot, r.entry, committed));
        hashes.push((hash, addr));
    }

    assert_eq!(index.entry_count(), num_entries as u64);
    // Must have overflow buckets (7 slots per bucket, 22 entries).
    assert!(index.overflow_count() > 0, "should have overflow buckets");

    // All entries must be findable with correct addresses.
    for (i, (hash, expected_addr)) in hashes.iter().enumerate() {
        let (found, _) = index.find(*hash).unwrap_or_else(|| panic!("entry {i} not found"));
        assert_eq!(found.address(), *expected_addr, "entry {i} address mismatch");
    }
}

// ===========================================================================
// 7. Empty table: find returns None for any hash
// ===========================================================================

/// An empty hash index returns None for any arbitrary hash lookup.
#[test]
fn empty_table_find_returns_none() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    for seed in [0u64, 1, 42, 999, u64::MAX, 0xDEAD_BEEF] {
        assert!(index.find(make_hash(seed)).is_none(), "seed {seed} should be None");
    }
    assert_eq!(index.entry_count(), 0);
}

// ===========================================================================
// 8. High load factor: >90% capacity → all entries findable
// ===========================================================================

/// Insert entries up to ~90% of primary bucket capacity (256 buckets × 7 slots
/// = 1792 capacity; 90% ≈ 1613). Verify all are findable. This exercises the
/// table under heavy load without overflow (most buckets nearly full).
#[test]
fn high_load_factor_all_findable() {
    let index = test_index(); // 256 buckets × 7 slots = 1792
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let target = 1600u64;
    let mut entries = Vec::with_capacity(target as usize);

    for i in 0..target {
        let hash = make_hash(10_000 + i);
        let addr = make_addr((i / 100) as u32, (i % 100 + 2) as u32);
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        // Under high load, some hash collisions may find existing entries.
        if r.created {
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            assert!(index.update(r.slot, r.entry, committed));
            entries.push((hash, addr));
        }
    }

    // Verify all committed entries are findable.
    for (hash, addr) in &entries {
        let (found, _) = index.find(*hash).expect("should find entry");
        assert_eq!(found.address(), *addr);
    }

    // Entry count should match inserted.
    assert_eq!(index.entry_count(), entries.len() as u64);
}

// ===========================================================================
// 9. GC invalidate empty range → 0 invalidated, entries intact
// ===========================================================================

/// Invalidating an empty range (begin == end or disjoint from entries)
/// should invalidate 0 entries and leave all entries intact.
#[test]
fn gc_invalidate_empty_range() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    for i in 0..20u64 {
        insert_committed(&index, 5000 + i, 10, i as u32 + 2);
    }
    assert_eq!(index.entry_count(), 20);

    // Invalidate range on page 50 — none of our entries are there.
    let count = index.invalidate_entries_in_range(make_addr(50, 0), make_addr(51, 0));
    assert_eq!(count, 0);
    assert_eq!(index.entry_count(), 20);
}

// ===========================================================================
// 10. GC invalidate entire range → all entries invalidated
// ===========================================================================

/// Invalidating the entire address space should remove all entries.
#[test]
fn gc_invalidate_entire_range() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let n = 50u64;
    for i in 0..n {
        insert_committed(&index, 6000 + i, (i % 10 + 1) as u32, i as u32 + 2);
    }
    assert_eq!(index.entry_count(), n);

    let count = index.invalidate_entries_in_range(LogicalAddress::ZERO, LogicalAddress::MAX);
    assert_eq!(count, n);
    assert_eq!(index.entry_count(), 0);
}

// ===========================================================================
// 11. cleanup_tentative_entries removes stale tentative entries
// ===========================================================================

/// Leave some entries tentative, some committed. cleanup_tentative_entries
/// should only remove the tentative ones.
#[test]
fn cleanup_tentative_removes_only_tentative() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    // Create and commit 5 entries.
    for i in 0..5u64 {
        insert_committed(&index, 7000 + i, 1, i as u32 + 2);
    }

    // Create 3 tentative entries (don't commit).
    for i in 0..3u64 {
        let hash = make_hash(8000 + i);
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r.created);
    }

    assert_eq!(index.entry_count(), 8);

    let cleaned = index.cleanup_tentative_entries();
    assert_eq!(cleaned, 3);
    assert_eq!(index.entry_count(), 5);

    // Committed entries still findable.
    for i in 0..5u64 {
        assert!(index.find(make_hash(7000 + i)).is_some());
    }
    // Tentative entries gone.
    for i in 0..3u64 {
        assert!(index.find(make_hash(8000 + i)).is_none());
    }
}

// ===========================================================================
// 12. Mixed operations: interleave find/create/update → consistent state
// ===========================================================================

/// Interleave create, find, update, and invalidate operations and verify the
/// hash index remains in a consistent state throughout.
#[test]
fn mixed_operations_consistent_state() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    // Phase 1: Insert 50 entries.
    for i in 0..50u64 {
        insert_committed(&index, 9000 + i, 1, i as u32 + 2);
    }
    assert_eq!(index.entry_count(), 50);

    // Phase 2: Update the first 10 entries to point to page 2.
    for i in 0..10u64 {
        let hash = make_hash(9000 + i);
        let (old_entry, slot) = index.find(hash).unwrap();
        let new_addr = make_addr(2, i as u32 + 2);
        let new_entry = HashBucketEntry::new(old_entry.tag(), new_addr, false);
        assert!(index.update(slot, old_entry, new_entry));
    }

    // Phase 3: Invalidate page 1 entries (seeds 10..49 → 40 entries on page 1).
    let invalidated = index.invalidate_entries_in_range(make_addr(1, 0), make_addr(2, 0));
    assert_eq!(invalidated, 40);

    // Phase 4: The 10 updated entries (now on page 2) should survive.
    assert_eq!(index.entry_count(), 10);
    for i in 0..10u64 {
        let hash = make_hash(9000 + i);
        let (found, _) = index.find(hash).expect("updated entry should survive");
        assert_eq!(found.address(), make_addr(2, i as u32 + 2));
    }

    // Phase 5: Insert 20 more.
    for i in 0..20u64 {
        insert_committed(&index, 9100 + i, 3, i as u32 + 2);
    }
    assert_eq!(index.entry_count(), 30);
}

// ===========================================================================
// 13. Entry count accuracy
// ===========================================================================

/// Insert N → entry_count() == N → invalidate M → entry_count() == N-M.
#[test]
fn entry_count_accuracy() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    assert_eq!(index.entry_count(), 0);

    let n = 100u64;
    for i in 0..n {
        // All on page 5 for easy range invalidation.
        insert_committed(&index, 10_000 + i, 5, i as u32 + 2);
    }
    assert_eq!(index.entry_count(), n);

    // Invalidate 30 entries (those with offset [2, 32) on page 5).
    let m = 30u64;
    let begin = make_addr(5, 2);
    let end = make_addr(5, 32);
    let invalidated = index.invalidate_entries_in_range(begin, end);
    assert_eq!(invalidated, m);
    assert_eq!(index.entry_count(), n - m);
}

// ===========================================================================
// 14. Register thread → protect/unprotect cycle → operations work
// ===========================================================================

/// Verify the full epoch lifecycle: register → protect → operations → unprotect
/// → re-protect → more operations.
#[test]
fn epoch_protect_unprotect_cycle() {
    let index = test_index();
    let thread = index.register_thread().unwrap();

    // First protect/unprotect cycle.
    {
        let _guard = thread.protect();
        insert_committed(&index, 11_000, 1, 100);
        assert_eq!(index.entry_count(), 1);
    }
    // Guard dropped — thread is unprotected.

    // Second protect/unprotect cycle.
    {
        let _guard = thread.protect();
        let hash = make_hash(11_000);
        let (found, _) = index.find(hash).expect("entry should survive across guards");
        assert_eq!(found.address(), make_addr(1, 100));

        insert_committed(&index, 11_001, 2, 200);
        assert_eq!(index.entry_count(), 2);
    }
}

// ===========================================================================
// 15. Multiple register_thread calls → independent EpochThread handles
// ===========================================================================

/// Each register_thread call returns an independent EpochThread with its own
/// epoch slot. Operations under different threads don't interfere.
#[test]
fn multiple_register_thread_independent() {
    let index = test_index();

    let thread1 = index.register_thread().expect("register thread 1");
    let thread2 = index.register_thread().expect("register thread 2");
    assert_eq!(index.epoch().registered_count(), 2);

    // Thread 1 inserts an entry.
    {
        let _guard = thread1.protect();
        insert_committed(&index, 12_000, 1, 10);
    }

    // Thread 2 can find it independently.
    {
        let _guard = thread2.protect();
        let (found, _) = index.find(make_hash(12_000)).expect("should find across threads");
        assert_eq!(found.address(), make_addr(1, 10));
    }

    // Drop thread 1, thread 2 still works.
    drop(thread1);
    assert_eq!(index.epoch().registered_count(), 1);

    {
        let _guard = thread2.protect();
        insert_committed(&index, 12_001, 2, 20);
        assert_eq!(index.entry_count(), 2);
    }
}

// ===========================================================================
// 16. find_or_create with same hash returns existing entry
// ===========================================================================

/// Calling find_or_create twice with the same hash (after committing the first)
/// should return the existing committed entry without creating a duplicate.
#[test]
fn find_or_create_idempotent_after_commit() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    let hash = make_hash(13_000);
    let addr = make_addr(3, 300);

    // First call: creates tentative.
    let r1 = index.find_or_create(hash, LogicalAddress::INVALID);
    assert!(r1.created);
    let committed = HashBucketEntry::new(r1.entry.tag(), addr, false);
    assert!(index.update(r1.slot, r1.entry, committed));

    // Second call: should return existing.
    let r2 = index.find_or_create(hash, LogicalAddress::INVALID);
    assert!(!r2.created);
    assert_eq!(r2.entry.address(), addr);
    assert!(!r2.entry.is_tentative());

    assert_eq!(index.entry_count(), 1);
}

// ===========================================================================
// 17. Overflow count increases when bucket overflows
// ===========================================================================

/// Verify overflow_count() tracks overflow bucket allocations.
#[test]
fn overflow_count_tracks_allocations() {
    let index = HashIndex::new(4); // 16 buckets
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    assert_eq!(index.overflow_count(), 0);

    // Force 10 entries into the same bucket (7 primary + 3 overflow).
    for i in 0..10u64 {
        let tag_value = (i + 1) << 48;
        let hash = KeyHash::new(tag_value | 0x5); // all map to bucket 5
        let addr = make_addr(0, i as u32 + 10);
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r.created);
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        assert!(index.update(r.slot, r.entry, committed));
    }

    assert!(index.overflow_count() > 0, "overflow buckets should have been allocated");
}

// ===========================================================================
// 18. GC invalidate with tentative entries — tentative also invalidated
// ===========================================================================

/// Tentative entries whose address falls in the invalidation range should
/// also be invalidated (since GC doesn't distinguish tentative from committed).
#[test]
fn gc_invalidate_includes_tentative() {
    let index = test_index();
    let thread = index.register_thread().unwrap();
    let _guard = thread.protect();

    // Create a tentative entry with address on page 3.
    let hash = make_hash(14_000);
    let r = index.find_or_create(hash, make_addr(3, 100));
    assert!(r.created);

    // Create a committed entry on page 3.
    insert_committed(&index, 14_001, 3, 200);

    assert_eq!(index.entry_count(), 2);

    // Invalidate page 3.
    let count = index.invalidate_entries_in_range(make_addr(3, 0), make_addr(4, 0));
    assert_eq!(count, 2);
    assert_eq!(index.entry_count(), 0);
}
