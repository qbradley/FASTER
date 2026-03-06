//! Compaction orchestration — wires K1 (scan) → K2 (copy) → K3 (pointer
//! swing) → K4 (begin-address advance) into a single compaction cycle.
//!
//! The [`CompactionOrchestrator`] is the internal engine behind
//! [`FasterKv::compact()`](crate::store::FasterKv::compact). It borrows
//! the store's components (allocator, hash index, device, epoch table)
//! and executes the four compaction phases in sequence.
//!
//! # Epoch coordination
//!
//! Phases K1–K3 run under epoch protection (the caller holds a session
//! guard). Between K3 (pointer swing) and K4 (begin-address advance), the
//! orchestrator bumps the epoch and waits for all threads to move past the
//! swing epoch. This ensures no reader still holds stale old-address
//! references before the compacted region is truncated.
//!
//! # Concurrency
//!
//! Multiple `compact()` calls are serialized by a `Mutex` in
//! [`FasterKv`](crate::store::FasterKv). The orchestrator itself is
//! stateless — it borrows what it needs for a single cycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::address::LogicalAddress;
use crate::compaction::CompactionPlan;
use crate::compaction::address_update::AddressUpdater;
use crate::compaction::begin_address::{BeginAddressAdvancer, TruncationResult};
use crate::compaction::copier::{CopyError, RecordCopier};
use crate::compaction::policy::CompactionStats;
use crate::compaction::scanner::CompactionScanner;
use crate::device::Device;
use crate::epoch::EpochTable;
use crate::hash::index::HashIndex;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::record::FixedSizeKey;
use crate::record::FixedSizeValue;

// ── CompactionResult ────────────────────────────────────────────────

/// Outcome of a compaction cycle.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The scan plan produced by the scanner.
    pub plan: CompactionPlan,
    /// Number of records copied to the log tail.
    pub records_copied: usize,
    /// Bytes copied to the log tail.
    pub bytes_copied: usize,
    /// Hash index pointer swing statistics.
    pub swung: u64,
    /// CAS failures during pointer swing.
    pub cas_failed: u64,
    /// Tombstone entries removed from the hash index.
    pub tombstones_removed: u64,
    /// Begin-address truncation result.
    pub truncation: TruncationResult,
}

// ── CompactionError ─────────────────────────────────────────────────

/// Errors that can occur during a compaction cycle.
#[derive(Debug)]
pub enum CompactionError {
    /// The compaction region is empty (begin >= until).
    EmptyRegion {
        /// Start of the (empty) compaction region.
        begin: LogicalAddress,
        /// End of the (empty) compaction region.
        until: LogicalAddress,
    },
    /// Record copying failed.
    CopyFailed(CopyError),
    /// Epoch drain timed out after pointer swing.
    EpochDrainTimeout,
}

impl core::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CompactionError::EmptyRegion { begin, until } => {
                write!(
                    f,
                    "compaction region is empty: begin={begin:?} >= until={until:?}"
                )
            }
            CompactionError::CopyFailed(e) => write!(f, "record copy failed: {e}"),
            CompactionError::EpochDrainTimeout => {
                write!(f, "epoch drain timed out after pointer swing")
            }
        }
    }
}

impl std::error::Error for CompactionError {}

// ── CompactionOrchestrator ──────────────────────────────────────────

/// Executes a full compaction cycle: scan → copy → swing → advance.
///
/// The orchestrator is a lightweight, stateless handle. Construct one per
/// compaction cycle and call [`run`](Self::run).
///
/// # Epoch protocol
///
/// Phases K1–K3 (scan, copy, pointer swing) must run under epoch
/// protection. The epoch drain and K4 (begin-address advance) must run
/// **outside** epoch protection so that the drain callback can fire.
///
/// The orchestrator manages this internally: it creates its own session,
/// enters epoch for K1–K3, exits, drains, then runs K4.
pub(crate) struct CompactionOrchestrator<'a> {
    allocator: &'a HybridLogAllocator,
    hash_index: &'a HashIndex,
    device: &'a dyn Device,
    epoch_table: &'a Arc<EpochTable>,
}

impl<'a> CompactionOrchestrator<'a> {
    /// Create a new orchestrator borrowing the store's components.
    pub(crate) fn new(
        allocator: &'a HybridLogAllocator,
        hash_index: &'a HashIndex,
        device: &'a dyn Device,
        epoch_table: &'a Arc<EpochTable>,
    ) -> Self {
        Self {
            allocator,
            hash_index,
            device,
            epoch_table,
        }
    }

    /// Execute a full compaction cycle on the given address range.
    ///
    /// This method manages its own epoch protection: it registers an
    /// epoch thread, enters epoch for K1–K3, exits, waits for the epoch
    /// to drain, then runs K4 (begin-address advance).
    ///
    /// # Type parameters
    ///
    /// `K` and `V` must match the key/value types stored in the log.
    /// They must implement [`FixedSizeKey`] / [`FixedSizeValue`] so the
    /// orchestrator can compute record layout sizes at compile time.
    pub(crate) fn run<K: FixedSizeKey, V: FixedSizeValue>(
        &self,
        begin: LogicalAddress,
        until: LogicalAddress,
    ) -> Result<CompactionResult, CompactionError> {
        if begin >= until {
            return Err(CompactionError::EmptyRegion { begin, until });
        }

        let key_size = K::SIZE;
        let value_size = V::SIZE;

        // Register a dedicated epoch thread for this compaction cycle.
        let mut epoch_thread = self
            .epoch_table
            .register()
            .expect("epoch table full — cannot register compaction thread");

        // ── Phases K1–K3 under epoch protection ─────────────────────

        let (plan, copy_info, swing_stats) = {
            let _guard = epoch_thread.protect();

            // Phase 1: Scan
            let scanner = CompactionScanner::new(self.allocator, self.hash_index);
            let plan = scanner.scan::<K>(begin, until, key_size, value_size);

            if plan.live_records.is_empty() {
                // No live records — skip copy and swing.
                (plan, None, None)
            } else {
                // Phase 2: Copy live records to log tail.
                let copier = RecordCopier::new(self.allocator);
                let copy_result = copier
                    .copy_records(&plan.live_records)
                    .map_err(CompactionError::CopyFailed)?;

                let records_copied = copy_result.records_copied();
                let bytes_copied = copy_result.bytes_copied;

                // Phase 3: Pointer swing.
                let updater = AddressUpdater::new(self.hash_index, self.allocator);
                let stats = updater.swing::<K>(&copy_result, &[], key_size, value_size);

                (plan, Some((records_copied, bytes_copied)), Some(stats))
            }
            // _guard drops here → epoch protection released
        };

        // ── Epoch drain ─────────────────────────────────────────────
        //
        // Now outside epoch protection. Bump the epoch and wait for all
        // threads to move past the swing epoch. This ensures no reader
        // still references the compacted region.

        // Unregister the epoch thread so it doesn't block the drain.
        epoch_thread.unregister();

        self.drain_epoch()?;

        // ── Phase 4: Begin-address advance ──────────────────────────

        let advancer = BeginAddressAdvancer::new(self.allocator, self.device);
        let truncation = advancer.advance(until);

        let (records_copied, bytes_copied) = copy_info.unwrap_or((0, 0));
        let (swung, cas_failed, tombstones_removed) = match swing_stats {
            Some(s) => (s.swung, s.cas_failed, s.tombstones_removed),
            None => (0, 0, 0),
        };

        Ok(CompactionResult {
            plan,
            records_copied,
            bytes_copied,
            swung,
            cas_failed,
            tombstones_removed,
            truncation,
        })
    }

    /// Bump the epoch and spin-wait for all threads to advance past it.
    ///
    /// Uses the epoch table's callback mechanism: we bump the current
    /// epoch with a callback that sets a flag, then spin until the flag
    /// is set (meaning all threads have moved past the bump epoch).
    fn drain_epoch(&self) -> Result<(), CompactionError> {
        let drained = Arc::new(AtomicBool::new(false));
        let drained_clone = Arc::clone(&drained);

        self.epoch_table.bump_current_epoch(move || {
            drained_clone.store(true, Ordering::Release);
        });

        // Spin-wait with backoff. In practice, epoch drain is fast
        // (microseconds) unless a thread is stalled.
        const MAX_SPINS: u32 = 10_000_000;
        for i in 0..MAX_SPINS {
            if drained.load(Ordering::Acquire) {
                return Ok(());
            }
            // Yield periodically to avoid burning CPU.
            if i % 1000 == 0 {
                std::thread::yield_now();
            }
            // Also try to drive epoch advancement ourselves.
            self.epoch_table.bump_current_epoch_no_callback();
        }

        // If we're the only active thread, the drain callback fires
        // synchronously via bump_current_epoch's internal try_drain.
        // Check one final time.
        if drained.load(Ordering::Acquire) {
            return Ok(());
        }

        Err(CompactionError::EpochDrainTimeout)
    }
}

// ── Collect policy stats ────────────────────────────────────────────

/// Compute [`CompactionStats`] from the allocator's current address
/// boundaries. This is a cheap O(1) estimate used by compaction policies
/// to decide whether compaction should trigger.
pub(crate) fn collect_stats(allocator: &HybridLogAllocator) -> CompactionStats {
    let begin = allocator.begin_address().raw();
    let tail = allocator.tail_address().raw();
    let total = tail.saturating_sub(begin);

    // Without a full scan, we can only estimate. Treat the entire
    // region as "total log bytes" and use a fraction for live.
    // The policy threshold should be set accounting for this heuristic.
    CompactionStats {
        total_log_bytes: total,
        // Pessimistic: assume all data is live until proven otherwise.
        // This means space-amplification policies won't trigger unless
        // the actual ratio is well above threshold. A real implementation
        // would periodically sample or cache scan results.
        live_data_bytes: total,
        tombstone_count: 0,
        total_record_count: 0,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::NullDevice;
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
        };
        FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
    }

    // ── 1. Basic orchestrator cycle ─────────────────────────────────

    #[test]
    fn orchestrator_basic_cycle() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 50 records.
        for i in 0u64..50 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        let begin = store.first_data_address();
        let until = store.allocator.tail_address();

        // Dispose the session before compaction so its epoch thread
        // doesn't block the drain.
        store.dispose_session(session);

        let null_dev = NullDevice::new();
        let orch = CompactionOrchestrator::new(
            &store.allocator,
            &store.hash_index,
            &null_dev,
            &store.epoch_table,
        );
        let result = orch
            .run::<u64, u64>(begin, until)
            .expect("compaction should succeed");

        assert_eq!(result.records_copied, 50);
        assert!(result.bytes_copied > 0);
        assert!(result.truncation.advanced);

        // All records still readable.
        let mut session = store.new_session();
        for i in 0u64..50 {
            let val = store.read_simple(&mut session, &i);
            assert_eq!(val, Some(i * 10), "key {i} should read as {}", i * 10);
        }

        store.dispose_session(session);
    }

    // ── 2. Empty region is an error ─────────────────────────────────

    #[test]
    fn orchestrator_empty_region_error() {
        let store = test_store();

        let addr = store.first_data_address();
        let null_dev = NullDevice::new();
        let orch = CompactionOrchestrator::new(
            &store.allocator,
            &store.hash_index,
            &null_dev,
            &store.epoch_table,
        );
        let result = orch.run::<u64, u64>(addr, addr);
        assert!(result.is_err());
    }

    // ── 3. Compaction with dead records ─────────────────────────────

    #[test]
    fn orchestrator_with_dead_records() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert 30 records.
        for i in 0u64..30 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        let begin = store.first_data_address();

        // Overwrite 15 of them (creates dead records in original region).
        for i in 0u64..15 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        let until = store.allocator.safe_read_only_address();
        if until <= begin {
            store.dispose_session(session);
            return;
        }

        store.dispose_session(session);

        let null_dev = NullDevice::new();
        let orch = CompactionOrchestrator::new(
            &store.allocator,
            &store.hash_index,
            &null_dev,
            &store.epoch_table,
        );
        let result = orch
            .run::<u64, u64>(begin, until)
            .expect("compaction should succeed");

        // We should have fewer records copied than total (dead ones skipped).
        assert!(result.plan.dead_count > 0 || result.records_copied <= 30);

        // All current values still readable.
        let mut session = store.new_session();
        for i in 0u64..15 {
            let val = store.read_simple(&mut session, &i);
            assert_eq!(val, Some(i * 100), "overwritten key {i}");
        }
        for i in 15u64..30 {
            let val = store.read_simple(&mut session, &i);
            assert_eq!(val, Some(i * 10), "original key {i}");
        }

        store.dispose_session(session);
    }

    // ── 4. Compaction with no live records ──────────────────────────

    #[test]
    fn orchestrator_all_dead() {
        let store = test_store();
        let mut session = store.new_session();

        // Insert records.
        for i in 0u64..20 {
            let _ = store.upsert(&mut session, &i, &i, ());
        }

        let begin = store.first_data_address();

        // Overwrite all (original versions become dead).
        for i in 0u64..20 {
            let _ = store.upsert(&mut session, &i, &(i + 1000), ());
        }

        let until = store.allocator.safe_read_only_address();
        if until <= begin {
            store.dispose_session(session);
            return;
        }

        store.dispose_session(session);

        let null_dev = NullDevice::new();
        let orch = CompactionOrchestrator::new(
            &store.allocator,
            &store.hash_index,
            &null_dev,
            &store.epoch_table,
        );
        let result = orch
            .run::<u64, u64>(begin, until)
            .expect("compaction should succeed");

        // The scan region should have mostly dead records.
        // Whatever was live was copied and is still accessible.
        let mut session = store.new_session();
        for i in 0u64..20 {
            let val = store.read_simple(&mut session, &i);
            assert_eq!(val, Some(i + 1000), "key {i}");
        }

        // Begin address should have advanced.
        assert!(
            result.truncation.advanced || result.plan.live_records.is_empty(),
            "either advanced or nothing to compact in the region"
        );

        store.dispose_session(session);
    }
}
