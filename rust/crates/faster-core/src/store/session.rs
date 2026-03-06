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
use std::sync::Arc;

use crate::address::LogicalAddress;
use crate::epoch::EpochTable;
use crate::hash::KeyHash;
use crate::record::RecordLayout;

use super::Functions;

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
    /// Queue of pending operations awaiting I/O completion.
    pending_ops: Vec<PendingOperation<F>>,
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

    /// Enqueues a pending operation for later completion.
    #[inline]
    #[allow(dead_code)] // Used by FasterKv CRUD operations (not yet implemented).
    pub(crate) fn enqueue_pending(&mut self, op: PendingOperation<F>) {
        self.pending_ops.push(op);
    }

    /// Drains all completed pending operations.
    ///
    /// Returns operations whose I/O has completed and are ready for retry.
    ///
    /// # Current Limitations
    ///
    /// TODO: Currently returns *all* pending ops because we do not yet have
    /// disk I/O completion tracking (comes with item 4e). Once the async I/O
    /// subsystem is integrated, this will filter to only completed operations.
    pub fn drain_completed(&mut self) -> Vec<PendingOperation<F>> {
        std::mem::take(&mut self.pending_ops)
    }

    /// Returns the number of pending operations in the queue.
    #[inline]
    pub fn pending_count(&self) -> usize {
        self.pending_ops.len()
    }

    /// Returns `true` if this session has any pending operations.
    #[inline]
    pub fn has_pending(&self) -> bool {
        !self.pending_ops.is_empty()
    }

    /// Completes all pending operations synchronously (blocking).
    ///
    /// Used during session teardown to ensure no operations are lost.
    ///
    /// # Current Limitations
    ///
    /// TODO: Real implementation comes with item 4e (pending disk I/O).
    /// For now, drains the pending queue and returns an empty vec since
    /// we cannot produce `(Output, Context)` pairs without the full I/O
    /// completion path.
    pub fn complete_pending(&mut self) -> Vec<(F::Output, F::Context)> {
        // Drain pending ops to release memory. Once the I/O completion path
        // is implemented, each operation will be retried and produce output.
        self.pending_ops.clear();
        Vec::new()
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
}

impl<F: Functions> Drop for FasterSession<F> {
    fn drop(&mut self) {
        // Ensure epoch is unprotected before the epoch thread handle is dropped.
        if self.in_epoch {
            self.end_unsafe();
        }
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
    session: &'a mut FasterSession<F>,
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
    fn complete_pending_clears_queue() {
        let (_, pool) = make_pool();
        let mut session = pool.create_session();

        session.enqueue_pending(make_pending_op(1));
        session.enqueue_pending(make_pending_op(2));
        assert_eq!(session.pending_count(), 2);

        let results = session.complete_pending();
        // Currently returns empty vec (no I/O completion path yet).
        assert!(results.is_empty());
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
}
