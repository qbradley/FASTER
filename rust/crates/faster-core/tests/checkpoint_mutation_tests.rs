//! Mutation-testing gap-fill tests for `checkpoint/` and `recovery/` modules.
//!
//! Targets surviving mutants in checkpoint orchestration, metadata handling,
//! and recovery validation.

use faster_core::checkpoint::CheckpointType;

use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::sync_file_device::SyncFileDevice;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("faster_mutation_cp_{}", name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Create a store whose device writes log files into `dir`.
/// Recovery expects log.{n} files co-located with checkpoint metadata,
/// so use the same directory for both device and checkpoint.
fn disk_store(dir: &std::path::Path) -> FasterKv<SimpleFunctions<u64, u64>> {
    let dev = SyncFileDevice::new(dir, "log.", 512, 1 << 20, 1).expect("create device");
    let config = FasterKvConfig {
        grow_config: GrowConfig {
            enabled: false,
            ..Default::default()
        },
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), dev)
}

// ===========================================================================
// Checkpoint -> Recover roundtrip
// ===========================================================================

#[test]
fn checkpoint_recover_roundtrip_preserves_data() {
    let dir = temp_dir("cp_roundtrip");

    {
        let store = disk_store(&dir);
        let mut session = store.new_session();

        for i in 0..500u64 {
            let _ = store.upsert(&mut session, &i, &(i * 7), ());
        }

        store
            .checkpoint(&dir, CheckpointType::FoldOver)
            .expect("checkpoint should succeed");

        store.dispose_session(session);
    }

    {
        let mut store = disk_store(&dir);
        store.recover(&dir, None).expect("recovery should succeed");

        let mut session = store.new_session();

        for i in 0..500u64 {
            let mut output: Option<u64> = None;
            let _ = store.read(&mut session, &i, &0u64, &mut output, ());
            assert_eq!(
                output,
                Some(i * 7),
                "key {i} should survive checkpoint/recovery"
            );
        }

        store.dispose_session(session);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checkpoint_recover_minimal() {
    let dir = temp_dir("cp_minimal");

    {
        let store = disk_store(&dir);
        let mut session = store.new_session();
        let _ = store.upsert(&mut session, &1u64, &42u64, ());

        store
            .checkpoint(&dir, CheckpointType::FoldOver)
            .expect("checkpoint");
        store.dispose_session(session);
    }

    {
        let mut store = disk_store(&dir);
        store.recover(&dir, None).expect("recovery");

        let mut session = store.new_session();
        let mut output: Option<u64> = None;
        let _ = store.read(&mut session, &1u64, &0u64, &mut output, ());
        assert_eq!(output, Some(42), "single key should survive recovery");
        store.dispose_session(session);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Checkpoint with many keys exercises log segment handling and page iteration.
#[test]
fn checkpoint_recover_many_keys() {
    let dir = temp_dir("cp_many");

    {
        let store = disk_store(&dir);
        let mut session = store.new_session();

        for i in 0..1000u64 {
            let _ = store.upsert(&mut session, &i, &(i + 1000), ());
        }

        store
            .checkpoint(&dir, CheckpointType::FoldOver)
            .expect("checkpoint");
        store.dispose_session(session);
    }

    {
        let mut store = disk_store(&dir);
        store.recover(&dir, None).expect("recovery");

        let mut session = store.new_session();
        // Spot-check several keys across the range
        for &i in &[0u64, 1, 50, 500, 999] {
            let mut output: Option<u64> = None;
            let _ = store.read(&mut session, &i, &0u64, &mut output, ());
            assert_eq!(output, Some(i + 1000), "key {i} should survive recovery");
        }
        store.dispose_session(session);
    }

    let _ = std::fs::remove_dir_all(&dir);
}
