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

use crate::sync::{Arc, Duration, Instant, Mutex};
use std::path::Path;

use crate::address::{LogicalAddress, Page};
use crate::checkpoint::{
    CheckpointConfig, CheckpointError, CheckpointOrchestrator, CheckpointToken, CheckpointType,
};
use crate::compaction::orchestrator::{
    CompactionError, CompactionOrchestrator, CompactionResult, collect_stats,
};
use crate::compaction::policy::CompactionPolicy;
use crate::device::Device;
use crate::epoch::EpochTable;
use crate::grow::{GrowConfig, GrowError, GrowManager};
use crate::hash::bucket::HashBucketEntry;
use crate::hash::index::HashIndex;
use crate::hybrid_log::eviction::{EvictionPolicy, PageEvictor};
use crate::hybrid_log::flush::PageFlusher;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::page::PageState;
use crate::record::{RecordInfo, read_record_info, read_value};
use crate::recovery::index_recovery::IndexRecoveryEngine;
use crate::recovery::log_recovery::LogRecoveryEngine;
use crate::recovery::{RecoveryError, RecoveryManager};
use crate::status::{OperationOutcome, OperationStatus};
use crate::store::functions::{Functions, ReadInfo, RmwInfo, UpsertInfo};
use crate::store::operations::{
    InternalContext, allocate_at_tail, internal_delete, internal_read, internal_rmw,
    internal_upsert,
};
use crate::store::pending_io::PendingIoManager;
use crate::store::session::{
    CompletePendingResult, FasterSession, PendingOpType, SessionPool, UnsafeContext,
};

// ── RecoveryInfo ────────────────────────────────────────────────────

/// Information returned after a successful store recovery.
///
/// Contains the checkpoint identity, restored address boundaries, and
/// statistics about how much data was loaded back into memory.
#[derive(Debug, Clone)]
pub struct RecoveryInfo {
    /// The checkpoint token that was recovered.
    pub token: CheckpointToken,
    /// Strategy used for the recovered checkpoint.
    pub checkpoint_type: CheckpointType,
    /// Number of live entries in the recovered hash index.
    pub num_index_entries: u64,
    /// Earliest valid address in the recovered log.
    pub begin_address: LogicalAddress,
    /// Tail (exclusive upper-bound) of the recovered log.
    pub tail_address: LogicalAddress,
    /// Number of pages loaded from the device into memory.
    pub pages_loaded: u32,
}

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
    /// Whether automatic compaction is enabled in [`maintenance()`](FasterKv::maintenance).
    ///
    /// When `true` and a [`compaction_policy`](Self::compaction_policy) is
    /// set, `maintenance()` checks the policy on each call and triggers
    /// compaction when the policy fires.
    ///
    /// Default: `false` (manual compaction only via [`FasterKv::compact()`]).
    pub auto_compact: bool,
    /// Enable lossy (LRU cache) mode.
    ///
    /// When `true`, [`maintenance()`](FasterKv::maintenance) will:
    /// 1. Advance `begin_address` to match `head_address` after eviction,
    ///    effectively discarding on-disk data.
    /// 2. Truncate the storage device to reclaim disk space.
    /// 3. Invalidate hash index entries pointing to truncated addresses.
    ///
    /// In this mode the store acts as a bounded LRU cache: old records are
    /// silently lost when the log wraps. Reads of evicted keys return
    /// [`NotFound`](crate::status::OperationStatus::NotFound), and writes
    /// transparently create fresh records.
    ///
    /// Default: `false`.
    pub lossy: bool,
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
            auto_compact: false,
            lossy: false,
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
    pub(crate) hash_index: HashIndex,
    /// The hybrid log allocator.
    pub(crate) allocator: HybridLogAllocator,
    /// User-defined operation callbacks.
    pub(crate) functions: F,
    /// The storage device.
    device: Box<dyn Device>,
    /// The shared epoch table (kept alive for sessions and hash index).
    pub(crate) epoch_table: Arc<EpochTable>,
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
    /// Serializes concurrent compaction attempts.
    compaction_lock: Mutex<()>,
    /// Policy that decides when automatic compaction should trigger.
    /// `None` means manual-only (no auto-compact in `maintenance()`).
    compaction_policy: Option<Box<dyn CompactionPolicy>>,
    /// Observable operation counters (gated by `metrics` feature).
    #[cfg(feature = "metrics")]
    metrics: crate::metrics::Metrics,
}

// SAFETY: FasterKv is Send+Sync because all its fields are Send+Sync.
// - HashIndex, HybridLogAllocator, PageFlusher, PageEvictor are Send+Sync
//   (they use atomics internally).
// - Box<dyn Device> is Send+Sync because Device: Send + Sync + 'static.
// - Arc<EpochTable> is Send+Sync.
// - F: Functions requires Send + Sync.
// - SessionPool<F> contains Arc<EpochTable> + PhantomData<F>, both Send+Sync.
// - Mutex<()> is Send+Sync.
// - Option<Box<dyn CompactionPolicy>> is Send+Sync because CompactionPolicy: Send+Sync.
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

        use crate::sync::Ordering;

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
        const TIMEOUT: Duration = Duration::from_secs(5);
        const POLL_INTERVAL: Duration = Duration::from_millis(1);
        let deadline = Instant::now() + TIMEOUT;

        loop {
            let has_flushing = (head_page..=tail_page).any(|p| {
                page_table
                    .get_frame(Page(p))
                    .is_some_and(|f| f.state().load(Ordering::Acquire) == PageState::Flushing)
            });

            if !has_flushing {
                break;
            }

            if Instant::now() >= deadline {
                eprintln!(
                    "FasterKv::drop: timeout waiting for in-flight I/O \
                     ({TIMEOUT:?} exceeded)"
                );
                break;
            }

            crate::sync::thread::sleep(POLL_INTERVAL);
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
    ///     FasterKv::<SimpleFunctions<u64, u64>>::builder()
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
            compaction_lock: Mutex::new(()),
            compaction_policy: None,
            config,
            #[cfg(feature = "metrics")]
            metrics: crate::metrics::Metrics::new(),
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
    pub fn session_scope<R>(&self, f: impl FnOnce(&mut FasterSession<F>, &Self) -> R) -> R {
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

    // ── Epoch-Amortized Batch API ───────────────────────────────────

    /// Create an epoch-amortized [`UnsafeContext`] for batch operations.
    ///
    /// While the returned context exists, the session stays in epoch
    /// protection. Operations performed through the context skip the
    /// per-operation epoch enter/exit overhead (protect + unprotect +
    /// try_drain), which can reduce per-op latency by ~124 ns.
    ///
    /// The context is dropped when it goes out of scope, releasing epoch
    /// protection automatically.
    ///
    /// # Safety Contract
    ///
    /// The caller must periodically call [`UnsafeContext::refresh()`] to
    /// allow epoch advancement. A good rule of thumb: refresh every
    /// 64–256 operations.
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
    /// {
    ///     let mut ctx = store.unsafe_context(&mut session);
    ///     for i in 0u64..100 {
    ///         ctx.upsert(&store, &i, &(i * 10), ());
    ///         if i % 64 == 0 {
    ///             ctx.refresh();
    ///         }
    ///     }
    /// }
    /// // Read back through the normal per-operation API
    /// let mut output: Option<u64> = None;
    /// store.read(&mut session, &50u64, &0u64, &mut output, ());
    /// assert_eq!(output, Some(500));
    ///
    /// store.dispose_session(session);
    /// ```
    pub fn unsafe_context<'a>(&self, session: &'a mut FasterSession<F>) -> UnsafeContext<'a, F> {
        UnsafeContext::new(session)
    }

    // -- Batch Methods --

    /// Execute a batch of reads with hash-bucket prefetching.
    ///
    /// Creates an [`UnsafeContext`] (single epoch enter/exit for the entire
    /// batch), prefetches all hash buckets, then reads each key.
    ///
    /// # Panics
    ///
    /// Panics if `keys.len() != outputs.len()`.
    pub fn batch_read(
        &self,
        session: &mut FasterSession<F>,
        keys: &[F::Key],
        outputs: &mut [F::Output],
    ) -> super::batch::BatchResult
    where
        F::Input: Default,
        F::Context: Default,
    {
        let mut ctx = self.unsafe_context(session);
        ctx.batch_read(self, keys, outputs)
    }

    /// Execute a batch of upserts with hash-bucket prefetching.
    ///
    /// # Panics
    ///
    /// Panics if `keys.len() != inputs.len()`.
    pub fn batch_upsert(
        &self,
        session: &mut FasterSession<F>,
        keys: &[F::Key],
        inputs: &[F::Input],
    ) -> super::batch::BatchResult
    where
        F::Context: Default,
    {
        let mut ctx = self.unsafe_context(session);
        ctx.batch_upsert(self, keys, inputs)
    }

    /// Execute a batch of read-modify-write operations with prefetching.
    ///
    /// # Panics
    ///
    /// Panics if `keys.len() != inputs.len()` or `keys.len() != outputs.len()`.
    pub fn batch_rmw(
        &self,
        session: &mut FasterSession<F>,
        keys: &[F::Key],
        inputs: &[F::Input],
        outputs: &mut [F::Output],
    ) -> super::batch::BatchResult
    where
        F::Context: Default,
    {
        let mut ctx = self.unsafe_context(session);
        ctx.batch_rmw(self, keys, inputs, outputs)
    }

    /// Execute a batch of deletes with hash-bucket prefetching.
    pub fn batch_delete(
        &self,
        session: &mut FasterSession<F>,
        keys: &[F::Key],
    ) -> super::batch::BatchResult
    where
        F::Context: Default,
    {
        let mut ctx = self.unsafe_context(session);
        ctx.batch_delete(self, keys)
    }

    /// Execute a mixed batch of heterogeneous operations.
    ///
    /// Each [`BatchOp`](super::batch::BatchOp) can be a read, upsert, RMW,
    /// or delete. Results are returned positionally in the
    /// [`BatchResult`](super::batch::BatchResult).
    ///
    /// # Panics
    ///
    /// Panics if `ops.len() != outputs.len()`.
    pub fn batch_execute(
        &self,
        session: &mut FasterSession<F>,
        ops: &[super::batch::BatchOp<F::Key, F::Input>],
        outputs: &mut [F::Output],
    ) -> super::batch::BatchResult
    where
        F::Input: Default,
        F::Context: Default,
    {
        let mut ctx = self.unsafe_context(session);
        ctx.batch_execute(self, ops, outputs)
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
            .status()
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
    pub fn read_simple(&self, session: &mut FasterSession<F>, key: &F::Key) -> F::Output
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
    pub fn delete_simple(&self, session: &mut FasterSession<F>, key: &F::Key) -> OperationStatus
    where
        F::Context: Default,
    {
        self.delete(session, key, F::Context::default()).status()
    }

    // ── Checkpoint / Recovery ───────────────────────────────────────

    /// Take a checkpoint of the current store state.
    ///
    /// Persists the hash index and hybrid log metadata to `checkpoint_dir`.
    /// All in-memory pages are flushed to the device before the checkpoint
    /// completes.
    ///
    /// Returns the checkpoint token that identifies this checkpoint for
    /// later recovery via [`recover`](Self::recover).
    pub fn checkpoint(
        &self,
        checkpoint_dir: &Path,
        checkpoint_type: CheckpointType,
    ) -> Result<CheckpointToken, CheckpointError> {
        // Ensure all mutable pages become read-only so they can be flushed.
        self.allocator.shift_read_only_to_tail();

        // Flush all in-memory pages to the device synchronously so that
        // flushed_until_address advances to the tail before the orchestrator
        // checks it.
        let page_table = self.allocator.page_table();
        let head_page = self.allocator.head_address().page().0;
        let tail = self.allocator.tail_address();
        let tail_page = tail.page().0;
        let page_size = self.allocator.page_size();

        for p in head_page..=tail_page {
            let page = Page(p);
            if let Some(frame) = page_table.get_frame(page) {
                let state = frame.state().load(std::sync::atomic::Ordering::Acquire);
                // Seal Open pages so flush_page_sync can process them.
                if state == PageState::Open {
                    let _ = frame
                        .state()
                        .try_transition(PageState::Open, PageState::Sealed);
                }
                // Now flush any Sealed pages.
                if frame.state().load(std::sync::atomic::Ordering::Acquire) == PageState::Sealed {
                    let _ = self.flusher.flush_page_sync(
                        page,
                        page_table,
                        self.device.as_ref(),
                        page_size,
                    );
                }
            }
        }

        // Advance flushed_until to the tail so the orchestrator sees all
        // pages as flushed.
        self.allocator.try_advance_flushed_until(tail);

        let config = CheckpointConfig::new(checkpoint_dir.to_path_buf());
        let orchestrator = CheckpointOrchestrator::new(config);
        let token = orchestrator.take_checkpoint(
            checkpoint_type,
            &self.hash_index,
            &self.allocator,
            &[],
            checkpoint_dir,
        )?;

        // The orchestrator writes the index file to `{dir}/{token}.index`,
        // but recovery expects it at `{dir}/checkpoints/{token}/{token}.index`.
        // Move it to the canonical location.
        let src = checkpoint_dir.join(format!("{token}.index"));
        let dst = checkpoint_dir
            .join("checkpoints")
            .join(token.to_string())
            .join(format!("{token}.index"));
        if src.exists() && !dst.exists() {
            std::fs::rename(&src, &dst).map_err(CheckpointError::IoError)?;
        }

        metrics_inc!(self.metrics, checkpoint_count);
        Ok(token)
    }

    /// Recover the store from a checkpoint.
    ///
    /// Restores the hash index and log address boundaries from the
    /// checkpoint. If `token` is `None`, the most recent checkpoint is
    /// recovered.
    ///
    /// Pages are loaded from the device into memory, so reads work
    /// immediately after recovery without going through the pending I/O
    /// path.
    ///
    /// # Errors
    ///
    /// Returns [`RecoveryError`] if the checkpoint is not found, corrupt,
    /// or if I/O fails during page loading.
    ///
    /// # Panics
    ///
    /// Must be called before creating any sessions.
    pub fn recover(
        &mut self,
        checkpoint_dir: &Path,
        token: Option<CheckpointToken>,
    ) -> Result<RecoveryInfo, RecoveryError> {
        // Step 1: Select a checkpoint and produce a recovery plan.
        let recovery_mgr = RecoveryManager::new(checkpoint_dir.to_path_buf());
        let plan = recovery_mgr.select_checkpoint(token)?;

        // Step 2: Recover the hash index.
        let index_engine = IndexRecoveryEngine::new();
        let recovered_index = index_engine.recover_index_with_epoch(
            &plan,
            checkpoint_dir,
            Arc::clone(&self.epoch_table),
        )?;
        let num_index_entries = recovered_index.scan_entry_count();
        self.hash_index = recovered_index;

        // Step 3: Recover log address boundaries.
        let log_engine = LogRecoveryEngine::new();
        let log_result = match plan.checkpoint_type {
            CheckpointType::FoldOver => log_engine.recover_fold_over(&plan, checkpoint_dir)?,
            CheckpointType::Snapshot => log_engine.recover_snapshot(&plan, checkpoint_dir)?,
        };

        // Step 4: Restore allocator addresses.
        self.allocator.restore_from_recovery(&log_result);

        // Step 5: Load pages from device into memory.
        let pages_loaded = self
            .allocator
            .load_pages_from_device(
                &*self.device,
                log_result.head_address,
                log_result.tail_address,
            )
            .map_err(RecoveryError::IoError)?;

        Ok(RecoveryInfo {
            token: plan.token,
            checkpoint_type: plan.checkpoint_type,
            num_index_entries,
            begin_address: log_result.begin_address,
            tail_address: log_result.tail_address,
            pages_loaded,
        })
    }

    // ── CRUD Operations ─────────────────────────────────────────────

    /// Read a key's value.
    ///
    /// Enters epoch protection, performs the read, and returns the outcome.
    /// On success (`OperationStatus::Ok`), the result is written to `output`
    /// via the [`Functions::read`] callback.
    ///
    /// # Returns
    ///
    /// An [`OperationOutcome`] whose status is one of:
    ///
    /// - [`OperationStatus::Ok`] — value read successfully.
    /// - [`OperationStatus::NotFound`] — key does not exist.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    ///
    /// On non-Pending paths the context is returned inside the outcome so
    /// the caller can reuse it (e.g., retry on Aborted).
    pub fn read(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> OperationOutcome<F::Context> {
        let mut guard = session.begin_unsafe();
        sim_yield!("read::after_epoch_protect");
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
            on_alloc_failure: None,
        };
        let (status, recovered_ctx) = internal_read(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            output,
            context,
        );
        drop(guard);
        metrics_inc!(self.metrics, total_operations);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
            OperationOutcome::pending()
        } else {
            OperationOutcome::completed(
                status,
                recovered_ctx.expect("context must be returned on non-Pending path"),
            )
        }
    }

    /// Insert or update a key-value pair.
    ///
    /// Enters epoch protection, performs the upsert, and returns the outcome.
    /// The `input` is passed to the [`Functions::upsert`] callback which writes
    /// the desired value into the record. For [`SimpleFunctions`](super::SimpleFunctions),
    /// `Input = Value`, so `input` is the value to store.
    ///
    /// # Returns
    ///
    /// An [`OperationOutcome`] whose status is one of:
    ///
    /// - [`OperationStatus::Created`] — new record inserted.
    /// - [`OperationStatus::InPlaceUpdated`] — existing mutable record updated.
    /// - [`OperationStatus::CopyUpdated`] — read-only record copied to tail.
    /// - [`OperationStatus::Revivified`] — sealed record revivified in-place.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    ///
    /// On non-Pending paths the context is returned inside the outcome so
    /// the caller can reuse it (e.g., retry on Aborted).
    pub fn upsert(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        input: &F::Input,
        context: F::Context,
    ) -> OperationOutcome<F::Context> {
        let mut guard = session.begin_unsafe();
        sim_yield!("upsert::after_epoch_protect");
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
            on_alloc_failure: Some(&|| self.maintenance()),
        };
        let (status, recovered_ctx) = internal_upsert(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            context,
        );
        sim_yield!("upsert::after_hash_cas");
        drop(guard);
        metrics_inc!(self.metrics, total_operations);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
            OperationOutcome::pending()
        } else {
            OperationOutcome::completed(
                status,
                recovered_ctx.expect("context must be returned on non-Pending path"),
            )
        }
    }

    /// Read-modify-write a key.
    ///
    /// Enters epoch protection, performs the RMW, and returns the outcome.
    /// If the key exists, the [`Functions::rmw_in_place`] or
    /// [`Functions::rmw_copy_update`] callback is invoked. If the key
    /// does not exist, [`Functions::rmw_initial`] creates a new record.
    ///
    /// # Returns
    ///
    /// An [`OperationOutcome`] whose status is one of:
    ///
    /// - [`OperationStatus::Created`] — new record created via `rmw_initial`.
    /// - [`OperationStatus::InPlaceUpdated`] — updated in mutable region.
    /// - [`OperationStatus::CopyUpdated`] — read-only record copied to tail.
    /// - [`OperationStatus::Revivified`] — sealed record revivified in-place.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    ///
    /// On non-Pending paths the context is returned inside the outcome so
    /// the caller can reuse it (e.g., retry on Aborted).
    pub fn rmw(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        input: &F::Input,
        output: &mut F::Output,
        context: F::Context,
    ) -> OperationOutcome<F::Context> {
        let mut guard = session.begin_unsafe();
        sim_yield!("rmw::after_epoch_protect");
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
            on_alloc_failure: Some(&|| self.maintenance()),
        };
        let (status, recovered_ctx) = internal_rmw(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            output,
            context,
        );
        sim_yield!("rmw::after_hash_cas");
        drop(guard);
        metrics_inc!(self.metrics, total_operations);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
            OperationOutcome::pending()
        } else {
            OperationOutcome::completed(
                status,
                recovered_ctx.expect("context must be returned on non-Pending path"),
            )
        }
    }

    /// Delete a key.
    ///
    /// Enters epoch protection and marks the record as a tombstone.
    /// In the mutable region the tombstone is set in-place; in the
    /// read-only region a tombstone record is appended at the tail.
    ///
    /// # Returns
    ///
    /// An [`OperationOutcome`] whose status is one of:
    ///
    /// - [`OperationStatus::Deleted`] — key deleted successfully.
    /// - [`OperationStatus::NotFound`] — key does not exist.
    /// - [`OperationStatus::Pending`] — record is on disk; queued for async I/O.
    ///
    /// On non-Pending paths the context is returned inside the outcome so
    /// the caller can reuse it (e.g., retry on Aborted).
    pub fn delete(
        &self,
        session: &mut FasterSession<F>,
        key: &F::Key,
        context: F::Context,
    ) -> OperationOutcome<F::Context> {
        let mut guard = session.begin_unsafe();
        sim_yield!("delete::after_epoch_protect");
        let ctx = InternalContext {
            hash_index: &self.hash_index,
            allocator: &self.allocator,
            on_alloc_failure: Some(&|| self.maintenance()),
        };
        let (status, recovered_ctx) =
            internal_delete(&ctx, guard.session_mut(), &self.functions, key, context);
        drop(guard);
        metrics_inc!(self.metrics, total_operations);
        if status == OperationStatus::Pending {
            self.dispatch_pending_io(session);
            OperationOutcome::pending()
        } else {
            OperationOutcome::completed(
                status,
                recovered_ctx.expect("context must be returned on non-Pending path"),
            )
        }
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
    pub(crate) fn dispatch_pending_io(&self, session: &mut FasterSession<F>) {
        let ops = session.drain_pending_ops();
        for op in ops {
            match self.pending_io_mgr.issue_read(op, self.device.as_ref()) {
                Ok(io_ctx) => {
                    metrics_inc!(self.metrics, pending_io_inflight);
                    session.enqueue_io_context(io_ctx);
                }
                Err(_err) => {
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
    pub fn complete_pending(&self, session: &mut FasterSession<F>) -> Vec<(F::Output, F::Context)> {
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
    /// the user's `Functions::read` callback. For write operations, copies
    /// the record to the log tail (RCU update) with a single retry on
    /// allocation failure.
    fn process_completed_io(
        &self,
        cio: super::pending_io::CompletedIo<F>,
    ) -> Option<(F::Output, F::Context)> {
        trace_span!("read_completion");
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
                    self.functions.read(
                        &cio.operation.key,
                        &value,
                        input,
                        &mut output,
                        &ReadInfo::new(0, cio.operation.address, ri),
                    );
                }

                Some((output, cio.operation.context))
            }
            PendingOpType::Upsert | PendingOpType::Rmw | PendingOpType::Delete => {
                self.complete_write_pending(cio);
                None
            }
        }
    }

    /// Complete a write-path pending operation (upsert/rmw/delete).
    ///
    /// Reads the old value from the I/O buffer, allocates a new record at the
    /// log tail, and CAS-updates the hash index (copy-to-tail / RCU). This is
    /// the on-disk counterpart of the read-only copy-to-tail path in
    /// `internal_upsert` / `internal_rmw` / `internal_delete`.
    ///
    /// If the initial allocation fails (log full), sealed pages are flushed
    /// to free space and the allocation is retried once (H1/C-1).
    fn complete_write_pending(&self, cio: super::pending_io::CompletedIo<F>) {
        let layout = &cio.operation.record_layout;
        let key = &cio.operation.key;
        let key_hash = cio.operation.key_hash;

        // Look up the current hash entry for this key.
        let (entry, slot) = match self.hash_index.find(key_hash) {
            Some(pair) => pair,
            None => return, // Key no longer in the index — nothing to update.
        };

        let old_addr = entry.address();
        let ri = cio.record_info();

        match cio.operation.op_type {
            PendingOpType::Upsert => {
                // Produce the new value via the upsert callback. The old
                // value from the disk buffer is passed so merge-on-upsert
                // semantics work correctly.
                let old_value: F::Value = if ri.is_tombstone() || ri.is_invalid() {
                    F::Value::default()
                } else {
                    cio.read_value(layout)
                };

                let mut new_val = F::Value::default();
                let mut output = F::Output::default();
                let old_ref = if ri.is_tombstone() || ri.is_invalid() {
                    None
                } else {
                    Some(&old_value)
                };

                let input = cio
                    .operation
                    .input
                    .as_ref()
                    .expect("upsert pending must have input");
                self.functions.upsert(
                    key,
                    &mut new_val,
                    input,
                    old_ref,
                    &mut output,
                    &UpsertInfo::new(0, old_addr, ri),
                );

                let (new_addr, mut accessor) = match self.allocate_with_retry(key, &new_val) {
                    Some(pair) => pair,
                    None => return,
                };

                let new_ri = RecordInfo::new(old_addr, 0, false, false, false);
                accessor.write_full_record(&new_ri, key, &new_val, layout);

                let committed = HashBucketEntry::new(entry.tag(), new_addr, false);
                let _ = self.hash_index.update(slot, entry, committed);
            }
            PendingOpType::Rmw => {
                let input = cio
                    .operation
                    .input
                    .as_ref()
                    .expect("rmw pending must have input");

                if ri.is_tombstone() || ri.is_invalid() {
                    // No existing value — create from initial.
                    let mut new_val = F::Value::default();
                    let mut output = F::Output::default();
                    self.functions.rmw_initial(
                        key,
                        input,
                        &mut new_val,
                        &mut output,
                        &RmwInfo::new(0, old_addr, ri, false),
                    );

                    let (new_addr, mut accessor) = match self.allocate_with_retry(key, &new_val) {
                        Some(pair) => pair,
                        None => return,
                    };

                    let new_ri = RecordInfo::new(old_addr, 0, false, false, false);
                    accessor.write_full_record(&new_ri, key, &new_val, layout);

                    let committed = HashBucketEntry::new(entry.tag(), new_addr, false);
                    let _ = self.hash_index.update(slot, entry, committed);
                } else {
                    let old_value: F::Value = cio.read_value(layout);
                    let mut new_value = old_value.clone();
                    let mut output = F::Output::default();
                    self.functions.rmw_copy_update(
                        key,
                        input,
                        &old_value,
                        &mut new_value,
                        &mut output,
                        &RmwInfo::new(0, old_addr, ri, true),
                    );

                    let (new_addr, mut accessor) = match self.allocate_with_retry(key, &new_value) {
                        Some(pair) => pair,
                        None => return,
                    };

                    let new_ri = RecordInfo::new(old_addr, 0, false, false, false);
                    accessor.write_full_record(&new_ri, key, &new_value, layout);

                    let committed = HashBucketEntry::new(entry.tag(), new_addr, false);
                    let _ = self.hash_index.update(slot, entry, committed);
                }
            }
            PendingOpType::Delete => {
                if ri.is_tombstone() {
                    return; // Already deleted.
                }

                // Allocate a tombstone record at the tail.
                let dummy_value: F::Value = if ri.is_invalid() {
                    F::Value::default()
                } else {
                    cio.read_value(layout)
                };

                let (new_addr, mut accessor) = match self.allocate_with_retry(key, &dummy_value) {
                    Some(pair) => pair,
                    None => return,
                };

                let tombstone_ri = RecordInfo::new(old_addr, 0, false, true, false);
                accessor.write_full_record(&tombstone_ri, key, &dummy_value, layout);

                let committed = HashBucketEntry::new(entry.tag(), new_addr, false);
                let _ = self.hash_index.update(slot, entry, committed);
            }
            PendingOpType::Read => unreachable!("read handled above"),
        }
    }

    /// Try `allocate_at_tail`; on failure, flush sealed pages and retry once.
    ///
    /// This prevents silent data loss when the log is temporarily full
    /// during write-pending completion (H1/C-1 fix).
    fn allocate_with_retry<K: crate::record::Key, V: crate::record::Value>(
        &self,
        key: &K,
        value: &V,
    ) -> Option<(LogicalAddress, crate::hybrid_log::MutableRecordAccessor)> {
        if let Some(pair) = allocate_at_tail(&self.allocator, key, value, None) {
            return Some(pair);
        }
        // Flush sealed pages to free space, then retry.
        let _ = self.flush();
        allocate_at_tail(&self.allocator, key, value, None)
    }

    // ── Maintenance ─────────────────────────────────────────────────

    /// Flush sealed (read-only) pages to the storage device.
    ///
    /// Returns the number of pages flushed.
    pub fn flush(&self) -> u32 {
        trace_span!("store_flush");
        let count = self
            .flusher
            .flush_sealed_pages(&self.allocator, self.device.as_ref())
            .unwrap_or_default();
        #[cfg(feature = "metrics")]
        self.metrics
            .flush_count
            .fetch_add(u64::from(count), std::sync::atomic::Ordering::Relaxed);
        count
    }

    /// Evict flushed pages from memory.
    ///
    /// Returns the number of pages evicted.
    pub fn evict(&self) -> u32 {
        self.evictor.evict_pages(&self.allocator)
    }

    /// Flush all log data to device and evict from memory.
    ///
    /// This is the Rust equivalent of C# FASTER's `store.Log.FlushAndEvict(true)`.
    /// It moves all in-memory pages to the storage device:
    ///
    /// 1. Shifts the read-only boundary to the current tail (seals mutable region).
    /// 2. Seals any remaining `Open` pages.
    /// 3. Synchronously flushes all sealed pages to the device.
    /// 4. Evicts all flushed pages from memory.
    ///
    /// After this call, subsequent reads will return [`OperationStatus::Pending`]
    /// and require [`complete_pending`](Self::complete_pending) to retrieve results
    /// from disk. New writes still work — they allocate fresh pages in memory.
    ///
    /// Returns `(flushed, evicted)` — the number of pages flushed and evicted.
    pub fn flush_and_evict(&self) -> (u32, u32) {
        // 1. Shift read-only boundary to the tail.
        self.allocator.shift_read_only_to_tail();

        // 2. Seal all Open pages and flush Sealed pages synchronously.
        let page_table = self.allocator.page_table();
        let head_page = self.allocator.head_address().page().0;
        let tail = self.allocator.tail_address();
        let tail_page = tail.page().0;
        let page_size = self.allocator.page_size();
        let mut flushed = 0u32;

        for p in head_page..=tail_page {
            let page = Page(p);
            if let Some(frame) = page_table.get_frame(page) {
                let state = frame.state().load(std::sync::atomic::Ordering::Acquire);
                if state == PageState::Open {
                    let _ = frame
                        .state()
                        .try_transition(PageState::Open, PageState::Sealed);
                }
                if frame.state().load(std::sync::atomic::Ordering::Acquire) == PageState::Sealed
                    && self
                        .flusher
                        .flush_page_sync(page, page_table, self.device.as_ref(), page_size)
                        .is_ok()
                {
                    flushed += 1;
                }
            }
        }

        // Advance flushed_until so the evictor sees all pages as flushed.
        self.allocator.try_advance_flushed_until(tail);

        // 3. Evict all flushed pages.
        let mut evicted = 0u32;
        loop {
            let n = self.evictor.evict_pages(&self.allocator);
            if n == 0 {
                break;
            }
            evicted += n;
        }

        (flushed, evicted)
    }

    // ── Compaction ──────────────────────────────────────────────────

    /// Run online log compaction: scan, copy live records, swing
    /// pointers, and advance begin-address.
    ///
    /// Compacts the region from `begin_address` up to the current
    /// safe-read-only address. Dead and tombstoned records are discarded;
    /// live records are copied to the log tail.
    ///
    /// Supports **all** [`Key`]/[`Value`] types — fixed-size (`u64`, `u32`,
    /// etc.) and variable-length (`Vec<u8>`, `String`). Record sizes are
    /// discovered per-record via length prefix inspection, so no special
    /// configuration is needed for variable-length workloads.
    ///
    /// Concurrent `compact()` calls are serialized via an internal mutex.
    /// Normal read/write operations continue concurrently.
    ///
    /// The key and value types are derived from `F::Key` and `F::Value`
    /// (the associated types on the store's [`Functions`](crate::store::Functions)
    /// implementation), so there is no risk of passing mismatched types.
    ///
    /// # Errors
    ///
    /// Returns [`CompactionError`] if the compaction region is empty,
    /// record copying fails, or epoch drain times out. Returns a
    /// [`CorruptedRecord`](crate::record::RecordSizeError::CorruptedRecord)
    /// variant if a corrupted length prefix is detected — the original log
    /// region is left untouched in that case.
    ///
    /// # Examples
    ///
    /// Fixed-size keys and values:
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
    /// for i in 0u64..100 {
    ///     store.upsert(&mut session, &i, &i, ());
    /// }
    /// // Compact (may be a no-op if everything is still in-memory/mutable).
    /// let _ = store.compact();
    /// store.dispose_session(session);
    /// ```
    ///
    /// Variable-length keys and values (requires a custom [`Functions`](crate::store::Functions)
    /// implementation that does not require `Copy`):
    ///
    /// ```ignore
    /// use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    /// use faster_core::NullDevice;
    ///
    /// // With a Functions impl that supports Vec<u8>:
    /// let store: FasterKv<MyVarLenFunctions> = FasterKv::new(
    ///     FasterKvConfig::default(),
    ///     MyVarLenFunctions::default(),
    ///     NullDevice::new(),
    /// );
    ///
    /// let mut session = store.new_session();
    /// for i in 0u64..50 {
    ///     let key = format!("key-{i}").into_bytes();
    ///     let value = vec![0xABu8; 256];
    ///     store.upsert(&mut session, &key, &value, ());
    /// }
    /// let _ = store.compact();
    /// store.dispose_session(session);
    /// ```
    pub fn compact(&self) -> Result<CompactionResult, CompactionError> {
        // Serialize concurrent compaction attempts.
        let _lock = self
            .compaction_lock
            .lock()
            .expect("compaction lock poisoned");

        let begin = self.first_data_address();
        let until = self.allocator.safe_read_only_address();

        if begin >= until {
            return Err(CompactionError::EmptyRegion { begin, until });
        }

        // The orchestrator manages its own epoch thread internally:
        // it enters epoch for K1–K3 (scan, copy, swing), exits, drains,
        // then runs K4 (begin-address advance).
        let orch = CompactionOrchestrator::new(
            &self.allocator,
            &self.hash_index,
            self.device.as_ref(),
            &self.epoch_table,
        );
        orch.run::<F::Key, F::Value>(begin, until)
    }

    /// Set the compaction policy used by [`maintenance()`](Self::maintenance)
    /// for automatic compaction.
    ///
    /// Pass `None` to disable automatic compaction (manual only).
    /// Also set [`FasterKvConfig::auto_compact`] to `true` to enable
    /// the policy check in `maintenance()`.
    pub fn set_compaction_policy(&mut self, policy: Option<Box<dyn CompactionPolicy>>) {
        self.compaction_policy = policy;
    }

    /// Run a full maintenance cycle: shift read-only boundary, flush, evict.
    ///
    /// Call this periodically to keep the hybrid log healthy. It:
    /// 1. Shifts the read-only boundary if the mutable region is too large.
    /// 2. Flushes sealed pages to the device.
    /// 3. Evicts flushed pages if the in-memory footprint exceeds the policy.
    /// 4. (Optional) Checks the compaction policy and triggers compaction
    ///    if [`auto_compact`](FasterKvConfig::auto_compact) is enabled.
    pub fn maintenance(&self) {
        // 1. Shift read-only boundary if mutable region is too large.
        let info = self.allocator.snapshot();
        let buffer_size = self.allocator.page_table().buffer_size() as u32;

        // Detect buffer pressure: when the in-memory page count approaches the
        // circular buffer capacity, writers will stall on the SF-10 check even
        // though the normal mutable_fraction threshold has not been crossed.
        // Force a read-only shift and eviction in this case to unblock them.
        let buffer_pressure = info.in_memory_pages() + 2 >= buffer_size;

        // Eager flush watermark: start flushing at 75% capacity to prevent
        // the cliff where all writers stall simultaneously at 100%.
        let eager_flush = info.in_memory_pages() * 4 >= buffer_size * 3;

        if buffer_pressure
            || eager_flush
            || info.needs_read_only_shift(self.allocator.mutable_fraction_pages())
        {
            self.allocator.shift_read_only_to_tail();
        }

        // Buffer-pressure: force read-only shift and eviction when the buffer
        // is nearly full. With mutable_fraction ~0.9, the normal threshold may
        // never trigger because mutable_pages() == mutable_fraction_pages().
        let head_page = info.head_address.page().0 as u64;
        let tail_page = info.tail_address.page().0 as u64;
        let in_memory_pages = tail_page.saturating_sub(head_page);
        let buffer_size = self.allocator.page_table().buffer_size() as u64;
        if in_memory_pages + 2 >= buffer_size {
            self.allocator.shift_read_only_to_tail();
            if self.config.lossy {
                self.evictor
                    .evict_and_truncate(&self.allocator, self.device.as_ref());
            } else {
                self.evictor.evict_pages(&self.allocator);
            }
        }

        // 2. Flush sealed pages to the device.
        let _ = self
            .flusher
            .flush_sealed_pages(&self.allocator, self.device.as_ref());

        // 3. Evict if the in-memory footprint exceeds the policy threshold,
        //    the buffer is under pressure, or we've hit the eager flush watermark.
        if buffer_pressure || eager_flush || self.evictor.needs_eviction(&self.allocator.snapshot())
        {
            if self.config.lossy {
                // Lossy mode: evict, advance begin_address, truncate device,
                // and invalidate stale hash entries in a single step.
                let old_begin = self.allocator.begin_address();
                self.evictor
                    .evict_and_truncate(&self.allocator, self.device.as_ref());
                let new_begin = self.allocator.begin_address();
                if new_begin > old_begin {
                    self.hash_index
                        .invalidate_entries_in_range(old_begin, new_begin);
                }
            } else {
                self.evictor.evict_pages(&self.allocator);
            }
        }
    }

    /// Return a snapshot of pipeline addresses for diagnostic purposes.
    pub fn pipeline_snapshot(
        &self,
    ) -> (
        LogicalAddress,
        LogicalAddress,
        LogicalAddress,
        LogicalAddress,
    ) {
        let head = self.allocator.head_address();
        let ro = self.allocator.read_only_address();
        let tail = self.allocator.tail_address();
        let begin = self.allocator.begin_address();
        (begin, head, ro, tail)
    }

    /// Debug the pipeline state by printing page frame states.
    pub fn debug_pipeline_state(&self) {
        use crate::hybrid_log::page::PageState;
        let head = self.allocator.head_address();
        let ro = self.allocator.read_only_address();
        let tail = self.allocator.tail_address();
        let page_table = self.allocator.page_table();
        let buf_sz = page_table.buffer_size() as u32;

        eprint!(
            "  head={} ro={} tail={} buf={} | ",
            head.page().0,
            ro.page().0,
            tail.page().0,
            buf_sz,
        );
        let start = head.page().0;
        let end = tail.page().0.min(start + buf_sz);
        for p in start..=end {
            let page = crate::address::Page(p);
            match page_table.get_frame(page) {
                Some(frame) => {
                    let st = frame.state().load(core::sync::atomic::Ordering::Relaxed);
                    let c = match st {
                        PageState::Open => 'O',
                        PageState::Sealed => 'S',
                        PageState::Flushing => 'G',
                        PageState::Flushed => 'D',
                        PageState::Evicted => 'E',
                        PageState::Free => 'F',
                    };
                    eprint!("{c}");
                }
                None => eprint!("_"),
            }
        }
        eprintln!();
    }

    /// Check whether the configured compaction policy recommends
    /// compaction and run it if so.
    ///
    /// This is intended to be called from a background maintenance loop.
    /// It is a no-op if no policy is set, `auto_compact` is `false`, or
    /// the policy does not recommend compaction.
    ///
    /// Returns `Some(result)` if compaction ran, `None` otherwise.
    pub fn maybe_compact(&self) -> Option<Result<CompactionResult, CompactionError>> {
        if !self.config.auto_compact {
            return None;
        }

        let policy = self.compaction_policy.as_ref()?;
        let stats = collect_stats(&self.allocator);

        if !policy.should_compact(&stats) {
            return None;
        }

        Some(self.compact())
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

    /// Get the current read-only address (diagnostic use).
    #[inline]
    pub fn read_only_address(&self) -> LogicalAddress {
        self.allocator.read_only_address()
    }

    /// Get the current begin address (start of valid log data).
    ///
    /// Records below this address have been truncated by compaction.
    /// After a successful compaction, this advances past the compacted
    /// region.
    #[inline]
    pub fn begin_address(&self) -> LogicalAddress {
        self.allocator.begin_address()
    }

    /// Get the address of the first data record in the log.
    ///
    /// The store reserves a small sentinel region at the start of the log
    /// (to keep addresses 0 and 1 unused). This method returns the address
    /// immediately after that sentinel — the first position where a real
    /// record can be written.
    ///
    /// Use this (rather than `head_address`) as the `begin_address` for
    /// compaction scans when the log has never been truncated.
    #[inline]
    pub fn first_data_address(&self) -> LogicalAddress {
        use crate::record::RECORD_ALIGNMENT;
        LogicalAddress::from_raw(self.allocator.begin_address().raw() + RECORD_ALIGNMENT as u64)
    }

    /// Get the approximate number of entries in the store.
    ///
    /// **Note:** This is a rough estimate. A precise count requires
    /// scanning the hash index, which is not yet implemented.
    ///
    /// **Intentionally deferred:** Maintaining an atomic counter on every
    /// upsert/delete adds contention on a hot path. A future iteration may
    /// implement index scanning or sharded counters when usage warrants it.
    #[inline]
    pub fn entry_count(&self) -> u64 {
        0
    }

    /// Get a reference to the store configuration.
    #[inline]
    pub fn config(&self) -> &FasterKvConfig {
        &self.config
    }

    /// Get a reference to the observable metrics counters.
    ///
    /// Only available when the `metrics` feature is enabled.
    /// Returns `None` when compiled without the feature.
    #[inline]
    pub fn metrics(&self) -> Option<&crate::metrics::Metrics> {
        #[cfg(feature = "metrics")]
        {
            Some(&self.metrics)
        }
        #[cfg(not(feature = "metrics"))]
        {
            None
        }
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
            auto_compact: false,
            lossy: false,
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
        use crate::device::InMemoryDevice;
        use std::sync::Arc;

        let config = FasterKvConfig {
            hash_index_size_log2: 10,
            buffer_size_pages: 2,
            mutable_fraction: 0.5,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
            lossy: false,
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
            store.read_simple(session, &1u64)
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

    // ── Epoch-Amortized Batch API (UnsafeContext) ───────────────────

    #[test]
    fn unsafe_context_batch_upsert_and_read() {
        let store = test_store();
        let mut session = store.new_session();

        // Batch upsert via UnsafeContext
        {
            let mut ctx = store.unsafe_context(&mut session);
            for i in 0u64..100 {
                let status = ctx.upsert(&store, &i, &(i * 10), ());
                assert!(
                    status.is_success(),
                    "batch upsert failed for key {i}: {status:?}"
                );
                if i % 32 == 0 {
                    ctx.refresh();
                }
            }
        }
        // Epoch released — verify all reads succeed through normal API.
        for i in 0u64..100 {
            let mut output: Option<u64> = None;
            let status = store.read(&mut session, &i, &0u64, &mut output, ());
            assert_eq!(status, OperationStatus::Ok, "read failed for key {i}");
            assert_eq!(output, Some(i * 10));
        }

        store.dispose_session(session);
    }

    #[test]
    fn unsafe_context_batch_read() {
        let store = test_store();
        let mut session = store.new_session();

        // Populate via normal API
        for i in 0u64..50 {
            let _ = store.upsert(&mut session, &i, &(i + 100), ());
        }

        // Batch read via UnsafeContext
        {
            let mut ctx = store.unsafe_context(&mut session);
            for i in 0u64..50 {
                let mut output: Option<u64> = None;
                let status = ctx.read(&store, &i, &0u64, &mut output, ());
                assert_eq!(status, OperationStatus::Ok, "batch read failed for key {i}");
                assert_eq!(output, Some(i + 100));
            }
        }

        store.dispose_session(session);
    }

    #[test]
    fn unsafe_context_batch_delete() {
        let store = test_store();
        let mut session = store.new_session();

        // Populate
        {
            let mut ctx = store.unsafe_context(&mut session);
            for i in 0u64..20 {
                let _ = ctx.upsert(&store, &i, &(i * 5), ());
            }
        }

        // Delete via batch
        {
            let mut ctx = store.unsafe_context(&mut session);
            for i in 0u64..10 {
                let status = ctx.delete(&store, &i, ());
                assert_eq!(
                    status,
                    OperationStatus::Deleted,
                    "batch delete failed for key {i}"
                );
            }
        }

        // Verify: first 10 deleted, last 10 still present.
        for i in 0u64..10 {
            let mut output: Option<u64> = None;
            let status = store.read(&mut session, &i, &0u64, &mut output, ());
            assert_eq!(
                status,
                OperationStatus::NotFound,
                "key {i} should be deleted"
            );
        }
        for i in 10u64..20 {
            let mut output: Option<u64> = None;
            let status = store.read(&mut session, &i, &0u64, &mut output, ());
            assert_eq!(status, OperationStatus::Ok, "key {i} should still exist");
            assert_eq!(output, Some(i * 5));
        }

        store.dispose_session(session);
    }

    #[test]
    fn unsafe_context_batch_rmw() {
        let config = FasterKvConfig {
            hash_index_size_log2: 8,
            buffer_size_pages: 4,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
            lossy: false,
        };
        let store: FasterKv<CounterFunctions<u64>> =
            FasterKv::new(config, CounterFunctions::new(), NullDevice::new());

        let mut session = store.new_session();

        // Create initial values via batch upsert
        {
            let mut ctx = store.unsafe_context(&mut session);
            for i in 0u64..10 {
                let _ = ctx.upsert(&store, &i, &0i64, ());
            }
        }

        // Batch RMW (increment counters 5 times each)
        {
            let mut ctx = store.unsafe_context(&mut session);
            for _round in 0..5 {
                for i in 0u64..10 {
                    let mut output: i64 = 0;
                    let _ = ctx.rmw(&store, &i, &1i64, &mut output, ());
                }
                ctx.refresh();
            }
        }

        // Verify counter values
        for i in 0u64..10 {
            let mut output: i64 = 0;
            let status = store.read(&mut session, &i, &0i64, &mut output, ());
            assert_eq!(status, OperationStatus::Ok, "read failed for key {i}");
            assert_eq!(output, 5, "counter for key {i} should be 5");
        }

        store.dispose_session(session);
    }

    #[test]
    fn unsafe_context_full_crud_cycle() {
        let store = test_store();
        let mut session = store.new_session();

        // Full CRUD in a single UnsafeContext lifetime
        {
            let mut ctx = store.unsafe_context(&mut session);

            // Create
            let status = ctx.upsert(&store, &1u64, &100u64, ());
            assert!(status.is_success());

            // Read
            let mut output: Option<u64> = None;
            let status = ctx.read(&store, &1u64, &0u64, &mut output, ());
            assert_eq!(status, OperationStatus::Ok);
            assert_eq!(output, Some(100));

            // Update
            let status = ctx.upsert(&store, &1u64, &200u64, ());
            assert!(status.is_success());

            // Read updated
            let mut output: Option<u64> = None;
            let status = ctx.read(&store, &1u64, &0u64, &mut output, ());
            assert_eq!(status, OperationStatus::Ok);
            assert_eq!(output, Some(200));

            // Delete
            let status = ctx.delete(&store, &1u64, ());
            assert_eq!(status, OperationStatus::Deleted);

            // Read deleted
            let mut output: Option<u64> = None;
            let status = ctx.read(&store, &1u64, &0u64, &mut output, ());
            assert_eq!(status, OperationStatus::NotFound);
        }

        store.dispose_session(session);
    }

    #[test]
    fn unsafe_context_epoch_released_on_drop() {
        let store = test_store();
        let mut session = store.new_session();

        {
            let ctx = store.unsafe_context(&mut session);
            assert!(ctx.session().is_in_epoch());
        }
        // UnsafeContext dropped — epoch released
        assert!(!session.is_in_epoch());

        store.dispose_session(session);
    }
}
