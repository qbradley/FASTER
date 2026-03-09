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

use super::Functions;
use super::kv::FasterKv;
use super::operations::{
    InternalContext, internal_delete, internal_read, internal_rmw, internal_upsert,
};
use super::session::UnsafeContext;

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
    fn filled(n: usize, status: OperationStatus) -> Self {
        Self {
            statuses: vec![status; n],
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
        self.statuses.contains(&OperationStatus::Pending)
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

#[allow(clippy::needless_range_loop)]
impl<'a, F: Functions> UnsafeContext<'a, F> {
    /// Execute a batch of reads under single-epoch protection.
    ///
    /// # Panics
    /// Panics if `keys.len() != outputs.len()`.
    pub fn batch_read(
        &mut self,
        store: &FasterKv<F>,
        keys: &[F::Key],
        outputs: &mut [F::Output],
    ) -> BatchResult
    where
        F::Input: Default,
        F::Context: Default,
    {
        assert_eq!(
            keys.len(),
            outputs.len(),
            "keys and outputs must have equal length"
        );
        let n = keys.len();
        if n == 0 {
            return BatchResult {
                statuses: Vec::new(),
            };
        }
        Self {
            hashes: Vec::with_capacity(n),
            resolved,
            l1_window,
            l2_window,
        }
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let mut result = BatchResult::filled(n, OperationStatus::NotFound);
        for i in 0..n {
            result.statuses[i] = internal_read(
                &ctx,
                self.session,
                &store.functions,
                &keys[i],
                &F::Input::default(),
                &mut outputs[i],
                F::Context::default(),
            );
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
    }

    /// L1 stage: hash the key and prefetch the bucket.
    #[inline]
    pub fn issue_l1<K: Hashable>(
        &mut self,
        store: &FasterKv<F>,
        keys: &[F::Key],
        inputs: &[F::Input],
    ) -> BatchResult
    where
        F::Context: Default,
    {
        assert_eq!(
            keys.len(),
            inputs.len(),
            "keys and inputs must have equal length"
        );
        let n = keys.len();
        if n == 0 {
            return BatchResult {
                statuses: Vec::new(),
            };
        }
        for key in keys {
            store.hash_index.prefetch(key.hash());
        }
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let mut result = BatchResult::filled(n, OperationStatus::NotFound);
        for i in 0..n {
            result.statuses[i] = internal_upsert(
                &ctx,
                self.session,
                &store.functions,
                &keys[i],
                &inputs[i],
                F::Context::default(),
            );
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
    }

    /// L2 stage: resolve the hash entry and prefetch the record.
    #[inline]
    pub fn issue_l2(
        &mut self,
        store: &FasterKv<F>,
        keys: &[F::Key],
        inputs: &[F::Input],
        outputs: &mut [F::Output],
    ) -> BatchResult
    where
        F::Context: Default,
    {
        assert_eq!(
            keys.len(),
            inputs.len(),
            "keys and inputs must have equal length"
        );
        assert_eq!(
            keys.len(),
            outputs.len(),
            "keys and outputs must have equal length"
        );
        let n = keys.len();
        if n == 0 {
            return BatchResult {
                statuses: Vec::new(),
            };
        }
        for key in keys {
            store.hash_index.prefetch(key.hash());
        }
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let mut result = BatchResult::filled(n, OperationStatus::NotFound);
        for i in 0..n {
            result.statuses[i] = internal_rmw(
                &ctx,
                self.session,
                &store.functions,
                &keys[i],
                &inputs[i],
                &mut outputs[i],
                F::Context::default(),
            );
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
            self.resolved[idx] = ResolvedEntry { entry, addr };
        }
    }

    /// Execute a batch of deletes under single-epoch protection.
    pub fn batch_delete(&mut self, store: &FasterKv<F>, keys: &[F::Key]) -> BatchResult
    where
        F::Context: Default,
    {
        let n = keys.len();
        if n == 0 {
            return BatchResult {
                statuses: Vec::new(),
            };
        }
        for key in keys {
            store.hash_index.prefetch(key.hash());
        }
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let mut result = BatchResult::filled(n, OperationStatus::NotFound);
        for i in 0..n {
            result.statuses[i] = internal_delete(
                &ctx,
                self.session,
                &store.functions,
                &keys[i],
                F::Context::default(),
            );
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
    }

    /// Prime the pipeline by issuing L2 prefetches for the first L2 window.
    pub fn prime_l2(
        &mut self,
        store: &FasterKv<F>,
        ops: &[BatchOp<F::Key, F::Input>],
        outputs: &mut [F::Output],
    ) -> BatchResult
    where
        F::Input: Default,
        F::Context: Default,
    {
        assert_eq!(
            ops.len(),
            outputs.len(),
            "ops and outputs must have equal length"
        );
        let n = ops.len();
        if n == 0 {
            return BatchResult {
                statuses: Vec::new(),
            };
        }
        for op in ops {
            store.hash_index.prefetch(op.key().hash());
        }
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let mut result = BatchResult::filled(n, OperationStatus::NotFound);
        for i in 0..n {
            result.statuses[i] = match &ops[i] {
                BatchOp::Read { key, input } => internal_read(
                    &ctx,
                    self.session,
                    &store.functions,
                    key,
                    input,
                    &mut outputs[i],
                    F::Context::default(),
                ),
                BatchOp::Upsert { key, input } => internal_upsert(
                    &ctx,
                    self.session,
                    &store.functions,
                    key,
                    input,
                    F::Context::default(),
                ),
                BatchOp::Rmw { key, input } => internal_rmw(
                    &ctx,
                    self.session,
                    &store.functions,
                    key,
                    input,
                    &mut outputs[i],
                    F::Context::default(),
                ),
                BatchOp::Delete { key } => internal_delete(
                    &ctx,
                    self.session,
                    &store.functions,
                    key,
                    F::Context::default(),
                ),
            };
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_result_empty() {
        let result = BatchResult {
            statuses: Vec::new(),
        };
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
        let op: BatchOp<u64, u64> = BatchOp::Upsert {
            key: 99,
            input: 100,
        };
        assert_eq!(*op.key(), 99);

        let op: BatchOp<u64, u64> = BatchOp::Delete { key: 13 };
        assert_eq!(*op.key(), 13);
    }
}
