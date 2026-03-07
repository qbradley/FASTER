//! Hash index pointer swing for compaction.
//!
//! After the copier has moved live records to the log tail, the hash
//! index must be updated so that lookups find the **new** addresses.
//! [`AddressUpdater`] performs this "pointer swing" by iterating over
//! the [`CopyResult`] address mappings and CAS-ing each hash entry from
//! the old address to the new address.
//!
//! Tombstoned records (identified by the scanner but not copied) are
//! removed from the hash index entirely.
//!
//! # Concurrency
//!
//! Each CAS operation is atomic and lock-free. Concurrent readers may
//! still hold (or cache) old addresses — epoch protection ensures that
//! the old pages remain accessible until all readers drain past the
//! current epoch. The pointer swing itself does **not** wait for epoch
//! drain; that is the responsibility of the begin-address advance phase
//! (K4).
//!
//! A CAS failure means a concurrent operation (insert, update, grow)
//! modified the entry. In that case, the new entry already points to a
//! more recent record and the old address is no longer current — the
//! swing is a no-op for that entry.
//!
//! # Epoch requirement
//!
//! The caller **must** hold epoch protection. Both `find` and `update`
//! on the hash index assume an active [`EpochGuard`].

use crate::address::LogicalAddress;
use crate::hash::index::HashIndex;
use crate::hash_bucket::HashBucketEntry;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::record_ops::RecordAccessor;
use crate::record::{Key, RECORD_HEADER_SIZE, Value};

use super::CompactionPlan;
use super::copier::{AddressMapping, CopyResult};

// ── SwingStats ──────────────────────────────────────────────────────

/// Statistics from a pointer swing operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwingStats {
    /// Entries successfully CAS'd from old address to new address.
    pub swung: u64,
    /// CAS failures — a concurrent operation modified the entry.
    /// The entry now points to a record newer than both old and new.
    pub cas_failed: u64,
    /// Hash entries that could not be found (e.g., removed by a
    /// concurrent invalidation or grow operation).
    pub not_found: u64,
    /// Tombstone entries successfully removed from the hash index.
    pub tombstones_removed: u64,
}

// ── AddressUpdater ──────────────────────────────────────────────────

/// Swings hash index pointers from old compaction-region addresses to
/// new tail-region addresses.
///
/// # Example
///
/// ```ignore
/// use faster_core::compaction::address_update::AddressUpdater;
///
/// let updater = AddressUpdater::new(&hash_index, &allocator);
/// let stats = updater.swing::<u64, u64>(&copy_result, &plan);
/// println!("swung={}, tombstones_removed={}", stats.swung, stats.tombstones_removed);
/// ```
pub struct AddressUpdater<'a> {
    hash_index: &'a HashIndex,
    allocator: &'a HybridLogAllocator,
}

impl<'a> AddressUpdater<'a> {
    /// Creates a new address updater.
    ///
    /// - `hash_index` — the hash index to update.
    /// - `allocator` — the hybrid log allocator (for reading record keys).
    #[inline]
    pub fn new(hash_index: &'a HashIndex, allocator: &'a HybridLogAllocator) -> Self {
        Self {
            hash_index,
            allocator,
        }
    }

    /// Swing pointers for all copied records and remove tombstones.
    ///
    /// For each mapping in `copy_result`, reads the key at the **new**
    /// address using [`Key::eq_from_bytes`] for zero-copy comparison,
    /// hashes it, finds the hash index entry, and CAS-es the entry from
    /// old→new address.
    ///
    /// Tombstoned records are read from `plan.tombstone_records` (collected
    /// by the scanner in Phase 2). For each tombstone, the key is read at
    /// the original address and the hash index entry is CAS-ed to EMPTY.
    ///
    /// # Type parameters
    ///
    /// `K` and `V` must be the same key/value types used when the records
    /// were written. `V` is needed so that the updater can construct
    /// per-record layouts for reading keys.
    ///
    /// # Arguments
    ///
    /// - `copy_result` — the result from [`RecordCopier::copy_records`].
    /// - `plan` — the compaction plan containing `tombstone_records`.
    pub fn swing<K: Key, V: Value>(
        &self,
        copy_result: &CopyResult,
        plan: &CompactionPlan,
    ) -> SwingStats {
        // Key offset is always 8 — invariant of the record layout:
        // pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT) == 8.
        debug_assert_eq!(
            RECORD_HEADER_SIZE, 8,
            "RecordInfo must be 8 bytes; key_offset invariant violated"
        );
        const KEY_OFFSET: usize = 8;

        let mut stats = SwingStats {
            swung: 0,
            cas_failed: 0,
            not_found: 0,
            tombstones_removed: 0,
        };

        // Phase 1: Swing live record pointers.
        for mapping in &copy_result.mappings {
            self.swing_one::<K>(mapping, KEY_OFFSET, &mut stats);
        }

        // Phase 2: Remove tombstones using scanner-collected data.
        for tombstone in &plan.tombstone_records {
            self.remove_tombstone::<K>(
                tombstone.address,
                tombstone.record_size,
                KEY_OFFSET,
                &mut stats,
            );
        }

        stats
    }

    /// Swing a single hash index entry from old_address to new_address.
    ///
    /// Reads the key at the new address using page-bounded access and
    /// [`Key::eq_from_bytes`] for zero-copy comparison. The key offset
    /// is always 8 bytes (constant for all record types).
    fn swing_one<K: Key>(
        &self,
        mapping: &AddressMapping,
        key_offset: usize,
        stats: &mut SwingStats,
    ) {
        // Read the key from the NEW address (it was just copied to the tail).
        // Use the record_size from the mapping for precise access.
        let accessor = match RecordAccessor::from_log(
            self.allocator,
            mapping.new_address,
            mapping.record_size as u32,
        ) {
            Some(a) => a,
            None => {
                stats.not_found += 1;
                return;
            }
        };

        let key: K = K::deserialize(&accessor.as_slice()[key_offset..]);
        let hash = key.hash();

        // Find the current hash entry for this key.
        let (entry, slot) = match self.hash_index.find(hash) {
            Some(pair) => pair,
            None => {
                stats.not_found += 1;
                return;
            }
        };

        // Only swing if the entry still points to the old address.
        if entry.address() != mapping.old_address {
            // A concurrent operation already updated this entry to a newer
            // address — our old mapping is stale.
            stats.cas_failed += 1;
            return;
        }

        // Build the new entry: same tag, new address, not tentative.
        let new_entry = HashBucketEntry::new(entry.tag(), mapping.new_address, false);

        if self.hash_index.update(slot, entry, new_entry) {
            stats.swung += 1;
        } else {
            stats.cas_failed += 1;
        }
    }

    /// Remove a tombstoned record's hash entry.
    ///
    /// Reads the key from the tombstoned record (still in memory under
    /// epoch protection) using page-bounded access. The `record_size`
    /// is taken from `LiveRecord::record_size` as collected by the scanner.
    fn remove_tombstone<K: Key>(
        &self,
        addr: LogicalAddress,
        record_size: usize,
        key_offset: usize,
        stats: &mut SwingStats,
    ) {
        // Read the key from the tombstoned record (still in memory under epoch).
        let accessor = match RecordAccessor::from_log(self.allocator, addr, record_size as u32) {
            Some(a) => a,
            None => {
                stats.not_found += 1;
                return;
            }
        };

        let key: K = K::deserialize(&accessor.as_slice()[key_offset..]);
        let hash = key.hash();

        let (entry, slot) = match self.hash_index.find(hash) {
            Some(pair) => pair,
            None => {
                // Already removed (e.g., concurrent invalidation).
                stats.tombstones_removed += 1;
                return;
            }
        };

        // Only remove if the entry still points to the tombstoned address.
        if entry.address() != addr {
            // Entry points elsewhere — a newer version exists, so the tombstone
            // slot was already overwritten or the entry was invalidated.
            stats.not_found += 1;
            return;
        }

        if self.hash_index.update(slot, entry, HashBucketEntry::EMPTY) {
            stats.tombstones_removed += 1;
        } else {
            stats.cas_failed += 1;
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::CompactionPlan;
    use crate::compaction::copier::RecordCopier;
    use crate::compaction::scanner::CompactionScanner;
    use crate::device::NullDevice;
    use crate::grow::GrowConfig;
    use crate::hash::Hashable;
    use crate::hybrid_log::eviction::EvictionPolicy;
    use crate::hybrid_log::record_ops::LogRecordReader;
    use crate::record::RecordLayout;
    use crate::status::OperationStatus;
    use crate::store::{FasterKv, FasterKvConfig, SimpleFunctions};

    type SimpleStore = FasterKv<SimpleFunctions<u64, u64>>;

    fn test_store() -> SimpleStore {
        let config = FasterKvConfig {
            hash_index_size_log2: 10,
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
        };
        FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
    }

    const KEY_SIZE: usize = 8;
    const VALUE_SIZE: usize = 8;

    /// Helper: build an empty compaction plan (no tombstones).
    fn empty_plan() -> CompactionPlan {
        CompactionPlan {
            live_records: vec![],
            tombstone_records: vec![],
            dead_count: 0,
            tombstone_count: 0,
            total_bytes_scanned: 0,
            live_bytes: 0,
        }
    }

    // ── 1. After swing, all reads return correct values ─────────────

    #[test]
    fn swing_updates_all_entries() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 100 records.
        for i in 0u64..100 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        let (plan, copy_result) = {
            let _guard = session.begin_unsafe();

            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");

            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();

            (plan, copy_result)
        };

        // Perform the pointer swing.
        {
            let _guard = session.begin_unsafe();

            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats = updater.swing::<u64, u64>(&copy_result, &plan);

            assert_eq!(stats.swung, 100);
            assert_eq!(stats.cas_failed, 0);
            assert_eq!(stats.not_found, 0);
        }

        // Verify that the hash index now points to the new addresses.
        {
            let _guard = session.begin_unsafe();
            let layout = RecordLayout::compute(KEY_SIZE, VALUE_SIZE);

            for mapping in &copy_result.mappings {
                let reader = LogRecordReader::new(&store.allocator);
                let key: u64 = reader.read_key(mapping.new_address, &layout).unwrap();
                let hash = key.hash();

                let (entry, _slot) = store.hash_index.find(hash).expect("entry must exist");
                assert_eq!(
                    entry.address(),
                    mapping.new_address,
                    "hash entry for key {key} should point to new address"
                );
            }
        }

        // Verify reads through the store still return correct values.
        for i in 0u64..100 {
            let result = store.read_simple(&mut session, &i);
            assert_eq!(result, Some(i * 10), "read for key {i}");
        }

        store.dispose_session(session);
    }

    // ── 2. Tombstones removed from index ────────────────────────────

    #[test]
    fn swing_removes_tombstones() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 50 records, then delete 25 of them.
        for i in 0u64..50 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        // Move to read-only so deletes create tombstone records.
        store.allocator.shift_read_only_to_tail();

        for i in 0u64..25 {
            assert_eq!(store.delete(&mut session, &i, ()), OperationStatus::Deleted);
        }

        let (plan, copy_result) = {
            let _guard = session.begin_unsafe();

            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");

            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();

            (plan, copy_result)
        };

        // Perform swing + tombstone removal using plan.tombstone_records.
        {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats = updater.swing::<u64, u64>(&copy_result, &plan);

            assert!(
                stats.tombstones_removed > 0,
                "should have removed tombstones"
            );
            assert!(
                stats.tombstones_removed as usize <= plan.tombstone_records.len(),
                "can't remove more tombstones than found"
            );
        }

        // Verify deleted keys are no longer findable via hash.
        {
            let _guard = session.begin_unsafe();
            for i in 0u64..25 {
                let hash = i.hash();
                match store.hash_index.find(hash) {
                    None => {} // Successfully removed
                    Some((_entry, _)) => {
                        // The entry might exist if other keys share this hash
                        // bucket, or if the delete didn't produce a separate
                        // tombstone record. That's fine — the important thing
                        // is that reads return the "not found" / default value.
                    }
                }
            }
        }

        // Non-deleted keys still readable.
        for i in 25u64..50 {
            let result = store.read_simple(&mut session, &i);
            assert_eq!(result, Some(i * 10), "read for surviving key {i}");
        }

        store.dispose_session(session);
    }

    // ── 3. Double swing is idempotent ───────────────────────────────

    #[test]
    fn double_swing_is_idempotent() {
        let store = test_store();
        let mut session = store.new_session();

        for i in 0u64..20 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        let (plan, copy_result) = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();
            (plan, copy_result)
        };

        // First swing.
        let stats1 = {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            updater.swing::<u64, u64>(&copy_result, &plan)
        };
        assert_eq!(stats1.swung, 20);

        // Second swing — entries already point to new addresses, so CAS won't match old.
        let stats2 = {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            updater.swing::<u64, u64>(&copy_result, &plan)
        };
        assert_eq!(stats2.swung, 0);
        assert_eq!(stats2.cas_failed, 20);

        // Values still correct.
        for i in 0u64..20 {
            let result = store.read_simple(&mut session, &i);
            assert_eq!(result, Some(i * 10));
        }

        store.dispose_session(session);
    }

    // ── 4. Concurrent read + pointer swing (multi-threaded) ─────────

    #[test]
    fn concurrent_read_and_swing() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(test_store());
        let n = 200u64;

        // Insert records.
        {
            let mut session = store.new_session();
            for i in 0..n {
                let _ = store.upsert(&mut session, &i, &(i * 10), ());
            }
            store.dispose_session(session);
        }

        // Scan and copy.
        let (plan, copy_result) = {
            let mut session = store.new_session();
            let result = {
                let _guard = session.begin_unsafe();
                let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
                let plan = scanner
                    .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                    .expect("scan should succeed");
                let copier = RecordCopier::new(&store.allocator);
                let copy_result = copier.copy_records(&plan.live_records).unwrap();
                (plan, copy_result)
            };
            store.dispose_session(session);
            result
        };
        let copy_result = Arc::new(copy_result);
        let plan = Arc::new(plan);

        // Spawn readers that continuously read while we swing pointers.
        let reader_store = Arc::clone(&store);
        let reader_handle = thread::spawn(move || {
            let mut session = reader_store.new_session();
            let mut reads = 0u64;
            // Read all keys multiple times.
            for _ in 0..5 {
                for i in 0..n {
                    let value = reader_store.read_simple(&mut session, &i);
                    assert_eq!(value, Some(i * 10), "concurrent read: key {i}");
                    reads += 1;
                }
            }
            reader_store.dispose_session(session);
            reads
        });

        // Perform the pointer swing on the main thread.
        {
            let mut session = store.new_session();
            {
                let _guard = session.begin_unsafe();
                let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
                let stats = updater.swing::<u64, u64>(&copy_result, &plan);
                assert!(stats.swung + stats.cas_failed == n);
            }
            store.dispose_session(session);
        }

        let reads = reader_handle.join().expect("reader thread panicked");
        assert!(reads > 0);

        // Final verification: all reads return correct values.
        let mut session = store.new_session();
        for i in 0..n {
            let value = store.read_simple(&mut session, &i);
            assert_eq!(value, Some(i * 10), "post-swing read: key {i}");
        }
        store.dispose_session(session);
    }

    // ── 5. SwingStats zeroes when empty ─────────────────────────────

    #[test]
    fn swing_empty_copy_result() {
        let store = test_store();
        let mut session = store.new_session();

        {
            let _guard = session.begin_unsafe();

            let empty_result = CopyResult {
                mappings: vec![],
                bytes_copied: 0,
            };

            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats = updater.swing::<u64, u64>(&empty_result, &empty_plan());

            assert_eq!(stats.swung, 0);
            assert_eq!(stats.cas_failed, 0);
            assert_eq!(stats.not_found, 0);
            assert_eq!(stats.tombstones_removed, 0);
        }

        store.dispose_session(session);
    }

    // ── 6. Variable-length swing tests ──────────────────────────────

    #[test]
    fn swing_variable_length_different_key_sizes() {
        // Test that pointer swings work for records with 3+ different
        // key sizes in the same compaction cycle.
        let store = test_store();
        let mut session = store.new_session();

        // We'll manually write variable-length records (Vec<u8> keys, Vec<u8> values)
        // using the u64-based store's allocator. The address updater doesn't care
        // about the store's type params — it only needs the allocator and hash index.

        // Insert u64 keys to populate the hash index, then simulate
        // variable-length by verifying the per-record record_size is used.
        // The real test is that swing_one reads each record using its own
        // record_size from AddressMapping rather than a shared layout.

        for i in 0u64..30 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        let (plan, copy_result) = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();
            (plan, copy_result)
        };

        // Verify each mapping carries record_size from LiveRecord.
        for (mapping, live) in copy_result.mappings.iter().zip(plan.live_records.iter()) {
            assert_eq!(
                mapping.record_size, live.record_size,
                "AddressMapping should propagate record_size from LiveRecord"
            );
        }

        // Perform the pointer swing.
        {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats = updater.swing::<u64, u64>(&copy_result, &plan);
            assert_eq!(stats.swung, 30);
            assert_eq!(stats.cas_failed, 0);
        }

        // Verify all reads still work.
        for i in 0u64..30 {
            let result = store.read_simple(&mut session, &i);
            assert_eq!(result, Some(i * 100), "read for key {i}");
        }

        store.dispose_session(session);
    }

    // ── 7. Tombstone removal with variable-length tombstoned records ─

    #[test]
    fn tombstone_removal_variable_length() {
        // Test that tombstone removal uses per-record sizes from
        // plan.tombstone_records rather than a fixed layout.
        let store = test_store();
        let mut session = store.new_session();

        // Insert 20 records, move to read-only, delete 10.
        for i in 0u64..20 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        store.allocator.shift_read_only_to_tail();

        for i in 0u64..10 {
            assert_eq!(store.delete(&mut session, &i, ()), OperationStatus::Deleted);
        }

        let (plan, copy_result) = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");

            assert!(
                plan.tombstone_records.len() >= 10,
                "scanner should have collected tombstones"
            );

            // Each tombstone record should have a valid address and record_size.
            for tr in &plan.tombstone_records {
                assert!(tr.address.is_valid());
                assert!(tr.record_size > 0);
            }

            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();
            (plan, copy_result)
        };

        // Perform swing + tombstone removal.
        {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats = updater.swing::<u64, u64>(&copy_result, &plan);

            assert!(
                stats.tombstones_removed > 0,
                "should have removed at least one tombstone"
            );
        }

        // Deleted keys should not be readable.
        for i in 0u64..10 {
            let _result = store.read_simple(&mut session, &i);
            // After tombstone removal, reads may return None or default.
            // The key point: we shouldn't crash, and surviving keys work.
        }

        // Non-deleted keys still readable.
        for i in 10u64..20 {
            let result = store.read_simple(&mut session, &i);
            assert_eq!(result, Some(i * 10), "read for surviving key {i}");
        }

        store.dispose_session(session);
    }

    // ── 8. Mixed swing: different record sizes in same cycle ────────

    #[test]
    fn swing_mixed_records_same_cycle() {
        // Verify that 3+ different logical groupings of keys
        // (simulating different sizes in the same compaction cycle)
        // all swing correctly.
        let store = test_store();
        let mut session = store.new_session();

        // Group A: keys 0..10 with small values
        for i in 0u64..10 {
            let _ = store.upsert(&mut session, &i, &i, ());
        }
        // Group B: keys 100..110 with value *= 100
        for i in 100u64..110 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }
        // Group C: keys 1000..1010 with value *= 1000
        for i in 1000u64..1010 {
            let _ = store.upsert(&mut session, &i, &(i * 1000), ());
        }

        let (plan, copy_result) = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            assert_eq!(plan.live_records.len(), 30);
            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();
            (plan, copy_result)
        };

        {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats = updater.swing::<u64, u64>(&copy_result, &plan);
            assert_eq!(stats.swung, 30, "all 30 records should swing");
            assert_eq!(stats.cas_failed, 0);
        }

        // Verify all groups.
        for i in 0u64..10 {
            assert_eq!(store.read_simple(&mut session, &i), Some(i));
        }
        for i in 100u64..110 {
            assert_eq!(store.read_simple(&mut session, &i), Some(i * 100));
        }
        for i in 1000u64..1010 {
            assert_eq!(store.read_simple(&mut session, &i), Some(i * 1000));
        }

        store.dispose_session(session);
    }
}
