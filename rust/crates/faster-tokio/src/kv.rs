//! Async-managed FASTER key-value store with background maintenance.
//!
//! [`AsyncFasterKv`] wraps [`FasterKv`] and spawns a background Tokio task
//! that periodically calls [`maintenance()`](FasterKv::maintenance) via
//! [`spawn_blocking`](tokio::task::spawn_blocking). This keeps the log
//! healthy (shifting read-only boundaries, flushing, evicting) without
//! blocking the async runtime.
//!
//! # Example
//!
//! ```ignore
//! use faster_core::store::{FasterKvConfig, SimpleFunctions};
//! use faster_core::NullDevice;
//! use faster_tokio::AsyncFasterKv;
//! use std::time::Duration;
//!
//! let kv = AsyncFasterKv::new(
//!     FasterKvConfig::default(),
//!     SimpleFunctions::<u64, u64>::default(),
//!     NullDevice::new(),
//!     Duration::from_millis(50),
//! );
//!
//! let mut session = kv.new_session();
//! session.upsert_simple(&1u64, &42u64);
//! let value = session.read_simple(&1u64);
//! assert_eq!(value, Some(42));
//! ```

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use faster_core::checkpoint::{CheckpointError, CheckpointToken, CheckpointType};
use faster_core::device::Device;
use faster_core::store::{FasterKv, FasterKvConfig, Functions};
use tokio::task::JoinHandle;

use crate::session::AsyncSession;

/// Default maintenance interval.
///
/// 50 ms balances responsiveness (flushing sealed pages promptly) against
/// overhead (each pass is cheap but not free).
pub const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_millis(50);

/// Async-managed FASTER key-value store.
///
/// Wraps a [`FasterKv`] and runs a background task that periodically
/// calls [`maintenance()`](FasterKv::maintenance) on the Tokio blocking
/// thread pool. Provides async methods for checkpointing, compaction,
/// and session creation.
///
/// # Shutdown
///
/// For graceful shutdown, call [`shutdown()`](Self::shutdown) which signals
/// the maintenance loop and awaits its completion. If the value is simply
/// dropped, the background task is aborted — safe, but a maintenance pass
/// already dispatched to the blocking pool will run to completion
/// independently.
///
/// The underlying [`FasterKv`] performs its own ordered shutdown (flush,
/// wait for in-flight I/O, close device) when the last `Arc` reference
/// is released, so data integrity is preserved either way.
pub struct AsyncFasterKv<F: Functions> {
    store: Arc<FasterKv<F>>,
    maintenance_handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl<F: Functions> AsyncFasterKv<F> {
    /// Create a new async-managed store with the given maintenance interval.
    ///
    /// Spawns a background Tokio task that calls `FasterKv::maintenance()`
    /// via `spawn_blocking` every `maintenance_interval`. Must be called
    /// from within a Tokio runtime context.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime (no current `Handle`).
    pub fn new(
        config: FasterKvConfig,
        functions: F,
        device: impl Device,
        maintenance_interval: Duration,
    ) -> Self {
        let store = Arc::new(FasterKv::new(config, functions, device));
        Self::from_store(store, maintenance_interval)
    }

    /// Wrap an existing `Arc<FasterKv<F>>` with a maintenance task.
    ///
    /// Useful when you already have a store instance and want to add
    /// background maintenance.
    pub fn from_store(store: Arc<FasterKv<F>>, maintenance_interval: Duration) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));

        let bg_store = store.clone();
        let bg_shutdown = shutdown.clone();
        let maintenance_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(maintenance_interval);
            // First tick fires immediately — skip it so we don't run
            // maintenance before the caller has even used the store.
            interval.tick().await;
            loop {
                interval.tick().await;
                if bg_shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let store = bg_store.clone();
                let _ = tokio::task::spawn_blocking(move || store.maintenance()).await;
            }
        });

        Self {
            store,
            maintenance_handle: Some(maintenance_handle),
            shutdown,
        }
    }

    /// Create a new async-managed store with the
    /// [`DEFAULT_MAINTENANCE_INTERVAL`] (50 ms).
    pub fn with_defaults(config: FasterKvConfig, functions: F, device: impl Device) -> Self {
        Self::new(config, functions, device, DEFAULT_MAINTENANCE_INTERVAL)
    }

    // ── Session Management ──────────────────────────────────────────

    /// Create a new [`AsyncSession`] bound to this store.
    ///
    /// The returned session borrows `self`, so it cannot outlive the
    /// `AsyncFasterKv`. Like the underlying [`FasterSession`], the
    /// `AsyncSession` is `!Send` and must be used on the creating thread.
    ///
    /// [`FasterSession`]: faster_core::store::FasterSession
    pub fn new_session(&self) -> AsyncSession<'_, F> {
        AsyncSession::new(&self.store)
    }

    // ── Checkpoint / Recovery ───────────────────────────────────────

    /// Perform a checkpoint, dispatching the blocking work to the Tokio
    /// blocking thread pool.
    ///
    /// Returns the [`CheckpointToken`] that can later be used for recovery.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError`] if a checkpoint is already in progress
    /// or if an I/O error occurs while persisting metadata.
    pub async fn checkpoint(
        &self,
        checkpoint_dir: &Path,
        checkpoint_type: CheckpointType,
    ) -> Result<CheckpointToken, CheckpointError> {
        let store = self.store.clone();
        let dir = checkpoint_dir.to_path_buf();
        tokio::task::spawn_blocking(move || store.checkpoint(&dir, checkpoint_type))
            .await
            .expect("checkpoint task panicked")
    }

    // ── Compaction ──────────────────────────────────────────────────

    /// Compact the log (placeholder).
    ///
    /// Log compaction is not yet exposed through the async API. This
    /// method reserves the API surface and will be implemented when
    /// `FasterKv` gains a public compaction entry-point.
    pub async fn compact(&self) {
        // Compaction is a future addition — no-op placeholder.
    }

    // ── Maintenance Control ─────────────────────────────────────────

    /// Trigger a single maintenance pass on the blocking thread pool.
    ///
    /// Useful when you want an explicit flush/evict cycle outside the
    /// regular background interval.
    pub async fn maintenance(&self) {
        let store = self.store.clone();
        let _ = tokio::task::spawn_blocking(move || store.maintenance()).await;
    }

    // ── Shutdown ────────────────────────────────────────────────────

    /// Gracefully shut down the background maintenance task.
    ///
    /// Signals the maintenance loop to stop and waits for it to exit.
    /// After this returns, no more maintenance passes will be started.
    ///
    /// If you only need best-effort cleanup, simply dropping the value
    /// is sufficient — [`Drop`] aborts the background task.
    pub async fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.maintenance_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    // ── Accessors ───────────────────────────────────────────────────

    /// Returns a reference to the underlying [`FasterKv`].
    pub fn store(&self) -> &FasterKv<F> {
        &self.store
    }

    /// Returns a clone of the `Arc` holding the underlying [`FasterKv`].
    pub fn store_arc(&self) -> Arc<FasterKv<F>> {
        self.store.clone()
    }

    /// Returns `true` if the background maintenance task has been
    /// signalled to stop (via [`shutdown`](Self::shutdown) or drop).
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }
}

impl<F: Functions> Drop for AsyncFasterKv<F> {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.maintenance_handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faster_core::NullDevice;
    use faster_core::store::SimpleFunctions;

    type TestFns = SimpleFunctions<u64, u64>;

    fn test_kv() -> AsyncFasterKv<TestFns> {
        AsyncFasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::default(),
            NullDevice::new(),
            Duration::from_millis(10),
        )
    }

    // ── Full Lifecycle ─────────────────────────────────────────────

    #[tokio::test]
    async fn full_lifecycle_crud_and_shutdown() {
        let mut kv = test_kv();

        // CRUD via AsyncSession
        {
            let mut session = kv.new_session();

            // Create
            let status = session.upsert_simple(&1u64, &42u64);
            assert!(status.is_success(), "upsert failed: {status}");

            // Read
            let value = session.read_simple(&1u64);
            assert_eq!(value, Some(42));

            // Update
            let status = session.upsert_simple(&1u64, &100u64);
            assert!(status.is_success(), "update failed: {status}");
            assert_eq!(session.read_simple(&1u64), Some(100));

            // Delete
            let status = session.delete_simple(&1u64);
            assert!(status.is_success(), "delete failed: {status}");

            let mut out: Option<u64> = None;
            let status = session.read(&1u64, &0u64, &mut out, ());
            assert_eq!(status, faster_core::status::OperationStatus::NotFound);
        }

        // Graceful shutdown
        kv.shutdown().await;
        assert!(kv.is_shutdown());
    }

    // ── Checkpoint ────────────────────────────────────────────────

    #[tokio::test]
    async fn checkpoint_round_trip() {
        let kv = test_kv();

        let mut session = kv.new_session();
        for i in 0u64..10 {
            let _ = session.upsert_simple(&i, &(i * 10));
        }
        drop(session);

        let dir = tempfile::tempdir().unwrap();
        let result = kv.checkpoint(dir.path(), CheckpointType::FoldOver).await;
        assert!(result.is_ok(), "checkpoint failed: {result:?}");
    }

    // ── Maintenance Task Lifecycle ────────────────────────────────

    #[tokio::test]
    async fn maintenance_shuts_down_on_drop() {
        let kv = test_kv();
        // Let the maintenance loop run a few ticks.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Drop triggers abort — if it hangs, the test will time out.
        drop(kv);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn graceful_shutdown_completes() {
        let mut kv = test_kv();
        tokio::time::sleep(Duration::from_millis(30)).await;
        kv.shutdown().await;
        assert!(kv.is_shutdown());
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let mut kv = test_kv();
        kv.shutdown().await;
        kv.shutdown().await; // second call is a no-op
        assert!(kv.is_shutdown());
    }

    // ── Multiple Sessions ─────────────────────────────────────────

    #[tokio::test]
    async fn multiple_sessions_share_store() {
        let kv = test_kv();

        let mut s1 = kv.new_session();
        let mut s2 = kv.new_session();

        let _ = s1.upsert_simple(&1u64, &100u64);
        let _ = s2.upsert_simple(&2u64, &200u64);

        // Cross-session visibility
        assert_eq!(s1.read_simple(&2u64), Some(200));
        assert_eq!(s2.read_simple(&1u64), Some(100));
    }

    // ── Explicit Maintenance ──────────────────────────────────────

    #[tokio::test]
    async fn explicit_maintenance_completes() {
        let kv = test_kv();
        kv.maintenance().await; // should not panic or hang
    }

    // ── Accessors ────────────────────────────────────────────────

    #[tokio::test]
    async fn store_accessor_works() {
        let kv = test_kv();
        let _config = kv.store().config();
        let _arc = kv.store_arc();
        assert!(!kv.is_shutdown());
    }

    // ── from_store constructor ───────────────────────────────────

    #[tokio::test]
    async fn from_existing_store() {
        let store = Arc::new(FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::<u64, u64>::default(),
            NullDevice::new(),
        ));
        let mut kv = AsyncFasterKv::from_store(store.clone(), Duration::from_millis(10));

        let mut session = kv.new_session();
        let _ = session.upsert_simple(&5u64, &50u64);
        assert_eq!(session.read_simple(&5u64), Some(50));
        drop(session);

        kv.shutdown().await;
    }

    // ── with_defaults constructor ────────────────────────────────

    #[tokio::test]
    async fn with_defaults_constructor() {
        let mut kv = AsyncFasterKv::with_defaults(
            FasterKvConfig::default(),
            SimpleFunctions::<u64, u64>::default(),
            NullDevice::new(),
        );
        let mut session = kv.new_session();
        let _ = session.upsert_simple(&1u64, &1u64);
        assert_eq!(session.read_simple(&1u64), Some(1));
        drop(session);
        kv.shutdown().await;
    }

    // ── Batch operations via AsyncFasterKv ───────────────────────

    #[tokio::test]
    async fn batch_operations() {
        let kv = test_kv();
        let mut session = kv.new_session();

        for i in 0u64..100 {
            let _ = session.upsert_simple(&i, &(i * 10));
        }

        for i in 0u64..100 {
            assert_eq!(session.read_simple(&i), Some(i * 10), "mismatch at key {i}");
        }
    }

    // ── Compact placeholder ──────────────────────────────────────

    #[tokio::test]
    async fn compact_is_noop() {
        let kv = test_kv();
        kv.compact().await; // should return without error
    }
}
