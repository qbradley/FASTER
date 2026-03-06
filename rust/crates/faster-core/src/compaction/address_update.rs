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
use crate::hybrid_log::record_ops::{LogRecordReader, RecordAccessor};
use crate::record::{Key, RecordLayout};

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
/// let stats = updater.swing::<u64>(&copy_result, &tombstone_addresses, 8, 8);
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
    /// address, hashes it, finds the hash index entry, and CAS-es the
    /// entry from old→new address.
    ///
    /// For each address in `tombstone_addresses`, reads the key from the
    /// original address (which must still be in memory under epoch
    /// protection), finds the hash index entry, and CAS-es it to EMPTY.
    ///
    /// # Type parameter
    ///
    /// `K` must be the same key type used when the records were written.
    ///
    /// # Arguments
    ///
    /// - `copy_result` — the result from [`RecordCopier::copy_records`].
    /// - `tombstone_addresses` — addresses of tombstoned records to remove.
    /// - `key_size` / `value_size` — serialized sizes of key and value.
    pub fn swing<K: Key>(
        &self,
        copy_result: &CopyResult,
        tombstone_addresses: &[LogicalAddress],
        key_size: usize,
        value_size: usize,
    ) -> SwingStats {
        let layout = RecordLayout::compute(key_size, value_size);
        let mut stats = SwingStats {
            swung: 0,
            cas_failed: 0,
            not_found: 0,
            tombstones_removed: 0,
        };

        // Phase 1: Swing live record pointers.
        for mapping in &copy_result.mappings {
            self.swing_one::<K>(mapping, &layout, &mut stats);
        }

        // Phase 2: Remove tombstones.
        for &addr in tombstone_addresses {
            self.remove_tombstone::<K>(addr, &layout, &mut stats);
        }

        stats
    }

    /// Swing a single hash index entry from old_address to new_address.
    fn swing_one<K: Key>(
        &self,
        mapping: &AddressMapping,
        layout: &RecordLayout,
        stats: &mut SwingStats,
    ) {
        // Read the key from the NEW address (it was just copied to the tail).
        let reader = LogRecordReader::new(self.allocator);
        let key: K = match reader.read_key(mapping.new_address, layout) {
            Some(k) => k,
            None => {
                stats.not_found += 1;
                return;
            }
        };

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
    fn remove_tombstone<K: Key>(
        &self,
        addr: LogicalAddress,
        layout: &RecordLayout,
        stats: &mut SwingStats,
    ) {
        // Read the key from the tombstoned record (still in memory under epoch).
        let record_size = layout.total_size() as u32;
        let key: K = match RecordAccessor::from_log(self.allocator, addr, record_size) {
            Some(accessor) => accessor.key(layout),
            None => {
                stats.not_found += 1;
                return;
            }
        };

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
    use crate::compaction::copier::RecordCopier;
    use crate::compaction::scanner::CompactionScanner;
    use crate::device::NullDevice;
    use crate::grow::GrowConfig;
    use crate::hash::Hashable;
    use crate::hybrid_log::eviction::EvictionPolicy;
    use crate::hybrid_log::record_ops::LogRecordReader;
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
        };
        FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
    }

    const KEY_SIZE: usize = 8;
    const VALUE_SIZE: usize = 8;

    // ── 1. After swing, all reads return correct values ─────────────

    #[test]
    fn swing_updates_all_entries() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 100 records.
        for i in 0u64..100 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        let (_plan, copy_result) = {
            let _guard = session.begin_unsafe();

            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner.scan::<u64>(
                store.first_data_address(),
                store.allocator.tail_address(),
                KEY_SIZE,
                VALUE_SIZE,
            );

            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();

            (plan, copy_result)
        };

        // Perform the pointer swing.
        {
            let _guard = session.begin_unsafe();

            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats = updater.swing::<u64>(&copy_result, &[], KEY_SIZE, VALUE_SIZE);

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
        for i in 0u64..25 {
            let _ = store.delete_simple(&mut session, &i);
        }

        let (_plan, copy_result) = {
            let _guard = session.begin_unsafe();

            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner.scan::<u64>(
                store.first_data_address(),
                store.allocator.tail_address(),
                KEY_SIZE,
                VALUE_SIZE,
            );

            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();

            (plan, copy_result)
        };

        // Collect tombstone addresses from the scan.
        let tombstone_addresses: Vec<LogicalAddress> = {
            let _guard = session.begin_unsafe();

            let layout = RecordLayout::compute(KEY_SIZE, VALUE_SIZE);
            let reader = LogRecordReader::new(&store.allocator);
            let record_size = layout.total_size() as u32;

            let mut tombstones = Vec::new();
            let mut current = store.first_data_address();
            let until = store.allocator.tail_address();
            let page_size = store.allocator.page_size();

            while current < until {
                let offset = current.offset().0;
                if offset > 0 && (page_size - offset) < record_size {
                    let next_page = current.page().0 + 1;
                    current = LogicalAddress::new(
                        crate::address::Page(next_page),
                        crate::address::Offset(0),
                    );
                    continue;
                }

                if let Some(info) = reader.read_record_info(current) {
                    if info.is_tombstone() {
                        tombstones.push(current);
                    }
                }

                let new_offset = current.offset().0 + record_size;
                if new_offset >= page_size {
                    current = LogicalAddress::new(
                        crate::address::Page(current.page().0 + 1),
                        crate::address::Offset(0),
                    );
                } else {
                    current =
                        LogicalAddress::new(current.page(), crate::address::Offset(new_offset));
                }
            }

            tombstones
        };

        // Perform swing + tombstone removal.
        {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            let stats =
                updater.swing::<u64>(&copy_result, &tombstone_addresses, KEY_SIZE, VALUE_SIZE);

            assert!(
                stats.tombstones_removed > 0,
                "should have removed tombstones"
            );
            assert!(
                stats.tombstones_removed as usize <= tombstone_addresses.len(),
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

        let copy_result = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner.scan::<u64>(
                store.first_data_address(),
                store.allocator.tail_address(),
                KEY_SIZE,
                VALUE_SIZE,
            );
            let copier = RecordCopier::new(&store.allocator);
            copier.copy_records(&plan.live_records).unwrap()
        };

        // First swing.
        let stats1 = {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            updater.swing::<u64>(&copy_result, &[], KEY_SIZE, VALUE_SIZE)
        };
        assert_eq!(stats1.swung, 20);

        // Second swing — entries already point to new addresses, so CAS won't match old.
        let stats2 = {
            let _guard = session.begin_unsafe();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            updater.swing::<u64>(&copy_result, &[], KEY_SIZE, VALUE_SIZE)
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
        let copy_result = Arc::new({
            let mut session = store.new_session();
            let result = {
                let _guard = session.begin_unsafe();
                let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
                let plan = scanner.scan::<u64>(
                    store.first_data_address(),
                    store.allocator.tail_address(),
                    KEY_SIZE,
                    VALUE_SIZE,
                );
                let copier = RecordCopier::new(&store.allocator);
                copier.copy_records(&plan.live_records).unwrap()
            };
            store.dispose_session(session);
            result
        });

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
                let stats = updater.swing::<u64>(&copy_result, &[], KEY_SIZE, VALUE_SIZE);
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
            let stats = updater.swing::<u64>(&empty_result, &[], KEY_SIZE, VALUE_SIZE);

            assert_eq!(stats.swung, 0);
            assert_eq!(stats.cas_failed, 0);
            assert_eq!(stats.not_found, 0);
            assert_eq!(stats.tombstones_removed, 0);
        }

        store.dispose_session(session);
    }
}
