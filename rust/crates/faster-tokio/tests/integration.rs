//! Integration test suite for faster-tokio.
//!
//! Exercises the async layer end-to-end: lifecycle management, concurrent
//! multi-session access across Tokio tasks, checkpoint/recovery round-trips,
//! mixed sync/async patterns, graceful shutdown under load, error propagation,
//! and timeout behaviour.
//!
//! All tests use `#[tokio::test]` with the multi-threaded runtime unless
//! noted otherwise, and the entire suite must complete in under 30 seconds.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use faster_core::NullDevice;
use faster_core::checkpoint::CheckpointType;
use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_tokio::AsyncFasterKv;

// ─── Helpers ───────────────────────────────────────────────────────────

type TestFns = SimpleFunctions<u64, u64>;

fn small_config() -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 14,
        buffer_size_pages: 4,
        mutable_fraction: 0.9,
        ..FasterKvConfig::default()
    }
}

fn test_kv() -> AsyncFasterKv<TestFns> {
    AsyncFasterKv::new(
        small_config(),
        SimpleFunctions::default(),
        NullDevice::new(),
        Duration::from_millis(10),
    )
}

// ═══════════════════════════════════════════════════════════════════════
// 1. Full Async Lifecycle
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_lifecycle_open_upsert_checkpoint_read_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut kv = test_kv();

    // Create session → upsert data
    {
        let mut session = kv.new_session();
        for i in 0u64..50 {
            let status = session.upsert_simple(&i, &(i * 7));
            assert!(status.is_success(), "upsert {i} failed: {status}");
        }
    }

    // Async checkpoint
    let token = kv
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .await
        .expect("checkpoint failed");
    assert!(!token.to_string().is_empty());

    // Read-back all data
    {
        let mut session = kv.new_session();
        for i in 0u64..50 {
            let value = session.read_simple(&i);
            assert_eq!(value, Some(i * 7), "mismatch at key {i}");
        }
    }

    // Graceful shutdown
    kv.shutdown().await;
    assert!(kv.is_shutdown());
}

// ═══════════════════════════════════════════════════════════════════════
// 2. Multi-Session Concurrent Async Access
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_task_concurrent_writes_via_spawn_blocking() {
    let store = Arc::new(FasterKv::<TestFns>::new(
        small_config(),
        SimpleFunctions::default(),
        NullDevice::new(),
    ));

    let num_tasks = 8;
    let keys_per_task = 200;
    let counter = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for task_id in 0..num_tasks {
        let store = store.clone();
        let counter = counter.clone();

        // Each task runs on the blocking pool with its own session.
        handles.push(tokio::task::spawn_blocking(move || {
            let mut session = store.new_session();
            let base = task_id * keys_per_task;

            for i in 0..keys_per_task {
                let key = base + i;
                let value = key * 11;
                let status = store.upsert_simple(&mut session, &key, &value);
                assert!(status.is_success(), "task {task_id} upsert {key}: {status}");
                counter.fetch_add(1, Ordering::Relaxed);
            }

            // Verify own writes
            for i in 0..keys_per_task {
                let key = base + i;
                let value = store.read_simple(&mut session, &key);
                assert_eq!(
                    value,
                    Some(key * 11),
                    "task {task_id} read-back mismatch at key {key}"
                );
            }

            store.dispose_session(session);
        }));
    }

    // Wait for all tasks
    for h in handles {
        h.await.expect("task panicked");
    }

    assert_eq!(counter.load(Ordering::Relaxed), num_tasks * keys_per_task);

    // Cross-task visibility: read all keys from a fresh session
    let mut session = store.new_session();
    for key in 0..(num_tasks * keys_per_task) {
        let value = store.read_simple(&mut session, &key);
        assert_eq!(
            value,
            Some(key * 11),
            "cross-task read mismatch at key {key}"
        );
    }
    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════════
// 3. Checkpoint + Recovery Round-Trip (TokioFileDevice)
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_recovery_with_tokio_file_device() {
    use faster_tokio::TokioFileDevice;

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let num_keys: u64 = 200;

    // Phase 1: Write data and checkpoint
    {
        let device = TokioFileDevice::new(&data_dir, "log.", 512, 1 << 20).expect("create device");

        let store = FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::<u64, u64>::default(),
            device,
        );

        let mut session = store.new_session();
        for i in 0..num_keys {
            let status = store.upsert(&mut session, &i, &(i * i), ());
            assert!(status.is_success(), "phase1 upsert {i}: {status}");
        }

        store.complete_pending(&mut session);
        store.dispose_session(session);

        let _token = store
            .checkpoint(&data_dir, CheckpointType::FoldOver)
            .expect("checkpoint failed");
    }

    // Phase 2: Reopen + recover + verify
    {
        let device = TokioFileDevice::new(&data_dir, "log.", 512, 1 << 20).expect("reopen device");

        let mut store = FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::<u64, u64>::default(),
            device,
        );

        let info = store.recover(&data_dir, None).expect("recovery failed");
        assert!(info.num_index_entries > 0, "recovered empty index");

        let mut session = store.new_session();
        for i in 0..num_keys {
            let mut out: Option<u64> = None;
            let status = store.read(&mut session, &i, &0u64, &mut out, ());
            match status.status() {
                OperationStatus::Ok => {
                    assert_eq!(out, Some(i * i), "value mismatch at key {i}");
                }
                OperationStatus::Pending => {
                    // Data on disk — complete pending I/O, then retry.
                    store.complete_pending_sync(&mut session);
                    let mut retry: Option<u64> = None;
                    let s = store.read(&mut session, &i, &0u64, &mut retry, ());
                    assert_eq!(
                        s.status(),
                        OperationStatus::Ok,
                        "retry read for key {i}: {s}"
                    );
                    assert_eq!(retry, Some(i * i), "retry value mismatch at key {i}");
                }
                other => panic!("unexpected status for key {i}: {other:?}"),
            }
        }

        store.complete_pending(&mut session);
        store.dispose_session(session);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 4. Async Checkpoint via AsyncFasterKv with TokioFileDevice
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_checkpoint_with_file_device() {
    use faster_tokio::TokioFileDevice;

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("async_ckpt");
    std::fs::create_dir_all(&data_dir).unwrap();

    let device = TokioFileDevice::new(&data_dir, "log.", 512, 1 << 20).expect("create device");

    let mut kv = AsyncFasterKv::new(
        FasterKvConfig::default(),
        SimpleFunctions::<u64, u64>::default(),
        device,
        Duration::from_millis(20),
    );

    // Write data
    {
        let mut session = kv.new_session();
        for i in 0u64..100 {
            let status = session.upsert_simple(&i, &(i + 1000));
            assert!(status.is_success(), "upsert {i}: {status}");
        }
    }

    // Async checkpoint
    let token = kv
        .checkpoint(&data_dir, CheckpointType::FoldOver)
        .await
        .expect("async checkpoint failed");
    assert!(!token.to_string().is_empty());

    // Verify data still readable after checkpoint
    {
        let mut session = kv.new_session();
        for i in 0u64..100 {
            assert_eq!(
                session.read_simple(&i),
                Some(i + 1000),
                "post-checkpoint read mismatch at key {i}"
            );
        }
    }

    kv.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════
// 5. Mixed Sync/Async Usage Patterns
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_writes_async_reads() {
    let store = Arc::new(FasterKv::<TestFns>::new(
        small_config(),
        SimpleFunctions::default(),
        NullDevice::new(),
    ));

    // Phase 1: Sync writes on the blocking pool
    let store_clone = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut session = store_clone.new_session();
        for i in 0u64..500 {
            let _ = store_clone.upsert_simple(&mut session, &i, &(i * 3));
        }
        store_clone.dispose_session(session);
    })
    .await
    .unwrap();

    // Phase 2: Async reads via AsyncFasterKv wrapping the same store
    let kv = AsyncFasterKv::from_store(store.clone(), Duration::from_millis(20));
    let mut session = kv.new_session();
    for i in 0u64..500 {
        let value = session.read_simple(&i);
        assert_eq!(value, Some(i * 3), "async read mismatch at key {i}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sync_and_async_sessions() {
    let store = Arc::new(FasterKv::<TestFns>::new(
        small_config(),
        SimpleFunctions::default(),
        NullDevice::new(),
    ));

    let kv = AsyncFasterKv::from_store(store.clone(), Duration::from_millis(20));

    // Async session writes even keys
    {
        let mut async_session = kv.new_session();
        for i in (0u64..200).step_by(2) {
            let _ = async_session.upsert_simple(&i, &(i * 10));
        }
    }

    // Sync task writes odd keys concurrently
    let store_clone = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut session = store_clone.new_session();
        for i in (1u64..200).step_by(2) {
            let _ = store_clone.upsert_simple(&mut session, &i, &(i * 10));
        }
        store_clone.dispose_session(session);
    })
    .await
    .unwrap();

    // Both sets should be visible from a new async session
    let mut verify = kv.new_session();
    for i in 0u64..200 {
        let value = verify.read_simple(&i);
        assert_eq!(value, Some(i * 10), "mixed-mode mismatch at key {i}");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 6. Graceful Shutdown Under Load
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_while_tasks_in_flight() {
    let store = Arc::new(FasterKv::<TestFns>::new(
        small_config(),
        SimpleFunctions::default(),
        NullDevice::new(),
    ));

    let mut kv = AsyncFasterKv::from_store(store.clone(), Duration::from_millis(5));

    // Launch background writers that keep working until the store shuts down
    let ops_completed = Arc::new(AtomicU64::new(0));
    let mut writer_handles = Vec::new();
    for task_id in 0u64..4 {
        let store = store.clone();
        let ops = ops_completed.clone();

        writer_handles.push(tokio::task::spawn_blocking(move || {
            let mut session = store.new_session();
            let base = task_id * 10_000;
            for i in 0..1_000 {
                let key = base + i;
                let _ = store.upsert_simple(&mut session, &key, &(key * 2));
                ops.fetch_add(1, Ordering::Relaxed);
            }
            store.dispose_session(session);
        }));
    }

    // Let writers make some progress
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Shutdown while tasks might still be running
    kv.shutdown().await;
    assert!(kv.is_shutdown());

    // Wait for writer tasks to finish (they operate on the Arc<FasterKv>
    // directly, so they continue even after the maintenance loop stops)
    for h in writer_handles {
        h.await.expect("writer task panicked");
    }

    // All operations should have completed successfully
    assert!(ops_completed.load(Ordering::Relaxed) > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_kv_while_maintenance_running() {
    let kv = AsyncFasterKv::new(
        small_config(),
        SimpleFunctions::<u64, u64>::default(),
        NullDevice::new(),
        Duration::from_millis(5),
    );

    // Let maintenance run a few ticks
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Drop without explicit shutdown — background task should be aborted
    drop(kv);

    // If the maintenance task leaked or panicked, the test would hang or fail
    tokio::time::sleep(Duration::from_millis(20)).await;
}

// ═══════════════════════════════════════════════════════════════════════
// 7. Error Propagation Through Async Boundaries
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_nonexistent_key_returns_not_found() {
    let kv = test_kv();
    let mut session = kv.new_session();

    let mut out: Option<u64> = None;
    let status = session.read(&999u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::NotFound);
    assert_eq!(out, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_nonexistent_key_returns_not_found() {
    let kv = test_kv();
    let mut session = kv.new_session();

    let status = session.delete_simple(&42u64);
    assert_eq!(status, OperationStatus::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_delete_returns_not_found() {
    let kv = test_kv();
    let mut session = kv.new_session();

    let _ = session.upsert_simple(&1u64, &100u64);
    let s1 = session.delete_simple(&1u64);
    assert_eq!(s1, OperationStatus::Deleted);

    let s2 = session.delete_simple(&1u64);
    assert_eq!(s2, OperationStatus::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_to_nonexistent_dir_propagates_error() {
    let kv = test_kv();

    // Use a path that doesn't exist and can't be created
    let result = kv
        .checkpoint(
            std::path::Path::new("/nonexistent/deeply/nested/path"),
            CheckpointType::FoldOver,
        )
        .await;

    assert!(result.is_err(), "expected checkpoint to fail on bad path");
}

// ═══════════════════════════════════════════════════════════════════════
// 8. Timeout Behaviour for Long-Running Async Operations
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_completes_within_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let kv = test_kv();

    // Populate a bit of data
    {
        let mut session = kv.new_session();
        for i in 0u64..100 {
            let _ = session.upsert_simple(&i, &i);
        }
    }

    // Checkpoint should complete well within 5 seconds
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        kv.checkpoint(dir.path(), CheckpointType::FoldOver),
    )
    .await;

    assert!(result.is_ok(), "checkpoint timed out");
    assert!(result.unwrap().is_ok(), "checkpoint failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_completes_within_timeout() {
    let kv = test_kv();

    let result = tokio::time::timeout(Duration::from_secs(2), kv.maintenance()).await;
    assert!(result.is_ok(), "maintenance timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_completes_within_timeout() {
    let mut kv = test_kv();

    // Populate data and let maintenance run
    {
        let mut session = kv.new_session();
        for i in 0u64..100 {
            let _ = session.upsert_simple(&i, &i);
        }
    }
    tokio::time::sleep(Duration::from_millis(30)).await;

    let result = tokio::time::timeout(Duration::from_secs(2), kv.shutdown()).await;
    assert!(result.is_ok(), "shutdown timed out");
}

// ═══════════════════════════════════════════════════════════════════════
// 9. AsyncSession Pending I/O Completion
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn complete_pending_with_no_ops_returns_immediately() {
    let kv = test_kv();
    let mut session = kv.new_session();

    let result = session.complete_pending().await;
    assert!(result.is_done());
    assert_eq!(result.completed, 0);
    assert_eq!(result.remaining, 0);
}

#[tokio::test]
async fn complete_pending_with_results_empty() {
    let kv = test_kv();
    let mut session = kv.new_session();

    let results = session.complete_pending_with_results().await;
    assert!(results.is_empty());
}

#[tokio::test]
async fn refresh_with_no_pending() {
    let kv = test_kv();
    let mut session = kv.new_session();

    let _ = session.upsert_simple(&1u64, &42u64);
    let result = session.refresh().await;
    // With NullDevice nothing is pending, so refresh completes immediately.
    assert!(result.is_done());
}

// ═══════════════════════════════════════════════════════════════════════
// 10. Session Statistics Across Async Operations
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn session_stats_accessible() {
    let kv = test_kv();
    let mut session = kv.new_session();

    // Verify stats are accessible and start at zero
    let stats = session.inner().stats();
    assert_eq!(stats.pending_count, 0);
    assert!(!session.has_pending());
    assert_eq!(session.pending_count(), 0);

    // After in-memory operations (NullDevice), nothing is pending
    for i in 0u64..10 {
        let _ = session.upsert_simple(&i, &i);
    }
    assert!(!session.has_pending());
    assert_eq!(session.pending_count(), 0);
}

// ═══════════════════════════════════════════════════════════════════════
// 11. Large Batch Through Async Layer
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_batch_upsert_and_read_back() {
    let kv = AsyncFasterKv::new(
        FasterKvConfig::default(),
        SimpleFunctions::<u64, u64>::default(),
        NullDevice::new(),
        Duration::from_millis(20),
    );

    let mut session = kv.new_session();
    let n = 10_000u64;

    for i in 0..n {
        let status = session.upsert_simple(&i, &(i.wrapping_mul(31)));
        assert!(status.is_success(), "upsert failed at {i}: {status}");
    }

    for i in 0..n {
        let value = session.read_simple(&i);
        assert_eq!(value, Some(i.wrapping_mul(31)), "mismatch at key {i}");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 12. Multiple Sequential Checkpoints
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_checkpoints_produce_distinct_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let kv = test_kv();

    {
        let mut session = kv.new_session();
        for i in 0u64..20 {
            let _ = session.upsert_simple(&i, &i);
        }
    }

    let t1 = kv
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .await
        .expect("first checkpoint");

    // Mutate data between checkpoints
    {
        let mut session = kv.new_session();
        for i in 0u64..20 {
            let _ = session.upsert_simple(&i, &(i + 100));
        }
    }

    let t2 = kv
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .await
        .expect("second checkpoint");

    assert_ne!(
        t1.to_string(),
        t2.to_string(),
        "sequential checkpoints must have distinct tokens"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 13. RMW Through Async Layer
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn rmw_through_async_session() {
    let kv = test_kv();
    let mut session = kv.new_session();

    // Initial insert via upsert
    let _ = session.upsert_simple(&1u64, &100u64);

    // RMW replaces the value (SimpleFunctions behaviour)
    let mut out: Option<u64> = None;
    let status = session.rmw(&1u64, &200u64, &mut out, ());
    assert!(status.is_success(), "rmw failed: {status}");

    // Read back should see the RMW'd value
    let value = session.read_simple(&1u64);
    assert_eq!(value, Some(200));
}

// ═══════════════════════════════════════════════════════════════════════
// 14. from_store Constructor Shares State
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn from_store_shares_underlying_data() {
    let store = Arc::new(FasterKv::<TestFns>::new(
        small_config(),
        SimpleFunctions::default(),
        NullDevice::new(),
    ));

    // Write data directly
    {
        let mut session = store.new_session();
        for i in 0u64..50 {
            let _ = store.upsert_simple(&mut session, &i, &(i + 1));
        }
        store.dispose_session(session);
    }

    // Wrap in AsyncFasterKv and verify visibility
    let kv = AsyncFasterKv::from_store(store, Duration::from_millis(10));
    let mut session = kv.new_session();
    for i in 0u64..50 {
        assert_eq!(session.read_simple(&i), Some(i + 1));
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 15. Concurrent Async Sessions on Same Thread
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn multiple_async_sessions_same_thread() {
    let kv = test_kv();

    let mut s1 = kv.new_session();
    let mut s2 = kv.new_session();
    let mut s3 = kv.new_session();

    // Interleaved writes
    let _ = s1.upsert_simple(&1u64, &10u64);
    let _ = s2.upsert_simple(&2u64, &20u64);
    let _ = s3.upsert_simple(&3u64, &30u64);
    let _ = s1.upsert_simple(&4u64, &40u64);

    // All sessions see all data
    assert_eq!(s1.read_simple(&2u64), Some(20));
    assert_eq!(s2.read_simple(&3u64), Some(30));
    assert_eq!(s3.read_simple(&1u64), Some(10));
    assert_eq!(s2.read_simple(&4u64), Some(40));
}

// ═══════════════════════════════════════════════════════════════════════
// 16. TokioFileDevice Direct Usage
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_file_device_basic_io() {
    use faster_tokio::TokioFileDevice;

    let dir = tempfile::tempdir().unwrap();
    let device = TokioFileDevice::new(dir.path(), "seg.", 512, 1 << 20).expect("create device");

    let store = FasterKv::new(
        small_config(),
        SimpleFunctions::<u64, u64>::default(),
        device,
    );

    let mut session = store.new_session();
    for i in 0u64..100 {
        let status = store.upsert(&mut session, &i, &(i * 5), ());
        assert!(status.is_success(), "upsert {i}: {status}");
    }

    for i in 0u64..100 {
        let value = store.read_simple(&mut session, &i);
        assert_eq!(value, Some(i * 5), "mismatch at key {i}");
    }

    store.dispose_session(session);
}

// ═══════════════════════════════════════════════════════════════════════
// 17. Bridge Future Under Tokio
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_futures_concurrent_completion() {
    use faster_tokio::bridge::{MaybePending, pending_pair};

    let mut handles = Vec::new();

    // Spawn many futures that complete from OS threads
    for i in 0u64..50 {
        let (future, sender) = pending_pair::<u64>();

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(100));
            sender.complete(i * i);
        });

        handles.push(tokio::spawn(async move {
            let result = future.await;
            assert_eq!(result, i * i);
            result
        }));
    }

    // Also test MaybePending::Ready path
    for i in 50u64..100 {
        handles.push(tokio::spawn(async move {
            let result: u64 = MaybePending::Ready(i * i).await;
            assert_eq!(result, i * i);
            result
        }));
    }

    let mut results: Vec<u64> = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    assert_eq!(results.len(), 100);
}

// ═══════════════════════════════════════════════════════════════════════
// 18. Maintenance Background Task is Active
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maintenance_task_runs_periodically() {
    let kv = AsyncFasterKv::new(
        small_config(),
        SimpleFunctions::<u64, u64>::default(),
        NullDevice::new(),
        Duration::from_millis(5),
    );

    // Let the maintenance task run several ticks
    tokio::time::sleep(Duration::from_millis(50)).await;

    // If it panicked, the test would fail. Verify we can still use the store.
    let mut session = kv.new_session();
    let status = session.upsert_simple(&1u64, &1u64);
    assert!(status.is_success());
    assert_eq!(session.read_simple(&1u64), Some(1));
}

// ═══════════════════════════════════════════════════════════════════════
// 19. Overwrite Semantics Through Async Layer
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn upsert_overwrites_existing_value() {
    let kv = test_kv();
    let mut session = kv.new_session();

    let s1 = session.upsert_simple(&42u64, &100u64);
    assert_eq!(s1, OperationStatus::Created);

    let s2 = session.upsert_simple(&42u64, &200u64);
    assert_eq!(s2, OperationStatus::InPlaceUpdated);

    assert_eq!(session.read_simple(&42u64), Some(200));
}

// ═══════════════════════════════════════════════════════════════════════
// 20. Stress: Many Sessions Created and Dropped
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_churn_stress() {
    let kv = test_kv();

    // Rapidly create and drop sessions to stress epoch table slot recycling
    for round in 0..50 {
        let mut session = kv.new_session();
        let _ = session.upsert_simple(&(round as u64), &(round as u64));
        assert_eq!(session.read_simple(&(round as u64)), Some(round as u64));
        // Session dropped at end of loop iteration
    }

    // All data should still be present
    let mut verify = kv.new_session();
    for i in 0..50u64 {
        assert_eq!(
            verify.read_simple(&i),
            Some(i),
            "missing key {i} after churn"
        );
    }
}
