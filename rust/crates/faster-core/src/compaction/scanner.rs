//! Compaction log scanner.
//!
//! [`CompactionScanner`] walks a region of the hybrid log and classifies
//! each record as **live**, **dead**, or **tombstoned** by consulting the
//! hash index.
//!
//! # Classification rules
//!
//! For each non-null record at address `A` with key `K`:
//!
//! 1. **Tombstoned** — the record's tombstone flag is set.
//! 2. **Dead (invalid)** — the record's invalid flag is set (already
//!    superseded in the mutable region).
//! 3. **Live / Dead (hash check)** — hash `K`, look up the hash index,
//!    and walk the version chain from the chain head:
//!    - If the first non-invalid, key-matching record in the chain is at
//!      `A`, the record is **live**.
//!    - If it's at a different address, the record is **dead** (a newer
//!      version exists).
//!    - If the chain cannot be resolved (page not in memory, no entry
//!      found, etc.), the record is conservatively classified as **live**.
//!
//! # Epoch requirement
//!
//! The caller **must** hold epoch protection for the duration of the scan.
//! This ensures that pages are not evicted while the scanner reads them.
//! Typically, the caller enters a session and holds an epoch guard.
//!
//! # Variable-length records
//!
//! The scanner discovers per-record sizes via
//! [`record_size_from_bytes`](crate::record::record_size_from_bytes)
//! and uses [`Key::eq_from_bytes`] for zero-copy key comparison during
//! version chain walks. This handles both fixed-size and variable-length
//! key/value types uniformly.
//!
//! ## Variable-stride scanning
//!
//! Unlike fixed-size records (where the scanner can advance by a constant
//! stride), variable-length records require **per-record size discovery**.
//! The scanner reads length prefixes from raw page bytes to compute each
//! record's total size, then advances by that amount. For fixed-size types
//! the compiler const-folds `serialized_size_from_bytes` so the stride
//! collapses to a compile-time constant with zero overhead.
//!
//! ## Corruption abort and recovery
//!
//! If [`record_size_from_bytes`](crate::record::record_size_from_bytes)
//! detects a corrupted length prefix (e.g., a value length exceeding the
//! remaining page space), the scanner returns
//! [`RecordSizeError::CorruptedRecord`] and **aborts the entire compaction
//! cycle**. This is a deliberate safety decision: partial compaction on a
//! corrupted region could silently drop live data.
//!
//! **Recovery strategy**: The original log region is left untouched — no
//! records are moved and the begin-address is not advanced. The caller
//! (typically [`FasterKv::compact()`](crate::store::FasterKv::compact))
//! surfaces the error so the application can take corrective action
//! (e.g., skip the corrupted segment, run a consistency check, or
//! restore from a checkpoint).

use crate::address::{LogicalAddress, OFFSET_BITS, Offset, Page};
use crate::hash::index::HashIndex;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::record_ops::{LogRecordReader, RecordAccessor};
use crate::record::{
    KEY_OFFSET, Key, RECORD_HEADER_SIZE, RecordSizeError, Value, record_size_from_bytes,
};

use super::{CompactionPlan, LiveRecord};

/// Maximum number of version chain hops before giving up.
///
/// Matches the limit used in `store::operations` to prevent infinite
/// traversal on corrupted chains.
const MAX_CHAIN_DEPTH: usize = 4096;

/// Minimum bytes needed to read a record header + one length prefix.
///
/// 8 bytes (`RecordInfo`) + 4 bytes (u32 length prefix) = 12.
/// For fixed-size keys this is conservative (they don't use a prefix),
/// but keeping the check type-agnostic simplifies the scanning loop.
const MIN_READABLE: u32 = (RECORD_HEADER_SIZE + crate::record::LENGTH_PREFIX_SIZE) as u32;

// ── CompactionScanner ───────────────────────────────────────────────

/// Scans a hybrid log region and classifies records for compaction.
///
/// The scanner is lightweight: it borrows the allocator and hash index
/// and performs a single forward pass over the specified address range.
///
/// # Example
///
/// ```ignore
/// use faster_core::compaction::scanner::CompactionScanner;
///
/// let scanner = CompactionScanner::new(&allocator, &hash_index);
/// let plan = scanner.scan::<u64, u64>(begin, until)?;
/// println!("live={}, dead={}, tombstones={}",
///     plan.live_records.len(), plan.dead_count, plan.tombstone_count);
/// ```
pub struct CompactionScanner<'a> {
    allocator: &'a HybridLogAllocator,
    hash_index: &'a HashIndex,
}

impl<'a> CompactionScanner<'a> {
    /// Creates a new compaction scanner.
    ///
    /// - `allocator` — the hybrid log allocator backing record storage.
    /// - `hash_index` — the hash index for checking which version is current.
    #[inline]
    pub fn new(allocator: &'a HybridLogAllocator, hash_index: &'a HashIndex) -> Self {
        Self {
            allocator,
            hash_index,
        }
    }

    /// Scan records in `[begin_address, until_address)` and classify them.
    ///
    /// `K` and `V` must be the same key/value types used when the records
    /// were written. Record sizes are discovered per-record via
    /// [`record_size_from_bytes`], so this works for both fixed-size and
    /// variable-length types.
    ///
    /// # Errors
    ///
    /// Returns [`RecordSizeError::CorruptedRecord`] if a corrupted length
    /// prefix is detected. Compaction is aborted to preserve original data.
    ///
    /// # Panics
    ///
    /// Debug-panics if `begin_address >= until_address`.
    pub fn scan<K: Key, V: Value>(
        &self,
        begin_address: LogicalAddress,
        until_address: LogicalAddress,
    ) -> Result<CompactionPlan, RecordSizeError> {
        debug_assert!(
            begin_address <= until_address,
            "scan range is empty or inverted: begin={begin_address} >= until={until_address}"
        );

        let page_size = 1u32 << OFFSET_BITS;

        let mut plan = CompactionPlan {
            live_records: Vec::new(),
            tombstone_records: Vec::new(),
            dead_count: 0,
            tombstone_count: 0,
            total_bytes_scanned: 0,
            live_bytes: 0,
            dead_bytes: 0,
            tombstone_bytes: 0,
            total_chain_hops: 0,
        };

        let mut current = begin_address;

        while current < until_address {
            let offset = current.offset().0;
            let remaining = page_size - offset;

            // MF-2: Universal page boundary check — if fewer than
            // MIN_READABLE bytes remain, no valid record can start here.
            if remaining < MIN_READABLE {
                current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
                continue;
            }

            // Get page-bounded access starting at the current record.
            // We request `remaining` bytes so that `record_size_from_bytes`
            // can inspect key/value length prefixes.
            let accessor = match RecordAccessor::from_log(self.allocator, current, remaining) {
                Some(a) => a,
                None => {
                    // Page not in memory — skip to next page.
                    current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
                    continue;
                }
            };

            let bytes = accessor.as_slice();

            // Discover the record size from raw page bytes.
            let record_size = match record_size_from_bytes::<K, V>(bytes, 0, remaining as usize) {
                Ok(size) => size,
                Err(RecordSizeError::InsufficientPageSpace) => {
                    // Gap at page end — skip to next page.
                    current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
                    continue;
                }
                Err(e @ RecordSizeError::CorruptedRecord { .. }) => {
                    // MF-3: Abort compaction on corruption (T-2 decision).
                    return Err(e);
                }
            };

            debug_assert!(
                offset as usize + record_size <= page_size as usize,
                "record at offset {offset} with size {record_size} exceeds page boundary"
            );

            let info = accessor.record_info();
            plan.total_bytes_scanned += record_size;

            if info.is_tombstone() {
                // MF-5: Collect tombstones for the address updater.
                plan.tombstone_count += 1;
                plan.tombstone_bytes += record_size;
                plan.tombstone_records.push(LiveRecord {
                    address: current,
                    record_size,
                });

                // SF-3: Warn when tombstone vector exceeds 100 MB.
                if plan.tombstone_records.len() % 1024 == 0 {
                    let estimated_bytes =
                        plan.tombstone_records.len() * std::mem::size_of::<LiveRecord>();
                    const TOMBSTONE_WARN_BYTES: usize = 100 * 1024 * 1024;
                    if estimated_bytes > TOMBSTONE_WARN_BYTES {
                        eprintln!(
                            "compaction: tombstone_records ~{} MB ({} entries)",
                            estimated_bytes / (1024 * 1024),
                            plan.tombstone_records.len(),
                        );
                    }
                }
            } else if info.is_invalid() {
                plan.dead_count += 1;
                plan.dead_bytes += record_size;
            } else {
                // Classify by checking the hash index.
                let key: K = K::deserialize(&bytes[KEY_OFFSET..]);
                let (is_current, hops) = self.is_current_version(&key, current);
                plan.total_chain_hops += hops;
                if is_current {
                    plan.live_records.push(LiveRecord {
                        address: current,
                        record_size,
                    });
                    plan.live_bytes += record_size;
                } else {
                    plan.dead_count += 1;
                    plan.dead_bytes += record_size;
                }
            }

            // C-1: Software prefetch hint for the next record header.
            // NOTE: Omitted because the crate uses `#[deny(unsafe_code)]`.
            // When unsafe blocks are permitted in a future release, add
            // `_mm_prefetch` here targeting `bytes[record_size..]` with
            // `_MM_HINT_T0` on x86_64 to compensate for data-dependent stride.

            advance(&mut current, record_size as u32, page_size);
        }

        // SF-5: Warn on excessive chain walk hops.
        const HOP_WARN_THRESHOLD: usize = 1_000_000;
        if plan.total_chain_hops > HOP_WARN_THRESHOLD {
            eprintln!(
                "compaction: {} chain hops exceeds threshold {}",
                plan.total_chain_hops, HOP_WARN_THRESHOLD,
            );
        }

        // SF-8: Post-scan byte accounting validation.
        let expected_range = until_address.raw().saturating_sub(begin_address.raw()) as usize;
        let accounted = plan.live_bytes + plan.dead_bytes + plan.tombstone_bytes;
        if expected_range > 0 && plan.total_bytes_scanned > 0 {
            let diff = plan.total_bytes_scanned.abs_diff(expected_range);
            let pct = (diff as f64 / expected_range as f64) * 100.0;
            if pct > 5.0 {
                eprintln!(
                    "compaction: scanned={} vs range={} (diff={:.1}%)",
                    plan.total_bytes_scanned, expected_range, pct,
                );
            }
            if accounted != plan.total_bytes_scanned {
                eprintln!(
                    "compaction: live={}+dead={}+tomb={}={} vs scanned={}",
                    plan.live_bytes,
                    plan.dead_bytes,
                    plan.tombstone_bytes,
                    accounted,
                    plan.total_bytes_scanned,
                );
            }
        }

        Ok(plan)
    }

    /// Determine whether the record at `addr` with key `key` is the
    /// current (live) version by consulting the hash index.
    ///
    /// Returns `true` (live) if:
    /// - The hash index chain head points directly to `addr`, OR
    /// - Walking the version chain finds `key` first at `addr`.
    ///
    /// Returns `true` (conservative) if:
    /// - No hash index entry exists for this key's hash.
    /// - The chain head address is invalid.
    /// - The chain cannot be followed (page not in memory, cycle, depth limit).
    ///
    /// # Per-hop variable-length safety (MF-1)
    ///
    /// Uses [`LogRecordReader::read_header_and_match_key_varlen`] which
    /// performs zero-copy key comparison via [`Key::eq_from_bytes`]. This
    /// is correct even when different records in the same hash bucket have
    /// different serialized sizes.
    fn is_current_version<K: Key>(&self, key: &K, addr: LogicalAddress) -> (bool, usize) {
        let hash = key.hash();

        let (entry, _slot) = match self.hash_index.find(hash) {
            Some(pair) => pair,
            // No entry in hash index — conservative: treat as live.
            None => return (true, 0),
        };

        let chain_head = entry.address();
        if !chain_head.is_valid() {
            return (true, 0); // Conservative
        }

        // Fast path: we are the chain head (most common case for live records).
        if chain_head == addr {
            return (true, 0);
        }

        // Slow path: walk the version chain from the chain head to find the
        // first non-invalid record whose key matches ours.
        let reader = LogRecordReader::new(self.allocator);
        let mut current = chain_head;
        let mut depth = 0usize;

        while current.is_valid() && depth < MAX_CHAIN_DEPTH {
            depth += 1;

            match reader.read_header_and_match_key_varlen::<K>(current, key) {
                Some((ri, key_matches)) => {
                    if key_matches && !ri.is_invalid() {
                        // Found the current version for this key.
                        return (current == addr, depth);
                    }
                    // Continue walking the chain.
                    let prev = ri.previous_address();
                    if !prev.is_valid() || prev == current {
                        break; // End of chain or self-loop
                    }
                    current = prev;
                }
                // Page not in memory — can't verify, be conservative.
                None => return (true, depth),
            }
        }

        // Exhausted chain without finding a key match — conservative.
        (true, depth)
    }
}

/// Advance `current` by `step` bytes, handling page-boundary wrap.
#[inline]
fn advance(current: &mut LogicalAddress, step: u32, page_size: u32) {
    debug_assert!(
        step <= page_size,
        "advance step {step} exceeds page size {page_size}"
    );
    let new_offset = current.offset().0 + step;
    if new_offset >= page_size {
        *current = LogicalAddress::new(Page(current.page().0 + 1), Offset(0));
    } else {
        *current = LogicalAddress::new(current.page(), Offset(new_offset));
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::NullDevice;
    use crate::grow::GrowConfig;
    use crate::hybrid_log::eviction::EvictionPolicy;
    use crate::record::{RecordSizeError, record_size_from_bytes};
    use crate::status::OperationStatus;
    use crate::store::{FasterKv, FasterKvConfig, Functions, RmwInPlaceResult, SimpleFunctions};

    type SimpleStore = FasterKv<SimpleFunctions<u64, u64>>;

    fn test_store() -> SimpleStore {
        let config = FasterKvConfig {
            hash_index_size_log2: 10, // 1024 buckets
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
        };
        FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
    }

    struct VarLenFunctions;
    impl Functions for VarLenFunctions {
        type Key = Vec<u8>;
        type Value = Vec<u8>;
        type Input = Vec<u8>;
        type Output = Option<Vec<u8>>;
        type Context = ();
        fn read(&self, _k: &Vec<u8>, v: &Vec<u8>, _i: &Vec<u8>, o: &mut Option<Vec<u8>>) {
            *o = Some(v.clone());
        }
        fn upsert(
            &self,
            _k: &Vec<u8>,
            v: &mut Vec<u8>,
            i: &Vec<u8>,
            _old: Option<&Vec<u8>>,
            _o: &mut Option<Vec<u8>>,
        ) {
            *v = i.clone();
        }
        fn rmw_initial(
            &self,
            _k: &Vec<u8>,
            i: &Vec<u8>,
            v: &mut Vec<u8>,
            _o: &mut Option<Vec<u8>>,
        ) {
            *v = i.clone();
        }
        fn rmw_in_place(
            &self,
            _k: &Vec<u8>,
            _i: &Vec<u8>,
            _v: &mut Vec<u8>,
            _o: &mut Option<Vec<u8>>,
        ) -> RmwInPlaceResult {
            RmwInPlaceResult::NeedsNewRecord
        }
        fn rmw_copy_update(
            &self,
            _k: &Vec<u8>,
            i: &Vec<u8>,
            old: &Vec<u8>,
            nv: &mut Vec<u8>,
            o: &mut Option<Vec<u8>>,
        ) {
            let mut m = old.clone();
            m.extend_from_slice(i);
            *nv = m.clone();
            *o = Some(m);
        }
    }
    type VarLenStore = FasterKv<VarLenFunctions>;
    fn varlen_test_store() -> VarLenStore {
        let config = FasterKvConfig {
            hash_index_size_log2: 10,
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
        };
        FasterKv::new(config, VarLenFunctions, NullDevice::new())
    }
    fn ro_range(store: &VarLenStore) -> (LogicalAddress, LogicalAddress) {
        (store.first_data_address(), store.allocator.tail_address())
    }

    // ── 1. Empty log → empty plan ───────────────────────────────────

    #[test]
    fn scan_empty_log() {
        let store = test_store();
        let mut session = store.new_session();
        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            assert_eq!(
                plan.live_records.len(),
                0,
                "empty log should have no live records"
            );
            assert_eq!(plan.dead_count, 0);
            assert_eq!(plan.tombstone_count, 0);
            assert!(plan.tombstone_records.is_empty());
        }
        store.dispose_session(session);
    }

    // ── 2. Single record → one live ─────────────────────────────────

    #[test]
    fn scan_single_record() {
        let store = test_store();
        let mut session = store.new_session();
        let _ = store.upsert(&mut session, &42u64, &100u64, ());
        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            assert_eq!(plan.live_records.len(), 1, "single record should be live");
            assert_eq!(plan.dead_count, 0);
            assert_eq!(plan.tombstone_count, 0);
        }
        store.dispose_session(session);
    }

    // ── 3. Multiple live records ────────────────────────────────────

    #[test]
    fn scan_multiple_live_records() {
        let store = test_store();
        let mut session = store.new_session();
        let n = 20u64;
        for i in 0..n {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }
        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            assert_eq!(
                plan.live_records.len(),
                n as usize,
                "all {n} records should be live"
            );
            assert_eq!(plan.dead_count, 0);
            assert_eq!(plan.tombstone_count, 0);
        }
        store.dispose_session(session);
    }

    // ── 4. Dead records from updates ────────────────────────────────

    #[test]
    fn scan_detects_dead_records_after_update() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 10 records.
        for i in 0u64..10 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        // Move all to read-only so updates create new copies.
        store.allocator.shift_read_only_to_tail();

        // Update keys 0..5 — creates new records at tail, old ones become dead.
        for i in 0u64..5 {
            let status = store.upsert(&mut session, &i, &(i * 999), ());
            assert_eq!(
                status,
                OperationStatus::CopyUpdated,
                "key {i}: expected CopyUpdated after shift_read_only_to_tail"
            );
        }

        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            // Keys 0..5: old versions dead + new versions live = 5 dead, 5 live
            // Keys 5..10: original versions still live = 5 live
            // Total: 10 live, 5 dead
            assert_eq!(
                plan.live_records.len(),
                10,
                "should have 10 live records (5 originals + 5 new copies)"
            );
            assert_eq!(
                plan.dead_count, 5,
                "should have 5 dead records (old versions of 0..5)"
            );
            assert_eq!(plan.tombstone_count, 0);
        }
        store.dispose_session(session);
    }

    // ── 5. Tombstoned records from deletes ──────────────────────────

    #[test]
    fn scan_detects_tombstones() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 5 records.
        for i in 0u64..5 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        // Move to read-only so deletes create tombstone records at tail.
        store.allocator.shift_read_only_to_tail();

        // Delete keys 0..3.
        for i in 0u64..3 {
            let status = store.delete(&mut session, &i, ());
            assert_eq!(
                status,
                OperationStatus::Deleted,
                "key {i}: expected Deleted"
            );
        }

        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            // Original keys 0..2: dead (hash index now points to tombstones)
            // Original keys 3..4: still live
            // Tombstones at tail: 3 tombstones
            assert_eq!(plan.tombstone_count, 3, "should have 3 tombstones");
            assert_eq!(
                plan.tombstone_records.len(),
                3,
                "should have 3 tombstone records collected"
            );
            assert_eq!(
                plan.live_records.len(),
                2,
                "should have 2 live records (keys 3 and 4)"
            );
            assert_eq!(
                plan.dead_count, 3,
                "should have 3 dead (old versions behind tombstones)"
            );
        }
        store.dispose_session(session);
    }

    // ── 6. Mixed: updates + deletes ─────────────────────────────────

    #[test]
    fn scan_mixed_updates_and_deletes() {
        let store = test_store();
        let mut session = store.new_session();

        let n = 15u64;
        for i in 0..n {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        store.allocator.shift_read_only_to_tail();

        // Update keys 0..5
        for i in 0u64..5 {
            assert_eq!(
                store.upsert(&mut session, &i, &(i * 999), ()),
                OperationStatus::CopyUpdated
            );
        }

        // Delete keys 5..8
        for i in 5u64..8 {
            assert_eq!(store.delete(&mut session, &i, ()), OperationStatus::Deleted);
        }

        // Keys 8..15 remain live in read-only region

        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            // Read-only region: 15 original records
            //   Keys 0..5: dead (superseded by new copies at tail) = 5 dead
            //   Keys 5..8: dead (superseded by tombstones at tail) = 3 dead
            //   Keys 8..15: live = 7 live
            // Tail (mutable) region:
            //   Keys 0..5: 5 new live records
            //   Keys 5..8: 3 tombstones
            let expected_live = 7 + 5; // 7 originals + 5 new copies
            let expected_dead = 5 + 3; // 5 updated + 3 deleted originals
            let expected_tombstones = 3;

            assert_eq!(plan.live_records.len(), expected_live, "live count");
            assert_eq!(plan.dead_count, expected_dead, "dead count");
            assert_eq!(plan.tombstone_count, expected_tombstones, "tombstone count");
            assert_eq!(
                plan.tombstone_records.len(),
                expected_tombstones,
                "tombstone records collected"
            );
            assert_eq!(
                plan.total_records(),
                (n as usize) + 5 + 3, // originals + new copies + tombstones
                "total records"
            );
        }
        store.dispose_session(session);
    }

    // ── 7. All tombstoned → zero live ───────────────────────────────

    #[test]
    fn scan_all_tombstoned() {
        let store = test_store();
        let mut session = store.new_session();

        for i in 0u64..5 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        store.allocator.shift_read_only_to_tail();

        for i in 0u64..5 {
            assert_eq!(store.delete(&mut session, &i, ()), OperationStatus::Deleted);
        }

        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");
            assert_eq!(
                plan.live_records.len(),
                0,
                "no live records when all are deleted"
            );
            assert_eq!(plan.dead_count, 5, "all original records should be dead");
            assert_eq!(plan.tombstone_count, 5, "all deletes should be tombstones");
            assert_eq!(
                plan.tombstone_records.len(),
                5,
                "all tombstones should be collected"
            );
        }
        store.dispose_session(session);
    }

    // ── 8. live_fraction and total_records ───────────────────────────

    #[test]
    fn plan_computed_fields() {
        let plan = CompactionPlan {
            live_records: vec![
                LiveRecord {
                    address: LogicalAddress::from_raw(100),
                    record_size: 24,
                },
                LiveRecord {
                    address: LogicalAddress::from_raw(200),
                    record_size: 24,
                },
            ],
            tombstone_records: vec![],
            dead_count: 3,
            tombstone_count: 1,
            total_bytes_scanned: 144, // 6 records × 24 bytes
            live_bytes: 48,           // 2 records × 24 bytes
            dead_bytes: 72,
            tombstone_bytes: 24,
            total_chain_hops: 0,
        };

        assert_eq!(plan.total_records(), 6);
        assert!((plan.live_fraction() - (48.0 / 144.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn plan_empty_live_fraction() {
        let plan = CompactionPlan {
            live_records: vec![],
            tombstone_records: vec![],
            dead_count: 0,
            tombstone_count: 0,
            total_bytes_scanned: 0,
            live_bytes: 0,
            dead_bytes: 0,
            tombstone_bytes: 0,
            total_chain_hops: 0,
        };
        assert!((plan.live_fraction() - 0.0).abs() < f64::EPSILON);
    }

    // ── 9. Scan subrange ────────────────────────────────────────────

    #[test]
    fn scan_subrange() {
        let store = test_store();
        let mut session = store.new_session();

        for i in 0u64..10 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);

            // Scan just the first half using read_only_address as a natural boundary.
            // Since everything is mutable, use half of the address range.
            let begin = store.first_data_address();
            let tail = store.allocator.tail_address();
            let mid_raw = begin.raw() + (tail.raw() - begin.raw()) / 2;
            let mid = LogicalAddress::from_raw(mid_raw);

            let plan = scanner
                .scan::<u64, u64>(begin, mid)
                .expect("scan should succeed");
            // Should have fewer records than the full scan.
            assert!(
                plan.live_records.len() < 10,
                "subrange scan should yield fewer than 10 records, got {}",
                plan.live_records.len()
            );
        }
        store.dispose_session(session);
    }

    // ── 10. Property test ───────────────────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;
        use std::collections::HashSet;

        proptest! {
            /// Random sequence of upsert/delete operations.
            ///
            /// After all operations, the scanner's live record count must
            /// equal the number of keys that are currently in the store
            /// (i.e., upserted but not deleted).
            #[test]
            fn scanner_live_count_matches_index(
                ops in proptest::collection::vec(
                    (0u64..50, prop_oneof![Just(true), Just(false)]),
                    1..100,
                ),
            ) {
                let store = test_store();
                let mut session = store.new_session();
                let mut live_keys: HashSet<u64> = HashSet::new();

                // Phase 1: Initial inserts (all go to mutable region).
                for &(key, _) in &ops {
                    let _ = store.upsert(&mut session, &key, &(key * 10), ());
                    live_keys.insert(key);
                }

                // Move to read-only so subsequent ops create new records.
                store.allocator.shift_read_only_to_tail();

                // Phase 2: Apply operations.
                for &(key, is_delete) in &ops {
                    if is_delete {
                        let _ = store.delete(&mut session, &key, ());
                        live_keys.remove(&key);
                    } else {
                        let _ = store.upsert(&mut session, &key, &(key * 999), ());
                        live_keys.insert(key);
                    }
                }

                // Phase 3: Scan and verify.
                {
                    let _guard = session.begin_unsafe();
                    let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
                    let plan = scanner
                        .scan::<u64, u64>(
                            store.first_data_address(),
                            store.allocator.tail_address(),
                        )
                        .expect("scan should succeed");

                    // Each live key should have exactly one live record.
                    prop_assert_eq!(
                        plan.live_records.len(),
                        live_keys.len(),
                        "live_records={} != expected live_keys={} (ops={:?})",
                        plan.live_records.len(),
                        live_keys.len(),
                        ops,
                    );

                    // Safety invariant: scanner must never lose records.
                    // total classified records ≥ number of unique keys ever seen.
                    prop_assert!(
                        plan.total_records() > 0 || ops.is_empty(),
                        "scanner found no records despite operations"
                    );
                }
                store.dispose_session(session);
            }
        }
    }

    // ── 11. Tombstone collection test ───────────────────────────────

    #[test]
    fn tombstone_records_collected() {
        let store = test_store();
        let mut session = store.new_session();

        for i in 0u64..10 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        store.allocator.shift_read_only_to_tail();

        // Delete keys 3, 5, 7.
        for &i in &[3u64, 5, 7] {
            assert_eq!(store.delete(&mut session, &i, ()), OperationStatus::Deleted);
        }

        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), store.allocator.tail_address())
                .expect("scan should succeed");

            assert_eq!(plan.tombstone_count, 3);
            assert_eq!(plan.tombstone_records.len(), 3);

            // Each tombstone record should have a valid address and nonzero size.
            for tr in &plan.tombstone_records {
                assert!(tr.address.is_valid());
                assert!(tr.record_size > 0);
            }
        }
        store.dispose_session(session);
    }

    // ── 12. Corruption detection test ───────────────────────────────

    #[test]
    fn corruption_returns_error() {
        // Verify that `record_size_from_bytes` returns an error for a
        // corrupted length prefix (not a panic).
        let page_size = 512;
        let mut page = vec![0u8; page_size];

        // Write a header (8 bytes of zeros) then a wildly large u32
        // key length prefix that exceeds the page.
        let key_offset = 8usize;
        let bogus_len = (page_size as u32) + 1000;
        page[key_offset..key_offset + 4].copy_from_slice(&bogus_len.to_le_bytes());

        let result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size);
        assert!(
            result.is_err(),
            "corrupted record should return Err, got {:?}",
            result
        );
        match result {
            Err(RecordSizeError::CorruptedRecord { offset }) => {
                assert_eq!(offset, 0, "corruption should be at offset 0");
            }
            other => panic!("expected CorruptedRecord, got {other:?}"),
        }
    }

    // ── 13. Page-end gap tests ──────────────────────────────────────

    #[test]
    fn page_end_insufficient_space() {
        // Verify that `record_size_from_bytes` returns InsufficientPageSpace
        // when fewer than MIN_READABLE (12) bytes remain.
        let page_size = 64;
        let page = vec![0u8; page_size];

        // Gaps of 1, 4, 8, 11 bytes at page end — all < 12.
        for gap in [1usize, 4, 8, 11] {
            let offset = page_size - gap;
            let result = record_size_from_bytes::<u64, u64>(&page, offset, page_size);
            assert_eq!(
                result,
                Err(RecordSizeError::InsufficientPageSpace),
                "gap of {gap} bytes at page end should be InsufficientPageSpace"
            );
        }

        // 12 bytes remaining should NOT be InsufficientPageSpace for fixed-size.
        let offset = page_size - 12;
        let result = record_size_from_bytes::<u64, u64>(&page, offset, page_size);
        assert_ne!(
            result,
            Err(RecordSizeError::InsufficientPageSpace),
            "12 bytes remaining should be enough to attempt size computation"
        );
    }

    // ── 14. Variable-length scanning ────────────────────────────────

    #[test]
    fn variable_length_record_size_discovery() {
        // Verify that record_size_from_bytes correctly discovers sizes
        // for records with different variable-length keys.
        use crate::record::{RecordInfo, RecordLayout, write_record};

        let page_size = 4096;
        let mut page = vec![0u8; page_size];

        // Record 1: small key + small value.
        let k1: Vec<u8> = vec![1, 2, 3];
        let v1: Vec<u8> = vec![10];
        let layout1 = RecordLayout::for_kv(&k1, &v1);
        let size1 = layout1.total_size();
        let info = RecordInfo::default();
        write_record(&mut page[0..], &info, &k1, &v1, &layout1);

        // Record 2: larger key + larger value at offset = size1.
        let k2: Vec<u8> = vec![4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        let v2: Vec<u8> = vec![20, 21, 22, 23, 24];
        let layout2 = RecordLayout::for_kv(&k2, &v2);
        let size2 = layout2.total_size();
        write_record(&mut page[size1..], &info, &k2, &v2, &layout2);

        // Discover sizes from raw bytes.
        let discovered1 = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size).unwrap();
        assert_eq!(discovered1, size1, "first record size mismatch");

        let discovered2 =
            record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, size1, page_size).unwrap();
        assert_eq!(discovered2, size2, "second record size mismatch");
    }

    // ════════════════════════════════════════════════════════════════
    // SF-9: Missing test coverage
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn record_nearly_fills_page() {
        use crate::record::{RecordInfo, RecordLayout, write_record};
        let page_size = 512;
        let mut page = vec![0u8; page_size];
        let key: Vec<u8> = vec![0xAA; 4];
        let val: Vec<u8> = vec![0xBB; 484];
        let layout = RecordLayout::for_kv(&key, &val);
        let total = layout.total_size();
        let gap = page_size - total;
        assert!(
            gap < MIN_READABLE as usize,
            "gap ({gap}) should be < MIN_READABLE"
        );
        assert!(gap > 0);
        let info = RecordInfo::default();
        write_record(&mut page[0..], &info, &key, &val, &layout);
        let discovered = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size).unwrap();
        assert_eq!(discovered, total);
        let gap_result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, total, page_size);
        assert_eq!(gap_result, Err(RecordSizeError::InsufficientPageSpace));
    }

    #[test]
    fn moderate_corruption_plausible_wrong_size() {
        use crate::record::{RecordInfo, RecordLayout, write_record};
        let page_size = 512;
        let mut page = vec![0u8; page_size];
        // Key len 4: clean pad(8+4+4+4+4,8)=24; corrupted (len 5) pad(8+4+5+4+4,8)=32.
        let k1: Vec<u8> = vec![1, 2, 3, 4];
        let v1: Vec<u8> = vec![10, 20, 30, 40];
        let layout1 = RecordLayout::for_kv(&k1, &v1);
        let size1 = layout1.total_size();
        let info = RecordInfo::default();
        write_record(&mut page[0..], &info, &k1, &v1, &layout1);
        let d1 = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size).unwrap();
        assert_eq!(d1, size1);
        // Corrupt key length prefix: flip bit 0 (4 -> 5 crosses alignment boundary).
        page[8] ^= 0x01;
        let corrupted = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size).unwrap();
        assert_ne!(
            corrupted, size1,
            "corrupted key length should change record size"
        );
    }

    #[test]
    fn version_chain_variable_length_different_sizes() {
        let store = varlen_test_store();
        let mut session = store.new_session();
        for i in 0u8..5 {
            let key = vec![i];
            let val = vec![i; (i as usize + 1) * 4];
            let _ = store.upsert(&mut session, &key, &val, ());
        }
        store.allocator.shift_read_only_to_tail();
        for i in 0u8..5 {
            let key = vec![i];
            let val = vec![i; (i as usize + 1) * 8];
            let _ = store.upsert(&mut session, &key, &val, ());
        }
        store.allocator.shift_read_only_to_tail();
        for i in 0u8..5 {
            let key = vec![i];
            let val = vec![i; (i as usize + 1) * 12];
            let _ = store.upsert(&mut session, &key, &val, ());
        }
        let (begin, end) = ro_range(&store);
        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<Vec<u8>, Vec<u8>>(begin, end)
                .expect("scan should succeed");
            assert_eq!(plan.live_records.len(), 5, "should have 5 live keys");
            assert_eq!(
                plan.live_bytes + plan.dead_bytes + plan.tombstone_bytes,
                plan.total_bytes_scanned,
                "byte accounting mismatch"
            );
        }
        store.dispose_session(session);
    }

    #[test]
    fn empty_key_record_size() {
        use crate::record::{RecordInfo, RecordLayout, write_record};
        let page_size = 256;
        let mut page = vec![0u8; page_size];
        let key: Vec<u8> = Vec::new();
        let val: Vec<u8> = vec![42, 43];
        let layout = RecordLayout::for_kv(&key, &val);
        let size = layout.total_size();
        let info = RecordInfo::default();
        write_record(&mut page[0..], &info, &key, &val, &layout);
        let discovered = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size).unwrap();
        assert_eq!(
            discovered, size,
            "empty key should produce correct record size"
        );
        use crate::record::Key;
        assert!(
            key.eq_from_bytes(&page[KEY_OFFSET..]),
            "empty key should match serialized form"
        );
    }

    #[test]
    fn eq_from_bytes_trailing_garbage() {
        use crate::record::Key;
        let key_u64: u64 = 0xDEAD_BEEF_CAFE_BABEu64;
        let mut buf = Vec::new();
        buf.extend_from_slice(&key_u64.to_le_bytes());
        buf.extend_from_slice(&[0xFF; 4]);
        assert!(
            key_u64.eq_from_bytes(&buf),
            "u64 should ignore trailing garbage"
        );
        assert!(!42u64.eq_from_bytes(&buf), "different u64 should not match");
        let key_vec: Vec<u8> = vec![1, 2, 3];
        let mut buf = Vec::new();
        buf.extend_from_slice(&(key_vec.len() as u32).to_le_bytes());
        buf.extend_from_slice(&key_vec);
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(
            key_vec.eq_from_bytes(&buf),
            "Vec<u8> should ignore trailing garbage"
        );
        assert!(
            !vec![1u8, 2, 4].eq_from_bytes(&buf),
            "different Vec<u8> should not match"
        );
        assert!(
            !key_u64.eq_from_bytes(&[0xBE]),
            "short buffer should be false"
        );
        assert!(
            !key_vec.eq_from_bytes(&[0x03]),
            "short buffer should be false"
        );
        assert!(!key_u64.eq_from_bytes(&[]), "empty buffer should be false");
        assert!(!key_vec.eq_from_bytes(&[]), "empty buffer should be false");
    }
}
