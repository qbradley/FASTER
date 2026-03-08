//! Batch operation API with two-level prefetch pipeline for FASTER sessions.
//!
//! This module provides high-throughput batch operations that amortize
//! per-operation overhead. A batch:
//!
//! 1. Enters epoch protection **once** for the entire batch (not per-op).
//! 2. Uses a **two-level prefetch pipeline** to overlap memory fetches:
//!    - **L1**: Hash bucket prefetch — issued `L1_WINDOW` operations ahead.
//!    - **L2**: Record data prefetch — issued `L2_WINDOW` operations ahead,
//!      after L1 has resolved the hash entry.
//! 3. Processes all operations in sequence under a single epoch region.
//! 4. Dispatches any pending I/O and refreshes the epoch at configurable
//!    intervals.
//!
//! This can improve throughput by 30–50% compared to individual calls,
//! especially for larger datasets that exceed L3 cache where the
//! pointer chase (hash bucket → record) causes two serial cache misses.
//!
//! # API Styles
//!
//! Two styles are provided:
//!
//! - **Homogeneous batches** — `batch_read`, `batch_upsert`, `batch_rmw`,
//!   `batch_delete` on [`UnsafeContext`] accept parallel slices of keys/values.
//! - **Mixed batches** — `batch_execute` accepts a slice of [`BatchOp`]
//!   entries, each carrying its own operation type, key, and input.
//!
//! # Partial Failure
//!
//! Batches never fail atomically — each operation independently succeeds,
//! goes pending, or returns not-found. The [`BatchResult`] struct reports
//! per-operation [`OperationStatus`] values and aggregate counts.
//!
//! [`UnsafeContext`]: super::UnsafeContext
//! [`HashIndex::prefetch()`]: crate::hash::index::HashIndex::prefetch

use crate::address::LogicalAddress;
use crate::hash::Hashable;
use crate::hash::KeyHash;
use crate::hash::bucket::HashBucketEntry;
use crate::status::OperationStatus;

/// Number of operations between epoch refreshes within a batch.
///
/// Holding epoch protection too long stalls the safe-epoch from advancing.
/// This constant balances throughput (fewer refreshes) against epoch
/// responsiveness (more refreshes). 256 is a good default — each refresh
/// costs ~50 ns, amortized over 256 ops that's < 0.2 ns per op.
pub(crate) const BATCH_REFRESH_INTERVAL: usize = 256;

/// Returns the epoch refresh interval for batch operations.
#[inline]
pub(crate) const fn refresh_interval() -> usize {
    BATCH_REFRESH_INTERVAL
}

// ── BatchResult ─────────────────────────────────────────────────────

/// Per-operation results from a batch execution.
///
/// Each entry in [`statuses`](Self::statuses) corresponds positionally to the
/// input operation at the same index.
#[derive(Debug, Clone)]
pub struct BatchResult {
    /// Per-operation status, in the same order as the input batch.
    pub statuses: Vec<OperationStatus>,
}

impl BatchResult {
    /// Creates an empty `BatchResult` with pre-allocated capacity.
    #[inline]
    pub(crate) fn with_capacity(n: usize) -> Self {
        Self {
            statuses: Vec::with_capacity(n),
        }
    }

    /// Total number of operations in the batch.
    #[inline]
    pub fn total(&self) -> usize {
        self.statuses.len()
    }

    /// Number of operations that completed successfully (non-pending, non-error).
    #[inline]
    pub fn succeeded_count(&self) -> usize {
        self.statuses.iter().filter(|s| s.is_success()).count()
    }

    /// Number of operations that went pending (need async I/O).
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.statuses
            .iter()
            .filter(|s| **s == OperationStatus::Pending)
            .count()
    }

    /// Number of operations that returned `NotFound`.
    #[inline]
    pub fn not_found_count(&self) -> usize {
        self.statuses
            .iter()
            .filter(|s| **s == OperationStatus::NotFound)
            .count()
    }

    /// Returns `true` if every operation completed successfully
    /// (no pending, no not-found, no aborted).
    #[inline]
    pub fn all_succeeded(&self) -> bool {
        self.statuses.iter().all(|s| s.is_success())
    }

    /// Returns `true` if any operation went pending.
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.statuses
            .iter()
            .any(|s| *s == OperationStatus::Pending)
    }

    /// Returns `true` if the batch was empty (zero operations).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.statuses.is_empty()
    }
}

// ── BatchOp ─────────────────────────────────────────────────────────

/// A single operation within a mixed batch.
///
/// Used with `UnsafeContext::batch_execute` to submit heterogeneous
/// operations (reads, upserts, RMWs, deletes) in a single
/// prefetch-optimized batch.
#[derive(Debug, Clone)]
pub enum BatchOp<K, I> {
    /// Read the value for `key`.
    Read {
        /// The key to look up.
        key: K,
        /// Input passed to the `Functions::read` callback.
        input: I,
    },

    /// Insert or update `key` with `input`.
    Upsert {
        /// The key to insert or update.
        key: K,
        /// Input passed to the `Functions::upsert` callback.
        input: I,
    },

    /// Read-modify-write `key` with `input`.
    Rmw {
        /// The key to modify.
        key: K,
        /// Modification input (e.g., delta for counters).
        input: I,
    },

    /// Delete `key`.
    Delete {
        /// The key to delete.
        key: K,
    },
}

impl<K: Hashable, I> BatchOp<K, I> {
    /// Returns the key for this operation.
    #[inline]
    pub fn key(&self) -> &K {
        match self {
            BatchOp::Read { key, .. } => key,
            BatchOp::Upsert { key, .. } => key,
            BatchOp::Rmw { key, .. } => key,
            BatchOp::Delete { key } => key,
        }
    }

    /// Computes the [`KeyHash`] for this operation's key.
    #[inline]
    pub fn key_hash(&self) -> KeyHash {
        self.key().hash()
    }
}

// ── Hash-and-prefetch helpers ───────────────────────────────────────

/// Hash all keys and issue L1 prefetch hints for their hash buckets.
///
/// Returns the computed hashes for reuse during operation processing.
#[inline]
pub(crate) fn hash_and_prefetch_keys<K: Hashable>(
    keys: &[K],
    hash_index: &crate::hash::index::HashIndex,
) -> Vec<KeyHash> {
    let mut hashes = Vec::with_capacity(keys.len());
    for key in keys {
        let h = key.hash();
        hash_index.prefetch(h);
        hashes.push(h);
    }
    hashes
}

/// Hash all operations and issue L1 prefetch hints for their hash buckets.
#[inline]
pub(crate) fn hash_and_prefetch_ops<K: Hashable, I>(
    ops: &[BatchOp<K, I>],
    hash_index: &crate::hash::index::HashIndex,
) -> Vec<KeyHash> {
    let mut hashes = Vec::with_capacity(ops.len());
    for op in ops {
        let h = op.key_hash();
        hash_index.prefetch(h);
        hashes.push(h);
    }
    hashes
}

// ── Two-Level Prefetch Pipeline ─────────────────────────────────────

/// Default number of operations in the L1 prefetch look-ahead window.
///
/// Hash-and-prefetch for operations this many positions ahead of the
/// current execution cursor. The gap gives DRAM ~100-200ns to service
/// the bucket fetch before we need the data.
pub const DEFAULT_L1_WINDOW: usize = 8;

/// Default number of operations in the L2 prefetch look-ahead window.
///
/// After L1 has fetched the hash bucket and we\'ve resolved the record
/// address, this window issues prefetches for the record data itself.
/// Must be smaller than L1 window (L2 data depends on L1 completing).
pub const DEFAULT_L2_WINDOW: usize = 4;

/// Resolved hash bucket entry for the pipeline\'s L2 stage.
#[allow(dead_code)]
pub(crate) struct ResolvedEntry {
    /// The hash bucket entry found (or EMPTY if not found).
    pub entry: HashBucketEntry,
    /// Logical address from the entry.
    pub addr: LogicalAddress,
}

impl ResolvedEntry {
    pub const EMPTY: Self = Self {
        entry: HashBucketEntry::EMPTY,
        addr: LogicalAddress::INVALID,
    };

    #[inline]
    pub fn found(&self) -> bool {
        self.entry != HashBucketEntry::EMPTY && self.addr.is_valid()
    }
}

/// Two-level prefetch pipeline state.
///
/// Manages the sliding-window prefetch pipeline for batch operations.
/// The pipeline has three stages:
///
/// 1. **L1 (hash bucket)**: Compute hash + issue `prefetch_bucket` for
///    operations `l1_window` positions ahead of the execution cursor.
/// 2. **L2 (record data)**: Look up the hash entry + issue record
///    prefetch for operations `l2_window` positions ahead.
/// 3. **Execute**: Process the operation with both bucket and record
///    data (ideally) already in L1 cache.
pub(crate) struct PrefetchPipeline {
    /// Computed key hashes — filled during L1 stage.
    pub hashes: Vec<KeyHash>,
    /// Resolved entries — filled during L2 stage.
    pub resolved: Vec<ResolvedEntry>,
    /// L1 look-ahead distance.
    pub l1_window: usize,
    /// L2 look-ahead distance (must be < l1_window).
    pub l2_window: usize,
}

impl PrefetchPipeline {
    /// Create a new pipeline for `n` operations.
    pub fn new(n: usize, l1_window: usize, l2_window: usize) -> Self {
        let mut resolved = Vec::with_capacity(n);
        for _ in 0..n {
            resolved.push(ResolvedEntry::EMPTY);
        }
        Self {
            hashes: Vec::with_capacity(n),
            resolved,
            l1_window,
            l2_window,
        }
    }

    /// L1 stage: hash the key and prefetch the bucket.
    #[inline]
    pub fn issue_l1<K: Hashable>(
        &mut self,
        idx: usize,
        key: &K,
        hash_index: &crate::hash::index::HashIndex,
    ) {
        if idx < self.hashes.len() {
            return; // Already issued
        }
        let h = key.hash();
        hash_index.prefetch(h);
        self.hashes.push(h);
    }

    /// L2 stage: resolve the hash entry and prefetch the record.
    #[inline]
    pub fn issue_l2(
        &mut self,
        idx: usize,
        hash_index: &crate::hash::index::HashIndex,
        allocator: &crate::hybrid_log::log_allocator::HybridLogAllocator,
        is_write: bool,
    ) {
        if idx >= self.hashes.len() || self.resolved[idx].found() {
            return;
        }
        let h = self.hashes[idx];
        if let Some((entry, _slot)) = hash_index.find(h) {
            let addr = entry.address();
            if addr.is_valid() {
                if let Some(ptr) = allocator.get_physical_address(addr) {
                    if is_write {
                        crate::hash::prefetch::prefetch_read(ptr);
                    } else {
                        crate::hash::prefetch::prefetch_read(ptr as *const u8);
                    }
                }
            }
            self.resolved[idx] = ResolvedEntry { entry, addr };
        }
    }

    /// Prime the pipeline by issuing L1 prefetches for the first window.
    pub fn prime_l1<K: Hashable>(
        &mut self,
        keys: &[K],
        hash_index: &crate::hash::index::HashIndex,
    ) {
        let end = keys.len().min(self.l1_window);
        for i in 0..end {
            self.issue_l1(i, &keys[i], hash_index);
        }
    }

    /// Prime the pipeline by issuing L2 prefetches for the first L2 window.
    pub fn prime_l2(
        &mut self,
        hash_index: &crate::hash::index::HashIndex,
        allocator: &crate::hybrid_log::log_allocator::HybridLogAllocator,
        is_write: bool,
    ) {
        let end = self.hashes.len().min(self.l2_window);
        for i in 0..end {
            self.issue_l2(i, hash_index, allocator, is_write);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_result_empty() {
        let result = BatchResult::with_capacity(0);
        assert!(result.is_empty());
        assert_eq!(result.total(), 0);
        assert!(result.all_succeeded()); // vacuously true
        assert!(!result.has_pending());
    }

    #[test]
    fn batch_result_all_ok() {
        let result = BatchResult {
            statuses: vec![OperationStatus::Ok; 5],
        };
        assert_eq!(result.total(), 5);
        assert!(result.all_succeeded());
        assert_eq!(result.succeeded_count(), 5);
        assert_eq!(result.pending_count(), 0);
    }

    #[test]
    fn batch_result_mixed() {
        let result = BatchResult {
            statuses: vec![
                OperationStatus::Ok,
                OperationStatus::Created,
                OperationStatus::NotFound,
                OperationStatus::Pending,
                OperationStatus::Deleted,
            ],
        };
        assert_eq!(result.total(), 5);
        assert!(!result.all_succeeded());
        assert_eq!(result.succeeded_count(), 3);
        assert_eq!(result.pending_count(), 1);
        assert_eq!(result.not_found_count(), 1);
        assert!(result.has_pending());
    }

    #[test]
    fn batch_op_key_access() {
        let op: BatchOp<u64, u64> = BatchOp::Read { key: 42, input: 0 };
        assert_eq!(*op.key(), 42);

        let op: BatchOp<u64, u64> = BatchOp::Upsert { key: 99, input: 100 };
        assert_eq!(*op.key(), 99);

        let op: BatchOp<u64, u64> = BatchOp::Delete { key: 13 };
        assert_eq!(*op.key(), 13);
    }
}
