//! Batch operation API for FASTER sessions.
//!
//! This module provides high-throughput batch operations that amortize
//! per-operation overhead. A batch:
//!
//! 1. Enters epoch protection **once** for the entire batch (not per-op).
//! 2. Issues hash-bucket **prefetches** for all keys up front so that
//!    the CPU's memory subsystem can overlap DRAM latency with useful work.
//! 3. Processes all operations in sequence under a single epoch region.
//! 4. Refreshes the epoch every [`BATCH_REFRESH_INTERVAL`] operations to
//!    avoid stalling safe-epoch advancement.
//!
//! # API Styles
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
//! [`UnsafeContext`]: super::session::UnsafeContext

use crate::hash::Hashable;
use crate::status::OperationStatus;

use super::Functions;
use super::kv::FasterKv;
use super::operations::{
    InternalContext, internal_delete, internal_read, internal_rmw, internal_upsert,
};
use super::session::UnsafeContext;

/// Number of operations between epoch refreshes within a batch.
const BATCH_REFRESH_INTERVAL: usize = 256;

// -- BatchResult --

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

    /// Number of operations that completed successfully.
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

    /// Returns `true` if every operation succeeded.
    #[inline]
    pub fn all_succeeded(&self) -> bool {
        self.statuses.iter().all(|s| s.is_success())
    }

    /// Returns `true` if any operation went pending.
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.statuses.contains(&OperationStatus::Pending)
    }

    /// Returns `true` if the batch was empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.statuses.is_empty()
    }
}

// -- BatchOp --

/// A single operation within a mixed batch.
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
        /// Modification input.
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
            BatchOp::Read { key, .. }
            | BatchOp::Upsert { key, .. }
            | BatchOp::Rmw { key, .. }
            | BatchOp::Delete { key } => key,
        }
    }
}

// -- Batch methods on UnsafeContext --

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
        for key in keys {
            store.hash_index.prefetch(key.hash());
        }
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
            on_alloc_failure: Some(&|| store.maintenance()),
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
            )
            .0;
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
    }

    /// Execute a batch of upserts under single-epoch protection.
    ///
    /// # Panics
    /// Panics if `keys.len() != inputs.len()`.
    pub fn batch_upsert(
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
            on_alloc_failure: Some(&|| store.maintenance()),
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
            )
            .0;
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
    }

    /// Execute a batch of read-modify-write operations.
    ///
    /// # Panics
    /// Panics if lengths of `keys`, `inputs`, and `outputs` differ.
    pub fn batch_rmw(
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
            on_alloc_failure: Some(&|| store.maintenance()),
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
            )
            .0;
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
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
            on_alloc_failure: Some(&|| store.maintenance()),
        };
        let mut result = BatchResult::filled(n, OperationStatus::NotFound);
        for i in 0..n {
            result.statuses[i] = internal_delete(
                &ctx,
                self.session,
                &store.functions,
                &keys[i],
                F::Context::default(),
            )
            .0;
            if result.statuses[i] == OperationStatus::Pending {
                store.dispatch_pending_io(self.session);
            }
            if (i + 1) % BATCH_REFRESH_INTERVAL == 0 {
                self.refresh();
            }
        }
        result
    }

    /// Execute a mixed batch of heterogeneous operations.
    ///
    /// # Panics
    /// Panics if `ops.len() != outputs.len()`.
    pub fn batch_execute(
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
            on_alloc_failure: Some(&|| store.maintenance()),
        };
        let mut result = BatchResult::filled(n, OperationStatus::NotFound);
        for i in 0..n {
            result.statuses[i] = match &ops[i] {
                BatchOp::Read { key, input } => {
                    internal_read(
                        &ctx,
                        self.session,
                        &store.functions,
                        key,
                        input,
                        &mut outputs[i],
                        F::Context::default(),
                    )
                    .0
                }
                BatchOp::Upsert { key, input } => {
                    internal_upsert(
                        &ctx,
                        self.session,
                        &store.functions,
                        key,
                        input,
                        F::Context::default(),
                    )
                    .0
                }
                BatchOp::Rmw { key, input } => {
                    internal_rmw(
                        &ctx,
                        self.session,
                        &store.functions,
                        key,
                        input,
                        &mut outputs[i],
                        F::Context::default(),
                    )
                    .0
                }
                BatchOp::Delete { key } => {
                    internal_delete(
                        &ctx,
                        self.session,
                        &store.functions,
                        key,
                        F::Context::default(),
                    )
                    .0
                }
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
        assert!(result.all_succeeded());
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
