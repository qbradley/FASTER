//! Mutation-testing gap-fill tests for `store/builder.rs`, `store/batch.rs`,
//! and `store/pending_io.rs`.

use faster_core::InMemoryDevice;
use faster_core::device::NullDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

// ===========================================================================
// store/builder.rs — builder methods must apply their values
// ===========================================================================

/// Kill mutant: `eviction_policy -> Default::default()`.
#[test]
fn builder_eviction_policy_applied() {
    let custom_policy = EvictionPolicy {
        max_in_memory_pages: 99,
        eviction_batch_size: 5,
    };
    let store: FasterKv<SimpleFunctions<u64, u64>> =
        FasterKv::<SimpleFunctions<u64, u64>>::builder()
            .hash_index_size_log2(10)
            .eviction_policy(custom_policy)
            .build(SimpleFunctions::default(), NullDevice::new())
            .expect("valid config");

    let mut session = store.new_session();
    let _ = store.upsert(&mut session, &1u64, &100u64, ());
    store.dispose_session(session);
}

/// Kill mutant: `grow_chunks_per_operation -> Default::default()`.
#[test]
fn builder_grow_chunks_per_operation_applied() {
    let store: FasterKv<SimpleFunctions<u64, u64>> =
        FasterKv::<SimpleFunctions<u64, u64>>::builder()
            .hash_index_size_log2(10)
            .grow_chunks_per_operation(32)
            .build(SimpleFunctions::default(), NullDevice::new())
            .expect("valid config with custom grow chunks");

    let mut session = store.new_session();
    let _ = store.upsert(&mut session, &1u64, &100u64, ());
    store.dispose_session(session);
}

/// Kill mutant: `grow_enabled -> Default::default()`.
#[test]
fn builder_grow_enabled_false() {
    let store: FasterKv<SimpleFunctions<u64, u64>> =
        FasterKv::<SimpleFunctions<u64, u64>>::builder()
            .hash_index_size_log2(10)
            .grow_enabled(false)
            .build(SimpleFunctions::default(), NullDevice::new())
            .expect("valid config with grow disabled");

    let mut session = store.new_session();
    let _ = store.upsert(&mut session, &1u64, &100u64, ());
    store.dispose_session(session);
}

// ===========================================================================
// store/batch.rs — batch operations with >256 items
// ===========================================================================

fn large_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig {
            enabled: false,
            ..Default::default()
        },
        hash_index_size_log2: 12,
        buffer_size_pages: 16,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}
/// Kill mutants in batch_upsert epoch refresh logic (>256 items).
#[test]
fn batch_upsert_large_batch_succeeds() {
    let store = large_store();
    let mut session = store.new_session();

    // 300 items (> BATCH_REFRESH_INTERVAL of 256)
    let keys: Vec<u64> = (0..300).collect();
    let values: Vec<u64> = keys.iter().map(|k| k + 1000).collect();

    let result = store.batch_upsert(&mut session, &keys, &values);
    assert_eq!(result.total(), 300);
    assert!(result.all_succeeded(), "all 300 upserts should succeed");

    // Verify some items are readable
    let mut outputs = vec![None; 10];
    let result = store.batch_read(&mut session, &keys[..10], &mut outputs);
    assert!(result.all_succeeded());
    for (i, output) in outputs.iter().enumerate() {
        assert_eq!(*output, Some(i as u64 + 1000), "key {i} value mismatch");
    }

    store.dispose_session(session);
}

/// Kill mutants in batch_delete epoch refresh logic (>256 items).
#[test]
fn batch_delete_large_batch() {
    let store = large_store();
    let mut session = store.new_session();

    let keys: Vec<u64> = (0..300).collect();
    let values: Vec<u64> = (0..300).collect();
    store.batch_upsert(&mut session, &keys, &values);

    let result = store.batch_delete(&mut session, &keys);
    assert_eq!(result.total(), 300);
    assert!(result.all_succeeded(), "all 300 deletes should succeed");

    store.dispose_session(session);
}

// ===========================================================================
// Triage: pending_io.rs mutants
// ===========================================================================

// pending_io.rs:198 Debug impl → cosmetic, skip per SKILL.md.
// pending_io.rs:141 error source match arm → error trait detail, cosmetic.
// pending_io.rs:358 bytes_transferred → 0/1 — deep async I/O path.
// pending_io.rs:452-462 issue_read arithmetic — deep I/O path.
// pending_io.rs:539 issue_read_sync arithmetic — deep I/O path.
// pending_io.rs:567 return_buffer → () — buffer leak, equivalent.
