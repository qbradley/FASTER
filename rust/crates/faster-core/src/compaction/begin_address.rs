//! Begin-address advance and device truncation after compaction.
//!
//! After the pointer swing (K3) completes and the epoch drains (ensuring
//! no readers hold references to old addresses), the compacted region
//! can be reclaimed:
//!
//! 1. **Advance `begin_address`** past the compacted region so that
//!    lookups recognise those addresses as truncated.
//! 2. **Truncate the device** below the new begin-address, freeing
//!    on-disk storage.
//!
//! This module provides [`BeginAddressAdvancer`], which performs both
//! steps and returns a [`TruncationResult`] summarising the operation.
//!
//! # Safety
//!
//! The caller **must** ensure that:
//! - The pointer swing has completed for all records in the compaction region.
//! - The epoch has drained so that no readers hold old-address references.
//! - The head address is at or above the new begin address (i.e., pages have
//!   been evicted from memory before truncation).
//!
//! Violating these preconditions does not cause memory unsafety (no `unsafe`
//! code here), but can lead to lost data or stale reads.

use crate::address::{LogicalAddress, OFFSET_BITS};
use crate::device::Device;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
#[allow(unused_imports)]
use crate::sim_hooks::crash_point;

// ── TruncationResult ────────────────────────────────────────────────

/// Result of advancing begin-address and truncating the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TruncationResult {
    /// The old begin-address before advancement.
    pub old_begin: LogicalAddress,
    /// The new begin-address after advancement.
    pub new_begin: LogicalAddress,
    /// Whether the begin-address actually changed.
    pub advanced: bool,
    /// Whether device truncation was performed.
    pub truncated: bool,
    /// The device offset (bytes) up to which truncation was performed.
    pub truncated_until_offset: u64,
}

// ── BeginAddressAdvancer ────────────────────────────────────────────

/// Advances the log's begin-address and truncates reclaimed device
/// segments after a successful compaction cycle.
///
/// # Usage
///
/// ```ignore
/// use faster_core::compaction::begin_address::BeginAddressAdvancer;
///
/// let advancer = BeginAddressAdvancer::new(&allocator, &*device);
/// let result = advancer.advance(new_begin);
/// println!("advanced: {}, truncated: {}", result.advanced, result.truncated);
/// ```
pub struct BeginAddressAdvancer<'a> {
    allocator: &'a HybridLogAllocator,
    device: &'a dyn Device,
}

impl<'a> BeginAddressAdvancer<'a> {
    /// Creates a new advancer.
    ///
    /// - `allocator` — the hybrid log allocator whose begin-address will
    ///   be advanced.
    /// - `device` — the storage device to truncate.
    #[inline]
    pub fn new(allocator: &'a HybridLogAllocator, device: &'a dyn Device) -> Self {
        Self { allocator, device }
    }

    /// Advance begin-address to `new_begin` and truncate the device.
    ///
    /// If `new_begin` is not greater than the current begin-address, this
    /// is a no-op and returns a result with `advanced = false`.
    ///
    /// The device is truncated up to the byte offset corresponding to
    /// `new_begin`. This frees all device segments below that point.
    ///
    /// # Monotonicity
    ///
    /// `begin_address` only moves forward. If `new_begin` is below the
    /// current begin, the call succeeds but has no effect.
    pub fn advance(&self, new_begin: LogicalAddress) -> TruncationResult {
        let old_begin = self.allocator.begin_address();

        // Attempt to advance. try_advance_begin uses a CAS loop internally
        // and returns the actual begin after the attempt.
        let actual = self.allocator.try_advance_begin(new_begin);
        let advanced = actual.raw() > old_begin.raw();

        if !advanced {
            return TruncationResult {
                old_begin,
                new_begin: old_begin,
                advanced: false,
                truncated: false,
                truncated_until_offset: 0,
            };
        }

        crash_point!("compaction_begin_address_advanced");

        // Compute the device offset for the new begin-address.
        // Device offsets are linear: page * page_size + offset.
        let page_size = 1u64 << OFFSET_BITS;
        let device_offset = actual.page().0 as u64 * page_size + actual.offset().0 as u64;

        // Truncate device segments below the new begin.
        self.device.truncate_until(device_offset);

        TruncationResult {
            old_begin,
            new_begin: actual,
            advanced: true,
            truncated: true,
            truncated_until_offset: device_offset,
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Offset, Page};
    use crate::compaction::address_update::AddressUpdater;
    use crate::compaction::copier::RecordCopier;
    use crate::compaction::scanner::CompactionScanner;
    use crate::device::{InMemoryDevice, NullDevice};
    use crate::grow::GrowConfig;
    use crate::hybrid_log::eviction::EvictionPolicy;
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
            lossy: false,
        };
        FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
    }

    // ── 1. Advance begin-address after compaction ───────────────────

    #[test]
    fn advance_begin_address_after_compaction() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert records.
        for i in 0u64..50 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        let original_begin = store.allocator.begin_address();
        let scan_until = store.allocator.tail_address();

        // Scan, copy, swing.
        let _copy_result = {
            let _guard = session.begin_unsafe();

            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), scan_until)
                .expect("scan should succeed");

            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();

            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            updater.swing::<u64, u64>(&copy_result, &plan);

            copy_result
        };

        // Advance begin-address past the compacted region.
        let null_dev = NullDevice::new();
        let advancer = BeginAddressAdvancer::new(&store.allocator, &null_dev);
        let result = advancer.advance(scan_until);

        assert!(result.advanced, "begin address should have advanced");
        assert_eq!(result.old_begin, original_begin);
        assert_eq!(result.new_begin, scan_until);
        assert!(result.truncated);
        assert!(result.truncated_until_offset > 0);

        // Verify begin_address was actually updated.
        assert_eq!(store.allocator.begin_address(), scan_until);

        // Data still accessible via new addresses.
        for i in 0u64..50 {
            let result = store.read_simple(&mut session, &i);
            assert_eq!(result, Some(i * 10), "key {i} should still be readable");
        }

        store.dispose_session(session);
    }

    // ── 2. Old addresses are no longer accessible ───────────────────

    #[test]
    fn old_addresses_not_accessible_after_advance() {
        let store = test_store();
        let mut session = store.new_session();

        for i in 0u64..20 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        let scan_until = store.allocator.tail_address();

        // Record old addresses.
        let old_addresses: Vec<LogicalAddress> = {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), scan_until)
                .expect("scan should succeed");
            plan.live_records.iter().map(|r| r.address).collect()
        };

        // Full compaction cycle: scan, copy, swing, advance.
        {
            let _guard = session.begin_unsafe();
            let scanner = CompactionScanner::new(&store.allocator, &store.hash_index);
            let plan = scanner
                .scan::<u64, u64>(store.first_data_address(), scan_until)
                .expect("scan should succeed");
            let copier = RecordCopier::new(&store.allocator);
            let copy_result = copier.copy_records(&plan.live_records).unwrap();
            let updater = AddressUpdater::new(&store.hash_index, &store.allocator);
            updater.swing::<u64, u64>(&copy_result, &plan);
        }

        let null_dev2 = NullDevice::new();
        let advancer = BeginAddressAdvancer::new(&store.allocator, &null_dev2);
        advancer.advance(scan_until);

        // Old addresses are below begin — they should be classified as truncated.
        let begin = store.allocator.begin_address();
        for old_addr in &old_addresses {
            assert!(
                old_addr.raw() < begin.raw(),
                "old address {old_addr:?} should be below new begin {begin:?}"
            );
        }

        store.dispose_session(session);
    }

    // ── 3. Device segments truncated ────────────────────────────────

    #[test]
    fn device_truncation() {
        use std::sync::Arc;

        let device = Arc::new(InMemoryDevice::new());

        // Write some data to the device to simulate flushed pages.
        let page_size = 1u64 << OFFSET_BITS;
        let data = vec![0xABu8; page_size as usize * 2];
        device.write_sync(0, &data).unwrap();
        assert_eq!(device.size(), page_size * 2);

        // Create allocator and advance begin.
        let config = FasterKvConfig {
            hash_index_size_log2: 10,
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
            lossy: false,
        };
        let store: SimpleStore =
            FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());

        let advancer = BeginAddressAdvancer::new(&store.allocator, &*device);
        let new_begin = LogicalAddress::new(Page(1), Offset(0));
        let result = advancer.advance(new_begin);

        assert!(result.advanced);
        assert!(result.truncated);
        assert_eq!(result.truncated_until_offset, page_size);

        // Verify truncated region is zeroed.
        let mut buf = vec![0u8; page_size as usize];
        device.read_sync(0, &mut buf).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0),
            "truncated region should be zeroed"
        );

        // Data after truncation point should still be present.
        let mut buf2 = vec![0u8; page_size as usize];
        device.read_sync(page_size, &mut buf2).unwrap();
        assert!(
            buf2.iter().all(|&b| b == 0xAB),
            "data after truncation should be intact"
        );
    }

    // ── 4. No-op when new_begin <= current begin ────────────────────

    #[test]
    fn advance_noop_when_not_ahead() {
        let store = test_store();

        let current = store.allocator.begin_address();
        let null_dev3 = NullDevice::new();
        let advancer = BeginAddressAdvancer::new(&store.allocator, &null_dev3);

        // Same address — no-op.
        let result = advancer.advance(current);
        assert!(!result.advanced);
        assert!(!result.truncated);
        assert_eq!(result.old_begin, current);
        assert_eq!(result.new_begin, current);

        // Below current — also no-op (begin is at 0, nothing below).
        let result = advancer.advance(LogicalAddress::ZERO);
        assert!(!result.advanced);
    }

    // ── 5. Monotonic advancement ────────────────────────────────────

    #[test]
    fn monotonic_advancement() {
        let store = test_store();
        let null_dev4 = NullDevice::new();
        let advancer = BeginAddressAdvancer::new(&store.allocator, &null_dev4);

        // First advance to page 1.
        let r1 = advancer.advance(LogicalAddress::new(Page(1), Offset(0)));
        assert!(r1.advanced);
        assert_eq!(r1.new_begin, LogicalAddress::new(Page(1), Offset(0)));

        // Second advance to page 2.
        let r2 = advancer.advance(LogicalAddress::new(Page(2), Offset(0)));
        assert!(r2.advanced);
        assert_eq!(r2.old_begin, LogicalAddress::new(Page(1), Offset(0)));
        assert_eq!(r2.new_begin, LogicalAddress::new(Page(2), Offset(0)));

        // Attempted backward advance — no-op.
        let r3 = advancer.advance(LogicalAddress::new(Page(1), Offset(0)));
        assert!(!r3.advanced);
    }
}
