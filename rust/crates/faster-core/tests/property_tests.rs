//! Property-based tests for cross-module interactions.
//!
//! Uses proptest to generate arbitrary inputs and verify invariants that
//! must hold across the address, hash, hash_bucket, record, and allocator
//! modules.

mod common;

use proptest::prelude::*;

use faster_core::address::{
    LogicalAddress, Offset, Page, MAX_OFFSET, MAX_PAGE,
};
use faster_core::hash::{faster_hash_bytes, faster_hash_u64, tag_from_hash, Hashable};
use faster_core::hash_bucket::{HashBucket, HashBucketEntry};
use faster_core::record::{
    read_key, read_record_info, read_value, write_record, RecordInfo, RecordLayout,
};

// ===========================================================================
// Strategy helpers
// ===========================================================================

/// Strategy for valid page numbers (0..MAX_PAGE).
fn page_strategy() -> impl Strategy<Value = u32> {
    0..=MAX_PAGE
}

/// Strategy for valid offsets (0..MAX_OFFSET).
fn offset_strategy() -> impl Strategy<Value = u32> {
    0..=MAX_OFFSET
}

/// Strategy for valid LogicalAddress values.
fn address_strategy() -> impl Strategy<Value = LogicalAddress> {
    (page_strategy(), offset_strategy())
        .prop_map(|(p, o)| LogicalAddress::new(Page(p), Offset(o)))
}

/// Strategy for 14-bit tags (0..16383).
fn tag_strategy() -> impl Strategy<Value = u16> {
    0..=0x3FFFu16
}

/// Strategy for 13-bit checkpoint versions (0..8191).
fn version_strategy() -> impl Strategy<Value = u16> {
    0..=8191u16
}

/// Strategy for variable-length byte vectors (0..256 bytes).
fn bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..256)
}

// ===========================================================================
// 1. LogicalAddress round-trip: new → page/offset extraction
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn address_page_offset_round_trip(
        page in page_strategy(),
        offset in offset_strategy(),
    ) {
        let addr = LogicalAddress::new(Page(page), Offset(offset));
        prop_assert_eq!(addr.page(), Page(page));
        prop_assert_eq!(addr.offset(), Offset(offset));
        // raw → from_raw round-trip
        prop_assert_eq!(LogicalAddress::from_raw(addr.raw()), addr);
    }
}

// ===========================================================================
// 2. RecordInfo round-trip: all fields survive encode/decode
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn record_info_round_trip(
        addr in address_strategy(),
        version in version_strategy(),
        invalid in any::<bool>(),
        tombstone in any::<bool>(),
        final_bit in any::<bool>(),
    ) {
        let info = RecordInfo::new(addr, version, invalid, tombstone, final_bit);
        prop_assert_eq!(info.previous_address(), addr);
        prop_assert_eq!(info.checkpoint_version(), version);
        prop_assert_eq!(info.is_invalid(), invalid);
        prop_assert_eq!(info.is_tombstone(), tombstone);
        prop_assert_eq!(info.is_final(), final_bit);

        // raw round-trip
        let raw = info.raw();
        let info2 = RecordInfo::from_raw(raw);
        prop_assert_eq!(info2, info);
    }
}

// ===========================================================================
// 3. Hash tag is always in valid range
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn hash_tag_in_range_u64(key: u64) {
        let kh = key.hash();
        let tag = kh.tag();
        prop_assert!(tag < (1 << 14), "tag must be < 16384, got {tag}");
    }

    #[test]
    fn hash_tag_in_range_bytes(data in bytes_strategy()) {
        let h = faster_hash_bytes(&data);
        let tag = tag_from_hash(h);
        prop_assert!(tag < (1 << 14), "tag must be < 16384, got {tag}");
    }

    #[test]
    fn hash_tag_via_keyhash_matches_standalone(key: u64) {
        let kh = key.hash();
        let tag_keyhash = kh.tag();
        let tag_standalone = tag_from_hash(kh.value());
        prop_assert_eq!(tag_keyhash, tag_standalone);
    }
}

// ===========================================================================
// 4. Hash → tag → bucket entry → find_entry: always finds it
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn hash_to_bucket_find_always_succeeds(key: u64) {
        let kh = key.hash();
        let tag = kh.tag();
        // Use a deterministic address derived from the key.
        let addr = LogicalAddress::new(Page(0), Offset(((key as u32) & MAX_OFFSET).max(2)));

        let bucket = HashBucket::new();
        bucket.entry(0).store(
            HashBucketEntry::new(tag, addr, false),
            std::sync::atomic::Ordering::Relaxed,
        );

        let result = bucket.find_entry(tag);
        prop_assert!(result.is_some(), "tag 0x{:04x} not found", tag);
        let (slot, entry) = result.unwrap();
        prop_assert_eq!(slot, 0);
        prop_assert_eq!(entry.address(), addr);
        prop_assert_eq!(entry.tag(), tag);
    }
}

// ===========================================================================
// 5. HashBucketEntry: encode/decode round-trip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn bucket_entry_round_trip(
        tag in tag_strategy(),
        addr in address_strategy(),
        tentative in any::<bool>(),
    ) {
        let entry = HashBucketEntry::new(tag, addr, tentative);
        prop_assert_eq!(entry.tag(), tag);
        prop_assert_eq!(entry.address(), addr);
        prop_assert_eq!(entry.is_tentative(), tentative);

        // from_raw round-trip
        let entry2 = HashBucketEntry::from_raw(entry.raw());
        prop_assert_eq!(entry2, entry);
    }
}

// ===========================================================================
// 6. Record write/read round-trip: u64 key + u64 value
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn record_round_trip_u64(
        key: u64,
        value: u64,
        prev_addr in address_strategy(),
        version in version_strategy(),
    ) {
        let info = RecordInfo::new(prev_addr, version, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);
        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        let ri = read_record_info(&buf);
        prop_assert_eq!(ri.previous_address(), prev_addr);
        prop_assert_eq!(ri.checkpoint_version(), version);

        let k: u64 = read_key(&buf, &layout);
        let v: u64 = read_value(&buf, &layout);
        prop_assert_eq!(k, key);
        prop_assert_eq!(v, value);
    }
}

// ===========================================================================
// 7. Record write/read round-trip: u32 key + u32 value
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn record_round_trip_u32(key: u32, value: u32) {
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);
        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        let k: u32 = read_key(&buf, &layout);
        let v: u32 = read_value(&buf, &layout);
        prop_assert_eq!(k, key);
        prop_assert_eq!(v, value);
    }
}

// ===========================================================================
// 8. Record write/read round-trip: variable-length Vec<u8>
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn record_round_trip_variable_length(
        key in bytes_strategy(),
        value in bytes_strategy(),
    ) {
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);
        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        let k: Vec<u8> = read_key(&buf, &layout);
        let v: Vec<u8> = read_value(&buf, &layout);
        prop_assert_eq!(k, key);
        prop_assert_eq!(v, value);
    }
}

// ===========================================================================
// 9. RecordLayout consistency: total_size >= header + key + value
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn record_layout_size_consistency(
        key_size in 0usize..256,
        value_size in 0usize..256,
    ) {
        let layout = RecordLayout::compute(key_size, value_size);
        // Total size must accommodate header + key + value.
        prop_assert!(layout.total_size() >= 8 + key_size + value_size);
        // Total size must be 8-byte aligned.
        prop_assert_eq!(layout.total_size() % 8, 0);
        // Key offset must be >= 8 (after header).
        prop_assert!(layout.key_offset() >= 8);
        // Value offset must be >= key_offset + key_size.
        prop_assert!(layout.value_offset() >= layout.key_offset() + key_size);
        // Value offset must be 8-byte aligned.
        prop_assert_eq!(layout.value_offset() % 8, 0);
    }
}

// ===========================================================================
// 10. Hash determinism: same key → same hash
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn hash_deterministic_u64(key: u64) {
        let h1 = faster_hash_u64(key);
        let h2 = faster_hash_u64(key);
        prop_assert_eq!(h1, h2);
    }

    #[test]
    fn hash_deterministic_bytes(data in bytes_strategy()) {
        let h1 = faster_hash_bytes(&data);
        let h2 = faster_hash_bytes(&data);
        prop_assert_eq!(h1, h2);
    }
}

// ===========================================================================
// 11. Hash index computation: always in range for power-of-two table sizes
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn hash_index_in_range(
        key: u64,
        table_size_exp in 1u32..20,
    ) {
        let table_size = 1u64 << table_size_exp;
        let kh = key.hash();
        let idx = kh.index(table_size);
        prop_assert!(idx < table_size, "index {idx} >= table_size {table_size}");
    }
}

// ===========================================================================
// 12. Address validity semantics
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn address_validity(page in page_strategy(), offset in offset_strategy()) {
        let addr = LogicalAddress::new(Page(page), Offset(offset));
        // Addresses > INVALID (1) are valid. page=0,offset=0 → raw=0 (ZERO, not valid).
        // page=0,offset=1 → raw=1 (INVALID, not valid). Others are valid.
        let expected_valid = addr.raw() > 1;
        prop_assert_eq!(addr.is_valid(), expected_valid,
            "addr raw=0x{:x} page={} offset={}", addr.raw(), page, offset);
    }
}

// ===========================================================================
// 13. Allocator: addresses are unique and valid
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn allocator_unique_addresses(n in 10usize..200) {
        let alloc = faster_core::allocator::MallocFixedPageSize::<[u64; 2]>::new();
        let mut addrs = Vec::with_capacity(n);
        for _ in 0..n {
            let addr = alloc.allocate();
            prop_assert!(addr.is_valid());
            addrs.push(addr.raw());
        }
        addrs.sort();
        addrs.dedup();
        prop_assert_eq!(addrs.len(), n, "all {} addresses must be unique", n);
    }
}

// ===========================================================================
// 14. RecordInfo mutation preserves unmodified fields
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn record_info_with_previous_preserves_others(
        addr1 in address_strategy(),
        addr2 in address_strategy(),
        version in version_strategy(),
    ) {
        let info = RecordInfo::new(addr1, version, true, false, true);
        let modified = info.with_previous_address(addr2);
        prop_assert_eq!(modified.previous_address(), addr2);
        prop_assert_eq!(modified.checkpoint_version(), version);
        prop_assert!(modified.is_invalid());
        prop_assert!(!modified.is_tombstone());
        prop_assert!(modified.is_final());
    }

    #[test]
    fn record_info_with_version_preserves_others(
        addr in address_strategy(),
        v1 in version_strategy(),
        v2 in version_strategy(),
    ) {
        let info = RecordInfo::new(addr, v1, false, true, false);
        let modified = info.with_checkpoint_version(v2);
        prop_assert_eq!(modified.previous_address(), addr);
        prop_assert_eq!(modified.checkpoint_version(), v2);
        prop_assert!(!modified.is_invalid());
        prop_assert!(modified.is_tombstone());
    }
}

// ===========================================================================
// 15. HashIndex: create → find round-trip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ∀ keys: create(k) → commit(k) → find(k) returns entry with correct tag.
    #[test]
    fn hash_index_create_then_find(seed: u64) {
        use faster_core::hash_index::HashIndex;

        let index = HashIndex::new(8); // 256 buckets — sufficient for single-key tests
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = faster_core::hash::KeyHash::new(
            seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );
        let addr = LogicalAddress::new(Page(0), Offset(((seed as u32) & 0x01FF_FFFF).max(2)));

        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        // Either created or found existing (possible tag collision across seeds).
        if r.created {
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            prop_assert!(index.update(r.slot, r.entry, committed));

            let found = index.find(hash);
            prop_assert!(found.is_some(), "find should return committed entry");
            let (entry, _) = found.unwrap();
            prop_assert_eq!(entry.tag(), hash.tag());
            prop_assert_eq!(entry.address(), addr);
        }
    }
}

// ===========================================================================
// 16. HashIndex: create → commit → find returns committed address
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ∀ keys: create(k) → commit(k) → find(k) == Some with committed address.
    #[test]
    fn hash_index_committed_address_matches(
        seed in 0u64..100_000,
        page in 1u32..100,
        offset in 2u32..1000,
    ) {
        use faster_core::hash_index::HashIndex;

        let index = HashIndex::new(8); // 256 buckets — single-key test
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = faster_core::hash::KeyHash::new(
            seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );
        let addr = LogicalAddress::new(Page(page), Offset(offset));

        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        if r.created {
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            prop_assert!(index.update(r.slot, r.entry, committed));

            let (found, _) = index.find(hash).unwrap();
            prop_assert_eq!(found.address(), addr);
            prop_assert!(!found.is_tentative());
        }
    }
}

// ===========================================================================
// 17. HashIndex: insert set → find all succeeds
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// ∀ key sets: insert_all → find_all succeeds.
    #[test]
    fn hash_index_insert_all_find_all(
        seeds in prop::collection::vec(0u64..1_000_000, 10..100),
    ) {
        use faster_core::hash_index::HashIndex;
        use std::collections::HashSet;

        let index = HashIndex::new(10); // 1024 buckets — multi-key test
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        // Deduplicate seeds to avoid hash collisions in the test logic.
        let unique_seeds: HashSet<u64> = seeds.into_iter().collect();

        let mut inserted = Vec::new();
        for &seed in &unique_seeds {
            let hash = faster_core::hash::KeyHash::new(
                seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            );
            let addr = LogicalAddress::new(Page(0), Offset((seed as u32 & 0x01FF_FFFF).max(2)));

            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            if r.created {
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                prop_assert!(index.update(r.slot, r.entry, committed));
                inserted.push((hash, addr));
            }
        }

        for (hash, addr) in &inserted {
            let result = index.find(*hash);
            prop_assert!(result.is_some(), "inserted entry not found");
            prop_assert_eq!(result.unwrap().0.address(), *addr);
        }
    }
}

// ===========================================================================
// 18. HashIndex: invalidate range preserves out-of-range entries
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// ∀ ranges: insert N → invalidate [a,b) → only entries in [a,b) are gone.
    #[test]
    fn hash_index_invalidate_range_preserves_others(
        n in 10usize..50,
        range_start in 1u32..50,
        range_width in 1u32..20,
    ) {
        use faster_core::hash_index::HashIndex;

        let index = HashIndex::new(10);
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let range_end = range_start + range_width;

        let mut in_range = Vec::new();
        let mut out_of_range = Vec::new();

        for i in 0..n {
            let seed = 50_000 + i as u64;
            let hash = faster_core::hash::KeyHash::new(
                seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            );
            // Alternate pages: some inside range, some outside.
            let page = if i % 2 == 0 {
                range_start + (i as u32 % range_width) // inside [range_start, range_end)
            } else {
                range_end + 10 + i as u32 // safely outside range
            };
            let addr = LogicalAddress::new(Page(page), Offset(i as u32 + 2));

            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            if r.created {
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                prop_assert!(index.update(r.slot, r.entry, committed));

                if page >= range_start && page < range_end {
                    in_range.push(hash);
                } else {
                    out_of_range.push((hash, addr));
                }
            }
        }

        let begin = LogicalAddress::new(Page(range_start), Offset(0));
        let end = LogicalAddress::new(Page(range_end), Offset(0));
        let invalidated = index.invalidate_entries_in_range(begin, end);
        prop_assert_eq!(invalidated, in_range.len() as u64);

        // Out-of-range entries still findable.
        for (hash, addr) in &out_of_range {
            let result = index.find(*hash);
            prop_assert!(result.is_some(), "out-of-range entry should survive");
            prop_assert_eq!(result.unwrap().0.address(), *addr);
        }

        // In-range entries gone.
        for hash in &in_range {
            prop_assert!(index.find(*hash).is_none(), "in-range entry should be gone");
        }
    }
}

// ===========================================================================
// 19. HashIndex: model-based — shadow HashMap tracks state
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Model-based test: maintain a HashMap shadow, apply random operations
    /// (insert/find/invalidate), verify the hash index matches the shadow.
    #[test]
    fn hash_index_model_based(
        ops in prop::collection::vec(
            (0u64..500, 0u8..3, 1u32..50, 2u32..1000),
            50..200,
        ),
    ) {
        use faster_core::hash_index::HashIndex;
        use std::collections::HashMap;

        let index = HashIndex::new(10); // 1024 buckets
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        // Shadow model: seed → LogicalAddress
        let mut shadow: HashMap<u64, LogicalAddress> = HashMap::new();

        for (seed, op_type, page, offset) in &ops {
            let hash = faster_core::hash::KeyHash::new(
                seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            );

            match op_type % 3 {
                0 => {
                    // Insert/upsert
                    let addr = LogicalAddress::new(Page(*page), Offset(*offset));
                    let r = index.find_or_create(hash, LogicalAddress::INVALID);
                    if r.created {
                        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                        if index.update(r.slot, r.entry, committed) {
                            shadow.insert(*seed, addr);
                        }
                    }
                    // If not created, entry already exists — shadow should already have it.
                }
                1 => {
                    // Find
                    let result = index.find(hash);
                    if let Some(expected_addr) = shadow.get(seed) {
                        prop_assert!(result.is_some(), "shadow has key but index doesn't");
                        prop_assert_eq!(result.unwrap().0.address(), *expected_addr);
                    }
                    // Note: if shadow doesn't have it, index.find might still
                    // return None OR something from a previous invalidation-
                    // surviving insert. We only assert the positive case.
                }
                _ => {
                    // Invalidate a small range around the page.
                    let begin = LogicalAddress::new(Page(*page), Offset(0));
                    let end = LogicalAddress::new(Page(*page + 1), Offset(0));
                    index.invalidate_entries_in_range(begin, end);

                    // Remove from shadow anything on that page.
                    shadow.retain(|s, addr| {
                        let h = faster_core::hash::KeyHash::new(
                            s.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                        );
                        let _ = h; // used only for hash, but addr is what matters
                        let raw = addr.raw();
                        let begin_raw = begin.raw();
                        let end_raw = end.raw();
                        !(raw >= begin_raw && raw < end_raw)
                    });
                }
            }
        }

        // Final verification: all shadow entries are findable.
        for (seed, addr) in &shadow {
            let hash = faster_core::hash::KeyHash::new(
                seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            );
            let result = index.find(hash);
            prop_assert!(result.is_some(), "shadow entry for seed {seed} not found");
            prop_assert_eq!(result.unwrap().0.address(), *addr);
        }
    }
}
