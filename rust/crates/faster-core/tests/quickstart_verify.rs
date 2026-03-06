//! Checkpoint/recover round-trip tests for the high-level `FasterKv` API.
//!
//! Verifies that data written, checkpointed, and recovered through the
//! public `FasterKv::checkpoint` / `FasterKv::recover` methods is readable
//! after recovery.

use faster_core::SyncFileDevice;
use faster_core::checkpoint::CheckpointType;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

/// Helper: small config suitable for checkpoint tests.
fn test_config() -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 10, // 1 024 buckets
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
    }
}

/// Helper: open a `SyncFileDevice` rooted at `dir`.
///
/// Uses prefix `"log."` so that segment files match the recovery engine's
/// expected naming convention (`log.0`, `log.1`, …).
fn open_device(dir: &std::path::Path) -> SyncFileDevice {
    SyncFileDevice::new(dir, "log.", 512, 1 << 30, 4).expect("failed to create SyncFileDevice")
}

#[test]
fn test_checkpoint_and_recover_fold_over() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().to_path_buf();

    // Phase 1: Write data and take a fold-over checkpoint.
    {
        let device = open_device(&base);
        let store: FasterKv<SimpleFunctions<u64, u64>> =
            FasterKv::new(test_config(), SimpleFunctions::default(), device);

        let mut session = store.new_session();
        for i in 0..1000u64 {
            let _ = store.upsert_simple(&mut session, &i, &(i * i));
        }
        store.dispose_session(session);

        let token = store
            .checkpoint(&base, CheckpointType::FoldOver)
            .expect("checkpoint should succeed");
        println!("Checkpoint token: {token:?}");
    }
    // Store is dropped, device is closed.

    // Phase 2: Reopen and recover.
    {
        let device = open_device(&base);
        let mut store: FasterKv<SimpleFunctions<u64, u64>> =
            FasterKv::new(test_config(), SimpleFunctions::default(), device);

        let info = store.recover(&base, None).expect("recovery should succeed");
        println!(
            "Recovered: {} entries, {} pages",
            info.num_index_entries, info.pages_loaded
        );

        let mut session = store.new_session();

        // Data from Phase 1 should be readable.
        assert_eq!(store.read_simple(&mut session, &0), Some(0));
        assert_eq!(store.read_simple(&mut session, &42), Some(42 * 42));
        assert_eq!(store.read_simple(&mut session, &999), Some(999 * 999));

        // Non-existent key should return None.
        assert_eq!(store.read_simple(&mut session, &1001), None);

        // Can still write new data after recovery.
        let _ = store.upsert_simple(&mut session, &2000, &42);
        assert_eq!(store.read_simple(&mut session, &2000), Some(42));

        store.dispose_session(session);
    }
}
