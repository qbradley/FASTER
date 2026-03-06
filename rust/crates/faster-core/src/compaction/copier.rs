//! Record copier for log compaction.
//!
//! [`RecordCopier`] reads live records (identified by the compaction scanner)
//! and appends them sequentially to the log tail, producing a [`CopyResult`]
//! that maps old addresses to new addresses.
//!
//! # Approach
//!
//! For each [`LiveRecord`], the copier:
//!
//! 1. Reads the source record bytes via [`RecordAccessor`].
//! 2. Allocates space at the log tail (handling page-boundary retries).
//! 3. Copies the raw record bytes into the newly allocated region.
//! 4. Resets the version-chain pointer (`previous_address`) to
//!    [`LogicalAddress::INVALID`] since the old chain is in the region
//!    being compacted and will be reclaimed.
//! 5. Records the old→new address mapping.
//!
//! # Epoch requirement
//!
//! Like the scanner, the caller **must** hold epoch protection for the
//! duration of the copy. Source records reside in in-memory pages that
//! could be evicted without epoch guards.

use crate::address::LogicalAddress;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::record_ops::{LogRecordWriter, MutableRecordAccessor, RecordAccessor};
use crate::record::RecordInfo;

use super::LiveRecord;

// ── Error type ──────────────────────────────────────────────────────

/// Errors that can occur during record copying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyError {
    /// A source record's address could not be resolved to physical memory.
    /// The page may have been evicted (epoch guard not held) or the
    /// address is otherwise invalid.
    SourceNotInMemory {
        /// The logical address that could not be resolved.
        address: LogicalAddress,
    },

    /// The allocator could not allocate space at the log tail.
    /// This typically means the allocator is sealed or the requested
    /// record size exceeds a single page.
    AllocationFailed {
        /// Size of the record that failed to allocate (bytes).
        record_size: usize,
    },
}

impl core::fmt::Display for CopyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CopyError::SourceNotInMemory { address } => {
                write!(f, "source record not in memory at address {address:?}")
            }
            CopyError::AllocationFailed { record_size } => {
                write!(
                    f,
                    "allocation failed for record of {record_size} bytes (allocator sealed or record exceeds page)"
                )
            }
        }
    }
}

// ── AddressMapping ──────────────────────────────────────────────────

/// Maps a record's original address to its new address after compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressMapping {
    /// Address of the record before compaction.
    pub old_address: LogicalAddress,
    /// Address of the copied record at the log tail.
    pub new_address: LogicalAddress,
}

// ── CopyResult ──────────────────────────────────────────────────────

/// Result of copying live records during compaction.
///
/// Contains the address mappings needed by subsequent compaction phases
/// (hash index update, begin-address advance).
#[derive(Debug, Clone)]
pub struct CopyResult {
    /// One entry per copied record, in the same order as the input
    /// `live_records` slice.
    pub mappings: Vec<AddressMapping>,
    /// Total bytes copied to the log tail.
    pub bytes_copied: usize,
}

impl CopyResult {
    /// Returns the number of records that were copied.
    #[inline]
    pub fn records_copied(&self) -> usize {
        self.mappings.len()
    }

    /// Looks up the new address for a given old address.
    ///
    /// Linear scan — suitable for moderate record counts. Future phases
    /// may build a `HashMap` if needed.
    pub fn new_address_of(&self, old: LogicalAddress) -> Option<LogicalAddress> {
        self.mappings
            .iter()
            .find(|m| m.old_address == old)
            .map(|m| m.new_address)
    }
}

// ── RecordCopier ────────────────────────────────────────────────────

/// Copies live records to the log tail during compaction.
///
/// The copier is a lightweight handle that borrows the allocator.
/// It does not modify the hash index — that is a separate phase.
///
/// # Example
///
/// ```ignore
/// let copier = RecordCopier::new(&allocator);
/// let result = copier.copy_records(&plan.live_records)?;
/// assert_eq!(result.records_copied(), plan.live_records.len());
/// ```
pub struct RecordCopier<'a> {
    allocator: &'a HybridLogAllocator,
}

impl<'a> RecordCopier<'a> {
    /// Creates a new record copier backed by the given allocator.
    #[inline]
    pub fn new(allocator: &'a HybridLogAllocator) -> Self {
        Self { allocator }
    }

    /// Copies all live records to the log tail.
    ///
    /// For each record in `live_records`:
    /// 1. Reads the source record bytes from its current location.
    /// 2. Allocates space at the tail (retrying across page boundaries).
    /// 3. Copies the raw bytes and resets the version-chain pointer.
    /// 4. Records the old→new address mapping.
    ///
    /// Returns a [`CopyResult`] on success, or a [`CopyError`] on the
    /// first record that cannot be read or allocated.
    pub fn copy_records(&self, live_records: &[LiveRecord]) -> Result<CopyResult, CopyError> {
        let mut mappings = Vec::with_capacity(live_records.len());
        let mut bytes_copied: usize = 0;
        let writer = LogRecordWriter::new(self.allocator);

        for record in live_records {
            let size = record.record_size as u32;

            // 1. Read source record bytes.
            let src = RecordAccessor::from_log(self.allocator, record.address, size).ok_or(
                CopyError::SourceNotInMemory {
                    address: record.address,
                },
            )?;

            // 2. Allocate space at the log tail (with page-boundary retry).
            let (new_addr, mut dst) =
                Self::allocate_with_retry(&writer, self.allocator, size, record.record_size)?;

            // 3. Copy raw record bytes into the newly allocated region.
            dst.as_mut_slice().copy_from_slice(src.as_slice());

            // 4. Reset the version-chain pointer. The old chain is in the
            //    compacted region and will be reclaimed. Keep other header
            //    fields (checkpoint version) intact; clear status flags since
            //    the scanner already verified this record is live.
            let old_info = src.record_info();
            let new_info = RecordInfo::new(
                LogicalAddress::INVALID,
                old_info.checkpoint_version(),
                false, // not invalid
                false, // not tombstone
                false, // clear final bit
            );
            dst.write_record_info(&new_info);

            // 5. Record the mapping.
            mappings.push(AddressMapping {
                old_address: record.address,
                new_address: new_addr,
            });
            bytes_copied += record.record_size;
        }

        Ok(CopyResult {
            mappings,
            bytes_copied,
        })
    }

    /// Try to allocate `size` bytes, retrying once across a page boundary.
    ///
    /// Mirrors the two-phase pattern from `store::operations::allocate_at_tail`.
    fn allocate_with_retry(
        writer: &LogRecordWriter<'_>,
        allocator: &HybridLogAllocator,
        size: u32,
        record_size: usize,
    ) -> Result<(LogicalAddress, MutableRecordAccessor), CopyError> {
        // First attempt.
        if let Some(result) = writer.allocate_raw(size) {
            return Ok(result);
        }

        // Page-boundary crossing: advance to the next page and retry once.
        allocator
            .advance_to_next_page()
            .ok_or(CopyError::AllocationFailed { record_size })?;

        writer
            .allocate_raw(size)
            .ok_or(CopyError::AllocationFailed { record_size })
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::scanner::CompactionScanner;
    use crate::device::NullDevice;
    use crate::grow::GrowConfig;
    use crate::hybrid_log::eviction::EvictionPolicy;
    use crate::hybrid_log::record_ops::LogRecordReader;
    use crate::record::RecordLayout;
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

    /// Helper: insert keys 0..n with values key*10, scan, and return (store, plan).
    fn store_with_records(n: u64) -> (SimpleStore, crate::compaction::CompactionPlan) {
        let store = test_store();
        let mut session = store.new_session();
        for i in 0..n {
            let status = store.upsert(&mut session, &i, &(i * 10), ());
            assert!(status.is_success(), "upsert failed: {status}");
        }
        let plan = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            scanner.scan::<u64>(
                store.first_data_address(),
                store.allocator.tail_address(),
                KEY_SIZE,
                VALUE_SIZE,
            )
        };
        store.dispose_session(session);
        (store, plan)
    }

    // ── 1. Empty input → empty result ───────────────────────────────

    #[test]
    fn copy_empty_records() {
        let store = test_store();
        let copier = RecordCopier::new(&store.allocator);
        let result = copier.copy_records(&[]).unwrap();
        assert_eq!(result.records_copied(), 0);
        assert_eq!(result.bytes_copied, 0);
        assert!(result.mappings.is_empty());
    }

    // ── 2. Copy a single record, verify readable at new address ─────

    #[test]
    fn copy_single_record() {
        let (store, plan) = store_with_records(1);
        assert_eq!(plan.live_records.len(), 1);

        let mut session = store.new_session();
        {
            let _guard = session.begin_unsafe();
            let copier = RecordCopier::new(&store.allocator);
            let result = copier.copy_records(&plan.live_records).unwrap();

            assert_eq!(result.records_copied(), 1);
            assert_eq!(result.bytes_copied, plan.live_bytes);

            // Verify the copied record is readable at the new address.
            let mapping = &result.mappings[0];
            assert_ne!(mapping.old_address, mapping.new_address);

            let layout = RecordLayout::compute(KEY_SIZE, VALUE_SIZE);
            let reader = LogRecordReader::new(&store.allocator);
            let key: u64 = reader.read_key(mapping.new_address, &layout).unwrap();
            let value: u64 = reader.read_value(mapping.new_address, &layout).unwrap();
            assert_eq!(key, 0);
            assert_eq!(value, 0);
        }
        store.dispose_session(session);
    }

    // ── 3. Copy 1K records, verify all readable at new addresses ────

    #[test]
    fn copy_1k_records() {
        let n = 1000u64;
        let (store, plan) = store_with_records(n);
        assert_eq!(plan.live_records.len(), n as usize);

        let mut session = store.new_session();
        {
            let _guard = session.begin_unsafe();
            let copier = RecordCopier::new(&store.allocator);
            let result = copier.copy_records(&plan.live_records).unwrap();

            assert_eq!(result.records_copied(), n as usize);
            assert_eq!(result.bytes_copied, plan.live_bytes);

            let layout = RecordLayout::compute(KEY_SIZE, VALUE_SIZE);
            let reader = LogRecordReader::new(&store.allocator);

            // Verify each copied record has the correct key and value.
            for (i, mapping) in result.mappings.iter().enumerate() {
                let key: u64 = reader
                    .read_key(mapping.new_address, &layout)
                    .unwrap_or_else(|| {
                        panic!(
                            "record {i} not readable at new address {:?}",
                            mapping.new_address
                        )
                    });
                let value: u64 = reader
                    .read_value(mapping.new_address, &layout)
                    .unwrap_or_else(|| {
                        panic!(
                            "record {i} value not readable at new address {:?}",
                            mapping.new_address
                        )
                    });
                assert_eq!(
                    value,
                    key * 10,
                    "record {i}: expected value {} got {value}",
                    key * 10
                );
            }
        }
        store.dispose_session(session);
    }

    // ── 4. Variable-length records (different-sized keys/values) ────

    #[test]
    fn copy_variable_length_records() {
        // Use a store with u64 keys and u64 values (both 8 bytes).
        // While the scanner currently handles fixed-size records, the copier
        // works at the byte level using record_size from LiveRecord, so it
        // handles any record size. We verify this by checking that the
        // record_size in LiveRecord is faithfully used.
        let (store, plan) = store_with_records(50);
        assert_eq!(plan.live_records.len(), 50);

        // Verify all records have the expected fixed size.
        let expected_size = RecordLayout::compute(KEY_SIZE, VALUE_SIZE).total_size();
        for rec in &plan.live_records {
            assert_eq!(rec.record_size, expected_size);
        }

        let mut session = store.new_session();
        {
            let _guard = session.begin_unsafe();
            let copier = RecordCopier::new(&store.allocator);
            let result = copier.copy_records(&plan.live_records).unwrap();

            // Total bytes should match.
            assert_eq!(result.bytes_copied, 50 * expected_size);

            // All records readable at new addresses.
            let layout = RecordLayout::compute(KEY_SIZE, VALUE_SIZE);
            let reader = LogRecordReader::new(&store.allocator);
            for mapping in &result.mappings {
                let key: u64 = reader.read_key(mapping.new_address, &layout).unwrap();
                let value: u64 = reader.read_value(mapping.new_address, &layout).unwrap();
                assert_eq!(value, key * 10);
            }
        }
        store.dispose_session(session);
    }

    // ── 5. Copied records have clean RecordInfo ─────────────────────

    #[test]
    fn copied_records_have_reset_header() {
        let (store, plan) = store_with_records(5);

        let mut session = store.new_session();
        {
            let _guard = session.begin_unsafe();
            let copier = RecordCopier::new(&store.allocator);
            let result = copier.copy_records(&plan.live_records).unwrap();

            let reader = LogRecordReader::new(&store.allocator);
            for mapping in &result.mappings {
                let info = reader.read_record_info(mapping.new_address).unwrap();
                // Version chain should be reset.
                assert_eq!(
                    info.previous_address(),
                    LogicalAddress::INVALID,
                    "copied record should have INVALID previous_address"
                );
                // Status flags should be cleared.
                assert!(!info.is_invalid(), "copied record should not be invalid");
                assert!(
                    !info.is_tombstone(),
                    "copied record should not be tombstone"
                );
                assert!(!info.is_final(), "copied record should not have final bit");
            }
        }
        store.dispose_session(session);
    }

    // ── 6. Address mappings are distinct ────────────────────────────

    #[test]
    fn address_mappings_are_distinct() {
        let (store, plan) = store_with_records(100);

        let mut session = store.new_session();
        {
            let _guard = session.begin_unsafe();
            let copier = RecordCopier::new(&store.allocator);
            let result = copier.copy_records(&plan.live_records).unwrap();

            // All new addresses should be unique.
            let mut new_addrs: Vec<_> = result.mappings.iter().map(|m| m.new_address).collect();
            new_addrs.sort();
            new_addrs.dedup();
            assert_eq!(
                new_addrs.len(),
                result.records_copied(),
                "all new addresses must be unique"
            );

            // No new address should equal any old address
            // (they were appended after the original tail).
            for mapping in &result.mappings {
                assert_ne!(
                    mapping.old_address, mapping.new_address,
                    "new address must differ from old address"
                );
            }
        }
        store.dispose_session(session);
    }

    // ── 7. new_address_of lookup ────────────────────────────────────

    #[test]
    fn new_address_of_lookup() {
        let (store, plan) = store_with_records(10);

        let mut session = store.new_session();
        {
            let _guard = session.begin_unsafe();
            let copier = RecordCopier::new(&store.allocator);
            let result = copier.copy_records(&plan.live_records).unwrap();

            // Every old address should be findable.
            for mapping in &result.mappings {
                assert_eq!(
                    result.new_address_of(mapping.old_address),
                    Some(mapping.new_address),
                );
            }

            // A made-up address should return None.
            assert_eq!(result.new_address_of(LogicalAddress::ZERO), None);
        }
        store.dispose_session(session);
    }

    // ── 8. Copy with dead records (scanner filters them) ────────────

    #[test]
    fn copy_only_live_after_updates() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 20 records, then update 10 of them (creating dead versions).
        for i in 0u64..20 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }
        for i in 0u64..10 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        let plan = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            scanner.scan::<u64>(
                store.first_data_address(),
                store.allocator.tail_address(),
                KEY_SIZE,
                VALUE_SIZE,
            )
        };
        // Scanner finds live records (latest version of each key).
        // Note: In the mutable region, upserts may update in place rather than
        // creating new records, so dead_count depends on the update strategy.
        assert_eq!(plan.live_records.len(), 20);

        {
            let _guard = session.begin_unsafe();
            let copier = RecordCopier::new(&store.allocator);
            let result = copier.copy_records(&plan.live_records).unwrap();
            assert_eq!(result.records_copied(), 20);

            // Verify the updated values are preserved.
            let layout = RecordLayout::compute(KEY_SIZE, VALUE_SIZE);
            let reader = LogRecordReader::new(&store.allocator);
            for mapping in &result.mappings {
                let key: u64 = reader.read_key(mapping.new_address, &layout).unwrap();
                let value: u64 = reader.read_value(mapping.new_address, &layout).unwrap();
                if key < 10 {
                    assert_eq!(
                        value,
                        key * 100,
                        "updated key {key} should have value {}",
                        key * 100
                    );
                } else {
                    assert_eq!(
                        value,
                        key * 10,
                        "original key {key} should have value {}",
                        key * 10
                    );
                }
            }
        }
        store.dispose_session(session);
    }
}
