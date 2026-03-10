//! Mutation testing — targeted tests to kill surviving mutants.
//!
//! These tests were written in response to cargo-mutants runs on critical modules:
//! - `allocator.rs`: free list push/pop integrity, eager page allocation
//! - `hash/hash.rs`: hash function determinism with pinned reference vectors
//! - `hash/table.rs`: overflow chain traversal, committed-duplicate detection
//! - `hash/overflow.rs`: free-list recycling
//! - `hash/index.rs`: range invalidation boundary precision

mod common;

use faster_core::address::{LogicalAddress, Offset, Page};
use faster_core::allocator::MallocFixedPageSize;
use faster_core::hash::{self, KeyHash};
use faster_core::hash_bucket::{HashBucketEntry, BUCKET_NUM_ENTRIES};
use faster_core::hash_index::HashIndex;
use faster_core::overflow::OverflowBucketPool;

// ===========================================================================
// Helpers
// ===========================================================================

fn make_addr(page: u32, offset: u32) -> LogicalAddress {
    LogicalAddress::new(Page(page), Offset(offset))
}

fn make_hash(seed: u64) -> KeyHash {
    KeyHash::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

/// Insert a key into the index at a given address, committed (non-tentative).
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
// 1. Allocator: free list push/pop integrity
//    Kills: allocator.rs:758 (| → ^), allocator.rs:792 (| → ^)
// ===========================================================================

/// A minimal 8-byte item for allocator tests.
#[repr(C)]
#[derive(Clone, Default)]
struct SmallItem {
    value: u64,
}

#[test]
fn free_list_push_pop_multiple_cycles() {
    // Exercises the Treiber stack through multiple ABA-tag increments.
    // If | is replaced with ^ in push_free_list or pop_free_list,
    // the tag+address combination corrupts and addresses come back wrong.
    let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();

    let mut addrs: Vec<LogicalAddress> = (0..50).map(|_| alloc.allocate()).collect();
    let count_after = alloc.count();

    // Cycle free/realloc 10 times to exercise ABA tag advancement.
    for cycle in 0..10 {
        // Free all in reverse order.
        for &a in addrs.iter().rev() {
            alloc.free(a);
        }
        // Reallocate — should get back the same addresses (LIFO order).
        let recovered: Vec<LogicalAddress> = (0..50).map(|_| alloc.allocate()).collect();
        // Sort both to compare as sets (LIFO order is reversed from alloc order).
        let mut expected_set: Vec<u64> = addrs.iter().map(|a| a.raw()).collect();
        let mut actual_set: Vec<u64> = recovered.iter().map(|a| a.raw()).collect();
        expected_set.sort_unstable();
        actual_set.sort_unstable();
        assert_eq!(
            expected_set, actual_set,
            "cycle {cycle}: free list recycled addresses must match originals"
        );
        assert_eq!(
            alloc.count(),
            count_after,
            "cycle {cycle}: count should not increase — all from free list"
        );
        addrs = recovered;
    }
}

#[test]
fn free_list_single_item_roundtrip() {
    // Simple single-item push/pop. Validates the most basic tag manipulation.
    let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
    let addr = alloc.allocate();
    let count = alloc.count();

    alloc.free(addr);
    let recovered = alloc.allocate();
    assert_eq!(
        recovered, addr,
        "single-item free-list roundtrip must return same address"
    );
    assert_eq!(alloc.count(), count, "count must not increase on reuse");
}

#[test]
fn free_list_interleaved_push_pop() {
    // Interleave free/alloc to catch tag corruption across transitions.
    let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();

    let a = alloc.allocate();
    let b = alloc.allocate();
    let c = alloc.allocate();
    let count = alloc.count();

    // Free a, allocate (should get a back), free b, free c, alloc, alloc
    alloc.free(a);
    let got_a = alloc.allocate();
    assert_eq!(got_a, a);

    alloc.free(b);
    alloc.free(c);
    let got_c = alloc.allocate(); // LIFO: c was freed last
    let got_b = alloc.allocate();
    assert_eq!(got_c, c, "LIFO: should get c first");
    assert_eq!(got_b, b, "LIFO: should get b second");
    assert_eq!(alloc.count(), count, "no new allocations needed");
}

// ===========================================================================
// 2. Allocator: eager page pre-allocation
//    Kills: allocator.rs:699 (== → !=, < → >, + → -, etc.)
// ===========================================================================

#[test]
fn page_boundary_crossing_allocates_correct_address() {
    // Verifies the page boundary logic: offset 0 on page N triggers
    // eager allocation of page N+1.
    let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();

    // Allocate enough to cross from page 0 to page 1.
    // The allocator starts at flat index 2 (first two slots reserved).
    let items_per_page = faster_core::allocator::ITEMS_PER_PAGE;
    let to_fill = items_per_page - 2; // remaining slots on page 0

    for i in 0..to_fill {
        let addr = alloc.allocate();
        assert_eq!(
            addr.page(),
            Page(0),
            "item {i} should be on page 0"
        );
    }

    // This allocation should land at page 1, offset 0 — the boundary.
    let boundary = alloc.allocate();
    assert_eq!(boundary.page(), Page(1));
    assert_eq!(boundary.offset(), Offset(0));

    // The next allocation should succeed on page 1, offset 1 — proving
    // the page was already created (eager alloc of page 1 happened when
    // page 0 last item was allocated, or page 1 offset 0 triggered eager
    // alloc of page 2).
    let after_boundary = alloc.allocate();
    assert_eq!(after_boundary.page(), Page(1));
    assert_eq!(after_boundary.offset(), Offset(1));

    // Verify we can actually read/write both boundary addresses.
    unsafe {
        alloc.get_mut(boundary).value = 0xDEAD;
        alloc.get_mut(after_boundary).value = 0xBEEF;
    }
    assert_eq!(alloc.get(boundary).value, 0xDEAD);
    assert_eq!(alloc.get(after_boundary).value, 0xBEEF);
}

// ===========================================================================
// 3. Hash function: pinned reference vectors
//    Kills: hash.rs:218 (>> → <<), hash.rs:221 (>> → <<), hash.rs:222 (>> → <<)
// ===========================================================================

#[test]
fn faster_hash_u64_reference_vectors() {
    // Pinned outputs from the correct implementation. Any shift-direction
    // mutation will produce different values and fail this test.
    let cases: &[(u64, u64)] = &[
        (0x0000_0000_0000_0000, 0x805A_7EB9_771C_7163),
        (0x0000_0000_0000_0001, 0x8E88_45B9_5B21_09C1),
        (0x0000_0000_0000_002A, 0xD3DD_24B4_E45D_70D1),
        (0xDEAD_BEEF_CAFE_BABE, 0x126A_DFD5_A25F_C8D1),
        (0xFFFF_FFFF_FFFF_FFFF, 0x2A86_C959_FD1D_5E31),
    ];
    for &(input, expected) in cases {
        let actual = hash::faster_hash_u64(input);
        assert_eq!(
            actual, expected,
            "faster_hash_u64(0x{input:016X}): expected 0x{expected:016X}, got 0x{actual:016X}"
        );
    }
}

#[test]
fn faster_hash_u64_each_16bit_chunk_matters() {
    // Changing any 16-bit chunk of the input must change the output.
    // This catches shift-direction mutations that make chunks overlap or zero out.
    let base = 0u64;
    let base_hash = hash::faster_hash_u64(base);

    // Flip each 16-bit chunk independently.
    let chunk_masks: &[u64] = &[
        0x0000_0000_0000_FFFF, // bits 0..15
        0x0000_0000_FFFF_0000, // bits 16..31
        0x0000_FFFF_0000_0000, // bits 32..47
        0xFFFF_0000_0000_0000, // bits 48..63
    ];

    for (i, &mask) in chunk_masks.iter().enumerate() {
        let modified = base ^ mask;
        let modified_hash = hash::faster_hash_u64(modified);
        assert_ne!(
            base_hash, modified_hash,
            "chunk {i} (mask 0x{mask:016X}) must affect hash output"
        );
    }
}

// ===========================================================================
// 4. Hash table: overflow chain traversal
//    Kills: bucket.rs:668 (overflow_bucket → None),
//           table.rs:559 (find_entry != → ==)
// ===========================================================================

#[test]
fn overflow_chain_entries_are_findable() {
    // Use a small index (2^3 = 8 buckets) and insert enough keys into
    // the same bucket to force overflow. All entries must be findable.
    let index = HashIndex::new(3); // 8 buckets × 7 entries = 56 slots

    // Insert 16 entries with hashes that target bucket 0.
    // We engineer hashes by finding ones that map to the same bucket.
    let mut inserted = Vec::new();
    let mut seed = 0u64;
    while inserted.len() < 16 {
        let hash = make_hash(seed);
        // Check which bucket this maps to.
        if hash.index(8) == 0 {
            let addr = make_addr(1, seed as u32);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            assert!(r.created, "seed {seed} should create new entry");
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            assert!(index.update(r.slot, r.entry, committed));
            inserted.push((seed, hash, addr));
        }
        seed += 1;
    }

    // Verify all 16 entries are findable — this requires traversing overflow chains.
    for &(seed, hash, expected_addr) in &inserted {
        let result = index.find(hash);
        assert!(
            result.is_some(),
            "seed {seed} must be findable (overflow chain traversal)"
        );
        let (entry, _) = result.unwrap();
        assert_eq!(
            entry.address(),
            expected_addr,
            "seed {seed}: found wrong address"
        );
    }
}

// ===========================================================================
// 5. Hash table: find_committed_duplicate skips tentative
//    Kills: table.rs:664 (delete ! in is_tentative check)
// ===========================================================================

#[test]
fn find_or_create_detects_committed_duplicate() {
    // Insert a committed entry. Then insert again with the same hash.
    // find_or_create should detect the existing committed entry, NOT create a new one.
    // If `!entry.is_tentative()` loses its negation, find_committed_duplicate
    // would only match tentative entries, breaking duplicate detection.
    let index = HashIndex::new(8);
    let hash = make_hash(42);
    let addr = make_addr(1, 100);

    // First insert — creates a tentative entry, then we commit it.
    let r1 = index.find_or_create(hash, LogicalAddress::INVALID);
    assert!(r1.created, "first insert should create");
    let committed = HashBucketEntry::new(r1.entry.tag(), addr, false);
    assert!(index.update(r1.slot, r1.entry, committed));

    // Second insert with same hash — should find the committed entry.
    let r2 = index.find_or_create(hash, LogicalAddress::INVALID);
    // The entry should exist (not created fresh) because there's already a
    // committed entry with matching tag.
    assert!(
        !r2.created,
        "second insert with same hash should find existing committed entry"
    );
}

// ===========================================================================
// 6. Overflow pool: free enables reuse
//    Kills: overflow.rs:142 (free → noop)
// ===========================================================================

#[test]
fn overflow_pool_free_enables_reuse() {
    let pool = OverflowBucketPool::new();

    // Allocate a bucket.
    let addr1 = pool.allocate();
    let count_after_first = pool.allocated_count();
    assert_eq!(count_after_first, 1);

    // Free it.
    pool.free(addr1);

    // Allocate again — should reuse the freed bucket.
    let addr2 = pool.allocate();
    assert_eq!(
        pool.allocated_count(),
        count_after_first,
        "free list reuse: allocated_count should not increase"
    );
    assert_eq!(addr2, addr1, "should reuse the same address");
}

#[test]
fn overflow_pool_free_multiple_reuse() {
    let pool = OverflowBucketPool::new();

    let addrs: Vec<LogicalAddress> = (0..20).map(|_| pool.allocate()).collect();
    let count = pool.allocated_count();
    assert_eq!(count, 20);

    // Free all.
    for &addr in &addrs {
        pool.free(addr);
    }

    // Reallocate all — count should not increase.
    for _ in 0..20 {
        let _ = pool.allocate();
    }
    assert_eq!(
        pool.allocated_count(),
        count,
        "all 20 buckets should be reused from free list"
    );
}

// ===========================================================================
// 7. Hash index: range invalidation boundary precision
//    Kills: index.rs:475 (> → >=), index.rs:599 (> → >=)
// ===========================================================================

#[test]
fn invalidate_entries_boundary_exclusive_end() {
    // Invalidate range [begin, end) must be half-open:
    // - `begin` is inclusive
    // - `end` is exclusive
    // If > is replaced with >= at line 475, the end boundary entry would
    // also be invalidated (off-by-one).
    let index = HashIndex::new(8);

    // Insert entries at specific addresses spanning a range.
    let addr_before = make_addr(0, 100);
    let addr_begin = make_addr(1, 0); // inclusive start
    let addr_middle = make_addr(1, 500);
    let addr_end = make_addr(2, 0); // exclusive end
    let addr_after = make_addr(2, 100);

    let seeds = [10u64, 20, 30, 40, 50];
    let addrs = [addr_before, addr_begin, addr_middle, addr_end, addr_after];

    for (&seed, &addr) in seeds.iter().zip(addrs.iter()) {
        let hash = make_hash(seed);
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r.created);
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        assert!(index.update(r.slot, r.entry, committed));
    }

    // Invalidate [Page(1,0)..Page(2,0)) — should remove addr_begin and addr_middle.
    let count = index.invalidate_entries_in_range(addr_begin, addr_end);
    assert_eq!(count, 2, "should invalidate exactly 2 entries in range");

    // addr_before (page 0) should still be findable.
    let found_before = index.find(make_hash(10));
    assert!(
        found_before.is_some(),
        "entry before range must survive invalidation"
    );

    // addr_begin should be gone.
    let found_begin = index.find(make_hash(20));
    assert!(
        found_begin.is_none(),
        "entry at begin (inclusive) must be invalidated"
    );

    // addr_middle should be gone.
    let found_middle = index.find(make_hash(30));
    assert!(
        found_middle.is_none(),
        "entry in middle of range must be invalidated"
    );

    // addr_end should still exist (end is exclusive).
    let found_end = index.find(make_hash(40));
    assert!(
        found_end.is_some(),
        "entry at end (exclusive) must survive invalidation"
    );
    assert_eq!(
        found_end.unwrap().0.address(),
        addr_end,
        "end boundary entry must have correct address"
    );

    // addr_after should still exist.
    let found_after = index.find(make_hash(50));
    assert!(
        found_after.is_some(),
        "entry after range must survive invalidation"
    );
}

#[test]
fn invalidate_at_exact_boundary() {
    // Edge case: range where begin and end are adjacent addresses.
    let index = HashIndex::new(8);

    let addr_exact = make_addr(5, 0);
    let addr_next = make_addr(5, 1);

    insert_committed(&index, 100, 5, 0);
    insert_committed(&index, 200, 5, 1);

    // Invalidate [addr_exact, addr_next) — only addr_exact should go.
    let count = index.invalidate_entries_in_range(addr_exact, addr_next);
    assert_eq!(count, 1, "should invalidate exactly 1 entry");

    assert!(
        index.find(make_hash(100)).is_none(),
        "entry at begin must be removed"
    );
    assert!(
        index.find(make_hash(200)).is_some(),
        "entry at end (exclusive) must remain"
    );
}

// ===========================================================================
// 8. Hash function: bit distribution quality
//    Additional coverage for shift-direction mutations.
// ===========================================================================

#[test]
fn faster_hash_u64_high_bits_affect_output() {
    // Mutations that change >> to << in chunk extraction will cause
    // high input bits to be ignored or misprocessed.
    // Verify that modifying only the upper 16 bits changes the hash.
    let input_lo = 0x0000_0000_0000_0001u64;
    let input_hi = 0x0001_0000_0000_0001u64; // differs only in bits 48+
    let h_lo = hash::faster_hash_u64(input_lo);
    let h_hi = hash::faster_hash_u64(input_hi);
    assert_ne!(
        h_lo, h_hi,
        "upper 16 bits of input must affect hash output"
    );

    // Also check bits 32..47.
    let input_mid = 0x0000_0001_0000_0001u64;
    let h_mid = hash::faster_hash_u64(input_mid);
    assert_ne!(h_lo, h_mid, "bits 32..47 must affect hash output");
    assert_ne!(h_hi, h_mid, "all three inputs must produce distinct hashes");
}

#[test]
fn faster_hash_avalanche_single_bit_flips() {
    // Flipping any single bit in the input should change a substantial
    // number of output bits (avalanche property). This catches cases where
    // a shift mutation makes a chunk read all zeros.
    let base = 0x5555_5555_5555_5555u64;
    let base_hash = hash::faster_hash_u64(base);

    for bit in 0..64 {
        let flipped = base ^ (1u64 << bit);
        let flipped_hash = hash::faster_hash_u64(flipped);
        assert_ne!(
            base_hash, flipped_hash,
            "flipping bit {bit} must change hash output"
        );
        let differing_bits = (base_hash ^ flipped_hash).count_ones();
        assert!(
            differing_bits >= 8,
            "bit {bit}: only {differing_bits} bits differ (want ≥8 for avalanche)"
        );
    }
}

// ===========================================================================
// 9. Overflow chain: allocated bucket is usable
//    Kills: bucket.rs:94 (<< → >>), strengthens overflow chain coverage
// ===========================================================================

#[test]
fn overflow_pool_allocated_bucket_is_zeroed() {
    // Newly allocated overflow bucket should have all empty entries.
    let pool = OverflowBucketPool::new();
    let addr = pool.allocate();
    let bucket = pool.get(addr);

    for i in 0..BUCKET_NUM_ENTRIES {
        let entry = bucket.entry(i).load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            entry.is_empty(),
            "entry {i} in fresh overflow bucket must be empty"
        );
    }

    // Overflow pointer should be zero.
    let overflow = bucket
        .overflow_address()
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        overflow,
        LogicalAddress::ZERO,
        "fresh bucket overflow must be zero"
    );
}

#[test]
fn overflow_pool_reused_bucket_is_rezeroed() {
    // After free + reallocate, the bucket should be re-zeroed even though
    // it contained data in its previous lifetime.
    let pool = OverflowBucketPool::new();
    let addr = pool.allocate();

    // Write non-zero data to the bucket.
    let bucket = pool.get(addr);
    let fake_entry = HashBucketEntry::new(42, make_addr(1, 1), false);
    bucket
        .entry(0)
        .store(fake_entry, std::sync::atomic::Ordering::Relaxed);

    // Free and reallocate.
    pool.free(addr);
    let addr2 = pool.allocate();
    assert_eq!(addr2, addr, "should reuse same address");

    // Must be zeroed.
    let bucket = pool.get(addr2);
    let entry = bucket.entry(0).load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        entry.is_empty(),
        "reused bucket entry must be re-zeroed"
    );
}
