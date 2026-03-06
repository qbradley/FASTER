//! Chunk-based bucket splitting for hash table grow operations.
//!
//! During a grow, FASTER doubles the hash table by splitting each old bucket
//! into two new buckets: the **left** partner (same index) and the **right**
//! partner (index + old_size). The split direction for each entry is
//! determined by re-hashing its key via a [`HashResolver`].
//!
//! Splitting is incremental: the table is divided into chunks of
//! [`CHUNK_SIZE`](super::CHUNK_SIZE) buckets, and each chunk is processed
//! independently by [`BucketSplitter::split_chunk`]. Multiple threads can
//! split different chunks in parallel.
//!
//! # Algorithm (per bucket)
//!
//! 1. Read all entries from the old bucket's overflow chain.
//! 2. For each non-empty entry, resolve the full key hash via [`HashResolver`].
//! 3. If `splits_right(hash, old_size)` → copy to the right partner bucket.
//! 4. Otherwise → copy to the left partner bucket.
//! 5. If the hash cannot be resolved (record on disk) → place in **both**
//!    partners (conservative, matching C++/C# FASTER).
//! 6. Invalidate the old entry (store `EMPTY`).
//!
//! # C++/C# correspondence
//!
//! | Rust                       | C++ (`mem_index.h`)                          | C# (`SplitIndex.cs`)                     |
//! |----------------------------|----------------------------------------------|-------------------------------------------|
//! | `BucketSplitter`           | `MemHashIndex::Grow()`                       | `FasterKV.SplitChunk()`                  |
//! | `HashResolver`             | `record->key().GetHash()`                    | `comparer.GetHashCode64(ref hlog.GetKey)` |
//! | `split_bucket()`           | inner loop of `Grow()`                       | `SplitChunk()` inner loop                |
//! | `SplitResult`              | (implicit counters)                          | (implicit)                               |
//!
//! # Thread safety
//!
//! During grow, each chunk is processed by exactly one thread (claimed via
//! [`GrowState::claim_next_chunk`](super::GrowState::claim_next_chunk)).
//! The old table is read-only; the new table's split partner buckets for
//! different old buckets are disjoint, so no synchronization is needed
//! between concurrent chunk splits. Within a single bucket chain, the
//! splitter uses atomic loads (Acquire) and stores (Release) for
//! correctness against concurrent readers using
//! [`bucket_index_for_version`](super::bucket_index_for_version).

use core::sync::atomic::Ordering;

use crate::address::LogicalAddress;
use crate::hash_bucket::{HashBucketEntry, BUCKET_NUM_ENTRIES};
use crate::hash_table::HashTable;

use super::{splits_right, CHUNK_SIZE};

// ---------------------------------------------------------------------------
// HashResolver
// ---------------------------------------------------------------------------

/// Resolves the full 64-bit key hash for a hash bucket entry.
///
/// During a grow operation, the split direction for each entry depends on its
/// full key hash. The hash index only stores a 14-bit tag and 48-bit address
/// per entry — the bucket-index bits of the original hash are lost. The full
/// hash must be recovered externally, typically by reading the key from the
/// hybrid log and re-hashing it.
///
/// # Return value
///
/// - `Some(hash)` — the full 64-bit hash of the entry's key. Used to compute
///   split direction via [`splits_right`](super::splits_right).
/// - `None` — the record is below the head address (on disk) and cannot be
///   read. The splitter places such entries in **both** split partners
///   (conservative approach, matching C++/C# FASTER).
///
/// # C++/C# correspondence
///
/// - C++: `record->key().GetHash()` — reads the key from the log record at
///   `old_entry.address()`, then hashes it.
/// - C#: `comparer.GetHashCode64(ref hlog.GetKey(physicalAddress))` — same
///   pattern via the key comparer.
pub trait HashResolver {
    /// Returns the full key hash for `entry`, or `None` if unresolvable.
    fn resolve_hash(&self, entry: HashBucketEntry) -> Option<u64>;
}

// ---------------------------------------------------------------------------
// SplitResult
// ---------------------------------------------------------------------------

/// Aggregate statistics from a chunk (or bucket) split operation.
///
/// Used to verify conservation invariants: the total number of entries before
/// the split must equal `entries_kept + entries_moved` (plus any entries
/// duplicated due to unresolvable hashes, which appear in both partners).
///
/// # Examples
///
/// ```
/// use faster_core::grow::splitter::SplitResult;
///
/// let result = SplitResult {
///     chunks_processed: 1,
///     entries_kept: 42,
///     entries_moved: 38,
///     buckets_processed: 16384,
/// };
/// assert_eq!(result.total_entries(), 80);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SplitResult {
    /// Number of chunks that were processed.
    pub chunks_processed: u32,
    /// Number of entries that stayed in the left (original index) bucket.
    pub entries_kept: u64,
    /// Number of entries that moved to the right (split partner) bucket.
    pub entries_moved: u64,
    /// Number of primary buckets that were scanned.
    pub buckets_processed: u64,
}

impl SplitResult {
    /// Total entries processed (`entries_kept + entries_moved`).
    ///
    /// Note: entries whose hash could not be resolved are counted in
    /// *both* `entries_kept` and `entries_moved` (they are placed in both
    /// split partners).
    #[inline]
    pub fn total_entries(&self) -> u64 {
        self.entries_kept + self.entries_moved
    }

    /// Merges another `SplitResult` into this one (accumulates all counters).
    #[inline]
    pub fn merge(&mut self, other: &SplitResult) {
        self.chunks_processed += other.chunks_processed;
        self.entries_kept += other.entries_kept;
        self.entries_moved += other.entries_moved;
        self.buckets_processed += other.buckets_processed;
    }
}

// ---------------------------------------------------------------------------
// BucketSplitter
// ---------------------------------------------------------------------------

/// Splits buckets from an old hash table into a new (doubled) hash table.
///
/// During a grow operation, each old bucket at index `i` is split into two
/// new buckets: left at index `i` and right at index `i + old_size`. The
/// [`BucketSplitter`] reads entries from the old table, determines their
/// split direction via a [`HashResolver`], writes them to the appropriate
/// new bucket, and invalidates the old entries.
///
/// # Construction
///
/// ```ignore
/// let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
/// let result = splitter.split_chunk(0, old_log2, new_log2);
/// ```
///
/// # Panics
///
/// Debug-asserts that `new_table.num_buckets() == 2 * old_table.num_buckets()`.
pub struct BucketSplitter<'a, R: HashResolver> {
    /// The table being grown FROM (read-only during split).
    old_table: &'a HashTable,
    /// The table being grown TO (double the size of old_table).
    new_table: &'a HashTable,
    /// Resolves the full key hash for each entry.
    resolver: &'a R,
}

impl<'a, R: HashResolver> BucketSplitter<'a, R> {
    /// Creates a new splitter for migrating entries from `old_table` to
    /// `new_table`.
    ///
    /// # Panics
    ///
    /// Debug-asserts that `new_table` is exactly double the size of
    /// `old_table`.
    pub fn new(old_table: &'a HashTable, new_table: &'a HashTable, resolver: &'a R) -> Self {
        debug_assert_eq!(
            new_table.num_buckets(),
            old_table.num_buckets() * 2,
            "new table must be exactly 2× old table"
        );
        Self {
            old_table,
            new_table,
            resolver,
        }
    }

    /// Splits all buckets in the given chunk.
    ///
    /// A chunk covers `CHUNK_SIZE` consecutive buckets starting at
    /// `chunk_index * CHUNK_SIZE`. The last chunk may cover fewer buckets
    /// if the old table size is not a multiple of `CHUNK_SIZE`.
    ///
    /// # Parameters
    ///
    /// - `chunk_index`: zero-based index of the chunk to process.
    /// - `old_size_bits`: log₂ of the old table size (e.g., 10 for 1024).
    /// - `new_size_bits`: log₂ of the new table size (`old_size_bits + 1`).
    ///
    /// # Returns
    ///
    /// A [`SplitResult`] with aggregate statistics for this chunk.
    pub fn split_chunk(
        &self,
        chunk_index: u32,
        old_size_bits: u8,
        new_size_bits: u8,
    ) -> SplitResult {
        debug_assert_eq!(
            new_size_bits,
            old_size_bits + 1,
            "new_size_bits must be old_size_bits + 1"
        );

        let old_size = 1u64 << old_size_bits;
        let chunk_start = chunk_index as u64 * CHUNK_SIZE as u64;
        let chunk_end = (chunk_start + CHUNK_SIZE as u64).min(old_size);

        let mut result = SplitResult {
            chunks_processed: 1,
            ..SplitResult::default()
        };

        for bucket_idx in chunk_start..chunk_end {
            let (kept, moved) = self.split_bucket(bucket_idx, old_size_bits, new_size_bits);
            result.entries_kept += kept as u64;
            result.entries_moved += moved as u64;
            result.buckets_processed += 1;
        }

        result
    }

    /// Splits a single old bucket (including its overflow chain) into the
    /// two new partner buckets.
    ///
    /// Returns `(entries_kept, entries_moved)`:
    /// - `entries_kept`: entries placed in the left bucket (index = `old_bucket_index`).
    /// - `entries_moved`: entries placed in the right bucket (index = `old_bucket_index + old_size`).
    ///
    /// Entries whose hash cannot be resolved are placed in **both** partners
    /// and counted in both `entries_kept` and `entries_moved`.
    pub fn split_bucket(
        &self,
        old_bucket_index: u64,
        old_size_bits: u8,
        new_size_bits: u8,
    ) -> (u32, u32) {
        let _ = new_size_bits; // Used only for debug assertions below.
        debug_assert_eq!(
            new_size_bits,
            old_size_bits + 1,
            "new_size_bits must be old_size_bits + 1"
        );

        let old_size = 1u64 << old_size_bits;
        let new_left_index = old_bucket_index;
        let new_right_index = old_bucket_index + old_size;

        let old_bucket = self.old_table.bucket_by_index(old_bucket_index);
        let new_left_bucket = self.new_table.bucket_by_index(new_left_index);
        let new_right_bucket = self.new_table.bucket_by_index(new_right_index);
        let new_pool = self.new_table.overflow_pool();

        let mut entries_kept: u32 = 0;
        let mut entries_moved: u32 = 0;

        // Walk the old bucket's overflow chain.
        let old_pool = self.old_table.overflow_pool();
        let mut current = old_bucket;
        loop {
            for slot_idx in 0..BUCKET_NUM_ENTRIES {
                // Ordering: Acquire to see the fully-written entry value
                // that was published with Release by the original inserter.
                let entry = current.entry(slot_idx).load(Ordering::Acquire);
                if entry.is_empty() {
                    continue;
                }

                // Skip tentative entries — they should have been committed
                // or aborted before grow begins. If one slips through, leave
                // it for safety (don't move partially-inserted records).
                if entry.is_tentative() {
                    continue;
                }

                let tag = entry.tag();
                let address = entry.address();

                match self.resolver.resolve_hash(entry) {
                    Some(hash) => {
                        if splits_right(hash, old_size) {
                            // Entry goes to the right (split partner) bucket.
                            new_right_bucket
                                .insert_in_chain(tag, address, new_pool)
                                .expect("insert into new right bucket should succeed");
                            entries_moved += 1;
                        } else {
                            // Entry stays in the left (same index) bucket.
                            new_left_bucket
                                .insert_in_chain(tag, address, new_pool)
                                .expect("insert into new left bucket should succeed");
                            entries_kept += 1;
                        }
                    }
                    None => {
                        // Hash unresolvable (record on disk). Place in BOTH
                        // partners — conservative approach matching C++/C#.
                        new_left_bucket
                            .insert_in_chain(tag, address, new_pool)
                            .expect("insert into new left bucket should succeed");
                        new_right_bucket
                            .insert_in_chain(tag, address, new_pool)
                            .expect("insert into new right bucket should succeed");
                        entries_kept += 1;
                        entries_moved += 1;
                    }
                }

                // Invalidate the old entry.
                // Ordering: Release to ensure the new entry is visible to
                // readers before the old one disappears. Readers using
                // bucket_index_for_version() will find the entry in the
                // new table before we remove it from the old table.
                current
                    .entry(slot_idx)
                    .store(HashBucketEntry::EMPTY, Ordering::Release);
            }

            // Follow overflow chain.
            let overflow_addr = current.overflow_address().load(Ordering::Acquire);
            if overflow_addr == LogicalAddress::ZERO {
                break;
            }
            current = old_pool.get(overflow_addr);
        }

        (entries_kept, entries_moved)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};
    use crate::hash_bucket::BUCKET_NUM_ENTRIES;
    use crate::hash_table::HashTable;
    use std::collections::HashMap;

    // -----------------------------------------------------------------------
    // Test HashResolver: maps entry addresses to pre-computed hashes
    // -----------------------------------------------------------------------

    /// A test resolver that maps [`LogicalAddress`] values to full hashes.
    struct MapResolver {
        addr_to_hash: HashMap<u64, u64>,
    }

    impl MapResolver {
        fn new() -> Self {
            Self {
                addr_to_hash: HashMap::new(),
            }
        }

        fn insert(&mut self, addr: LogicalAddress, hash: u64) {
            self.addr_to_hash.insert(addr.raw(), hash);
        }
    }

    impl HashResolver for MapResolver {
        fn resolve_hash(&self, entry: HashBucketEntry) -> Option<u64> {
            self.addr_to_hash.get(&entry.address().raw()).copied()
        }
    }

    /// A resolver that always returns `None` (simulates all records on disk).
    struct UnresolvableResolver;

    impl HashResolver for UnresolvableResolver {
        fn resolve_hash(&self, _entry: HashBucketEntry) -> Option<u64> {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Creates a unique LogicalAddress from an index.
    fn make_addr(idx: u32) -> LogicalAddress {
        LogicalAddress::new(Page(1), Offset(idx * 64))
    }

    /// Counts non-empty entries in a table (all primary buckets + overflow).
    fn count_entries(table: &HashTable) -> u64 {
        table.entry_count()
    }

    /// Counts non-empty entries in a single bucket chain.
    fn count_bucket_chain_entries(table: &HashTable, bucket_index: u64) -> u64 {
        let bucket = table.bucket_by_index(bucket_index);
        let pool = table.overflow_pool();
        let mut count = 0u64;
        let mut current = bucket;
        loop {
            for i in 0..BUCKET_NUM_ENTRIES {
                let entry = current.entry(i).load(Ordering::Relaxed);
                if !entry.is_empty() {
                    count += 1;
                }
            }
            let overflow_addr = current.overflow_address().load(Ordering::Relaxed);
            if overflow_addr == LogicalAddress::ZERO {
                break;
            }
            current = pool.get(overflow_addr);
        }
        count
    }

    /// Collects all (tag, address) pairs from a bucket chain.
    fn collect_bucket_entries(
        table: &HashTable,
        bucket_index: u64,
    ) -> Vec<(u16, LogicalAddress)> {
        let bucket = table.bucket_by_index(bucket_index);
        let pool = table.overflow_pool();
        let mut entries = Vec::new();
        let mut current = bucket;
        loop {
            for i in 0..BUCKET_NUM_ENTRIES {
                let entry = current.entry(i).load(Ordering::Relaxed);
                if !entry.is_empty() {
                    entries.push((entry.tag(), entry.address()));
                }
            }
            let overflow_addr = current.overflow_address().load(Ordering::Relaxed);
            if overflow_addr == LogicalAddress::ZERO {
                break;
            }
            current = pool.get(overflow_addr);
        }
        entries
    }

    // -----------------------------------------------------------------------
    // Tests: empty bucket split (no-op)
    // -----------------------------------------------------------------------

    #[test]
    fn split_empty_bucket() {
        let old_table = HashTable::new(4); // 16 buckets
        let new_table = HashTable::new(5); // 32 buckets
        let resolver = MapResolver::new();
        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);

        let (kept, moved) = splitter.split_bucket(0, 4, 5);
        assert_eq!(kept, 0);
        assert_eq!(moved, 0);

        // New table buckets should still be empty.
        assert_eq!(count_bucket_chain_entries(&new_table, 0), 0);
        assert_eq!(count_bucket_chain_entries(&new_table, 16), 0);
    }

    #[test]
    fn split_chunk_empty_table() {
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let resolver = MapResolver::new();
        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);

        let result = splitter.split_chunk(0, 4, 5);
        assert_eq!(result.chunks_processed, 1);
        assert_eq!(result.entries_kept, 0);
        assert_eq!(result.entries_moved, 0);
        assert_eq!(result.buckets_processed, 16);
    }

    // -----------------------------------------------------------------------
    // Tests: single bucket split with known hash values
    // -----------------------------------------------------------------------

    #[test]
    fn split_bucket_all_stay_left() {
        // Old table: 16 buckets (4 bits). New table: 32 buckets (5 bits).
        // old_size = 16. Split bit = bit 4.
        // Hashes with bit 4 = 0 → stay left.
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 16;
        let bucket_idx: u64 = 3; // entries go into bucket 3

        // Insert 3 entries that all stay left (bit 4 clear).
        for i in 0..3u32 {
            let addr = make_addr(i);
            let tag = 0x100 + i as u16;
            // Hash: lower 4 bits = 3 (bucket 3), bit 4 = 0 (stay left).
            // Upper bits encode tag.
            let hash = ((tag as u64) << 48) | bucket_idx;
            assert!(!splits_right(hash, old_size), "entry should stay left");
            resolver.insert(addr, hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(tag, addr, old_table.overflow_pool())
                .unwrap();
        }

        assert_eq!(count_bucket_chain_entries(&old_table, bucket_idx), 3);

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let (kept, moved) = splitter.split_bucket(bucket_idx, 4, 5);

        assert_eq!(kept, 3);
        assert_eq!(moved, 0);
        assert_eq!(count_bucket_chain_entries(&new_table, bucket_idx), 3);
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx + old_size),
            0
        );
        // Old bucket should be invalidated.
        assert_eq!(count_bucket_chain_entries(&old_table, bucket_idx), 0);
    }

    #[test]
    fn split_bucket_all_move_right() {
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 16;
        let bucket_idx: u64 = 5;

        for i in 0..3u32 {
            let addr = make_addr(i);
            let tag = 0x200 + i as u16;
            // Hash: lower 4 bits = 5 (bucket 5), bit 4 = 1 (move right).
            let hash = ((tag as u64) << 48) | old_size | bucket_idx;
            assert!(splits_right(hash, old_size), "entry should move right");
            resolver.insert(addr, hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(tag, addr, old_table.overflow_pool())
                .unwrap();
        }

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let (kept, moved) = splitter.split_bucket(bucket_idx, 4, 5);

        assert_eq!(kept, 0);
        assert_eq!(moved, 3);
        assert_eq!(count_bucket_chain_entries(&new_table, bucket_idx), 0);
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx + old_size),
            3
        );
    }

    #[test]
    fn split_bucket_mixed_left_and_right() {
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 16;
        let bucket_idx: u64 = 7;

        // 4 entries: 2 left, 2 right.
        let entries: Vec<(u16, u64, bool)> = vec![
            (0x0A0, bucket_idx, false),                  // left
            (0x0A1, old_size | bucket_idx, true),        // right
            (0x0A2, bucket_idx | (0x20), false),         // left (bit 5 set, but bit 4 clear)
            (0x0A3, old_size | bucket_idx | 0x20, true), // right
        ];

        for (i, &(tag, hash, expect_right)) in entries.iter().enumerate() {
            let addr = make_addr(i as u32);
            assert_eq!(splits_right(hash, old_size), expect_right);
            resolver.insert(addr, hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(tag, addr, old_table.overflow_pool())
                .unwrap();
        }

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let (kept, moved) = splitter.split_bucket(bucket_idx, 4, 5);

        assert_eq!(kept, 2);
        assert_eq!(moved, 2);
        assert_eq!(count_bucket_chain_entries(&new_table, bucket_idx), 2);
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx + old_size),
            2
        );
    }

    // -----------------------------------------------------------------------
    // Tests: overflow chain handling
    // -----------------------------------------------------------------------

    #[test]
    fn split_bucket_with_overflow_chain() {
        // Fill more than 7 entries (BUCKET_NUM_ENTRIES) to force overflow.
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 16;
        let bucket_idx: u64 = 2;
        let num_entries = BUCKET_NUM_ENTRIES as u32 + 3; // 10 entries → overflow

        let mut expect_left = 0u32;
        let mut expect_right = 0u32;

        for i in 0..num_entries {
            let addr = make_addr(i);
            let tag = 0x300 + i as u16;
            // Alternate left/right.
            let goes_right = i % 2 == 0;
            let hash = if goes_right {
                old_size | bucket_idx | ((tag as u64) << 48)
            } else {
                bucket_idx | ((tag as u64) << 48)
            };
            assert_eq!(splits_right(hash, old_size), goes_right);
            resolver.insert(addr, hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(tag, addr, old_table.overflow_pool())
                .unwrap();

            if goes_right {
                expect_right += 1;
            } else {
                expect_left += 1;
            }
        }

        // Verify overflow was used.
        assert!(old_table.overflow_count() > 0, "expected overflow buckets");
        assert_eq!(
            count_bucket_chain_entries(&old_table, bucket_idx),
            num_entries as u64
        );

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let (kept, moved) = splitter.split_bucket(bucket_idx, 4, 5);

        assert_eq!(kept, expect_left);
        assert_eq!(moved, expect_right);
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx),
            expect_left as u64
        );
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx + old_size),
            expect_right as u64
        );
        // Old bucket invalidated.
        assert_eq!(count_bucket_chain_entries(&old_table, bucket_idx), 0);
    }

    // -----------------------------------------------------------------------
    // Tests: conservation invariant
    // -----------------------------------------------------------------------

    #[test]
    fn conservation_invariant_no_entries_lost() {
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 16;
        let mut total_inserted = 0u64;

        // Insert entries into several buckets.
        for bucket_idx in 0..old_size {
            let num = (bucket_idx % 5) as u32; // 0..4 entries per bucket
            for i in 0..num {
                let addr = make_addr((bucket_idx as u32) * 10 + i);
                let tag = ((bucket_idx * 100 + i as u64) & 0x3FFF) as u16;
                let goes_right = i % 2 == 0;
                let hash = if goes_right {
                    old_size | bucket_idx | ((tag as u64) << 48)
                } else {
                    bucket_idx | ((tag as u64) << 48)
                };
                resolver.insert(addr, hash);
                old_table
                    .bucket_by_index(bucket_idx)
                    .insert_in_chain(tag, addr, old_table.overflow_pool())
                    .unwrap();
                total_inserted += 1;
            }
        }

        let old_count = count_entries(&old_table);
        assert_eq!(old_count, total_inserted);

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let result = splitter.split_chunk(0, 4, 5);

        // Conservation: all entries accounted for.
        assert_eq!(result.total_entries(), total_inserted);
        assert_eq!(result.buckets_processed, old_size);

        // New table has all entries.
        let new_count = count_entries(&new_table);
        assert_eq!(new_count, total_inserted);

        // Old table is fully emptied.
        assert_eq!(count_entries(&old_table), 0);
    }

    // -----------------------------------------------------------------------
    // Tests: entries land in correct bucket post-split
    // -----------------------------------------------------------------------

    #[test]
    fn entries_land_in_correct_new_bucket() {
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 16;

        // For bucket 9: insert one left entry and one right entry with
        // known tags and addresses.
        let bucket_idx: u64 = 9;

        let left_addr = make_addr(100);
        let left_tag: u16 = 0x0BCD;
        let left_hash = bucket_idx | ((left_tag as u64) << 48); // bit 4 = 0
        resolver.insert(left_addr, left_hash);
        old_table
            .bucket_by_index(bucket_idx)
            .insert_in_chain(left_tag, left_addr, old_table.overflow_pool())
            .unwrap();

        let right_addr = make_addr(200);
        let right_tag: u16 = 0x0DEF;
        let right_hash = old_size | bucket_idx | ((right_tag as u64) << 48); // bit 4 = 1
        resolver.insert(right_addr, right_hash);
        old_table
            .bucket_by_index(bucket_idx)
            .insert_in_chain(right_tag, right_addr, old_table.overflow_pool())
            .unwrap();

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        splitter.split_bucket(bucket_idx, 4, 5);

        // Verify left entry is in new_table[9].
        let left_entries = collect_bucket_entries(&new_table, bucket_idx);
        assert_eq!(left_entries.len(), 1);
        assert_eq!(left_entries[0], (left_tag, left_addr));

        // Verify right entry is in new_table[9 + 16 = 25].
        let right_entries = collect_bucket_entries(&new_table, bucket_idx + old_size);
        assert_eq!(right_entries.len(), 1);
        assert_eq!(right_entries[0], (right_tag, right_addr));
    }

    // -----------------------------------------------------------------------
    // Tests: multi-bucket chunk split
    // -----------------------------------------------------------------------

    #[test]
    fn split_chunk_processes_all_buckets() {
        // Use a table small enough that one chunk covers everything.
        let old_table = HashTable::new(3); // 8 buckets
        let new_table = HashTable::new(4); // 16 buckets
        let mut resolver = MapResolver::new();

        let old_size: u64 = 8;

        // Insert one entry per bucket.
        for bucket_idx in 0..old_size {
            let addr = make_addr(bucket_idx as u32);
            let tag = (0x100 + bucket_idx as u16) & 0x3FFF;
            // All entries move right (bit 3 set).
            let hash = old_size | bucket_idx | ((tag as u64) << 48);
            resolver.insert(addr, hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(tag, addr, old_table.overflow_pool())
                .unwrap();
        }

        assert_eq!(count_entries(&old_table), old_size);

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let result = splitter.split_chunk(0, 3, 4);

        assert_eq!(result.chunks_processed, 1);
        assert_eq!(result.entries_moved, old_size);
        assert_eq!(result.entries_kept, 0);
        assert_eq!(result.buckets_processed, old_size);
        assert_eq!(count_entries(&old_table), 0);
        assert_eq!(count_entries(&new_table), old_size);

        // Each entry landed in the right partner.
        for bucket_idx in 0..old_size {
            assert_eq!(count_bucket_chain_entries(&new_table, bucket_idx), 0);
            assert_eq!(
                count_bucket_chain_entries(&new_table, bucket_idx + old_size),
                1
            );
        }
    }

    // -----------------------------------------------------------------------
    // Tests: full table grow (split all chunks)
    // -----------------------------------------------------------------------

    #[test]
    fn full_table_grow_all_chunks() {
        let old_table = HashTable::new(5); // 32 buckets
        let new_table = HashTable::new(6); // 64 buckets
        let mut resolver = MapResolver::new();

        let old_size: u64 = 32;
        let old_bits: u8 = 5;
        let new_bits: u8 = 6;

        // Insert 2 entries per bucket (1 left, 1 right).
        for bucket_idx in 0..old_size {
            // Left entry.
            let left_addr = make_addr(bucket_idx as u32 * 2);
            let left_tag = (0x100 + bucket_idx as u16) & 0x3FFF;
            let left_hash = bucket_idx | ((left_tag as u64) << 48);
            resolver.insert(left_addr, left_hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(left_tag, left_addr, old_table.overflow_pool())
                .unwrap();

            // Right entry.
            let right_addr = make_addr(bucket_idx as u32 * 2 + 1);
            let right_tag = (0x200 + bucket_idx as u16) & 0x3FFF;
            let right_hash = old_size | bucket_idx | ((right_tag as u64) << 48);
            resolver.insert(right_addr, right_hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(right_tag, right_addr, old_table.overflow_pool())
                .unwrap();
        }

        let total = old_size * 2;
        assert_eq!(count_entries(&old_table), total);

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);

        // CHUNK_SIZE is 16384, which is much larger than 32 buckets.
        // So one chunk covers the entire table.
        let num_chunks = ((old_size as u32) + super::CHUNK_SIZE - 1) / super::CHUNK_SIZE;
        assert_eq!(num_chunks, 1);

        let mut combined = SplitResult::default();
        for chunk in 0..num_chunks {
            let result = splitter.split_chunk(chunk, old_bits, new_bits);
            combined.merge(&result);
        }

        assert_eq!(combined.entries_kept, old_size);
        assert_eq!(combined.entries_moved, old_size);
        assert_eq!(combined.total_entries(), total);
        assert_eq!(count_entries(&old_table), 0);
        assert_eq!(count_entries(&new_table), total);

        // Verify each new bucket has exactly 1 entry.
        for i in 0..new_table.num_buckets() {
            assert_eq!(
                count_bucket_chain_entries(&new_table, i),
                1,
                "new bucket {i} should have exactly 1 entry"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Tests: unresolvable entries go to both partners
    // -----------------------------------------------------------------------

    #[test]
    fn unresolvable_entries_placed_in_both_partners() {
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let resolver = UnresolvableResolver;

        let bucket_idx: u64 = 0;
        let old_size: u64 = 16;

        // Insert 2 entries.
        for i in 0..2u32 {
            let addr = make_addr(i);
            let tag = 0x400 + i as u16;
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(tag, addr, old_table.overflow_pool())
                .unwrap();
        }

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let (kept, moved) = splitter.split_bucket(bucket_idx, 4, 5);

        // Both entries are unresolvable → placed in both partners.
        assert_eq!(kept, 2);
        assert_eq!(moved, 2);
        assert_eq!(count_bucket_chain_entries(&new_table, bucket_idx), 2);
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx + old_size),
            2
        );
    }

    // -----------------------------------------------------------------------
    // Tests: SplitResult
    // -----------------------------------------------------------------------

    #[test]
    fn split_result_total_entries() {
        let r = SplitResult {
            chunks_processed: 2,
            entries_kept: 100,
            entries_moved: 50,
            buckets_processed: 200,
        };
        assert_eq!(r.total_entries(), 150);
    }

    #[test]
    fn split_result_merge() {
        let mut a = SplitResult {
            chunks_processed: 1,
            entries_kept: 10,
            entries_moved: 5,
            buckets_processed: 100,
        };
        let b = SplitResult {
            chunks_processed: 1,
            entries_kept: 20,
            entries_moved: 15,
            buckets_processed: 100,
        };
        a.merge(&b);
        assert_eq!(a.chunks_processed, 2);
        assert_eq!(a.entries_kept, 30);
        assert_eq!(a.entries_moved, 20);
        assert_eq!(a.buckets_processed, 200);
    }

    // -----------------------------------------------------------------------
    // Tests: large overflow chain
    // -----------------------------------------------------------------------

    #[test]
    fn split_bucket_large_overflow_all_left() {
        let old_table = HashTable::new(4);
        let new_table = HashTable::new(5);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 16;
        let bucket_idx: u64 = 1;
        let num_entries = BUCKET_NUM_ENTRIES as u32 * 3; // 21 entries → 2 overflow buckets

        for i in 0..num_entries {
            let addr = make_addr(i + 1000);
            let tag = ((i as u16) + 1) & 0x3FFF; // tag must be > 0
            let hash = bucket_idx | ((tag as u64) << 48); // all stay left
            resolver.insert(addr, hash);
            old_table
                .bucket_by_index(bucket_idx)
                .insert_in_chain(tag, addr, old_table.overflow_pool())
                .unwrap();
        }

        assert_eq!(
            count_bucket_chain_entries(&old_table, bucket_idx),
            num_entries as u64
        );

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        let (kept, moved) = splitter.split_bucket(bucket_idx, 4, 5);

        assert_eq!(kept, num_entries);
        assert_eq!(moved, 0);
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx),
            num_entries as u64
        );
        assert_eq!(
            count_bucket_chain_entries(&new_table, bucket_idx + old_size),
            0
        );
        assert_eq!(count_bucket_chain_entries(&old_table, bucket_idx), 0);
    }

    // -----------------------------------------------------------------------
    // Tests: verify tag and address preserved through split
    // -----------------------------------------------------------------------

    #[test]
    fn tag_and_address_preserved_after_split() {
        let old_table = HashTable::new(3);
        let new_table = HashTable::new(4);
        let mut resolver = MapResolver::new();

        let old_size: u64 = 8;
        let bucket_idx: u64 = 4;

        let addr1 = LogicalAddress::new(Page(10), Offset(256));
        let tag1: u16 = 0x1ABC;
        let hash1 = bucket_idx | ((tag1 as u64) << 48); // left
        resolver.insert(addr1, hash1);

        let addr2 = LogicalAddress::new(Page(20), Offset(512));
        let tag2: u16 = 0x2DEF;
        let hash2 = old_size | bucket_idx | ((tag2 as u64) << 48); // right
        resolver.insert(addr2, hash2);

        old_table
            .bucket_by_index(bucket_idx)
            .insert_in_chain(tag1, addr1, old_table.overflow_pool())
            .unwrap();
        old_table
            .bucket_by_index(bucket_idx)
            .insert_in_chain(tag2, addr2, old_table.overflow_pool())
            .unwrap();

        let splitter = BucketSplitter::new(&old_table, &new_table, &resolver);
        splitter.split_bucket(bucket_idx, 3, 4);

        // Verify left entry preserved.
        let left = collect_bucket_entries(&new_table, bucket_idx);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, tag1);
        assert_eq!(left[0].1, addr1);

        // Verify right entry preserved.
        let right = collect_bucket_entries(&new_table, bucket_idx + old_size);
        assert_eq!(right.len(), 1);
        assert_eq!(right[0].0, tag2);
        assert_eq!(right[0].1, addr2);
    }
}
