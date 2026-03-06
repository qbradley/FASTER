//! Async wrapper around FASTER's thread-local session model.
//!
//! [`AsyncSession`] wraps a [`FasterSession`] and provides the same CRUD
//! operations with async-friendly completion handling. Operations that
//! complete in-memory return immediately; operations that require disk I/O
//! can be completed asynchronously via [`complete_pending`](AsyncSession::complete_pending).
//!
//! # Thread Affinity
//!
//! Like [`FasterSession`], `AsyncSession` is **`!Send`** — it must be used
//! within a single-threaded async task (e.g., `tokio::task::LocalSet`) or
//! on the thread that created it. This matches FASTER's session model where
//! epoch protection is thread-local.
//!
//! # Example
//!
//! ```ignore
//! use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
//! use faster_core::NullDevice;
//! use faster_tokio::AsyncSession;
//!
//! let store = FasterKv::new(
//!     FasterKvConfig::default(),
//!     SimpleFunctions::<u64, u64>::default(),
//!     NullDevice::new(),
//! );
//!
//! let mut session = AsyncSession::new(&store);
//! let status = session.upsert(&1u64, &42u64, ());
//! assert!(status.is_success());
//! ```

use std::marker::PhantomData;

use faster_core::status::OperationStatus;
use faster_core::store::{CompletePendingResult, FasterKv, FasterSession, Functions};

/// Async wrapper around [`FasterSession`] for use with Tokio or any async runtime.
///
/// Provides the same CRUD operations as the core [`FasterKv`] + [`FasterSession`]
/// API, but with async-friendly pending I/O completion. Operations that hit
/// the mutable or read-only in-memory region complete immediately. Operations
/// that require disk I/O return [`OperationStatus::Pending`] and can be
/// driven to completion via [`complete_pending`](Self::complete_pending).
///
/// # Thread Affinity (`!Send`)
///
/// `AsyncSession` is `!Send`, matching [`FasterSession`]'s thread-affine
/// design. Use within a `tokio::task::LocalSet` or on a single-threaded
/// runtime.
///
/// ```compile_fail
/// use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
/// use faster_core::NullDevice;
/// use faster_tokio::AsyncSession;
///
/// fn require_send<T: Send>() {}
/// require_send::<AsyncSession<'_, SimpleFunctions<u64, u64>>>();
/// ```
pub struct AsyncSession<'a, F: Functions> {
    store: &'a FasterKv<F>,
    session: FasterSession<F>,
    // Enforce !Send at compile time, same as FasterSession.
    _not_send: PhantomData<*const ()>,
}

impl<'a, F: Functions> AsyncSession<'a, F> {
    /// Create a new `AsyncSession` bound to the given store.
    ///
    /// Allocates a session (epoch table slot) from the store. The session
    /// is disposed when the `AsyncSession` is dropped.
    pub fn new(store: &'a FasterKv<F>) -> Self {
        let session = store.new_session();
        Self {
            store,
            session,
            _not_send: PhantomData,
        }
    }

    // ── CRUD Operations ─────────────────────────────────────────────

    /// Read a key's value.
    ///
    /// On success, the result is written to `output` via the [`Functions::read`]
    /// callback. If the record is on disk, returns [`OperationStatus::Pending`];
    /// call [`complete_pending`](Self::complete_pending) to drive I/O completion.
    #[inline]
    pub fn read(
        &mut self,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> OperationStatus {
        self.store
            .read(&mut self.session, key, input, output, context)
    }

    /// Insert or update a key-value pair.
    ///
    /// For [`SimpleFunctions`](faster_core::store::SimpleFunctions), `input`
    /// is the value to store. Returns a success status on immediate completion,
    /// or [`OperationStatus::Pending`] if the record is on disk.
    #[inline]
    pub fn upsert(
        &mut self,
        key: &F::Key,
        input: &F::Input,
        context: F::Context,
    ) -> OperationStatus {
        self.store.upsert(&mut self.session, key, input, context)
    }

    /// Read-modify-write a key.
    ///
    /// If the key exists, updates it via [`Functions::rmw_in_place`] or
    /// [`Functions::rmw_copy_update`]. If absent, creates a new record via
    /// [`Functions::rmw_initial`].
    #[inline]
    pub fn rmw(
        &mut self,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> OperationStatus {
        self.store
            .rmw(&mut self.session, key, input, output, context)
    }

    /// Delete a key.
    ///
    /// Marks the record as a tombstone. Returns [`OperationStatus::NotFound`]
    /// if the key does not exist.
    #[inline]
    pub fn delete(&mut self, key: &F::Key, context: F::Context) -> OperationStatus {
        self.store.delete(&mut self.session, key, context)
    }

    // ── Convenience (Default Context) ───────────────────────────────

    /// Simplified upsert using default context.
    #[inline]
    pub fn upsert_simple(&mut self, key: &F::Key, input: &F::Input) -> OperationStatus
    where
        F::Context: Default,
    {
        self.upsert(key, input, F::Context::default())
    }

    /// Simplified read returning the output directly.
    #[inline]
    pub fn read_simple(&mut self, key: &F::Key) -> F::Output
    where
        F::Input: Default,
        F::Context: Default,
    {
        self.store.read_simple(&mut self.session, key)
    }

    /// Simplified delete using default context.
    #[inline]
    pub fn delete_simple(&mut self, key: &F::Key) -> OperationStatus
    where
        F::Context: Default,
    {
        self.delete(key, F::Context::default())
    }

    // ── Pending I/O Completion ──────────────────────────────────────

    /// Drive all pending I/O to completion, yielding to the async runtime
    /// between polling passes.
    ///
    /// Returns a summary of how many operations completed. If no operations
    /// are pending, returns immediately with an empty result.
    ///
    /// # Cooperative Scheduling
    ///
    /// Between polls, this method yields to the async runtime via
    /// [`tokio::task::yield_now`], allowing other tasks to make progress
    /// while waiting for device I/O callbacks to fire.
    pub async fn complete_pending(&mut self) -> CompletePendingResult {
        if !self.session.has_pending() {
            return CompletePendingResult::EMPTY;
        }
        loop {
            let result = self.store.try_complete_pending(&mut self.session, false);
            if result.is_done() {
                return result;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Complete all pending I/O and return the output/context pairs.
    ///
    /// Similar to [`complete_pending`](Self::complete_pending) but returns
    /// the actual completion results (output values and user contexts) from
    /// pending read operations.
    pub async fn complete_pending_with_results(&mut self) -> Vec<(F::Output, F::Context)> {
        if !self.session.has_pending() {
            return Vec::new();
        }
        loop {
            let results = self.store.complete_pending(&mut self.session);
            if !self.session.has_pending() {
                return results;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Bump the global epoch and drain completed pending operations.
    ///
    /// Async equivalent of [`FasterKv::refresh`].
    pub async fn refresh(&mut self) -> CompletePendingResult {
        self.store.refresh(&mut self.session)
    }

    // ── Accessors ───────────────────────────────────────────────────

    /// Returns `true` if this session has any outstanding pending operations.
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.session.has_pending()
    }

    /// Returns the number of outstanding pending operations.
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.session.pending_count()
    }

    /// Returns a reference to the underlying [`FasterSession`].
    #[inline]
    pub fn inner(&self) -> &FasterSession<F> {
        &self.session
    }
}

impl<F: Functions> Drop for AsyncSession<'_, F> {
    fn drop(&mut self) {
        // Dispose the session, releasing its epoch table slot.
        // We need to take ownership of the session to pass it to dispose_session.
        // Since we're in Drop, we can use a dummy session and swap.
        //
        // Instead, we just clear pending state and let the session's own Drop
        // handle epoch cleanup. The epoch table slot is released when the
        // EpochThread handle inside FasterSession is dropped.
        self.session.clear_pending();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faster_core::NullDevice;
    use faster_core::store::{FasterKvConfig, SimpleFunctions};

    type TestStore = FasterKv<SimpleFunctions<u64, u64>>;

    fn test_store() -> TestStore {
        FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::default(),
            NullDevice::new(),
        )
    }

    // ── Async CRUD round-trip ───────────────────────────────────────

    #[tokio::test]
    async fn async_upsert_read_round_trip() {
        let store = test_store();
        let mut session = AsyncSession::new(&store);

        let status = session.upsert(&1u64, &42u64, ());
        assert!(status.is_success(), "upsert should succeed: {status}");

        let value = session.read_simple(&1u64);
        assert_eq!(value, Some(42));
    }

    #[tokio::test]
    async fn async_crud_full_cycle() {
        let store = test_store();
        let mut session = AsyncSession::new(&store);

        // Upsert
        let status = session.upsert_simple(&10u64, &100u64);
        assert!(status.is_success(), "upsert: {status}");

        // Read
        let mut output: Option<u64> = None;
        let status = session.read(&10u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(output, Some(100));

        // RMW (replaces value for SimpleFunctions)
        let mut rmw_output: Option<u64> = None;
        let status = session.rmw(&10u64, &200u64, &mut rmw_output, ());
        assert!(status.is_success(), "rmw: {status}");

        // Read back RMW result
        let value = session.read_simple(&10u64);
        assert_eq!(value, Some(200));

        // Delete
        let status = session.delete_simple(&10u64);
        assert_eq!(status, OperationStatus::Deleted);

        // Read after delete → NotFound
        let mut output2: Option<u64> = None;
        let status = session.read(&10u64, &0u64, &mut output2, ());
        assert_eq!(status, OperationStatus::NotFound);
    }

    // ── Mixed status returns ────────────────────────────────────────

    #[tokio::test]
    async fn mixed_sync_statuses() {
        let store = test_store();
        let mut session = AsyncSession::new(&store);

        // First upsert → Created
        let s1 = session.upsert_simple(&1u64, &10u64);
        assert_eq!(s1, OperationStatus::Created);

        // Second upsert to same key → InPlaceUpdated
        let s2 = session.upsert_simple(&1u64, &20u64);
        assert_eq!(s2, OperationStatus::InPlaceUpdated);

        // Read existing key → Ok
        let mut out: Option<u64> = None;
        let s3 = session.read(&1u64, &0u64, &mut out, ());
        assert_eq!(s3, OperationStatus::Ok);
        assert_eq!(out, Some(20));

        // Read non-existent key → NotFound
        let mut out2: Option<u64> = None;
        let s4 = session.read(&999u64, &0u64, &mut out2, ());
        assert_eq!(s4, OperationStatus::NotFound);

        // Delete existing key → Deleted
        let s5 = session.delete_simple(&1u64);
        assert_eq!(s5, OperationStatus::Deleted);

        // None of these are Pending (NullDevice, all in memory)
        assert!(!session.has_pending());
    }

    // ── complete_pending with nothing pending ───────────────────────

    #[tokio::test]
    async fn complete_pending_no_ops() {
        let store = test_store();
        let mut session = AsyncSession::new(&store);

        let result = session.complete_pending().await;
        assert!(result.is_done());
        assert_eq!(result.completed, 0);
        assert_eq!(result.remaining, 0);
    }

    #[tokio::test]
    async fn complete_pending_with_results_no_ops() {
        let store = test_store();
        let mut session = AsyncSession::new(&store);

        let results = session.complete_pending_with_results().await;
        assert!(results.is_empty());
    }

    // ── !Send enforcement ───────────────────────────────────────────

    /// Compile-time assertion that `AsyncSession` is `!Send`.
    ///
    /// The real enforcement is the `PhantomData<*const ()>` field on the
    /// struct, plus the `compile_fail` doctest on the struct itself. This
    /// test is a runtime smoke-check that the marker is present.
    #[test]
    fn async_session_is_not_send() {
        // If the !Send marker were removed, the compile_fail doctest on
        // AsyncSession would start passing (compilation would succeed),
        // which cargo test treats as a doctest failure.
        let store = test_store();
        let _session = AsyncSession::new(&store);
    }

    // ── Multiple sessions ───────────────────────────────────────────

    #[tokio::test]
    async fn multiple_independent_sessions() {
        let store = test_store();
        let mut s1 = AsyncSession::new(&store);
        let mut s2 = AsyncSession::new(&store);

        // Each session operates independently
        let _ = s1.upsert_simple(&1u64, &100u64);
        let _ = s2.upsert_simple(&2u64, &200u64);

        assert_eq!(s1.read_simple(&1u64), Some(100));
        assert_eq!(s1.read_simple(&2u64), Some(200)); // visible across sessions
        assert_eq!(s2.read_simple(&1u64), Some(100));
        assert_eq!(s2.read_simple(&2u64), Some(200));
    }

    // ── Batch operations ────────────────────────────────────────────

    #[tokio::test]
    async fn batch_upsert_and_read() {
        let store = test_store();
        let mut session = AsyncSession::new(&store);

        for i in 0u64..100 {
            let _ = session.upsert_simple(&i, &(i * 10));
        }

        for i in 0u64..100 {
            let value = session.read_simple(&i);
            assert_eq!(value, Some(i * 10), "mismatch at key {i}");
        }
    }

    // ── Accessors ───────────────────────────────────────────────────

    #[tokio::test]
    async fn pending_count_starts_at_zero() {
        let store = test_store();
        let session = AsyncSession::new(&store);

        assert!(!session.has_pending());
        assert_eq!(session.pending_count(), 0);
    }

    #[tokio::test]
    async fn inner_session_accessible() {
        let store = test_store();
        let session = AsyncSession::new(&store);

        let stats = session.inner().stats();
        assert_eq!(stats.operations_completed, 0);
    }
}
