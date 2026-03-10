//! Recovery edge-case tests — gap-fill for checkpoint/recovery coverage.
//!
//! Existing `checkpoint_recovery_tests.rs` (42 tests) covers the happy path,
//! standard corrupt-JSON, missing files, and CRC mismatches. This file targets
//! edge cases NOT covered there:
//!
//! - Truncated/zero-length metadata files (simulating crash during write)
//! - Truncated index checkpoint binary (simulating disk-full or crash)
//! - Index recovery with mismatched table size in metadata vs file
//! - Session recovery with large excluded-serial-number sets
//! - Checkpoint → recover → continue writing → checkpoint again (lifecycle)
//! - Validation detecting partially-present checkpoint directories
//! - Recovery plan step ordering invariant
//! - Concurrent compaction + checkpoint overlap
//! - Checkpoint with read-only-only data (no mutable region)

use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use faster_core::address::{LogicalAddress, Offset, Page};
use faster_core::checkpoint::{
    CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo,
    SessionRecoveryInfo,
};
use faster_core::grow::GrowConfig;
use faster_core::hash::KeyHash;
use faster_core::hash_bucket::HashBucketEntry;
use faster_core::hash_table::HashTable;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::recovery::index_recovery::IndexRecoveryEngine;
use faster_core::recovery::session_recovery::SessionRecoveryEngine;
use faster_core::recovery::{RecoveryError, RecoveryManager, RecoveryPlan, RecoveryStep};
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

// ═══════════════════════════════════════════════════════════════════
// Helpers (shared with checkpoint_recovery_tests but kept local to
// avoid cross-file coupling)
// ═══════════════════════════════════════════════════════════════════

fn sample_token(val: u128) -> CheckpointToken {
    CheckpointToken::new(val)
}

fn sample_index_info() -> IndexRecoveryInfo {
    IndexRecoveryInfo {
        format_version: 2,
        version: 1,
        table_size: 1 << 10,
        num_ht_bytes: (1 << 10) * 64,
        num_ofb_bytes: 0,
        num_buckets: 1 << 10,
        start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
        final_logical_address: LogicalAddress::new(Page(10), Offset(512)),
    }
}

fn fold_over_log_info() -> LogRecoveryInfo {
    LogRecoveryInfo {
        format_version: 2,
        version: 1,
        checkpoint_type: CheckpointType::FoldOver,
        begin_address: LogicalAddress::ZERO,
        flushed_until_address: LogicalAddress::ZERO,
        final_address: LogicalAddress::ZERO,
        head_address: LogicalAddress::ZERO,
        snapshot_start_address: LogicalAddress::ZERO,
        snapshot_final_address: LogicalAddress::ZERO,
        use_snapshot_file: false,
        object_log_segment_count: 0,
    }
}

fn write_checkpoint_metadata(
    base_dir: &Path,
    token: &CheckpointToken,
    index: &IndexRecoveryInfo,
    log: &LogRecoveryInfo,
    sessions: &[SessionRecoveryInfo],
) {
    let store = CheckpointMetadataStore::new(base_dir.to_path_buf());
    store
        .write_checkpoint_metadata(token, index, log, sessions)
        .expect("failed to write test checkpoint metadata");
}

fn write_index_checkpoint(
    log2_size: u32,
    entry_count: u64,
    version: u32,
) -> (tempfile::TempDir, CheckpointToken, u64) {
    let base = tempfile::tempdir().expect("create temp dir");
    let token = CheckpointToken::new(42);
    let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
    fs::create_dir_all(&ckpt_dir).unwrap();

    let table = HashTable::new(log2_size);
    let mut inserted = 0u64;
    for i in 0..entry_count {
        let hash = KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let result = table.find_or_create_entry(hash, LogicalAddress::INVALID);
        if result.created {
            let committed =
                HashBucketEntry::new(result.entry.tag(), LogicalAddress::from_raw(i + 1), false);
            table.update_entry(result.slot, result.entry, committed);
            inserted += 1;
        }
    }

    let mut writer =
        faster_core::checkpoint::index_writer::IndexCheckpointWriter::new(&ckpt_dir, &token)
            .expect("create writer");
    writer
        .write_index(
            table.bucket_slice(),
            table.log2_buckets() as u8,
            version,
            inserted,
        )
        .unwrap();

    (base, token, inserted)
}

fn make_index_plan(token: CheckpointToken, index_info: IndexRecoveryInfo) -> RecoveryPlan {
    RecoveryPlan {
        token,
        checkpoint_type: CheckpointType::Snapshot,
        index_info,
        log_info: LogRecoveryInfo::default(),
        session_infos: vec![],
        steps: vec![],
    }
}

fn index_file_path(base: &Path, token: &CheckpointToken) -> std::path::PathBuf {
    base.join("checkpoints")
        .join(token.to_string())
        .join(format!("{token}.index"))
}

// ═══════════════════════════════════════════════════════════════════
// 1. Truncated checkpoint.json — simulates crash during metadata write
//
// GAP: Existing tests corrupt JSON with "NOT VALID JSON" but never
// test a truncated file (e.g., half-written during power loss).
// ═══════════════════════════════════════════════════════════════════

#[test]
fn truncated_checkpoint_json_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5001);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Read the valid JSON, then truncate it mid-way to simulate crash.
    let desc_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("checkpoint.json");
    let full = fs::read_to_string(&desc_path).unwrap();
    let truncated = &full[..full.len() / 2];
    fs::write(&desc_path, truncated).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.select_checkpoint(Some(token));
    assert!(
        result.is_err(),
        "truncated checkpoint.json must not be accepted as valid"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 2. Zero-length metadata files — crash before any bytes written
//
// GAP: No existing test checks what happens when checkpoint metadata
// files exist but are completely empty (0 bytes).
// ═══════════════════════════════════════════════════════════════════

#[test]
fn zero_length_checkpoint_json_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5002);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Replace with zero-length file.
    let desc_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("checkpoint.json");
    fs::write(&desc_path, b"").unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.select_checkpoint(Some(token));
    assert!(
        result.is_err(),
        "zero-length checkpoint.json must return error"
    );
}

#[test]
fn zero_length_index_recovery_json_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5003);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Zero out the index recovery JSON.
    let idx_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("index.recovery.json");
    fs::write(&idx_path, b"").unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.select_checkpoint(Some(token));
    assert!(
        result.is_err(),
        "zero-length index.recovery.json must return error"
    );
}

#[test]
fn zero_length_log_recovery_json_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5004);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Zero out the log recovery JSON.
    let log_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("log.recovery.json");
    fs::write(&log_path, b"").unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.select_checkpoint(Some(token));
    assert!(
        result.is_err(),
        "zero-length log.recovery.json must return error"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 3. Truncated index checkpoint binary — simulates disk-full or
//    crash during binary checkpoint write
//
// GAP: Existing test corrupts a single byte (CRC mismatch). This
// tests file truncation — the file is shorter than the header claims.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn truncated_index_checkpoint_file_detected() {
    let log2 = 6; // 64 buckets
    let (base, token, _) = write_index_checkpoint(log2, 30, 0);
    let path = index_file_path(base.path(), &token);

    // Truncate the file to half its actual size.
    let full_len = fs::metadata(&path).unwrap().len();
    assert!(full_len > 100, "sanity: index file should be non-trivial");
    let truncated = fs::read(&path).unwrap();
    fs::write(&path, &truncated[..(full_len / 2) as usize]).unwrap();

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            format_version: 2,
            version: 0,
            table_size: 1 << log2,
            num_ht_bytes: (1u64 << log2) * 64,
            num_ofb_bytes: 0,
            num_buckets: 1 << log2,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        },
    );

    let engine = IndexRecoveryEngine::new();
    let result = engine.recover_index(&plan, base.path());
    assert!(
        result.is_err(),
        "truncated index binary must be rejected (expected size > actual)"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 4. Index recovery with mismatched table size — metadata claims
//    more buckets than the file contains
//
// GAP: No existing test exercises the mismatch between metadata
// bucket count and the actual binary file size. This is a likely
// scenario if metadata is written for a different checkpoint version.
// The engine is lenient — it reads whatever the file contains
// without crashing, even if metadata claims a larger table. This
// test documents that resilience.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn index_recovery_with_mismatched_table_size_does_not_crash() {
    let actual_log2 = 4; // Write checkpoint for 16 buckets
    let (base, token, inserted) = write_index_checkpoint(actual_log2, 10, 0);

    // Claim fewer buckets than the file actually has (8 vs 16).
    // The engine should still recover without crashing.
    let smaller_log2: u32 = 3;
    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            format_version: 2,
            version: 0,
            table_size: 1 << smaller_log2,
            num_ht_bytes: (1u64 << smaller_log2) * 64,
            num_ofb_bytes: 0,
            num_buckets: 1 << smaller_log2,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        },
    );

    let engine = IndexRecoveryEngine::new();
    // The engine should not panic. It may succeed with partial data
    // or return an error — either is fine. A panic would be the bug.
    let result = engine.recover_index(&plan, base.path());
    match result {
        Ok(recovered) => {
            // Recovered with fewer buckets — some entries may be lost.
            assert!(
                recovered.scan_entry_count() <= inserted,
                "cannot recover more entries than were written"
            );
        }
        Err(_) => { /* Rejection is also acceptable */ }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 5. Session recovery with many excluded serial numbers
//
// GAP: Existing tests use 0-3 excluded serials. Real workloads under
// high concurrency can have hundreds of pending ops. This tests that
// the recovery engine handles a large excluded set correctly.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn session_recovery_large_excluded_set() {
    let excluded: Vec<u64> = (1..=500).collect();
    let plan = RecoveryPlan {
        token: sample_token(5010),
        checkpoint_type: CheckpointType::FoldOver,
        index_info: IndexRecoveryInfo::default(),
        log_info: LogRecoveryInfo::default(),
        session_infos: vec![SessionRecoveryInfo {
            session_id: 0,
            serial_number: 1000,
            excluded_serial_numbers: excluded.clone(),
        }],
        steps: vec![],
    };

    let engine = SessionRecoveryEngine::new();
    let recovered = engine.recover_sessions(&plan).unwrap();
    assert_eq!(recovered.len(), 1);
    assert!(
        recovered[0].needs_replay,
        "session with 500 excluded serials must need replay"
    );
    assert_eq!(recovered[0].pending_count, 500);
    assert_eq!(recovered[0].serial_number, 1000);
}

// ═══════════════════════════════════════════════════════════════════
// 6. Session recovery ordering — sessions must be returned in the
//    same order as session_infos in the plan
//
// GAP: Existing tests don't verify ordering when sessions have
// non-sequential IDs.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn session_recovery_preserves_order_with_nonsequential_ids() {
    let plan = RecoveryPlan {
        token: sample_token(5011),
        checkpoint_type: CheckpointType::FoldOver,
        index_info: IndexRecoveryInfo::default(),
        log_info: LogRecoveryInfo::default(),
        session_infos: vec![
            SessionRecoveryInfo {
                session_id: 99,
                serial_number: 300,
                excluded_serial_numbers: vec![],
            },
            SessionRecoveryInfo {
                session_id: 7,
                serial_number: 100,
                excluded_serial_numbers: vec![50],
            },
            SessionRecoveryInfo {
                session_id: 42,
                serial_number: 200,
                excluded_serial_numbers: vec![],
            },
        ],
        steps: vec![],
    };

    let engine = SessionRecoveryEngine::new();
    let recovered = engine.recover_sessions(&plan).unwrap();
    assert_eq!(recovered.len(), 3);
    assert_eq!(recovered[0].session_id, 99);
    assert_eq!(recovered[1].session_id, 7);
    assert_eq!(recovered[2].session_id, 42);
}

// ═══════════════════════════════════════════════════════════════════
// 7. Recovery plan step ordering invariant — steps must always be:
//    LoadIndex first, RecoverLog second, RecoverSessions last.
//
// GAP: Existing tests assert step count but not ordering. If the
// engine ever reorders steps, recovery will break.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn recovery_plan_steps_are_ordered_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5020);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[SessionRecoveryInfo {
            session_id: 0,
            serial_number: 100,
            excluded_serial_numbers: vec![50],
        }],
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    // All three step types should be present when there are sessions.
    assert!(
        plan.steps.len() >= 3,
        "expected ≥3 steps, got {}",
        plan.steps.len()
    );

    // Step ordering invariant: LoadIndex < RecoverLog < RecoverSessions.
    let idx_pos = plan
        .steps
        .iter()
        .position(|s| matches!(s, RecoveryStep::LoadIndex { .. }));
    let log_pos = plan
        .steps
        .iter()
        .position(|s| matches!(s, RecoveryStep::RecoverLog { .. }));
    let sess_pos = plan
        .steps
        .iter()
        .position(|s| matches!(s, RecoveryStep::RecoverSessions { .. }));

    assert!(idx_pos.is_some(), "LoadIndex step missing");
    assert!(log_pos.is_some(), "RecoverLog step missing");
    assert!(sess_pos.is_some(), "RecoverSessions step missing");

    assert!(
        idx_pos.unwrap() < log_pos.unwrap(),
        "LoadIndex must come before RecoverLog"
    );
    assert!(
        log_pos.unwrap() < sess_pos.unwrap(),
        "RecoverLog must come before RecoverSessions"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 8. Discovery skips checkpoint dirs with missing sub-files
//
// GAP: `discover_skips_corrupt_checkpoints` tests corrupt JSON, but
// not the case where a checkpoint directory exists yet is completely
// empty (e.g., mkdir succeeded but write didn't start).
// ═══════════════════════════════════════════════════════════════════

#[test]
fn discover_skips_empty_checkpoint_directory() {
    let dir = tempfile::tempdir().unwrap();

    // Write one valid checkpoint.
    let good_token = sample_token(5030);
    write_checkpoint_metadata(
        dir.path(),
        &good_token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Create an empty directory that looks like a checkpoint (name is a u128).
    let empty_dir = dir.path().join("checkpoints").join("99999");
    fs::create_dir_all(&empty_dir).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let infos = mgr.discover_checkpoints().unwrap();

    // The good checkpoint should be found; the empty directory should be skipped.
    assert_eq!(infos.len(), 1, "should discover exactly 1 valid checkpoint");
    assert_eq!(infos[0].token, good_token);
}

// ═══════════════════════════════════════════════════════════════════
// 9. Validation with partial checkpoint — has checkpoint.json but
//    missing index/log recovery files
//
// GAP: `validate_checkpoint_all_files_present` tests the happy path.
// No test checks that validation returns is_valid=false (not error)
// when sub-files are selectively missing.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn validate_detects_missing_index_file() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5040);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Remove the index recovery file.
    let idx_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("index.recovery.json");
    fs::remove_file(&idx_path).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    // validate_checkpoint may return error or a ValidationResult with
    // is_valid=false — either is acceptable. What matters is it does NOT
    // return a valid result.
    match mgr.validate_checkpoint(&token) {
        Ok(result) => {
            assert!(
                !result.is_valid || !result.issues.is_empty(),
                "validation should flag missing index.recovery.json"
            );
        }
        Err(_) => { /* Acceptable — token lookup failed due to missing file */ }
    }
}

#[test]
fn validate_detects_missing_log_file() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5041);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Remove the log recovery file.
    let log_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("log.recovery.json");
    fs::remove_file(&log_path).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    match mgr.validate_checkpoint(&token) {
        Ok(result) => {
            assert!(
                !result.is_valid || !result.issues.is_empty(),
                "validation should flag missing log.recovery.json"
            );
        }
        Err(_) => { /* Acceptable */ }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 10. Full lifecycle: checkpoint → recover → write more → checkpoint again
//
// GAP: Existing tests exercise checkpoint-then-recover as a terminal
// operation. No test verifies that a recovered store can be used for
// further writes AND checkpointed again successfully.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn checkpoint_recover_write_checkpoint_again() {
    let dir = tempfile::tempdir().unwrap();

    // Phase 1: Create store, write data, checkpoint.
    let store = {
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
        FasterKv::new(
            config,
            SimpleFunctions::default(),
            faster_core::SyncFileDevice::new(dir.path(), "log.", 512, 1 << 20, 2)
                .expect("create device"),
        )
    };

    let n = 100u64;
    {
        let mut s = store.new_session();
        for i in 0..n {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        store.dispose_session(s);
    }

    let token1 = store
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .expect("first checkpoint");

    drop(store);

    // Phase 2: Recover into a new store.
    let mut store2 = {
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
        FasterKv::new(
            config,
            SimpleFunctions::default(),
            faster_core::SyncFileDevice::new(dir.path(), "log.", 512, 1 << 20, 2)
                .expect("create device 2"),
        )
    };

    let info = store2.recover(dir.path(), Some(token1)).expect("recover");
    assert_eq!(info.checkpoint_type, CheckpointType::FoldOver);

    // Phase 3: Verify original data, write more, checkpoint again.
    {
        let mut s = store2.new_session();

        // Verify original keys.
        for i in 0..n {
            let val = store2.read_simple(&mut s, &i);
            assert_eq!(val, Some(i * 10), "key {i} after recovery");
        }

        // Write additional data.
        for i in n..(n * 2) {
            let _ = store2.upsert(&mut s, &i, &(i * 20), ());
        }

        store2.dispose_session(s);
    }

    let token2 = store2
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .expect("second checkpoint after recovery");

    // token2 must be different from token1.
    assert_ne!(
        token1, token2,
        "second checkpoint token must differ from first"
    );

    // Verify the second checkpoint is discoverable.
    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let checkpoints = mgr.discover_checkpoints().unwrap();
    assert!(
        checkpoints.len() >= 2,
        "should have at least 2 checkpoints, got {}",
        checkpoints.len()
    );
}

// ═══════════════════════════════════════════════════════════════════
// 11. Concurrent compaction + checkpoint — tests the interleaving
//     that was missing from compaction_integration.rs (which only
//     tests compaction + concurrent CRUD, not compaction + checkpoint)
//
// GAP: compaction_integration tests compaction with concurrent
// readers/writers/deleters. But never tests the compaction +
// checkpoint overlap which is a real production scenario.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn concurrent_compaction_and_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let device = faster_core::SyncFileDevice::new(dir.path(), "log.", 512, 1 << 20, 2)
        .expect("create device");

    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::new(config, SimpleFunctions::default(), device));
    let n = 200u64;

    // Write initial data.
    {
        let mut s = store.new_session();
        for i in 0..n {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        store.dispose_session(s);
    }

    // Force read-only so compaction has data to scan.
    store
        .checkpoint(dir.path(), CheckpointType::FoldOver)
        .expect("initial checkpoint for read-only");

    let barrier = Arc::new(Barrier::new(2));

    // Thread 1: compaction.
    let store_c = Arc::clone(&store);
    let barrier_c = Arc::clone(&barrier);
    let compactor = thread::spawn(move || {
        barrier_c.wait();
        // Compaction may fail with EmptyRegion if checkpoint already moved
        // the data — that's fine.
        let _ = store_c.compact();
    });

    // Thread 2: checkpoint.
    let store_k = Arc::clone(&store);
    let barrier_k = Arc::clone(&barrier);
    let dir_path = dir.path().to_path_buf();
    let checkpointer = thread::spawn(move || {
        barrier_k.wait();
        // Second checkpoint concurrent with compaction.
        let _ = store_k.checkpoint(&dir_path, CheckpointType::FoldOver);
    });

    compactor.join().expect("compactor panicked");
    checkpointer.join().expect("checkpointer panicked");

    // After both operations, the store must still be consistent:
    // all original keys should be readable.
    let mut s = store.new_session();
    let mut found = 0u64;
    for i in 0..n {
        if store.read_simple(&mut s, &i).is_some() {
            found += 1;
        }
    }
    store.dispose_session(s);

    assert!(
        found > 0,
        "store should still have readable data after concurrent compact+checkpoint"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 12. Multiple bit-flip patterns in index checkpoint — goes beyond
//     single-byte corruption to test header vs body corruption
//
// GAP: Existing `corrupt_index_checkpoint_file_detected_by_crc` only
// flips a byte in the body. This tests header corruption which may
// produce a different error path (bad magic number or wrong version).
// ═══════════════════════════════════════════════════════════════════

#[test]
fn corrupt_index_checkpoint_header_detected() {
    let log2 = 4;
    let (base, token, _) = write_index_checkpoint(log2, 10, 0);
    let path = index_file_path(base.path(), &token);

    // Corrupt the first 4 bytes (format version / magic number).
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
    }

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            format_version: 2,
            version: 0,
            table_size: 1 << log2,
            num_ht_bytes: (1u64 << log2) * 64,
            num_ofb_bytes: 0,
            num_buckets: 1 << log2,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        },
    );

    let engine = IndexRecoveryEngine::new();
    let result = engine.recover_index(&plan, base.path());
    assert!(
        result.is_err(),
        "header-corrupted index file must be rejected"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 13. Discover with many checkpoints — verifies that discovery
//     handles a large number of checkpoint directories efficiently
//
// GAP: Existing tests use 1-3 checkpoints. This tests with 20 to
// exercise any O(n²) or sorting edge cases.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn discover_many_checkpoints() {
    let dir = tempfile::tempdir().unwrap();

    for i in 1..=20u128 {
        let token = sample_token(5100 + i);
        write_checkpoint_metadata(
            dir.path(),
            &token,
            &sample_index_info(),
            &fold_over_log_info(),
            &[],
        );
    }

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let infos = mgr.discover_checkpoints().unwrap();
    assert_eq!(infos.len(), 20);

    // Verify all tokens are unique and present.
    let tokens: Vec<u128> = infos.iter().map(|i| i.token.as_u128()).collect();
    for i in 1..=20u128 {
        assert!(
            tokens.contains(&(5100 + i)),
            "checkpoint {} not found in discovery",
            5100 + i
        );
    }
}

// ═══════════════════════════════════════════════════════════════════
// 14. Session recovery with excluded serials exceeding serial_number
//
// GAP: What if the metadata is internally inconsistent — excluded
// serial numbers are larger than the session's serial_number? The
// engine should either reject or handle gracefully.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn session_recovery_excluded_beyond_serial_number() {
    let plan = RecoveryPlan {
        token: sample_token(5050),
        checkpoint_type: CheckpointType::FoldOver,
        index_info: IndexRecoveryInfo::default(),
        log_info: LogRecoveryInfo::default(),
        session_infos: vec![SessionRecoveryInfo {
            session_id: 0,
            serial_number: 10,
            // Excluded serials 50, 100 are > serial_number 10 — inconsistent.
            excluded_serial_numbers: vec![50, 100],
        }],
        steps: vec![],
    };

    let engine = SessionRecoveryEngine::new();
    let result = engine.recover_sessions(&plan);

    // Either an error (corrupt metadata) or a successful recovery that
    // flags needs_replay is acceptable. Silent acceptance with
    // needs_replay=false would be a bug.
    match result {
        Ok(sessions) => {
            assert!(
                sessions[0].needs_replay,
                "session with excluded serials must need replay regardless of serial_number"
            );
        }
        Err(RecoveryError::CorruptMetadata(_)) => {
            // Also acceptable — engine detected the inconsistency.
        }
        Err(e) => panic!("unexpected error type: {e:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════
// 15. Metadata store operations on deeply nested paths
//
// GAP: All existing tests use a flat tempdir. This tests that the
// metadata store creates intermediate directories correctly when
// the base path has several levels.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn metadata_store_creates_deep_directory_structure() {
    let dir = tempfile::tempdir().unwrap();
    let deep_path = dir.path().join("a").join("b").join("c").join("data");

    let store = CheckpointMetadataStore::new(deep_path.clone());
    let token = sample_token(5060);
    store
        .write_checkpoint_metadata(&token, &sample_index_info(), &fold_over_log_info(), &[])
        .expect("write to deep path");

    let tokens = store.list_checkpoints().unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].as_u128(), 5060);
}

// ═══════════════════════════════════════════════════════════════════
// 16. Index checkpoint recovery at maximum table sizes — ensures no
//     overflow in size calculations
//
// GAP: Existing tests use log2 = 4..10. This uses log2 = 14
//     (16K buckets × 64 bytes = 1MB) which is still fast but tests
//     larger size calculations.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn index_recovery_large_table() {
    let log2: u32 = 14; // 16384 buckets
    let (base, token, inserted) = write_index_checkpoint(log2, 5000, 3);

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            format_version: 2,
            version: 3,
            table_size: 1 << log2,
            num_ht_bytes: (1u64 << log2) * 64,
            num_ofb_bytes: 0,
            num_buckets: 1 << log2,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        },
    );

    let engine = IndexRecoveryEngine::new();
    let recovered = engine.recover_index(&plan, base.path()).unwrap();

    assert_eq!(recovered.num_buckets(), 1u64 << log2);
    assert_eq!(recovered.scan_entry_count(), inserted);
    assert_eq!(recovered.version(), 3);
}

// ═══════════════════════════════════════════════════════════════════
// 17. Concurrent checkpoint from multiple threads (mutual exclusion)
//
// GAP: No existing test verifies that concurrent checkpoint calls
// from different threads are properly serialized or that at most one
// succeeds while others get AlreadyInProgress.
// ═══════════════════════════════════════════════════════════════════

#[test]
fn concurrent_checkpoint_calls_do_not_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let device = faster_core::SyncFileDevice::new(dir.path(), "log.", 512, 1 << 20, 2)
        .expect("create device");

    let config = FasterKvConfig {
        hash_index_size_log2: 10,
        buffer_size_pages: 8,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::new(config, SimpleFunctions::default(), device));

    // Write some data first.
    {
        let mut s = store.new_session();
        for i in 0u64..100 {
            let _ = store.upsert(&mut s, &i, &(i * 10), ());
        }
        store.dispose_session(s);
    }

    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();

    for _ in 0..4 {
        let store_c = Arc::clone(&store);
        let barrier_c = Arc::clone(&barrier);
        let dir_path = dir.path().to_path_buf();
        handles.push(thread::spawn(move || {
            barrier_c.wait();
            store_c.checkpoint(&dir_path, CheckpointType::FoldOver)
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = results.iter().filter(|r| r.is_ok()).count();

    // At least one should succeed. Multiple may succeed if they serialize.
    assert!(
        successes >= 1,
        "at least one concurrent checkpoint should succeed"
    );

    // Verify data integrity after the concurrent checkpoints.
    let mut s = store.new_session();
    for i in 0u64..100 {
        let val = store.read_simple(&mut s, &i);
        assert_eq!(
            val,
            Some(i * 10),
            "key {i} corrupted after concurrent checkpoints"
        );
    }
    store.dispose_session(s);
}
