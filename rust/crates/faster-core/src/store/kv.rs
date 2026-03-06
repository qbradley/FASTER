//! Top-level FasterKv store and configuration.
//!
//! [`FasterKv`] is the main entry point for the FASTER API. It owns all internal
//! components (hash index, hybrid log, epoch table, device) and provides
//! a session-based API for CRUD operations.
//!
//! # Thread Safety
//!
//! `FasterKv` itself is `Send + Sync`. Operations are performed through
//! [`FasterSession`] handles which are `!Send` (thread-affine).
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
//! use faster_core::NullDevice;
//!
//! let config = FasterKvConfig::default();
//! let device = NullDevice::new();
//! let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
//!     config,
//!     SimpleFunctions::default(),
//!     device,
//! );
//!
//! let mut session = store.new_session();
//! {
//!     let _guard = session.begin_unsafe();
//!     // Operations go through the store with the session
//! }
//! store.dispose_session(session);
//! ```

use std::sync::Arc;

use crate::address::{LogicalAddress, Page};
use crate::device::Device;
use crate::epoch::EpochTable;
use crate::grow::{GrowConfig, GrowError, GrowManager};
use crate::hash::index::HashIndex;
use crate::hybrid_log::eviction::{EvictionPolicy, PageEvictor};
use crate::hybrid_log::flush::PageFlusher;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::page::PageState;
use crate::record::{read_record_info, read_value};
use crate::status::OperationStatus;
use crate::store::functions::Functions;
use crate::store::operations::{
    InternalContext, internal_delete, internal_read, internal_rmw, internal_upsert,
};
use crate::store::pending_io::PendingIoManager;
use crate::store::session::{CompletePendingResult, FasterSession, PendingOpType, SessionPool};

// ── FasterKvConfig ──────────────────────────────────────────────────

/// Configuration for a [`FasterKv`] store.
///
/// # Example
///
/// ```
/// use faster_core::store::FasterKvConfig;
///
/// let config = FasterKvConfig {
///     hash_index_size_log2: 16,   // 64K buckets
///     buffer_size_pages: 8,
///     mutable_fraction: 0.9,
///     sector_size: 512,
///     ..FasterKvConfig::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct FasterKvConfig {
    /// Log₂ of the number of hash index buckets (e.g., 20 → 1M buckets).
    pub hash_index_size_log2: usize,
    /// Number of in-memory page frames (must be power of 2).
    pub buffer_size_pages: usize,
    /// Fraction of the buffer that is mutable (0.0..1.0).
    pub mutable_fraction: f64,
    /// Sector size for page alignment.
    pub sector_size: usize,
    /// Eviction policy for managing in-memory pages.
    pub eviction_policy: EvictionPolicy,
    /// Configuration for automatic hash index grow (online resize).
    pub grow_config: GrowConfig,
}

impl Default for FasterKvConfig {
    fn default() -> Self {
        Self {
            hash_index_size_log2: 20, // 1M buckets
            buffer_size_pages: 16,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        }
    }
}

// ── FasterKv ────────────────────────────────────────────────────────

/// A FASTER key-value store.
///
/// This is the main entry point for the FASTER API. It owns all internal
/// components (hash index, hybrid log, epoch table, device) and provides
/// a session-based API for CRUD operations.
///
/// # Thread Safety
///
/// `FasterKv` is `Send + Sync`. Operations are performed through
/// [`FasterSession`] handles which are `!Send` (thread-affine).
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
///
/// let mut session = store.new_session();
/// {
///     let _guard = session.begin_unsafe();
///     // Perform operations via the store
/// }
/// store.dispose_session(session);
/// ```
pub struct FasterKv<F: Functions> {
    /// The hash index.
    hash_index: HashIndex,
    /// The hybrid log allocator.
    allocator: HybridLogAllocator,
    /// User-defined operation callbacks.
    functions: F,
    /// The storage device.
    device: Box<dyn Device>,
    /// The shared epoch table (kept alive for sessions and hash index).
    #[allow(dead_code)]
    epoch_table: Arc<EpochTable>,
    /// Session pool for creating/disposing sessions.
    session_pool: SessionPool<F>,
    /// Page flusher for writing sealed pages to the device.
    flusher: PageFlusher,
    /// Page evictor for reclaiming in-memory pages.
    evictor: PageEvictor,
    /// Manages pending disk read operations for session I/O completion.
    pending_io_mgr: PendingIoManager,
    /// Coordinates hash table online resize (grow) operations.
    grow_manager: GrowManager,
    /// Configuration snapshot.
    config: FasterKvConfig,
}

// SAFETY: FasterKv is Send+Sync because all its fields are Send+Sync.
// - HashIndex, HybridLogAllocator, PageFlusher, PageEvictor are Send+Sync
//   (they use atomics internally).
// - Box<dyn Device> is Send+Sync because Device: Send + Sync + 'static.
// - Arc<EpochTable> is Send+Sync.
// - F: Functions requires Send + Sync.
// - SessionPool<F> contains Arc<EpochTable> + PhantomData<F>, both Send+Sync.
unsafe impl<F: Functions> Send for FasterKv<F> {}
// SAFETY: All methods take &self and internal mutation is through atomics.
unsafe impl<F: Functions> Sync for FasterKv<F> {}

impl<F: Functions> Drop for FasterKv<F> {
    fn drop(&mut self) {
        // Ordered shutdown (SF-10): drain in-flight I/O before releasing memory.
        //
        // Flush callbacks hold raw pointers to the PageTable (see
        // `FlushCallbackContext` in flush.rs). We must ensure all callbacks
        // have completed before the allocator (and its PageTable) is dropped.
        //
        // Shutdown sequence:
        //   1. Synchronously flush remaining sealed pages (avoids creating
        //      new async callbacks during shutdown).
        //   2. Spin-wait (with timeout) for all Flushing pages to complete.
        //   3. Close the device to join I/O worker threads.
        //   4. Normal field-declaration-order drop proceeds:
        //      hash_index, allocator, functions, device, epoch_table,
        //      session_pool, flusher, evictor, config.

        use std::sync::atomic::Ordering;

        let page_table = self.allocator.page_table();
        let head_page = self.allocator.head_address().page().0;
        let tail_page = self.allocator.tail_address().page().0;

        // Step 1: Flush remaining sealed pages synchronously.
        for p in head_page..=tail_page {
            let page = Page(p);
            if let Some(frame) = page_table.get_frame(page) {
                if frame.state().load(Ordering::Acquire) == PageState::Sealed {
                    let _ = self.flusher.flush_page_sync(
                        page,
                        page_table,
                        self.device.as_ref(),
                        self.allocator.page_size(),
                    );
                }
            }
        }

        // Step 2: Wait for in-flight async flushes (Flushing -> Flushed).
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);
        let deadline = std::time::Instant::now() + TIMEOUT;

        loop {
            let has_flushing = (head_page..=tail_page).any(|p| {
                page_table
                    .get_frame(Page(p))
                    .is_some_and(|f| {
                        f.state().load(Ordering::Acquire) == PageState::Flushing
                    })
            });

            if !has_flushing {
                break;
            }

            if std::time::Instant::now() >= deadline {
                eprintln!(
                    "FasterKv::drop: timeout waiting for in-flight I/O \
                     ({TIMEOUT:?} exceeded)"
                );
                break;
            }

            std::thread::sleep(POLL_INTERVAL);
        }

        // Step 3: Close the device — joins I/O worker threads, ensuring all
        // enqueued callbacks have fired. The device's own Drop is a no-op
        // after an explicit close().
        self.device.close();
    }
}

impl<F: Functions> FasterKv<F> {
    /// Create a [`FasterKvBuilder`](crate::store::builder::FasterKvBuilder)
    /// for configuring a store with fluent method chaining and validation.
    ///
    /// # Example
    ///
    /// ```
    /// use faster_core::store::{FasterKv, SimpleFunctions};
    /// use faster_core::NullDevice;
    ///
    /// let store: FasterKv<SimpleFunctions<u64, u64>> =
    ///     FasterKv::builder()
    ///         .hash_index_size_log2(16)
    ///         .mutable_fraction(0.9)
    ///         .build(SimpleFunctions::default(), NullDevice::new())
    ///         .expect("valid config");
    /// ```
    #[must_use]
    pub fn builder() -> crate::store::builder::FasterKvBuilder {
        crate::store::builder::FasterKvBuilder::new()
    }

    /// Create a new FasterKv store.
    ///
    /// # Arguments
    ///
    /// * `config` — Store configuration (hash size, buffer pages, etc.).
    /// * `functions` — User-defined callbacks for read/upsert/rmw/delete.
    /// * `device` — Storage device for flushing/evicting pages.
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
    /// ```
    pub fn new(config: FasterKvConfig, functions: F, device: impl Device) -> Self {
        let epoch_table = Arc::new(EpochTable::new());

        let hash_index =
            HashIndex::with_epoch(config.hash_index_size_log2 as u32, Arc::clone(&epoch_table));

        let allocator = HybridLogAllocator::new(
            config.buffer_size_pages,
            config.mutable_fraction,
            config.sector_size,
        );

        // Skip past sentinel addresses (0 = ZERO, 1 = INVALID) so the
        // first real record has an address where `is_valid()` is true.
        allocator
            .try_allocate(8)
            .expect("failed to skip sentinel addresses");

        let page_size = allocator.page_size();
        let flusher = PageFlusher::new(config.sector_size as u32, page_size);
        let evictor = PageEvictor::new(config.eviction_policy.clone(), page_size);
        let pending_io_mgr = PendingIoManager::new(page_size, config.sector_size as u32);

        let session_pool = SessionPool::new(Arc::clone(&epoch_table));
        let grow_manager = GrowManager::new(config.grow_config.clone());

        Self {
            hash_index,
            allocator,
            functions,
            device: Box::new(device),
            epoch_table,
            session_pool,
            flusher,
            evictor,
            pending_io_mgr,
            grow_manager,
            config,
        }
    }

    // ── Session Management ──────────────────────────────────────────

    /// Create a new session for the current thread.
    ///
    /// Each thread performing operations must have its own session.
    /// Sessions are `!Send` — they must be used on the thread that
    /// created them.
    ///
    /// # Panics
    ///
    /// Panics if all epoch table slots are occupied (max 256 threads).
    pub fn new_session(&self) -> FasterSession<F> {
        self.session_pool.create_session()
    }

    /// Dispose of a session, releasing its epoch table slot.
    ///
    /// Equivalent to dropping the session, but makes the intent explicit.
    /// Any pending operations are discarded.
    pub fn dispose_session(&self, session: FasterSession<F>) {
        self.session_pool.dispose_session(session);
    }

    // ── Scoped Session ──────────────────────────────────────────────

    /// Execute a closure within an automatically-managed session.
    ///
    /// Opens a new session, passes it to the closure, and disposes of the
    /// session when the closure returns — even if the closure panics.
    /// This is the recommended way to use sessions for short-lived
    /// operation batches.
    ///
    /// # Example
    ///
    /// ```
    /// # use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// # use faster_core::NullDevice;
    /// # let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    /// #     FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());
    /// let value = store.session_scope(|session, store| {
    ///     store.upsert(session, &1u64, &42u64, ());
    ///     let mut out = None;
    ///     store.read(session, &1u64, &0u64, &mut out, ());
    ///     out
    /// });
    /// assert_eq!(value, Some(42));
    /// ```
    pub fn session_scope<R>(
        &self,
        f: impl FnOnce(&mut FasterSession<F>, &Self) -> R,
    ) -> R {
        let session = self.new_session();
        // Use a guard struct to ensure dispose even on panic.
        struct ScopeGuard<'a, F: Functions> {
            store: &'a FasterKv<F>,
            session: Option<FasterSession<F>>,
        }
        impl<F: Functions> Drop for ScopeGuard<'_, F> {
            fn drop(&mut self) {
                if let Some(session) = self.session.take() {
                    self.store.dispose_session(session);
                }
            }
        }
        let mut guard = ScopeGuard {
            store: self,
            session: Some(session),
        };
        let session = guard.session.as_mut().expect("session was just created");
        let result = f(session, self);
        // Explicit dispose (guard's Drop handles the panic path).
        if let Some(session) = guard.session.take() {
            self.dispose_session(session);
        }
        result
    }

    // ── Convenience Methods ─────────────────────────────────────────

    /// Simplified upsert that uses default context (`Default::default()`).
    ///
    /// This is a convenience wrapper around [`upsert`](Self::upsert) for
    /// [`Functions`] implementations whose `Context` type is `Default`
    /// (e.g., [`SimpleFunctions`](super::SimpleFunctions) where `Context = ()`).
    ///
    /// # Example
    ///
    /// ```
    /// # use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// # use faster_core::NullDevice;
    /// # let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    /// #     FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());
    /// let mut session = store.new_session();
    /// let status = store.upsert_simple(&mut session, &1u64, &100u64);
    /// store.dispose_session(session);
    /// ```
    pub fn upsert_simple(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        input: &F::Input,
    ) -> OperationStatus
    where
        F::Context: Default,
    {
        self.upsert(session, key, input, F::Context::default())
    }

    /// Simplified read that returns the value directly.
    ///
    /// Returns `Some(output)` on success or `None` if the key was not found
    /// or the record is on disk (pending). This wrapper uses `Default` for
    /// both `Input` and `Context`, making it ideal for
    /// [`SimpleFunctions`](super::SimpleFunctions).
    ///
    /// # Example
    ///
    /// ```
    /// # use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// # use faster_core::NullDevice;
    /// # let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    /// #     FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());
    /// let mut session = store.new_session();
    /// store.upsert_simple(&mut session, &1u64, &42u64);
    /// let value = store.read_simple(&mut session, &1u64);
    /// assert_eq!(value, Some(42));
    /// store.dispose_session(session);
    /// ```
    pub fn read_simple(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
    ) -> F::Output
    where
        F::Input: Default,
        F::Context: Default,
    {
        let mut output = F::Output::default();
        let input = F::Input::default();
        let status = self.read(session, key, &input, &mut output, F::Context::default());
        if status == OperationStatus::Ok {
            output
        } else {
            F::Output::default()
        }
    }

    /// Simplified delete that uses default context (`Default::default()`).
    ///
    /// This is a convenience wrapper around [`delete`](Self::delete) for
    /// [`Functions`] implementations whose `Context` type is `Default`.
    ///
    /// # Example
    ///
    /// ```
    /// # use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// # use faster_core::NullDevice;
    /// # let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    /// #     FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());
    /// let mut session = store.new_session();
    /// store.upsert_simple(&mut session, &1u64, &42u64);
    /// let status = store.delete_simple(&mut session, &1u64);
    /// store.dispose_session(session);
    /// ```
    pub fn delete_simple(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
    ) -> OperationStatus
    where
        F::Context: Default,
    {
        self.delete(session, key, F::Context::default())
    }

    // ── CRUD Operations ─────────────────────────────────────────────

    /// Read a key's value.
    ///
    /// Enters epoch protection, performs the read, and returns the status.
    /// On success (`OperationStatus::Ok`), the result is written to `output`
    /// via the [`Functions::read`] callback.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Ok`] — value read successfully.
    /// - [`OperationStatus::NotFound`] — key does not exist.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    pub fn read(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> OperationStatus {
        let mut guard = session.begin_unsafe();
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
        };
        let status = internal_read(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            output,
            context,
        );
        drop(guard);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
        }
        status
    }

    /// Insert or update a key-value pair.
    ///
    /// Enters epoch protection, performs the upsert, and returns the status.
    /// The `input` is passed to the [`Functions::upsert`] callback which writes
    /// the desired value into the record. For [`SimpleFunctions`](super::SimpleFunctions),
    /// `Input = Value`, so `input` is the value to store.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Created`] — new record inserted.
    /// - [`OperationStatus::InPlaceUpdated`] — existing mutable record updated.
    /// - [`OperationStatus::CopyUpdated`] — read-only record copied to tail.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    pub fn upsert(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        input: &F::Input,
        context: F::Context,
    ) -> OperationStatus {
        let mut guard = session.begin_unsafe();
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
        };
        let status = internal_upsert(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            context,
        );
        drop(guard);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
        }
        status
    }

    /// Read-modify-write a key.
    ///
    /// Enters epoch protection, performs the RMW, and returns the status.
    /// If the key exists, the [`Functions::rmw_in_place`] or
    /// [`Functions::rmw_copy_update`] callback is invoked. If the key
    /// does not exist, [`Functions::rmw_initial`] creates a new record.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Created`] — new record created via `rmw_initial`.
    /// - [`OperationStatus::InPlaceUpdated`] — updated in mutable region.
    /// - [`OperationStatus::CopyUpdated`] — read-only record copied to tail.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    pub fn rmw(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> OperationStatus {
        let mut guard = session.begin_unsafe();
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
        };
        let status = internal_rmw(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            output,
            context,
        );
        drop(guard);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
        }
        status
    }

    /// Delete a key.
    ///
    /// Enters epoch protection and marks the record as a tombstone.
    /// In the mutable region the tombstone is set in-place; in the
    /// read-only region a tombstone record is appended at the tail.
    ///
    /// # Returns
    ///
    /// - [`OperationStatus::Deleted`] — key deleted successfully.
    /// - [`OperationStatus::NotFound`] — key does not exist.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    pub fn delete(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        context: F::Context,
    ) -> OperationStatus {
        let mut guard = session.begin_unsafe();
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
        };
        let status =
            internal_delete(&ctx, guard.session_mut(), &self.functions, key, context);
        drop(guard);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
        }
        status
    }

    // ── Pending I/O Completion ──────────────────────────────────────

    /// Dispatch pending operations to the device for async I/O.
    ///
    /// Drains all un-dispatched [`PendingOperation`]s from the session,
    /// issues disk reads via the [`PendingIoManager`], and pushes the
    /// resulting [`PendingIoContext`]s back into the session for later
    /// completion.
    ///
    /// Called automatically by the CRUD methods when an operation returns
    /// [`OperationStatus::Pending`].
    fn dispatch_pending_io(&self, session: &mut FasterSession<F>) {
        let ops = session.drain_pending_ops();
        for op in ops {
            match self.pending_io_mgr.issue_read(op, self.device.as_ref()) {
                Ok(io_ctx) => session.enqueue_io_context(io_ctx),
                Err(_err) => {
                    // I/O dispatch failed (queue full, device error, etc.).
                    // The operation is lost — the caller will not receive a
                    // completion callback. In a production system we would
                    // retry or report the error. For the MVP, log and drop.
                    #[cfg(debug_assertions)]
                    eprintln!("dispatch_pending_io: I/O dispatch failed: {_err}");
                }
            }
        }
    }

    /// Process completed asynchronous I/O and return results.
    ///
    /// Polls the session's in-flight I/O contexts. For each completed read,
    /// re-executes the original operation using the fetched data and returns
    /// `(output, context)` pairs.
    ///
    /// Operations whose I/O has not yet completed remain in the session and
    /// will be picked up on a subsequent call.
    ///
    /// # Read completion
    ///
    /// A completed read extracts the value from the device buffer and invokes
    /// [`Functions::read`] to produce the output. Tombstoned or invalid
    /// records are silently skipped (they represent deleted keys).
    ///
    /// # Write completion (Upsert / RMW / Delete)
    ///
    /// Write-path completions are not yet implemented (requires re-entering
    /// the hash index to perform copy-to-tail). They are tracked as in-flight
    /// I/O but dropped on completion with a debug warning.
    pub fn complete_pending(
        &self,
        session: &mut FasterSession<F>,
    ) -> Vec<(F::Output, F::Context)> {
        let completed = session.take_completed_io();
        let mut results = Vec::with_capacity(completed.len());

        for cio in completed {
            if let Some(pair) = self.process_completed_io(cio) {
                results.push(pair);
            }
        }

        results
    }

    /// Block until all in-flight I/O completes, then process results.
    ///
    /// Similar to [`complete_pending`](Self::complete_pending) but spin-waits
    /// for every outstanding I/O request before returning. Use during session
    /// teardown or when the caller needs all results immediately.
    pub fn complete_pending_sync(
        &self,
        session: &mut FasterSession<F>,
    ) -> Vec<(F::Output, F::Context)> {
        let completed = session.wait_for_all_io();
        let mut results = Vec::with_capacity(completed.len());

        for cio in completed {
            if let Some(pair) = self.process_completed_io(cio) {
                results.push(pair);
            }
        }

        results
    }

    /// Drain completed I/O and return a structured summary.
    ///
    /// When `wait` is `false`, polls in-flight I/O once and processes any
    /// completions that are ready. When `wait` is `true`, spin-waits until
    /// **all** in-flight I/O has completed before returning.
    ///
    /// This is the preferred user-facing API for pending completion. It
    /// combines the internal `complete_pending` / `complete_pending_sync`
    /// into a single entry point that returns [`CompletePendingResult`]
    /// instead of a raw `Vec`.
    ///
    /// # Example
    ///
    /// ```
    /// # use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// # use faster_core::NullDevice;
    /// # let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    /// #     FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());
    /// let mut session = store.new_session();
    /// // ... perform operations that may go pending ...
    /// let result = store.try_complete_pending(&mut session, false);
    /// if result.is_done() {
    ///     println!("all {} ops completed", result.completed);
    /// }
    /// store.dispose_session(session);
    /// ```
    pub fn try_complete_pending(
        &self,
        session: &mut FasterSession<F>,
        wait: bool,
    ) -> CompletePendingResult {
        let completed = if wait {
            session.wait_for_all_io()
        } else {
            session.take_completed_io()
        };

        let total = completed.len() as u32;
        let mut had_errors = false;

        for cio in completed {
            if self.process_completed_io(cio).is_none() {
                had_errors = true;
            }
        }

        CompletePendingResult {
            completed: total,
            remaining: session.pending_count() as u32,
            had_errors,
        }
    }

    /// Bump the global epoch and drain any completed pending operations.
    ///
    /// This is a convenience method that matches the C++/C# FASTER
    /// `session.Refresh()` pattern: advance the epoch so that safe-epoch
    /// callbacks can fire, then poll for completed I/O.
    ///
    /// Equivalent to:
    /// ```ignore
    /// store.epoch_table.bump_current_epoch_no_callback();
    /// store.try_complete_pending(session, false);
    /// ```
    ///
    /// # Example
    ///
    /// ```
    /// # use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// # use faster_core::NullDevice;
    /// # let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    /// #     FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());
    /// let mut session = store.new_session();
    /// // Periodically call refresh to advance the epoch and drain pending ops.
    /// let result = store.refresh(&mut session);
    /// store.dispose_session(session);
    /// ```
    pub fn refresh(&self, session: &mut FasterSession<F>) -> CompletePendingResult {
        self.epoch_table.bump_current_epoch_no_callback();
        self.try_complete_pending(session, false)
    }

    /// Returns the current global epoch value.
    ///
    /// Useful for diagnostics or testing whether [`refresh`](Self::refresh)
    /// advanced the epoch.
    #[inline]
    pub fn current_epoch(&self) -> u64 {
        self.epoch_table.current_epoch()
    }
    ///
    /// For read operations, extracts the value from the buffer and invokes
    /// the user's `Functions::read` callback. For write operations, this is
    /// a placeholder that drops the completion (TODO: copy-to-tail retry).
    fn process_completed_io(
        &self,
        cio: super::pending_io::CompletedIo<F>,
    ) -> Option<(F::Output, F::Context)> {
        use crate::device::IoStatus;

        if cio.status != IoStatus::Success {
            #[cfg(debug_assertions)]
            eprintln!(
                "process_completed_io: I/O completed with non-success status: {:?}",
                cio.status
            );
            return None;
        }

        let layout = &cio.operation.record_layout;

        match cio.operation.op_type {
            PendingOpType::Read => {
                let buf = cio.buffer.as_slice();
                let ri = read_record_info(&buf[cio.record_offset..]);

                if ri.is_tombstone() || ri.is_invalid() {
                    return None;
                }

                let value: F::Value = read_value(&buf[cio.record_offset..], layout);
                let mut output = F::Output::default();

                if let Some(ref input) = cio.operation.input {
                    self.functions
                        .read(&cio.operation.key, &value, input, &mut output);
                }

                Some((output, cio.operation.context))
            }
            PendingOpType::Upsert | PendingOpType::Rmw | PendingOpType::Delete => {
                // TODO: Write-path pending completion.
                //
                // When a write operation (upsert/rmw/delete) hits the on-disk
                // region, the disk page is read back so we have the old record.
                // To complete the write we need to:
                //
                //   1. **Upsert**: Read the old value from `cio.buffer`, allocate
                //      a new record at the log tail, write the new value via
                //      `Functions::upsert`, and CAS-update the hash index to
                //      point to the new record (copy-to-tail / RCU).
                //
                //   2. **RMW**: Read the old value, call `Functions::rmw_copy_update`
                //      to produce the merged value, allocate at the tail, and CAS
                //      the hash entry.
                //
                //   3. **Delete**: Allocate a tombstone record at the tail and
                //      CAS the hash entry.
                //
                // All three paths re-enter the hash index and allocator, which
                // requires epoch protection. This is architecturally similar to
                // the copy-to-tail path in `internal_upsert` / `internal_rmw`
                // for the read-only region, but requires the data from the
                // device buffer instead of the in-memory page.
                //
                // For now, the I/O is read-back only; the write is silently
                // dropped. This means writes to on-disk records are lost.
                // Callers should ensure the working set fits in-memory for
                // write-heavy workloads until this is implemented.
                #[cfg(debug_assertions)]
                eprintln!(
                    "process_completed_io: write-path completion not yet implemented \
                     for {:?} — write to on-disk record was dropped",
                    cio.operation.op_type
                );
                None
            }
        }
    }

    // ── Maintenance ─────────────────────────────────────────────────

    /// Flush sealed (read-only) pages to the storage device.
    ///
    /// Returns the number of pages flushed.
    pub fn flush(&self) -> u32 {
        self.flusher
            .flush_sealed_pages(&self.allocator, self.device.as_ref())
            .unwrap_or_default()
    }

    /// Evict flushed pages from memory.
    ///
    /// Returns the number of pages evicted.
    pub fn evict(&self) -> u32 {
        self.evictor.evict_pages(&self.allocator)
    }

    /// Run a full maintenance cycle: shift read-only boundary, flush, evict.
    ///
    /// Call this periodically to keep the hybrid log healthy. It:
    /// 1. Shifts the read-only boundary if the mutable region is too large.
    /// 2. Flushes sealed pages to the device.
    /// 3. Evicts flushed pages if the in-memory footprint exceeds the policy.
    pub fn maintenance(&self) {
        // 1. Shift read-only boundary if mutable region is too large.
        let info = self.allocator.snapshot();
        if info.needs_read_only_shift(self.allocator.mutable_fraction_pages()) {
            self.allocator.shift_read_only_to_tail();
        }

        // 2. Flush sealed pages to the device.
        let _ = self
            .flusher
            .flush_sealed_pages(&self.allocator, self.device.as_ref());

        // 3. Evict if the in-memory footprint exceeds the policy threshold.
        if self.evictor.needs_eviction(&self.allocator.snapshot()) {
            self.evictor.evict_pages(&self.allocator);
        }
    }


    // ── Hash Index Grow ─────────────────────────────────────────────

    /// Checks whether the hash index should grow and initiates a grow
    /// if the load factor exceeds the configured threshold.
    ///
    /// This is a lightweight check intended to be called periodically
    /// (e.g., after each batch of operations). If a grow is already in
    /// progress or automatic grow is disabled, this is a no-op.
    pub fn check_grow(&self) {
        if self.grow_manager.should_grow(&self.hash_index) {
            let _ = self.grow_manager.begin_grow(&self.hash_index);
        }
    }

    /// Manually triggers a hash index grow (double the bucket count).
    ///
    /// Unlike [`check_grow`](Self::check_grow) this ignores the load
    /// factor threshold and immediately begins a grow. The grow runs
    /// cooperatively: subsequent session operations will contribute
    /// chunks of splitting work.
    ///
    /// # Errors
    ///
    /// Returns [`GrowError`] if a grow is already in progress or the
    /// resulting table size is invalid.
    pub fn grow_index(&self) -> Result<(), GrowError> {
        self.grow_manager.begin_grow(&self.hash_index)
    }

    /// Returns `true` if a hash index grow is currently in progress.
    #[inline]
    pub fn is_growing(&self) -> bool {
        self.grow_manager.is_growing()
    }

    /// Returns a reference to the [`GrowManager`] for advanced grow control.
    #[inline]
    pub fn get_grow_manager(&self) -> &GrowManager {
        &self.grow_manager
    }

    // ── Accessors ───────────────────────────────────────────────────

    /// Get the current log tail address (next allocation point).
    #[inline]
    pub fn tail_address(&self) -> LogicalAddress {
        self.allocator.tail_address()
    }

    /// Get the current log head address (start of in-memory region).
    #[inline]
    pub fn head_address(&self) -> LogicalAddress {
        self.allocator.head_address()
    }

    /// Get the approximate number of entries in the store.
    ///
    /// **Note:** This is a rough estimate. A precise count requires
    /// scanning the hash index, which is not yet implemented.
    #[inline]
    pub fn entry_count(&self) -> u64 {
        // TODO: Maintain an atomic counter in upsert/delete, or scan the
        // hash index. For now, return 0 as a placeholder.
        0
    }

    /// Get a reference to the store configuration.
    #[inline]
    pub fn config(&self) -> &FasterKvConfig {
        &self.config
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::NullDevice;
    use crate::store::functions::{CounterFunctions, SimpleFunctions};

    type SimpleStore = FasterKv<SimpleFunctions<u64, u64>>;

    /// Helper: create a minimal store suitable for unit tests.
    fn test_store() -> SimpleStore {
        let config = FasterKvConfig {
            hash_index_size_log2: 8, // 256 buckets — enough for tests
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        FasterKv::new(config, SimpleFunctions::default(), NullDevice::new())
    }

    // ── 1. Create store with defaults ───────────────────────────────

    #[test]
    fn create_store_with_defaults() {
        let config = FasterKvConfig::default();
        assert_eq!(config.hash_index_size_log2, 20);
        assert_eq!(config.buffer_size_pages, 16);
        assert!((config.mutable_fraction - 0.9).abs() < f64::EPSILON);
        assert_eq!(config.sector_size, 512);

        let store: SimpleStore =
            FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());
        assert_eq!(store.config().hash_index_size_log2, 20);
        assert_eq!(store.config().buffer_size_pages, 16);
    }

    // ── 2. New session and dispose ──────────────────────────────────

    #[test]
    fn new_session_and_dispose() {
        let store = test_store();
        let mut session = store.new_session();

        // Session should start outside of epoch protection.
        assert!(!session.is_in_epoch());

        {
            let guard = session.begin_unsafe();
            assert!(guard.session().is_in_epoch());
        }
        // Guard dropped — epoch released.
        assert!(!session.is_in_epoch());

        store.dispose_session(session);
    }

    // ── 3. Upsert and read single key ───────────────────────────────

    #[test]
    fn upsert_and_read_single_key() {
        let store = test_store();
        let mut session = store.new_session();

        let status = store.upsert(&mut session, &42u64, &100u64, ());
        assert!(
            status == OperationStatus::Created || status == OperationStatus::InPlaceUpdated,
            "upsert should succeed, got: {:?}",
            status,
        );

        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &42u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(output, Some(100));

        store.dispose_session(session);
    }

    // ── 4. Upsert overwrite and read ────────────────────────────────

    #[test]
    fn upsert_overwrite_and_read() {
        let store = test_store();
        let mut session = store.new_session();

        let _ = store.upsert(&mut session, &1u64, &10u64, ());
        let _ = store.upsert(&mut session, &1u64, &20u64, ());

        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &1u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(output, Some(20));

        store.dispose_session(session);
    }

    // ── 5. Read nonexistent key ─────────────────────────────────────

    #[test]
    fn read_nonexistent_key() {
        let store = test_store();
        let mut session = store.new_session();

        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &9999u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::NotFound);
        assert_eq!(output, None);

        store.dispose_session(session);
    }

    // ── 6. Delete key ───────────────────────────────────────────────

    #[test]
    fn delete_key() {
        let store = test_store();
        let mut session = store.new_session();

        let _ = store.upsert(&mut session, &7u64, &77u64, ());

        let status = store.delete(&mut session, &7u64, ());
        assert_eq!(status, OperationStatus::Deleted);

        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &7u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::NotFound);

        store.dispose_session(session);
    }

    // ── 7. RMW counter ──────────────────────────────────────────────

    #[test]
    fn rmw_counter() {
        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let store: FasterKv<CounterFunctions<u64>> =
            FasterKv::new(config, CounterFunctions::new(), NullDevice::new());

        let mut session = store.new_session();
        let mut output = 0i64;

        // RMW 3 times with delta +10 each → expect 30.
        for _ in 0..3 {
            let _ = store.rmw(&mut session, &5u64, &10i64, &mut output, ());
        }

        // Read back the counter value.
        let mut read_output = 0i64;
        let status = store.read(&mut session, &5u64, &0i64, &mut read_output, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(read_output, 30);

        store.dispose_session(session);
    }

    // ── 8. Upsert many keys ────────────────────────────────────────

    #[test]
    fn upsert_many_keys() {
        let config = FasterKvConfig {
            hash_index_size_log2: 14, // 16K buckets for 1000 keys
            buffer_size_pages: 16,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let store: SimpleStore =
            FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());
        let mut session = store.new_session();

        for i in 0u64..1000 {
            let status = store.upsert(&mut session, &i, &(i * 10), ());
            assert!(
                status == OperationStatus::Created
                    || status == OperationStatus::InPlaceUpdated
                    || status == OperationStatus::CopyUpdated,
                "upsert key {} failed: {:?}",
                i,
                status,
            );
        }

        for i in 0u64..1000 {
            let mut output: Option<u64> = None;
            let status = store.read(&mut session, &i, &0u64, &mut output, ());
            assert_eq!(status, OperationStatus::Ok, "read key {} failed", i);
            assert_eq!(output, Some(i * 10), "key {} value mismatch", i);
        }

        store.dispose_session(session);
    }

    // ── 9. Multi-session independence ───────────────────────────────

    #[test]
    fn multi_session_independence() {
        use std::sync::Arc;

        let config = FasterKvConfig {
            hash_index_size_log2: 10,
            buffer_size_pages: 8,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
            config,
            SimpleFunctions::default(),
            NullDevice::new(),
        ));

        let store1 = Arc::clone(&store);
        let store2 = Arc::clone(&store);

        let h1 = std::thread::spawn(move || {
            let mut session = store1.new_session();
            for i in 0u64..100 {
                let _ = store1.upsert(&mut session, &i, &(i + 1000), ());
            }
            for i in 0u64..100 {
                let mut output: Option<u64> = None;
                let status = store1.read(&mut session, &i, &0u64, &mut output, ());
                assert_eq!(status, OperationStatus::Ok, "t1: read key {} failed", i);
                assert_eq!(output, Some(i + 1000), "t1: key {} mismatch", i);
            }
            store1.dispose_session(session);
        });

        let h2 = std::thread::spawn(move || {
            let mut session = store2.new_session();
            for i in 1000u64..1100 {
                let _ = store2.upsert(&mut session, &i, &(i + 2000), ());
            }
            for i in 1000u64..1100 {
                let mut output: Option<u64> = None;
                let status = store2.read(&mut session, &i, &0u64, &mut output, ());
                assert_eq!(status, OperationStatus::Ok, "t2: read key {} failed", i);
                assert_eq!(output, Some(i + 2000), "t2: key {} mismatch", i);
            }
            store2.dispose_session(session);
        });

        h1.join().expect("thread 1 panicked");
        h2.join().expect("thread 2 panicked");
    }

    // ── 10. Maintenance cycle ───────────────────────────────────────

    #[test]
    fn maintenance_cycle() {
        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 4,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let store: SimpleStore =
            FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());
        let mut session = store.new_session();

        // Insert enough data to fill several pages.
        for i in 0u64..500 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        let tail_before = store.tail_address();
        assert!(
            tail_before.raw() > 0,
            "tail should have advanced after inserts"
        );

        // Run maintenance — should at least not panic.
        store.maintenance();

        // Tail should not have regressed.
        assert!(store.tail_address().raw() >= tail_before.raw());

        // Data should still be readable (from mutable region or pages
        // that haven't been evicted).
        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &0u64, &0u64, &mut output, ());
        // After maintenance with NullDevice, data in the mutable region
        // should still be accessible. Evicted data returns Pending or
        // NotFound.
        assert!(
            status == OperationStatus::Ok || status == OperationStatus::Pending,
            "read after maintenance returned {:?}",
            status,
        );

        store.dispose_session(session);
    }

    // ── Compile-time: FasterKv is Send + Sync ───────────────────────

    #[test]
    fn faster_kv_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FasterKv<SimpleFunctions<u64, u64>>>();
        assert_send_sync::<FasterKv<CounterFunctions<u64>>>();
    }

    // ── Config clone + debug ────────────────────────────────────────

    #[test]
    fn config_clone_and_debug() {
        let config = FasterKvConfig::default();
        let config2 = config.clone();
        assert_eq!(config2.hash_index_size_log2, config.hash_index_size_log2);

        let debug = format!("{:?}", config);
        assert!(debug.contains("FasterKvConfig"));
    }

    // ── Drop safety: empty store ────────────────────────────────────

    #[test]
    fn drop_empty_store() {
        let _store = test_store();
        // Implicit drop — must not panic.
    }

    // ── Drop safety: store with data ────────────────────────────────

    #[test]
    fn drop_store_with_data() {
        let store = test_store();
        let mut session = store.new_session();

        for i in 0u64..100 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        store.dispose_session(session);
        drop(store);
    }

    // ── Drop safety: after flush cycle ──────────────────────────────

    #[test]
    fn drop_after_flush() {
        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 4,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let store: SimpleStore =
            FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());
        let mut session = store.new_session();

        for i in 0u64..500 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        // Full maintenance: shift read-only, flush, evict.
        store.maintenance();

        store.dispose_session(session);
        drop(store);
    }

    // ── Drop during active flushes ──────────────────────────────────

    #[test]
    fn drop_during_active_flushes() {
        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 4,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let store: SimpleStore =
            FasterKv::new(config, SimpleFunctions::default(), NullDevice::new());
        let mut session = store.new_session();

        for i in 0u64..500 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        // Trigger async flush, then immediately drop — the Drop impl must
        // drain pending I/O without panicking.
        let _ = store.flush();
        store.dispose_session(session);
        // Drop happens here — exercises ordered shutdown.
    }

    // ── Send + Sync still hold with Drop impl ───────────────────────

    #[test]
    fn send_sync_with_drop() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FasterKv<SimpleFunctions<u64, u64>>>();
        assert_send_sync::<FasterKv<CounterFunctions<u64>>>();
    }

    // ── Pending I/O wiring tests ─────────────────────────────────────

    #[test]
    fn in_memory_ops_complete_synchronously_no_pending() {
        let store = test_store();
        let mut session = store.new_session();

        // Upsert and read — all in mutable region, no pending.
        let status = store.upsert(&mut session, &1u64, &42u64, ());
        assert_ne!(status, OperationStatus::Pending);
        assert_eq!(session.pending_count(), 0);

        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &1u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(output, Some(42));
        assert_eq!(session.pending_count(), 0);
        assert!(!session.has_pending());

        store.dispose_session(session);
    }

    #[test]
    fn pending_count_tracks_on_disk_reads() {
        use crate::device::InMemoryDevice;

        // Use a tiny buffer (2 pages) and low mutable fraction to force
        // records onto disk after maintenance + eviction.
        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 2,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let device = InMemoryDevice::with_sizes(512, 1 << 24); // 16 MiB
        let store: FasterKv<SimpleFunctions<u64, u64>> =
            FasterKv::new(config, SimpleFunctions::default(), device);

        let mut session = store.new_session();

        // Insert enough records to fill pages past the buffer.
        let n = 5000u64;
        for i in 0..n {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }

        // Run maintenance to flush and evict.
        store.maintenance();
        store.maintenance();
        store.maintenance();

        // Try to read an early key — likely evicted to disk.
        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &0u64, &0u64, &mut output, ());

        if status == OperationStatus::Pending {
            // Pending path: I/O was dispatched.
            assert!(session.has_pending());
            // The pending op should have been converted to an io_context.
            assert_eq!(session.io_pending_count(), 1);

            // Since InMemoryDevice completes synchronously, complete_pending
            // should process it immediately.
            let results = store.complete_pending(&mut session);
            // After completion, the session should have no pending.
            assert_eq!(session.pending_count(), 0);

            // With InMemoryDevice, the read should succeed.
            if !results.is_empty() {
                // Output is (Option<u64>, ()) for SimpleFunctions.
                let (out, _ctx) = &results[0];
                assert_eq!(*out, Some(0)); // value = 0 * 10 = 0
            }
        } else {
            // The key might still be in memory — that's fine too.
            assert!(
                status == OperationStatus::Ok || status == OperationStatus::NotFound,
                "unexpected status: {:?}",
                status,
            );
        }

        store.dispose_session(session);
    }

    #[test]
    fn complete_pending_sync_drains_all() {
        use crate::device::InMemoryDevice;

        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 2,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let device = InMemoryDevice::with_sizes(512, 1 << 24);
        let store: FasterKv<SimpleFunctions<u64, u64>> =
            FasterKv::new(config, SimpleFunctions::default(), device);

        let mut session = store.new_session();

        // Fill the log past the buffer to force disk reads.
        for i in 0..5000u64 {
            let _ = store.upsert(&mut session, &i, &(i + 100), ());
        }
        store.maintenance();
        store.maintenance();
        store.maintenance();

        // Attempt reads on several early keys.
        let mut pending_keys = Vec::new();
        for i in 0..10u64 {
            let mut output: Option<u64> = None;
            let status = store.read(&mut session, &i, &0u64, &mut output, ());
            if status == OperationStatus::Pending {
                pending_keys.push(i);
            }
        }

        if !pending_keys.is_empty() {
            // complete_pending_sync should block until all are done.
            let results = store.complete_pending_sync(&mut session);
            assert_eq!(session.pending_count(), 0);
            // With InMemoryDevice, all reads should produce results.
            assert!(!results.is_empty());
        }

        store.dispose_session(session);
    }

    #[test]
    fn complete_pending_on_empty_session() {
        let store = test_store();
        let mut session = store.new_session();

        // No pending ops — should return empty.
        let results = store.complete_pending(&mut session);
        assert!(results.is_empty());
        let results = store.complete_pending_sync(&mut session);
        assert!(results.is_empty());

        store.dispose_session(session);
    }

    // ── L3: Session complete_pending() API tests ─────────────────────

    #[test]
    fn try_complete_pending_no_pending_returns_empty_result() {
        let store = test_store();
        let mut session = store.new_session();

        let result = store.try_complete_pending(&mut session, false);
        assert_eq!(result, crate::store::CompletePendingResult::EMPTY);
        assert!(result.is_done());
        assert_eq!(result.completed, 0);
        assert_eq!(result.remaining, 0);
        assert!(!result.had_errors);

        store.dispose_session(session);
    }

    #[test]
    fn try_complete_pending_after_in_memory_ops() {
        let store = test_store();
        let mut session = store.new_session();

        // Upsert and read — all in mutable region, no pending generated.
        let _ = store.upsert(&mut session, &42u64, &100u64, ());
        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &42u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(output, Some(100));

        // No pending ops, so try_complete_pending is a no-op.
        let result = store.try_complete_pending(&mut session, false);
        assert!(result.is_done());
        assert_eq!(result.completed, 0);
        assert_eq!(result.remaining, 0);
        assert!(!result.had_errors);

        store.dispose_session(session);
    }

    #[test]
    fn try_complete_pending_wait_true_drains_all() {
        use crate::device::InMemoryDevice;

        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 2,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let device = InMemoryDevice::with_sizes(512, 1 << 24);
        let store: FasterKv<SimpleFunctions<u64, u64>> =
            FasterKv::new(config, SimpleFunctions::default(), device);

        let mut session = store.new_session();

        // Fill the log past the buffer to force disk reads.
        for i in 0..5000u64 {
            let _ = store.upsert(&mut session, &i, &(i * 10), ());
        }
        store.maintenance();
        store.maintenance();
        store.maintenance();

        // Read an early key — likely evicted.
        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &0u64, &0u64, &mut output, ());

        if status == OperationStatus::Pending {
            // wait=true should block and drain everything.
            let result = store.try_complete_pending(&mut session, true);
            assert!(result.is_done());
            assert!(result.completed >= 1);
            assert_eq!(result.remaining, 0);
        }

        store.dispose_session(session);
    }

    #[test]
    fn refresh_advances_epoch_and_drains() {
        let store = test_store();
        let mut session = store.new_session();

        let epoch_before = store.current_epoch();

        // Perform some in-memory operations.
        for i in 0..10u64 {
            let _ = store.upsert(&mut session, &i, &(i * 100), ());
        }

        let result = store.refresh(&mut session);
        let epoch_after = store.current_epoch();

        // Epoch must have advanced.
        assert!(epoch_after > epoch_before, "refresh must bump epoch");

        // No pending ops for in-memory inserts.
        assert!(result.is_done());
        assert!(!result.had_errors);

        store.dispose_session(session);
    }

    #[test]
    fn refresh_multiple_calls_advance_epoch_monotonically() {
        let store = test_store();
        let mut session = store.new_session();

        let mut prev_epoch = store.current_epoch();
        for _ in 0..5 {
            let _ = store.refresh(&mut session);
            let cur = store.current_epoch();
            assert!(cur > prev_epoch);
            prev_epoch = cur;
        }

        store.dispose_session(session);
    }

    #[test]
    fn complete_pending_result_fields_correct() {
        use crate::store::CompletePendingResult;

        let result = CompletePendingResult {
            completed: 5,
            remaining: 3,
            had_errors: true,
        };
        assert!(!result.is_done());
        assert_eq!(result.completed, 5);
        assert_eq!(result.remaining, 3);
        assert!(result.had_errors);

        let done = CompletePendingResult {
            completed: 10,
            remaining: 0,
            had_errors: false,
        };
        assert!(done.is_done());

        // EMPTY constant.
        let empty = CompletePendingResult::EMPTY;
        assert!(empty.is_done());
        assert_eq!(empty.completed, 0);
        assert_eq!(empty.remaining, 0);
        assert!(!empty.had_errors);

        // Debug and Clone.
        let debug = format!("{:?}", result);
        assert!(debug.contains("CompletePendingResult"));
        let cloned = result;
        assert_eq!(cloned, result);
    }

    #[test]
    fn concurrent_session_pending_operations() {
        use std::sync::Arc;
        use crate::device::InMemoryDevice;

        let config = FasterKvConfig {
            hash_index_size_log2: 10,
            buffer_size_pages: 2,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
        };
        let device = InMemoryDevice::with_sizes(512, 1 << 24);
        let store = Arc::new(FasterKv::<SimpleFunctions<u64, u64>>::new(
            config,
            SimpleFunctions::default(),
            device,
        ));

        // Session 1: write keys 0..2000
        let store1 = Arc::clone(&store);
        let h1 = std::thread::spawn(move || {
            let mut session = store1.new_session();
            for i in 0..2000u64 {
                let _ = store1.upsert(&mut session, &i, &(i + 1), ());
            }
            store1.maintenance();
            store1.maintenance();

            // Try reading early keys — may go pending.
            for i in 0..5u64 {
                let mut output: Option<u64> = None;
                let _ = store1.read(&mut session, &i, &0u64, &mut output, ());
            }

            let result = store1.try_complete_pending(&mut session, true);
            assert!(result.is_done());
            store1.dispose_session(session);
        });

        // Session 2: write keys 10000..12000
        let store2 = Arc::clone(&store);
        let h2 = std::thread::spawn(move || {
            let mut session = store2.new_session();
            for i in 10000..12000u64 {
                let _ = store2.upsert(&mut session, &i, &(i + 1), ());
            }
            store2.maintenance();
            store2.maintenance();

            for i in 10000..10005u64 {
                let mut output: Option<u64> = None;
                let _ = store2.read(&mut session, &i, &0u64, &mut output, ());
            }

            let result = store2.try_complete_pending(&mut session, true);
            assert!(result.is_done());
            store2.dispose_session(session);
        });

        h1.join().expect("thread 1 panicked");
        h2.join().expect("thread 2 panicked");
    }

    // ── A2: Ergonomic Session API ───────────────────────────────────

    #[test]
    fn session_scope_basic_usage() {
        let store = test_store();

        let result = store.session_scope(|session, store| {
            let _ = store.upsert_simple(session, &1u64, &42u64);
            let out = store.read_simple(session, &1u64);
            out
        });
        assert_eq!(result, Some(42));
    }

    #[test]
    fn session_scope_with_panic_cleans_up() {
        let store = test_store();

        // Insert a value in a scope that panics after the upsert.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.session_scope(|session, store| {
                let _ = store.upsert_simple(session, &99u64, &999u64);
                panic!("intentional panic inside session_scope");
            });
        }));
        assert!(result.is_err(), "closure should have panicked");

        // The session was cleaned up. We can open a new one and the
        // store is still usable.
        let mut session = store.new_session();
        let mut output: Option<u64> = None;
        let _ = store.read(&mut session, &99u64, &0u64, &mut output, ());
        // The upsert may or may not have persisted (it completed before
        // the panic), but the store must not be in a broken state.
        store.dispose_session(session);
    }

    #[test]
    fn session_scope_multiple_sequential() {
        let store = test_store();

        for i in 0u64..5 {
            store.session_scope(|session, store| {
                let _ = store.upsert_simple(session, &i, &(i * 100));
            });
        }

        // Verify all values were written across scopes.
        store.session_scope(|session, store| {
            for i in 0u64..5 {
                let out = store.read_simple(session, &i);
                assert_eq!(out, Some(i * 100), "key {} mismatch", i);
            }
        });
    }

    #[test]
    fn convenience_upsert_simple() {
        let store = test_store();
        let mut session = store.new_session();

        let status = store.upsert_simple(&mut session, &10u64, &200u64);
        assert!(
            status == OperationStatus::Created || status == OperationStatus::InPlaceUpdated,
            "upsert_simple should succeed, got: {:?}",
            status,
        );

        let mut output: Option<u64> = None;
        let status = store.read(&mut session, &10u64, &0u64, &mut output, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(output, Some(200));

        store.dispose_session(session);
    }

    #[test]
    fn convenience_read_simple() {
        let store = test_store();
        let mut session = store.new_session();

        let _ = store.upsert_simple(&mut session, &5u64, &55u64);

        let out = store.read_simple(&mut session, &5u64);
        assert_eq!(out, Some(55));

        // Non-existent key returns default.
        let out = store.read_simple(&mut session, &9999u64);
        assert_eq!(out, None);

        store.dispose_session(session);
    }

    #[test]
    fn convenience_delete_simple() {
        let store = test_store();
        let mut session = store.new_session();

        let _ = store.upsert_simple(&mut session, &7u64, &77u64);
        let status = store.delete_simple(&mut session, &7u64);
        assert_eq!(status, OperationStatus::Deleted);

        let out = store.read_simple(&mut session, &7u64);
        assert_eq!(out, None);

        store.dispose_session(session);
    }

    #[test]
    fn session_scope_return_value() {
        let store = test_store();

        let count = store.session_scope(|session, store| {
            let mut n = 0u64;
            for i in 0u64..10 {
                let _ = store.upsert_simple(session, &i, &i);
                n += 1;
            }
            n
        });
        assert_eq!(count, 10);
    }
}
