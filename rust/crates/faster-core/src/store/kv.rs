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
use crate::hash::index::HashIndex;
use crate::hybrid_log::eviction::{EvictionPolicy, PageEvictor};
use crate::hybrid_log::flush::PageFlusher;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::page::PageState;
use crate::status::OperationStatus;
use crate::store::functions::Functions;
use crate::store::operations::{
    InternalContext, internal_delete, internal_read, internal_rmw, internal_upsert,
};
use crate::store::session::{FasterSession, SessionPool};

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
}

impl Default for FasterKvConfig {
    fn default() -> Self {
        Self {
            hash_index_size_log2: 20, // 1M buckets
            buffer_size_pages: 16,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
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

        let session_pool = SessionPool::new(Arc::clone(&epoch_table));

        Self {
            hash_index,
            allocator,
            functions,
            device: Box::new(device),
            epoch_table,
            session_pool,
            flusher,
            evictor,
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
        internal_read(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            output,
            context,
        )
        // guard drops here, releasing epoch protection
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
        internal_upsert(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            context,
        )
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
        internal_rmw(
            &ctx,
            guard.session_mut(),
            &self.functions,
            key,
            input,
            output,
            context,
        )
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
        internal_delete(&ctx, guard.session_mut(), &self.functions, key, context)
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
}
