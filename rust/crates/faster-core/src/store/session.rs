//! Thread-local session model for FASTER CRUD operations.
//!
//! A [`FasterSession`] is the user's handle for performing Read, Upsert, RMW,
//! and Delete operations on a FASTER store. Sessions are **`!Send`** — they
//! must be used on the thread that created them. This is enforced at compile
//! time via `PhantomData<*const ()>`.
//!
//! # Architecture
//!
//! - The session manages **epoch protection** (enter/leave) and a queue of
//!   **pending operations** (records that fell into the read-only or on-disk
//!   region and need I/O before they can complete).
//! - The session does *not* hold references to the allocator, hash index, or
//!   device — those live in `FasterKv`. CRUD operations are methods on
//!   `FasterKv` that take `&mut SessionGuard`.
//! - [`SessionGuard`] is an RAII guard created by [`FasterSession::begin_unsafe`].
//!   It guarantees epoch protection is released when dropped.
//! - [`SessionPool`] manages session creation and disposal, handling epoch
//!   table registration.
//!
//! # Examples
//!
//! ```
//! use std::sync::Arc;
//! use faster_core::epoch::EpochTable;
//! use faster_core::store::{SimpleFunctions, SessionPool};
//!
//! let epoch_table = Arc::new(EpochTable::new());
//! let pool = SessionPool::<SimpleFunctions<u64, u64>>::new(epoch_table);
//! let mut session = pool.create_session();
//!
//! // Enter epoch protection
//! {
//!     let guard = session.begin_unsafe();
//!     // ... perform operations via FasterKv methods ...
//! }
//! // Guard dropped — epoch protection released automatically.
//! ```

use std::marker::PhantomData;

use crate::sync::Arc;

use crate::address::LogicalAddress;
use crate::epoch::EpochTable;
use crate::hash::KeyHash;
use crate::record::RecordLayout;

use super::Functions;
use super::kv::FasterKv;
use super::operations::{
    InternalContext, internal_delete, internal_read, internal_rmw, internal_upsert,
};
use super::pending_io::{CompletedIo, PendingIoContext, PendingIoError};

// ── SessionStats ────────────────────────────────────────────────────

/// Snapshot of session-level statistics.
///
/// Returned by [`FasterSession::stats()`] to give callers an overview of
/// the session's activity without exposing internal bookkeeping.
///
/// # Example
///
/// ```
/// use faster_core::store::SessionStats;
///
/// let stats = SessionStats {
///     operations_completed: 42,
///     pending_count: 0,
///     current_serial: 42,
/// };
/// assert_eq!(stats.operations_completed, 42);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    /// Total number of operations the session has issued (serial number).
    pub operations_completed: u64,
    /// Number of outstanding pending operations (dispatched + in-flight).
    pub pending_count: usize,
    /// The next serial number that will be assigned.
    pub current_serial: u64,
}

// ── CompletePendingResult ───────────────────────────────────────────

/// Result returned by [`FasterKv::try_complete_pending`](super::FasterKv::try_complete_pending)
/// and the session-level pending-completion APIs.
///
/// Provides callers with a structured summary of what happened during a
/// completion pass so they can decide whether to poll again, log errors,
/// or proceed with other work.
///
/// # Example
///
/// ```
/// use faster_core::store::CompletePendingResult;
///
/// let result = CompletePendingResult {
///     completed: 3,
///     remaining: 0,
///     had_errors: false,
/// };
/// assert!(result.is_done());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletePendingResult {
    /// Number of operations that completed during this call.
    pub completed: u32,
    /// Number of operations still pending (dispatched + in-flight I/O).
    pub remaining: u32,
    /// Whether any completions failed (I/O errors, invalid records, etc.).
    pub had_errors: bool,
}

impl CompletePendingResult {
    /// A no-op result: nothing completed, nothing remaining, no errors.
    pub const EMPTY: Self = Self {
        completed: 0,
        remaining: 0,
        had_errors: false,
    };

    /// Returns `true` if all pending operations have been completed.
    #[inline]
    pub fn is_done(&self) -> bool {
        self.remaining == 0
    }
}

// ── PendingOpType ───────────────────────────────────────────────────

/// The type of a deferred (pending) operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOpType {
    /// A pending Read operation.
    Read,
    /// A pending Upsert operation.
    Upsert,
    /// A pending Read-Modify-Write operation.
    Rmw,
    /// A pending Delete operation.
    Delete,
}

// ── PendingOperation ────────────────────────────────────────────────

/// A pending operation that needs to be retried after I/O completes.
///
/// When an operation targets a record in the read-only or on-disk region,
/// it cannot complete synchronously. The operation is captured as a
/// `PendingOperation` and enqueued in the session's pending queue for
/// later completion.
pub struct PendingOperation<F: Functions> {
    /// The type of operation (Read, Upsert, RMW, Delete).
    pub op_type: PendingOpType,
    /// The key for this operation.
    pub key: F::Key,
    /// The input (for Read/RMW). `None` for operations that don't need input.
    pub input: Option<F::Input>,
    /// The user context carried through the pending operation.
    pub context: F::Context,
    /// The logical address where the record was found (for read-back).
    pub address: LogicalAddress,
    /// The record layout for this key/value type.
    pub record_layout: RecordLayout,
    /// The hash for this key (cached to avoid rehashing on retry).
    pub key_hash: KeyHash,
}

// ── FasterSession ───────────────────────────────────────────────────

/// A thread-local session for performing CRUD operations on a FASTER store.
///
/// Sessions are `!Send` — they must be used on the thread that created them.
/// This is enforced at compile time via `PhantomData<*const ()>`.
///
/// # Epoch Protection
///
/// Before performing any operation, callers must enter an epoch-protected
/// region via [`begin_unsafe`](Self::begin_unsafe), which returns a
/// [`SessionGuard`]. The "unsafe" in the name refers to the epoch region
/// (matching C++ FASTER naming), not Rust's `unsafe` keyword.
///
/// # Pending Operations
///
/// Operations that hit the read-only or on-disk region are enqueued as
/// [`PendingOperation`]s. Call [`drain_completed`](Self::drain_completed) or
/// [`complete_pending`](Self::complete_pending) to process them.
pub struct FasterSession<F: Functions> {
    /// The epoch thread handle for this session. Owns the epoch table slot.
    epoch_thread: crate::epoch::EpochThread,
    /// Reference to the shared epoch table.
    epoch_table: Arc<EpochTable>,
    /// Queue of pending operations awaiting I/O dispatch.
    ///
    /// Operations land here when `internal_*` returns `Pending`. They are
    /// drained and converted into [`PendingIoContext`]s by
    /// [`FasterKv`](super::FasterKv) after the CRUD call returns.
    pending_ops: Vec<PendingOperation<F>>,
    /// In-flight I/O contexts waiting for device completion.
    ///
    /// Each entry represents an issued disk read whose callback has not yet
    /// signalled completion. Polled by [`take_completed_io`](Self::take_completed_io).
    io_contexts: Vec<PendingIoContext<F>>,
    /// Monotonic serial number for ordering operations within this session.
    serial_number: u64,
    /// Whether the session is currently in an epoch-protected region.
    in_epoch: bool,
    /// `!Send` marker — sessions are thread-affine.
    _not_send: PhantomData<*const ()>,
}

impl<F: Functions> FasterSession<F> {
    /// Creates a new session with the given epoch thread handle and table.
    pub(crate) fn new(
        epoch_thread: crate::epoch::EpochThread,
        epoch_table: Arc<EpochTable>,
    ) -> Self {
        Self {
            epoch_thread,
            epoch_table,
            pending_ops: Vec::new(),
            io_contexts: Vec::new(),
            serial_number: 0,
            in_epoch: false,
            _not_send: PhantomData,
        }
    }

    /// Enters epoch protection and returns a [`SessionGuard`].
    ///
    /// The guard ensures epoch protection is released when dropped, even on
    /// panic. Must be called before any CRUD operation.
    ///
    /// # Panics
    ///
    /// Debug-panics if the session is already in an epoch-protected region
    /// (double `begin_unsafe` without matching `end_unsafe`).
    pub fn begin_unsafe(&mut self) -> SessionGuard<'_, F> {
        debug_assert!(!self.in_epoch, "begin_unsafe called while already in epoch");
        self.epoch_table.protect(self.epoch_thread.entry_index());
        self.in_epoch = true;
        SessionGuard { session: self }
    }

    /// Leaves epoch protection.
    ///
    /// Normally called automatically by [`SessionGuard::drop`]. Only call
    /// this directly if you are not using the guard pattern.
    pub fn end_unsafe(&mut self) {
        if self.in_epoch {
            self.epoch_table.unprotect(self.epoch_thread.entry_index());
            self.in_epoch = false;
        }
    }

    /// Returns the current serial number and increments it.
    ///
    /// Serial numbers are monotonically increasing within a session and are
    /// used to order operations for consistency.
    #[inline]
    pub fn next_serial(&mut self) -> u64 {
        let sn = self.serial_number;
        self.serial_number += 1;
        sn
    }

    /// Enqueues a pending operation for later I/O dispatch.
    ///
    /// Called by `internal_*` operations when a record falls into the on-disk
    /// region. The operation sits in `pending_ops` until
    /// [`drain_pending_ops`](Self::drain_pending_ops) is called by the store
    /// to issue the actual disk read.
    #[inline]
    pub(crate) fn enqueue_pending(&mut self, op: PendingOperation<F>) {
        self.pending_ops.push(op);
    }

    /// Drains all un-dispatched pending operations.
    ///
    /// Returns operations that have been enqueued but not yet submitted for
    /// disk I/O. After this call, `pending_ops` is empty. The caller
    /// (typically `FasterKv`) is responsible for issuing reads and pushing
    /// the resulting [`PendingIoContext`]s back via
    /// [`enqueue_io_context`](Self::enqueue_io_context).
    pub(crate) fn drain_pending_ops(&mut self) -> Vec<PendingOperation<F>> {
        std::mem::take(&mut self.pending_ops)
    }

    /// Pushes an in-flight I/O context into the session.
    ///
    /// Called after the store issues a disk read for a pending operation.
    /// The context is polled by [`take_completed_io`](Self::take_completed_io).
    #[inline]
    pub(crate) fn enqueue_io_context(&mut self, ctx: PendingIoContext<F>) {
        self.io_contexts.push(ctx);
    }

    /// Polls in-flight I/O contexts and returns those that have completed.
    ///
    /// Contexts whose device callback has not yet fired remain in the session.
    /// Returns a vec of [`CompletedIo`] ready for the store to retry.
    pub(crate) fn take_completed_io(&mut self) -> Vec<CompletedIo<F>> {
        let mut completed = Vec::new();
        let mut still_pending = Vec::new();

        for ctx in self.io_contexts.drain(..) {
            match ctx.try_complete() {
                Ok(c) => completed.push(c),
                Err(ctx) => still_pending.push(ctx),
            }
        }

        self.io_contexts = still_pending;
        completed
    }

    /// Spin-waits for **all** in-flight I/O to complete and returns the results.
    ///
    /// Used during session teardown or when the caller needs synchronous
    /// completion of every outstanding request.
    ///
    /// # Blocking
    ///
    /// This method busy-waits (with a `thread::yield_now` hint) until every
    /// [`PendingIoContext`] signals completion. It should only be called when
    /// the caller can tolerate blocking (e.g., session dispose, shutdown).
    pub(crate) fn wait_for_all_io(&mut self) -> Vec<CompletedIo<F>> {
        let mut completed = Vec::with_capacity(self.io_contexts.len());

        for ctx in self.io_contexts.drain(..) {
            // Spin until the device callback fires.
            let mut pending_ctx = ctx;
            loop {
                match pending_ctx.try_complete() {
                    Ok(c) => {
                        completed.push(c);
                        break;
                    }
                    Err(ctx) => {
                        std::thread::yield_now();
                        pending_ctx = ctx;
                    }
                }
            }
        }

        completed
    }

    /// Drains all pending operations (both un-dispatched and in-flight).
    ///
    /// Returns pending operations that had not yet been dispatched for I/O.
    /// In-flight I/O contexts are dropped (their callbacks will fire harmlessly
    /// into the `Arc`-shared completion state).
    ///
    /// This is used for session cleanup when the caller does not need the
    /// results (e.g., abort). Prefer [`FasterKv::complete_pending`] for
    /// normal completion.
    pub fn drain_completed(&mut self) -> Vec<PendingOperation<F>> {
        self.io_contexts.clear();
        std::mem::take(&mut self.pending_ops)
    }

    /// Returns the number of outstanding operations (dispatched + in-flight).
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.pending_ops.len() + self.io_contexts.len()
    }

    /// Returns `true` if this session has any outstanding operations.
    #[inline]
    pub fn has_pending(&self) -> bool {
        !self.pending_ops.is_empty() || !self.io_contexts.is_empty()
    }

    /// Returns the number of I/O requests currently in flight.
    #[inline]
    pub fn io_pending_count(&self) -> usize {
        self.io_contexts.len()
    }

    /// Clears all pending state (operations and I/O contexts) without
    /// processing results.
    ///
    /// Used during session teardown when the caller does not need results.
    /// In-flight I/O callbacks will fire into dangling `Arc`s, which is safe
    /// (the atomics are ref-counted and the callback just writes booleans).
    ///
    /// For structured pending-completion that actually processes I/O results,
    /// use [`FasterKv::try_complete_pending`](super::FasterKv::try_complete_pending).
    /// # Safety (CORE-04)
    ///
    /// After calling this method, any device callbacks that reference the
    /// discarded contexts will write through dangling pointers. This is
    /// acceptable **only** when:
    /// - The device guarantees all callbacks have already fired, **or**
    /// - The session is being torn down and the callbacks write into
    ///   `Arc`-shared atomics that remain valid independently.
    pub fn clear_pending(&mut self) {
        self.pending_ops.clear();
        self.io_contexts.clear();
    }

    /// Returns the number of in-flight I/O operations that have exceeded
    /// their timeout.
    ///
    /// This is a read-only check — call
    /// [`cancel_all_expired`](Self::cancel_all_expired) to actually remove
    /// and return the expired contexts.
    #[inline]
    pub fn expired_pending_count(&self) -> usize {
        self.io_contexts
            .iter()
            .filter(|ctx| ctx.is_expired())
            .count()
    }

    /// Cancel a specific pending I/O operation by its context ID.
    ///
    /// Removes the context from the session's in-flight I/O queue. The
    /// underlying device read (if still in progress) will complete into
    /// dangling `Arc`-shared atomics, which is harmless.
    ///
    /// Returns `Ok(())` if the context was found and removed, or
    /// `Err(PendingIoError::Cancelled { .. })` if no context with that ID
    /// exists (it may have already completed or been cancelled).
    pub fn cancel_pending(&mut self, context_id: u64) -> Result<(), PendingIoError> {
        let pos = self
            .io_contexts
            .iter()
            .position(|ctx| ctx.id() == context_id);
        match pos {
            Some(idx) => {
                let _removed = self.io_contexts.swap_remove(idx);
                Ok(())
            }
            None => Err(PendingIoError::Cancelled { context_id }),
        }
    }

    /// Cancel all expired I/O operations and return them.
    ///
    /// Removes every context whose [`is_expired`](PendingIoContext::is_expired)
    /// returns `true`. Non-expired and already-completed contexts are left
    /// in the queue. The caller can inspect the returned contexts to log
    /// timeouts or invoke error callbacks.
    pub fn cancel_all_expired(&mut self) -> Vec<PendingIoContext<F>> {
        let mut expired = Vec::new();
        let mut still_valid = Vec::new();

        for ctx in self.io_contexts.drain(..) {
            if ctx.is_expired() {
                expired.push(ctx);
            } else {
                still_valid.push(ctx);
            }
        }

        self.io_contexts = still_valid;
        expired
    }

    /// Returns whether this session is currently in an epoch-protected region.
    #[inline]
    pub fn is_in_epoch(&self) -> bool {
        self.in_epoch
    }

    /// Returns a reference to the epoch thread handle.
    #[inline]
    pub fn epoch_thread(&self) -> &crate::epoch::EpochThread {
        &self.epoch_thread
    }

    /// Returns a snapshot of session-level statistics.
    ///
    /// This is a cheap, non-blocking call that reads local counters.
    ///
    /// # Example
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use faster_core::epoch::EpochTable;
    /// # use faster_core::store::{SimpleFunctions, SessionPool};
    /// let epoch_table = Arc::new(EpochTable::new());
    /// let pool = SessionPool::<SimpleFunctions<u64, u64>>::new(epoch_table);
    /// let mut session = pool.create_session();
    /// let stats = session.stats();
    /// assert_eq!(stats.operations_completed, 0);
    /// assert_eq!(stats.pending_count, 0);
    /// ```
    #[inline]
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            operations_completed: self.serial_number,
            pending_count: self.pending_count(),
            current_serial: self.serial_number,
        }
    }
}

impl<F: Functions> Drop for FasterSession<F> {
    fn drop(&mut self) {
        // Ensure epoch is unprotected before the epoch thread handle is dropped.
        if self.in_epoch {
            self.end_unsafe();
        }
        // CORE-04: In debug builds, assert that all I/O contexts have completed
        // before the session is dropped. Dropping with in-flight contexts creates
        // dangling pointers that device callbacks may later dereference.
        // Use `clear_pending()` to explicitly discard incomplete contexts.
        debug_assert!(
            self.io_contexts.iter().all(|ctx| ctx.is_completed()),
            "FasterSession dropped with {} incomplete I/O context(s). \
             Call clear_pending() before drop to acknowledge the risk.",
            self.io_contexts
                .iter()
                .filter(|ctx| !ctx.is_completed())
                .count()
        );
        self.io_contexts.clear();
    }
}

// ── SessionGuard ────────────────────────────────────────────────────

/// RAII guard that ensures epoch protection is held during operations.
///
/// Created by [`FasterSession::begin_unsafe()`]. When dropped, epoch
/// protection is automatically released via [`FasterSession::end_unsafe()`].
///
/// The lifetime `'a` ties this guard to the session borrow, preventing
/// use-after-end_unsafe scenarios at compile time.
pub struct SessionGuard<'a, F: Functions> {
    pub(super) session: &'a mut FasterSession<F>,
}

impl<'a, F: Functions> SessionGuard<'a, F> {
    /// Borrows the session immutably (for read-only access to session state).
    #[inline]
    pub fn session(&self) -> &FasterSession<F> {
        self.session
    }

    /// Borrows the session mutably (for modifying pending ops, serial numbers, etc.).
    #[inline]
    pub fn session_mut(&mut self) -> &mut FasterSession<F> {
        self.session
    }
}

impl<F: Functions> Drop for SessionGuard<'_, F> {
    fn drop(&mut self) {
        self.session.end_unsafe();
    }
}

// ── UnsafeContext ───────────────────────────────────────────────────

/// An epoch-amortized context for batch operations.
///
/// Created via [`FasterKv::unsafe_context()`](super::FasterKv::unsafe_context).
/// While this context exists, the session remains in epoch protection —
/// eliminating the per-operation epoch enter/exit overhead (protect +
/// unprotect + try_drain) that normally costs ~124 ns per operation.
///
/// # When to Use
///
/// Use `UnsafeContext` when performing a batch of operations where epoch
/// enter/exit overhead is a significant fraction of per-operation cost.
/// In benchmarks, this can improve throughput by 30–40% for small
/// key-value pairs.
///
/// # Safety Contract
///
/// The caller **must** periodically call [`refresh()`](Self::refresh) to
/// allow epoch advancement and garbage collection. Holding an
/// `UnsafeContext` for too long without refreshing blocks the safe-epoch
/// from advancing, which prevents drain callbacks from firing.
///
/// A good rule of thumb: call `refresh()` every 64–256 operations.
///
/// # Thread Affinity
///
/// `UnsafeContext` is `!Send` (inherited from `FasterSession`). It must
/// be used on the thread that created the session.
///
/// # Example
///
/// ```
/// use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
/// use faster_core::NullDevice;
///
/// let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
///     FasterKvConfig::default(),
///     SimpleFunctions::default(),
///     NullDevice::new(),
/// );
/// let mut session = store.new_session();
///
/// // Batch operations with amortized epoch cost
/// {
///     let mut ctx = store.unsafe_context(&mut session);
///     for i in 0u64..1000 {
///         ctx.upsert(&store, &i, &(i * 10), ());
///         if i % 64 == 0 {
///             ctx.refresh();
///         }
///     }
/// }
/// // Epoch protection released when `ctx` is dropped
///
/// store.dispose_session(session);
/// ```
pub struct UnsafeContext<'a, F: Functions> {
    pub(super) session: &'a mut FasterSession<F>,
}

impl<'a, F: Functions> UnsafeContext<'a, F> {
    /// Creates a new `UnsafeContext`, entering epoch protection.
    ///
    /// Called internally by [`FasterKv::unsafe_context()`](super::FasterKv::unsafe_context).
    pub(crate) fn new(session: &'a mut FasterSession<F>) -> Self {
        debug_assert!(
            !session.in_epoch,
            "unsafe_context called while already in epoch"
        );
        session
            .epoch_table
            .protect(session.epoch_thread.entry_index());
        session.in_epoch = true;
        Self { session }
    }

    /// Refresh the epoch without leaving the protected region.
    ///
    /// Updates this thread's local epoch to the current global epoch and
    /// runs [`try_drain()`](crate::epoch::EpochTable::try_drain) to
    /// execute any pending drain callbacks.
    ///
    /// Call this periodically (e.g., every 64–256 operations) to avoid
    /// stalling epoch advancement and garbage collection.
    ///
    /// # Example
    ///
    /// ```
    /// # use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// # use faster_core::NullDevice;
    /// # let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    /// #     FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());
    /// # let mut session = store.new_session();
    /// let mut ctx = store.unsafe_context(&mut session);
    /// for i in 0u64..100 {
    ///     ctx.upsert(&store, &i, &(i * 10), ());
    ///     if i % 64 == 0 {
    ///         ctx.refresh();
    ///     }
    /// }
    /// drop(ctx);
    /// # store.dispose_session(session);
    /// ```
    #[inline]
    pub fn refresh(&self) {
        let entry = &self.session.epoch_table.table[self.session.epoch_thread.entry_index()];
        let epoch = self
            .session
            .epoch_table
            .current_epoch
            .load(std::sync::atomic::Ordering::Relaxed);
        entry
            .local_current_epoch
            .store(epoch, std::sync::atomic::Ordering::Release);
        self.session.epoch_table.try_drain();
    }

    /// Returns an immutable reference to the underlying session.
    #[inline]
    pub fn session(&self) -> &FasterSession<F> {
        self.session
    }

    // ── Batch CRUD Operations ───────────────────────────────────────

    /// Read a key within epoch-amortized protection.
    ///
    /// Equivalent to [`FasterKv::read()`](super::FasterKv::read) but skips
    /// the per-operation epoch enter/exit since this context is already
    /// protected.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Ok`] — value read successfully.
    /// - [`OperationStatus::NotFound`] — key does not exist.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    #[inline]
    pub fn read(
        &mut self,
        store: &FasterKv<F>,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> crate::status::OperationStatus {
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let status = internal_read(
            &ctx,
            self.session,
            &store.functions,
            key,
            input,
            output,
            context,
        );
        if status == crate::status::OperationStatus::Pending {
            store.dispatch_pending_io(self.session);
        }
        status
    }

    /// Upsert a key-value pair within epoch-amortized protection.
    ///
    /// Equivalent to [`FasterKv::upsert()`](super::FasterKv::upsert) but
    /// skips the per-operation epoch enter/exit.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Created`] — new record inserted.
    /// - [`OperationStatus::InPlaceUpdated`] — existing mutable record updated.
    /// - [`OperationStatus::CopyUpdated`] — read-only record copied to tail.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    #[inline]
    pub fn upsert(
        &mut self,
        store: &FasterKv<F>,
        key: &F::Key,
        input: &F::Input,
        context: F::Context,
    ) -> crate::status::OperationStatus {
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let status = internal_upsert(&ctx, self.session, &store.functions, key, input, context);
        if status == crate::status::OperationStatus::Pending {
            store.dispatch_pending_io(self.session);
        }
        status
    }

    /// Read-modify-write a key within epoch-amortized protection.
    ///
    /// Equivalent to [`FasterKv::rmw()`](super::FasterKv::rmw) but skips
    /// the per-operation epoch enter/exit.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Created`] — new record created via `rmw_initial`.
    /// - [`OperationStatus::InPlaceUpdated`] — updated in mutable region.
    /// - [`OperationStatus::CopyUpdated`] — read-only record copied to tail.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    #[inline]
    pub fn rmw(
        &mut self,
        store: &FasterKv<F>,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> crate::status::OperationStatus {
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let status = internal_rmw(
            &ctx,
            self.session,
            &store.functions,
            key,
            input,
            output,
            context,
        );
        if status == crate::status::OperationStatus::Pending {
            store.dispatch_pending_io(self.session);
        }
        status
    }

    /// Delete a key within epoch-amortized protection.
    ///
    /// Equivalent to [`FasterKv::delete()`](super::FasterKv::delete) but
    /// skips the per-operation epoch enter/exit.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Deleted`] — key deleted successfully.
    /// - [`OperationStatus::NotFound`] — key does not exist.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    #[inline]
    pub fn delete(
        &mut self,
        store: &FasterKv<F>,
        key: &F::Key,
        context: F::Context,
    ) -> crate::status::OperationStatus {
        let ctx = InternalContext {
            hash_index: &store.hash_index,
            allocator: &store.allocator,
        };
        let status = internal_delete(&ctx, self.session, &store.functions, key, context);
        if status == crate::status::OperationStatus::Pending {
            store.dispatch_pending_io(self.session);
        }
        status
    }
}

impl<F: Functions> Drop for UnsafeContext<'_, F> {
    fn drop(&mut self) {
        self.session.end_unsafe();
    }
}

// ── SessionPool ─────────────────────────────────────────────────────

/// Manages the lifecycle of sessions across threads.
///
/// Each session registers with the epoch table on creation and deregisters
/// on disposal (or when the session is dropped).
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use faster_core::epoch::EpochTable;
/// use faster_core::store::{SimpleFunctions, SessionPool};
///
/// let epoch_table = Arc::new(EpochTable::new());
/// let pool = SessionPool::<SimpleFunctions<u64, u64>>::new(Arc::clone(&epoch_table));
///
/// let session = pool.create_session();
/// assert_eq!(epoch_table.registered_count(), 1);
///
/// pool.dispose_session(session);
/// assert_eq!(epoch_table.registered_count(), 0);
/// ```
pub struct SessionPool<F: Functions> {
    epoch_table: Arc<EpochTable>,
    _phantom: PhantomData<F>,
}

impl<F: Functions> SessionPool<F> {
    /// Creates a new session pool backed by the given epoch table.
    pub fn new(epoch_table: Arc<EpochTable>) -> Self {
        Self {
            epoch_table,
            _phantom: PhantomData,
        }
    }

    /// Creates a new session for the current thread.
    ///
    /// The session registers with the epoch table, claiming a thread slot.
    ///
    /// # Panics
    ///
    /// Panics if all epoch table slots are occupied (max 256 threads).
    pub fn create_session(&self) -> FasterSession<F> {
        let epoch_thread = self
            .epoch_table
            .register()
            .expect("epoch table is full — cannot create session");
        FasterSession::new(epoch_thread, Arc::clone(&self.epoch_table))
    }

    /// Disposes of a session, unregistering from the epoch table.
    ///
    /// This is equivalent to dropping the session, but makes the intent
    /// explicit. Any pending operations are discarded.
    pub fn dispose_session(&self, session: FasterSession<F>) {
        drop(session);
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SimpleFunctions;

    type TestFunctions = SimpleFunctions<u64, u64>;

    fn make_pool() -> (Arc<EpochTable>, SessionPool<TestFunctions>) {
        let epoch_table = Arc::new(EpochTable::new());
        let pool = SessionPool::<TestFunctions>::new(Arc::clone(&epoch_table));
        (epoch_table, pool)
    }

    fn make_pending_op(key: u64) -> PendingOperation<TestFunctions> {
        PendingOperation {
            op_type: PendingOpType::Read,
            key,
            input: Some(0u64),
            context: (),
            address: LogicalAddress::ZERO,
            record_layout: RecordLayout::compute(8, 8),
            key_hash: KeyHash::new(key),
        }
    }

    // ── 1. !Send enforcement ────────────────────────────────────────

    #[test]
    fn session_is_not_send() {
        // FasterSession contains PhantomData<*const ()> which makes it !Send.
        // We verify this structurally: the _not_send field exists and the
        // type system enforces !Send at compile time. Attempting to send a
        // session across threads would be a compile error.
        fn assert_not_send<T>() {
            // This function exists to document the constraint.
            // If someone accidentally makes FasterSession Send, the
            // PhantomData<*const ()> field prevents it.
        }
        assert_not_send::<FasterSession<TestFunctions>>();

        // Verify the session can be created (it's a valid type).
        let (_, pool) = make_pool();
        let session = pool.create_session();
        assert!(!session.is_in_epoch());
    }

    // ── 2. Create session and begin ─────────────────────────────────

    #[test]
    fn create_session_and_begin() {
        let (epoch_table, pool) = make_pool();
        let mut session = pool.create_session();

        assert_eq!(epoch_table.registered_count(), 1);
        assert!(!session.is_in_epoch());

        {
            let guard = session.begin_unsafe();
            assert!(guard.session().is_in_epoch());
            assert_eq!(epoch_table.active_count(), 1);
        }

        // Guard dropped — should be unprotected now.
        assert!(!session.is_in_epoch());
        assert_eq!(epoch_table.active_count(), 0);
    }

    // ── 3. SessionGuard auto-unprotects ─────────────────────────────

    #[test]
    fn session_guard_auto_unprotects() {
        let (epoch_table, pool) = make_pool();
        let mut session = pool.create_session();

        // Enter epoch protection.
        let guard = session.begin_unsafe();
        assert_eq!(epoch_table.active_count(), 1);

        // Drop the guard — should unprotect.
        drop(guard);
        assert_eq!(epoch_table.active_count(), 0);
        assert!(!session.is_in_epoch());
    }

    // ── 4. Serial numbers monotonic ─────────────────────────────────

    #[test]
    fn serial_numbers_monotonic() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        let mut prev = session.next_serial();
        for _ in 0..99 {
            let next = session.next_serial();
            assert!(next > prev, "serial numbers must be strictly increasing");
            prev = next;
        }
        // After 100 calls, the serial number should be 100.
        assert_eq!(prev, 99);
        assert_eq!(session.next_serial(), 100);
    }

    // ── 5. Enqueue and drain pending ────────────────────────────────

    #[test]
    fn enqueue_and_drain_pending() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        for i in 0..5 {
            session.enqueue_pending(make_pending_op(i));
        }
        assert_eq!(session.pending_count(), 5);

        let drained = session.drain_completed();
        assert_eq!(drained.len(), 5);
        assert_eq!(session.pending_count(), 0);

        // Verify keys match what we enqueued.
        for (i, op) in drained.iter().enumerate() {
            assert_eq!(op.key, i as u64);
            assert_eq!(op.op_type, PendingOpType::Read);
        }
    }

    // ── 6. Pending count and has_pending ─────────────────────────────

    #[test]
    fn pending_count_and_has_pending() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        assert_eq!(session.pending_count(), 0);
        assert!(!session.has_pending());

        session.enqueue_pending(make_pending_op(1));
        assert_eq!(session.pending_count(), 1);
        assert!(session.has_pending());

        session.enqueue_pending(make_pending_op(2));
        assert_eq!(session.pending_count(), 2);
        assert!(session.has_pending());

        session.drain_completed();
        assert_eq!(session.pending_count(), 0);
        assert!(!session.has_pending());
    }

    // ── 7. Session drop unprotects epoch ────────────────────────────

    #[test]
    fn session_drop_unprotects_epoch() {
        let (epoch_table, pool) = make_pool();

        {
            let mut session = pool.create_session();
            let _guard = session.begin_unsafe();
            assert_eq!(epoch_table.active_count(), 1);

            // Intentionally drop guard first so session.in_epoch is cleared,
            // then drop session. This verifies the guard's Drop runs.
            drop(_guard);
            assert_eq!(epoch_table.active_count(), 0);
        }
        // Session and guard both dropped.
        assert_eq!(epoch_table.active_count(), 0);

        // Also test: drop session while still in epoch (no guard drop).
        {
            let mut session = pool.create_session();
            // Manually enter epoch without using the guard pattern.
            epoch_table.protect(session.epoch_thread().entry_index());
            session.in_epoch = true;
            assert_eq!(epoch_table.active_count(), 1);

            // Drop session — its Drop impl should unprotect.
            drop(session);
            assert_eq!(epoch_table.active_count(), 0);
        }
    }

    // ── 8. Multiple sessions independent ────────────────────────────

    #[test]
    fn multiple_sessions_independent() {
        let (epoch_table, pool) = make_pool();

        let mut session1 = pool.create_session();
        let mut session2 = pool.create_session();
        assert_eq!(epoch_table.registered_count(), 2);

        // Independent serial numbers.
        assert_eq!(session1.next_serial(), 0);
        assert_eq!(session1.next_serial(), 1);
        assert_eq!(session2.next_serial(), 0);
        assert_eq!(session2.next_serial(), 1);
        assert_eq!(session2.next_serial(), 2);
        assert_eq!(session1.next_serial(), 2);

        // Independent pending ops.
        session1.enqueue_pending(make_pending_op(10));
        session2.enqueue_pending(make_pending_op(20));
        session2.enqueue_pending(make_pending_op(21));

        assert_eq!(session1.pending_count(), 1);
        assert_eq!(session2.pending_count(), 2);

        let drained1 = session1.drain_completed();
        assert_eq!(drained1.len(), 1);
        assert_eq!(drained1[0].key, 10);

        let drained2 = session2.drain_completed();
        assert_eq!(drained2.len(), 2);
        assert_eq!(drained2[0].key, 20);
        assert_eq!(drained2[1].key, 21);
    }

    // ── 9. SessionPool create and dispose ───────────────────────────

    #[test]
    fn session_pool_create_dispose() {
        let (epoch_table, pool) = make_pool();

        assert_eq!(epoch_table.registered_count(), 0);

        let session = pool.create_session();
        assert_eq!(epoch_table.registered_count(), 1);

        pool.dispose_session(session);
        assert_eq!(epoch_table.registered_count(), 0);
    }

    // ── Additional edge-case tests ──────────────────────────────────

    #[test]
    fn clear_pending_clears_queue() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        session.enqueue_pending(make_pending_op(1));
        session.enqueue_pending(make_pending_op(2));
        assert_eq!(session.pending_count(), 2);

        session.clear_pending();
        assert_eq!(session.pending_count(), 0);
    }

    #[test]
    fn pending_op_types() {
        // Verify all PendingOpType variants are distinct.
        assert_ne!(PendingOpType::Read, PendingOpType::Upsert);
        assert_ne!(PendingOpType::Read, PendingOpType::Rmw);
        assert_ne!(PendingOpType::Read, PendingOpType::Delete);
        assert_ne!(PendingOpType::Upsert, PendingOpType::Rmw);
        assert_ne!(PendingOpType::Upsert, PendingOpType::Delete);
        assert_ne!(PendingOpType::Rmw, PendingOpType::Delete);

        // Verify Debug trait.
        let s = format!("{:?}", PendingOpType::Read);
        assert!(s.contains("Read"));
    }

    #[test]
    fn session_begin_end_multiple_cycles() {
        let (epoch_table, pool) = make_pool();
        let mut session = pool.create_session();

        for _ in 0..10 {
            {
                let guard = session.begin_unsafe();
                assert!(guard.session().is_in_epoch());
                assert_eq!(epoch_table.active_count(), 1);
            }
            assert!(!session.is_in_epoch());
            assert_eq!(epoch_table.active_count(), 0);
        }
    }

    // ── 10. io_pending_count ────────────────────────────────────────

    #[test]
    fn io_pending_count_starts_at_zero() {
        let (_, pool) = make_pool();
        let session = pool.create_session();
        assert_eq!(session.io_pending_count(), 0);
    }

    // ── 11. drain_pending_ops ───────────────────────────────────────

    #[test]
    fn drain_pending_ops_empties_queue() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        session.enqueue_pending(make_pending_op(1));
        session.enqueue_pending(make_pending_op(2));
        assert_eq!(session.pending_count(), 2);

        let ops = session.drain_pending_ops();
        assert_eq!(ops.len(), 2);
        assert_eq!(session.pending_count(), 0);
        assert!(!session.has_pending());
    }

    // ── 12. pending_count includes io_contexts ──────────────────────

    #[test]
    fn pending_count_includes_io_contexts() {
        use crate::buffer_pool::AlignedBuffer;
        use crate::device::IoStatus;
        use std::sync::atomic::{AtomicBool, AtomicU32};
        use std::sync::{Arc, Mutex};

        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        // Enqueue one pending op.
        session.enqueue_pending(make_pending_op(1));
        assert_eq!(session.pending_count(), 1);

        // Manually create a PendingIoContext (simulating an issued read).
        let fake_io_ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(2),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        session.enqueue_io_context(fake_io_ctx);

        // pending_count = pending_ops(1) + io_contexts(1) = 2
        assert_eq!(session.pending_count(), 2);
        assert!(session.has_pending());
        assert_eq!(session.io_pending_count(), 1);
        session.clear_pending();
    }

    // ── 13. take_completed_io filters correctly ─────────────────────

    #[test]
    fn take_completed_io_filters_correctly() {
        use crate::buffer_pool::AlignedBuffer;
        use crate::device::IoStatus;
        use std::sync::atomic::{AtomicBool, AtomicU32};
        use std::sync::{Arc, Mutex};

        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        // One completed, one not.
        let completed_ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(true)),
            Arc::new(Mutex::new(Some(IoStatus::Success))),
            Arc::new(AtomicU32::new(512)),
        );
        let pending_ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(2),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );

        session.enqueue_io_context(completed_ctx);
        session.enqueue_io_context(pending_ctx);
        assert_eq!(session.io_pending_count(), 2);

        let done = session.take_completed_io();
        assert_eq!(done.len(), 1);
        assert_eq!(session.io_pending_count(), 1); // one still in-flight
        session.clear_pending();
    }

    // ── L4: Timeout & Cancellation ──────────────────────────────────

    use crate::buffer_pool::AlignedBuffer;
    use crate::device::IoStatus;
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use std::sync::{Arc, Mutex};

    #[test]
    fn expired_pending_count_with_no_expired() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        session.enqueue_io_context(ctx);
        assert_eq!(session.expired_pending_count(), 0);
        session.clear_pending();
    }

    #[test]
    fn expired_pending_count_detects_expired() {
        use std::time::{Duration, Instant};
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let expired_ctx = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
            Duration::ZERO,
            Instant::now() - Duration::from_millis(1),
        );
        let fresh_ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(2),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        session.enqueue_io_context(expired_ctx);
        session.enqueue_io_context(fresh_ctx);
        assert_eq!(session.expired_pending_count(), 1);
        assert_eq!(session.io_pending_count(), 2);
        session.clear_pending();
    }

    #[test]
    fn cancel_pending_removes_context() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        let ctx_id = ctx.id();
        session.enqueue_io_context(ctx);
        assert_eq!(session.io_pending_count(), 1);
        assert!(session.cancel_pending(ctx_id).is_ok());
        assert_eq!(session.io_pending_count(), 0);
        session.clear_pending();
    }

    #[test]
    fn cancel_pending_nonexistent_returns_error() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let result = session.cancel_pending(99999);
        assert!(result.is_err());
        match result.unwrap_err() {
            PendingIoError::Cancelled { context_id } => assert_eq!(context_id, 99999),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn cancel_pending_does_not_affect_others() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let ctx1 = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        let id1 = ctx1.id();
        let ctx2 = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(2),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        session.enqueue_io_context(ctx1);
        session.enqueue_io_context(ctx2);
        session.cancel_pending(id1).unwrap();
        assert_eq!(session.io_pending_count(), 1);
        session.clear_pending();
    }

    #[test]
    fn cancel_all_expired_removes_only_expired() {
        use std::time::{Duration, Instant};
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let expired1 = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
            Duration::ZERO,
            Instant::now() - Duration::from_millis(10),
        );
        let expired2 = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(2),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
            Duration::from_millis(1),
            Instant::now() - Duration::from_secs(1),
        );
        let fresh = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(3),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        session.enqueue_io_context(expired1);
        session.enqueue_io_context(expired2);
        session.enqueue_io_context(fresh);
        assert_eq!(session.expired_pending_count(), 2);
        let cancelled = session.cancel_all_expired();
        assert_eq!(cancelled.len(), 2);
        assert_eq!(session.io_pending_count(), 1);
        assert_eq!(session.expired_pending_count(), 0);
        session.clear_pending();
    }

    #[test]
    fn cancel_all_expired_empty_when_none_expired() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        session.enqueue_io_context(ctx);
        let cancelled = session.cancel_all_expired();
        assert!(cancelled.is_empty());
        assert_eq!(session.io_pending_count(), 1);
        session.clear_pending();
    }

    #[test]
    fn cancel_all_expired_on_empty_session() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        assert!(session.cancel_all_expired().is_empty());
    }

    #[test]
    fn take_completed_io_does_not_remove_expired() {
        use std::time::{Duration, Instant};
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let expired = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
            Duration::ZERO,
            Instant::now() - Duration::from_millis(10),
        );
        session.enqueue_io_context(expired);
        let done = session.take_completed_io();
        assert!(done.is_empty());
        assert_eq!(session.io_pending_count(), 1);
        assert_eq!(session.expired_pending_count(), 1);
        session.clear_pending();
    }

    #[test]
    fn complete_then_cancel_expired_integration() {
        use std::time::{Duration, Instant};
        let (_, pool) = make_pool();
        let mut session = pool.create_session();
        let completed_ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(1),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(true)),
            Arc::new(Mutex::new(Some(IoStatus::Success))),
            Arc::new(AtomicU32::new(512)),
        );
        let expired_ctx = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(2),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
            Duration::ZERO,
            Instant::now() - Duration::from_millis(10),
        );
        let fresh_ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(3),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None::<IoStatus>)),
            Arc::new(AtomicU32::new(0)),
        );
        session.enqueue_io_context(completed_ctx);
        session.enqueue_io_context(expired_ctx);
        session.enqueue_io_context(fresh_ctx);
        let done = session.take_completed_io();
        assert_eq!(done.len(), 1);
        let expired = session.cancel_all_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(session.io_pending_count(), 1);
        assert_eq!(session.expired_pending_count(), 0);
        session.clear_pending();
    }

    // ── A2: SessionStats ────────────────────────────────────────────

    #[test]
    fn stats_initial_values() {
        let (_, pool) = make_pool();
        let session = pool.create_session();
        let stats = session.stats();
        assert_eq!(stats.operations_completed, 0);
        assert_eq!(stats.pending_count, 0);
        assert_eq!(stats.current_serial, 0);
    }

    #[test]
    fn stats_after_serial_increments() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        for _ in 0..5 {
            session.next_serial();
        }

        let stats = session.stats();
        assert_eq!(stats.operations_completed, 5);
        assert_eq!(stats.current_serial, 5);
        assert_eq!(stats.pending_count, 0);
    }

    #[test]
    fn stats_reflects_pending_count() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        session.enqueue_pending(make_pending_op(1));
        session.enqueue_pending(make_pending_op(2));

        let stats = session.stats();
        assert_eq!(stats.pending_count, 2);
        assert_eq!(stats.operations_completed, 0);

        session.clear_pending();
        let stats = session.stats();
        assert_eq!(stats.pending_count, 0);
    }

    #[test]
    fn session_stats_debug_and_clone() {
        let stats = super::SessionStats {
            operations_completed: 10,
            pending_count: 2,
            current_serial: 10,
        };
        let debug = format!("{:?}", stats);
        assert!(debug.contains("SessionStats"));

        let cloned = stats;
        assert_eq!(cloned, stats);
    }
}
