//! Checkpoint/Recovery integration tests (R6).
//!
//! End-to-end tests that exercise the full checkpoint → recovery → verify
//! cycle, covering metadata persistence, index checkpoint/recovery,
//! log recovery, session recovery, multiple sequential checkpoints,
//! and error handling for corrupt/missing files.

use std::fs;
use std::path::Path;

use faster_core::address::{LogicalAddress, Offset, Page};
use faster_core::checkpoint::{
    CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo,
    SessionRecoveryInfo,
};
use faster_core::hash::KeyHash;
use faster_core::hash_bucket::HashBucketEntry;
use faster_core::hash_index::HashIndex;
use faster_core::hash_table::HashTable;
use faster_core::recovery::index_recovery::IndexRecoveryEngine;
use faster_core::recovery::log_recovery::LogRecoveryEngine;
use faster_core::recovery::session_recovery::SessionRecoveryEngine;
use faster_core::recovery::{RecoveryError, RecoveryManager, RecoveryPlan, RecoveryStep};

// ═══════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════

fn sample_token(val: u128) -> CheckpointToken {
    CheckpointToken::new(val)
}

fn sample_index_info() -> IndexRecoveryInfo {
    IndexRecoveryInfo {
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

fn snapshot_log_info() -> LogRecoveryInfo {
    LogRecoveryInfo {
        version: 1,
        checkpoint_type: CheckpointType::Snapshot,
        begin_address: LogicalAddress::new(Page(0), Offset(64)),
        flushed_until_address: LogicalAddress::new(Page(5), Offset(0)),
        final_address: LogicalAddress::new(Page(10), Offset(512)),
        head_address: LogicalAddress::new(Page(2), Offset(0)),
        snapshot_start_address: LogicalAddress::new(Page(5), Offset(0)),
        snapshot_final_address: LogicalAddress::new(Page(10), Offset(512)),
        use_snapshot_file: true,
        object_log_segment_count: 3,
    }
}

fn sample_sessions() -> Vec<SessionRecoveryInfo> {
    vec![
        SessionRecoveryInfo {
            session_id: 0,
            serial_number: 100,
            excluded_serial_numbers: vec![50, 75],
        },
        SessionRecoveryInfo {
            session_id: 1,
            serial_number: 200,
            excluded_serial_numbers: vec![],
        },
    ]
}

/// Writes checkpoint metadata to disk via CheckpointMetadataStore.
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

/// Creates a HashIndex, populates it with entries, and writes a binary
/// index checkpoint. Returns (TempDir, token, num_entries_inserted).
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
    let inserted = populate_table(&table, entry_count);

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

/// Populates a HashTable with entries using a deterministic hash function.
fn populate_table(table: &HashTable, count: u64) -> u64 {
    let mut inserted = 0u64;
    for i in 0..count {
        let hash = KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let result = table.find_or_create_entry(hash, LogicalAddress::INVALID);
        if result.created {
            let committed =
                HashBucketEntry::new(result.entry.tag(), LogicalAddress::from_raw(i + 1), false);
            table.update_entry(result.slot, result.entry, committed);
            inserted += 1;
        }
    }
    inserted
}

/// Builds a RecoveryPlan for index recovery tests.
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

/// Returns the path to the `.index` binary checkpoint file.
fn index_file_path(base: &Path, token: &CheckpointToken) -> std::path::PathBuf {
    base.join("checkpoints")
        .join(token.to_string())
        .join(format!("{token}.index"))
}

// ═══════════════════════════════════════════════════════════════════
// 1. Fold-over checkpoint metadata persistence and recovery
// ═══════════════════════════════════════════════════════════════════

#[test]
fn fold_over_metadata_write_and_recover() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(1);
    let index_info = sample_index_info();
    let log_info = fold_over_log_info();
    let sessions = sample_sessions();

    write_checkpoint_metadata(dir.path(), &token, &index_info, &log_info, &sessions);

    // Verify checkpoint directory structure exists
    let ckpt_dir = dir.path().join("checkpoints").join(token.to_string());
    assert!(ckpt_dir.join("checkpoint.json").exists());
    assert!(ckpt_dir.join("index.recovery.json").exists());
    assert!(ckpt_dir.join("log.recovery.json").exists());
    assert!(ckpt_dir.join("sessions").join("0.recovery.json").exists());
    assert!(ckpt_dir.join("sessions").join("1.recovery.json").exists());

    // Recover via RecoveryManager
    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    assert_eq!(plan.token, token);
    assert_eq!(plan.checkpoint_type, CheckpointType::FoldOver);
    assert_eq!(plan.index_info, index_info);
    assert_eq!(plan.log_info, log_info);
    assert_eq!(plan.session_infos.len(), 2);
    assert_eq!(plan.session_infos[0].session_id, 0);
    assert_eq!(plan.session_infos[0].serial_number, 100);
    assert_eq!(plan.session_infos[1].session_id, 1);
    assert_eq!(plan.session_infos[1].serial_number, 200);
}

// ═══════════════════════════════════════════════════════════════════
// 2. Snapshot checkpoint metadata persistence and recovery
// ═══════════════════════════════════════════════════════════════════

#[test]
fn snapshot_metadata_write_and_recover() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(2);
    let index_info = sample_index_info();
    let log_info = snapshot_log_info();
    let sessions = sample_sessions();

    write_checkpoint_metadata(dir.path(), &token, &index_info, &log_info, &sessions);

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    assert_eq!(plan.checkpoint_type, CheckpointType::Snapshot);
    assert!(plan.log_info.use_snapshot_file);
    assert_eq!(
        plan.log_info.snapshot_start_address,
        log_info.snapshot_start_address
    );
    assert_eq!(
        plan.log_info.snapshot_final_address,
        log_info.snapshot_final_address
    );

    // Recovery plan should use snapshot_final_address for log recovery
    let log_step = plan
        .steps
        .iter()
        .find(|s| matches!(s, RecoveryStep::RecoverLog { .. }));
    assert!(log_step.is_some(), "expected RecoverLog step");
    if let Some(RecoveryStep::RecoverLog {
        checkpoint_type,
        until_address,
        ..
    }) = log_step
    {
        assert_eq!(*checkpoint_type, CheckpointType::Snapshot);
        assert_eq!(*until_address, log_info.snapshot_final_address);
    }
}

// ═══════════════════════════════════════════════════════════════════
// 3. Checkpoint with mixed session states
// ═══════════════════════════════════════════════════════════════════

#[test]
fn checkpoint_mixed_session_states() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(3);

    let sessions = vec![
        SessionRecoveryInfo {
            session_id: 0,
            serial_number: 1000,
            excluded_serial_numbers: vec![],
        },
        SessionRecoveryInfo {
            session_id: 1,
            serial_number: 2000,
            excluded_serial_numbers: vec![1990, 1995],
        },
        SessionRecoveryInfo {
            session_id: 2,
            serial_number: 500,
            excluded_serial_numbers: vec![499],
        },
    ];

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &sessions,
    );

    // Recover and verify session states
    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    let engine = SessionRecoveryEngine::new();
    let recovered = engine.recover_sessions(&plan).unwrap();

    assert_eq!(recovered.len(), 3);

    // Session 0: clean, no replay needed
    assert_eq!(recovered[0].session_id, 0);
    assert_eq!(recovered[0].serial_number, 1000);
    assert!(!recovered[0].needs_replay);
    assert_eq!(recovered[0].pending_count, 0);

    // Session 1: had in-flight ops, needs replay
    assert_eq!(recovered[1].session_id, 1);
    assert_eq!(recovered[1].serial_number, 2000);
    assert!(recovered[1].needs_replay);
    assert_eq!(recovered[1].pending_count, 2);

    // Session 2: had in-flight ops, needs replay
    assert_eq!(recovered[2].session_id, 2);
    assert_eq!(recovered[2].serial_number, 500);
    assert!(recovered[2].needs_replay);
    assert_eq!(recovered[2].pending_count, 1);
}

// ═══════════════════════════════════════════════════════════════════
// 4. Multiple sequential checkpoints
// ═══════════════════════════════════════════════════════════════════

#[test]
fn multiple_sequential_checkpoints_recover_from_either() {
    let dir = tempfile::tempdir().unwrap();

    // Checkpoint 1: sessions at serial 100
    let t1 = sample_token(100);
    let sessions_1 = vec![SessionRecoveryInfo {
        session_id: 0,
        serial_number: 100,
        excluded_serial_numbers: vec![],
    }];
    let index_info_1 = IndexRecoveryInfo {
        version: 1,
        table_size: 1024,
        num_ht_bytes: 1024 * 64,
        num_ofb_bytes: 0,
        num_buckets: 1024,
        start_logical_address: LogicalAddress::ZERO,
        final_logical_address: LogicalAddress::new(Page(5), Offset(0)),
    };

    write_checkpoint_metadata(
        dir.path(),
        &t1,
        &index_info_1,
        &fold_over_log_info(),
        &sessions_1,
    );

    // Checkpoint 2: sessions at serial 200 (more progress)
    let t2 = sample_token(200);
    let sessions_2 = vec![SessionRecoveryInfo {
        session_id: 0,
        serial_number: 200,
        excluded_serial_numbers: vec![],
    }];
    let index_info_2 = IndexRecoveryInfo {
        version: 2,
        table_size: 1024,
        num_ht_bytes: 1024 * 64,
        num_ofb_bytes: 0,
        num_buckets: 1024,
        start_logical_address: LogicalAddress::ZERO,
        final_logical_address: LogicalAddress::new(Page(10), Offset(0)),
    };

    write_checkpoint_metadata(
        dir.path(),
        &t2,
        &index_info_2,
        &fold_over_log_info(),
        &sessions_2,
    );

    // Patch t2 to have a definitively later timestamp
    let desc_path = dir
        .path()
        .join("checkpoints")
        .join(t2.to_string())
        .join("checkpoint.json");
    let raw = fs::read_to_string(&desc_path).unwrap();
    let patched = raw.replace(
        &raw.lines()
            .find(|l| l.contains("created_at"))
            .unwrap()
            .to_string(),
        r#"  "created_at": "2099-12-31T23:59:59Z""#,
    );
    fs::write(&desc_path, patched).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());

    // Recover from checkpoint 2 (latest): should have serial 200
    let plan2 = mgr.select_checkpoint(None).unwrap();
    assert_eq!(plan2.token, t2);
    assert_eq!(plan2.session_infos[0].serial_number, 200);
    assert_eq!(plan2.index_info.version, 2);

    // Recover from checkpoint 1 (specific): should have serial 100
    let plan1 = mgr.select_checkpoint(Some(t1)).unwrap();
    assert_eq!(plan1.token, t1);
    assert_eq!(plan1.session_infos[0].serial_number, 100);
    assert_eq!(plan1.index_info.version, 1);
}

// ═══════════════════════════════════════════════════════════════════
// 5. Session state preservation
// ═══════════════════════════════════════════════════════════════════

#[test]
fn session_recovery_preserves_serial_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(5);

    let sessions = vec![
        SessionRecoveryInfo {
            session_id: 0,
            serial_number: 999_999,
            excluded_serial_numbers: vec![],
        },
        SessionRecoveryInfo {
            session_id: 1,
            serial_number: 42,
            excluded_serial_numbers: vec![40, 41],
        },
        SessionRecoveryInfo {
            session_id: 5,
            serial_number: 1,
            excluded_serial_numbers: vec![],
        },
    ];

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &sessions,
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    let engine = SessionRecoveryEngine::new();
    let recovered = engine.recover_sessions(&plan).unwrap();

    assert_eq!(recovered.len(), 3);

    // Verify serial numbers are exactly preserved
    let by_id: std::collections::HashMap<usize, _> =
        recovered.into_iter().map(|s| (s.session_id, s)).collect();

    assert_eq!(by_id[&0].serial_number, 999_999);
    assert!(!by_id[&0].needs_replay);

    assert_eq!(by_id[&1].serial_number, 42);
    assert!(by_id[&1].needs_replay);
    assert_eq!(by_id[&1].pending_count, 2);

    assert_eq!(by_id[&5].serial_number, 1);
    assert!(!by_id[&5].needs_replay);
}

#[test]
fn session_recovery_with_no_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(50);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    assert!(plan.session_infos.is_empty());

    // No RecoverSessions step in the plan
    assert!(
        !plan
            .steps
            .iter()
            .any(|s| matches!(s, RecoveryStep::RecoverSessions { .. }))
    );

    let engine = SessionRecoveryEngine::new();
    let recovered = engine.recover_sessions(&plan).unwrap();
    assert!(recovered.is_empty());
}

// ═══════════════════════════════════════════════════════════════════
// 6. Index integrity after recovery
// ═══════════════════════════════════════════════════════════════════

#[test]
fn index_checkpoint_recovery_roundtrip_small() {
    let log2 = 6; // 64 buckets
    let (base, token, inserted) = write_index_checkpoint(log2, 30, 1);

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            version: 1,
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
    assert_eq!(recovered.version(), 1);
}

#[test]
fn index_checkpoint_recovery_preserves_entries() {
    let log2: u32 = 8; // 256 buckets
    let base = tempfile::tempdir().unwrap();
    let token = CheckpointToken::new(99);
    let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
    fs::create_dir_all(&ckpt_dir).unwrap();

    // Create a HashIndex, populate it, and checkpoint
    let original = HashIndex::new(log2);
    let thread = original.register_thread().unwrap();
    let _guard = thread.protect();

    let hashes: Vec<KeyHash> = (0..40u64)
        .map(|i| KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
        .collect();

    for (i, &hash) in hashes.iter().enumerate() {
        let r = original.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r.created, "entry {i} was not newly created");
        let committed =
            HashBucketEntry::new(r.entry.tag(), LogicalAddress::from_raw(i as u64 + 1), false);
        original.update(r.slot, r.entry, committed);
    }

    let entry_count = original.entry_count();
    original.set_version(3);

    let mut writer =
        faster_core::checkpoint::index_writer::IndexCheckpointWriter::new(&ckpt_dir, &token)
            .unwrap();
    writer
        .write_index(
            original.checkpoint_bucket_slice(),
            original.log2_buckets() as u8,
            3,
            entry_count,
        )
        .unwrap();

    drop(_guard);
    drop(thread);

    // Recover the index
    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
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

    // Verify every entry is findable via hash lookup
    let thread2 = recovered.register_thread().unwrap();
    let _guard2 = thread2.protect();

    for (i, &hash) in hashes.iter().enumerate() {
        let result = recovered.find(hash);
        assert!(result.is_some(), "entry {i} not found after recovery");
        let (entry, _) = result.unwrap();
        assert_eq!(
            entry.address(),
            LogicalAddress::from_raw(i as u64 + 1),
            "address mismatch for entry {i}",
        );
    }

    assert_eq!(recovered.version(), 3);
    assert_eq!(recovered.scan_entry_count(), entry_count);
}

#[test]
fn index_recovery_with_many_entries_per_bucket() {
    // Use a small hash table (8 = 256 buckets) with many entries to
    // exercise multiple entries per bucket (7 slots each).
    let log2: u32 = 8; // 256 buckets × 7 slots = 1792 capacity
    let (base, token, inserted) = write_index_checkpoint(log2, 800, 0);

    // Many entries per bucket should be inserted
    assert!(inserted > 256, "expected heavy bucket fill, got {inserted}");

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
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
    let recovered = engine.recover_index(&plan, base.path()).unwrap();
    assert_eq!(recovered.scan_entry_count(), inserted);
}

// ═══════════════════════════════════════════════════════════════════
// 7. Large-scale index checkpoint/recovery
// ═══════════════════════════════════════════════════════════════════

#[test]
fn large_scale_index_checkpoint_recovery() {
    let log2: u32 = 14; // 16384 buckets
    let entry_target = 5_000u64;
    let (base, token, inserted) = write_index_checkpoint(log2, entry_target, 7);

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            version: 7,
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
    assert_eq!(recovered.version(), 7);

    // Verify a random sample of entries by hash lookup
    let thread = recovered.register_thread().unwrap();
    let _guard = thread.protect();

    for i in (0..entry_target).step_by(50) {
        let hash = KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let result = recovered.find(hash);
        if let Some((entry, _)) = result {
            assert!(!entry.is_empty());
            assert!(!entry.is_tentative());
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 8. Error handling — corrupt checkpoint metadata
// ═══════════════════════════════════════════════════════════════════

#[test]
fn corrupt_checkpoint_json_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(80);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Corrupt the checkpoint.json master descriptor
    let desc_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("checkpoint.json");
    fs::write(&desc_path, b"NOT VALID JSON {{{}").unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.select_checkpoint(Some(token));
    assert!(
        result.is_err(),
        "expected error for corrupt checkpoint.json"
    );
}

#[test]
fn corrupt_index_recovery_json_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(81);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Corrupt the index.recovery.json
    let idx_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("index.recovery.json");
    fs::write(&idx_path, b"CORRUPT").unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.select_checkpoint(Some(token));
    assert!(
        result.is_err(),
        "expected error for corrupt index.recovery.json"
    );
}

#[test]
fn missing_index_file_detected_during_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(82);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Remove the index.recovery.json file
    let idx_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("index.recovery.json");
    fs::remove_file(&idx_path).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    // read_checkpoint_metadata will fail because index file is missing
    let result = mgr.select_checkpoint(Some(token));
    assert!(result.is_err());
}

#[test]
fn missing_log_file_detected_during_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(83);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Remove the log.recovery.json file
    let log_path = dir
        .path()
        .join("checkpoints")
        .join(token.to_string())
        .join("log.recovery.json");
    fs::remove_file(&log_path).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.select_checkpoint(Some(token));
    assert!(result.is_err());
}

#[test]
fn nonexistent_token_returns_checkpoint_not_found() {
    let dir = tempfile::tempdir().unwrap();
    // Write one checkpoint so the checkpoints/ dir exists
    write_checkpoint_metadata(
        dir.path(),
        &sample_token(1),
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let err = mgr.select_checkpoint(Some(sample_token(9999))).unwrap_err();
    assert!(matches!(err, RecoveryError::CheckpointNotFound(9999)));
}

#[test]
fn no_checkpoints_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let err = mgr.select_checkpoint(None).unwrap_err();
    assert!(matches!(err, RecoveryError::NoCheckpointsFound));
}

#[test]
fn corrupt_index_checkpoint_file_detected_by_crc() {
    let log2 = 4;
    let (base, token, _) = write_index_checkpoint(log2, 10, 0);
    let path = index_file_path(base.path(), &token);

    // Corrupt a byte in the body area
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        // Seek past the 28-byte header into the bucket body
        file.seek(SeekFrom::Start(28 + 32)).unwrap();
        file.write_all(&[0xFF]).unwrap();
    }

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
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
        "expected error for CRC-corrupted index file"
    );
}

// ═══════════════════════════════════════════════════════════════════
// 9. Validation result checks
// ═══════════════════════════════════════════════════════════════════

#[test]
fn validate_checkpoint_all_files_present() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(90);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &sample_sessions(),
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let result = mgr.validate_checkpoint(&token).unwrap();

    assert!(result.is_valid);
    assert!(result.index_file_exists);
    assert!(result.log_files_exist);
    assert!(result.session_files_exist);
    assert!(result.issues.is_empty());
}

#[test]
fn validate_checkpoint_nonexistent_token() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let err = mgr.validate_checkpoint(&sample_token(999)).unwrap_err();
    assert!(matches!(err, RecoveryError::CheckpointNotFound(999)));
}

// ═══════════════════════════════════════════════════════════════════
// 10. Discovery of multiple checkpoints
// ═══════════════════════════════════════════════════════════════════

#[test]
fn discover_all_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let tokens: Vec<_> = (1..=5).map(sample_token).collect();

    for token in &tokens {
        write_checkpoint_metadata(
            dir.path(),
            token,
            &sample_index_info(),
            &fold_over_log_info(),
            &[],
        );
    }

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let infos = mgr.discover_checkpoints().unwrap();
    assert_eq!(infos.len(), 5);

    let found_tokens: Vec<_> = infos.iter().map(|i| i.token).collect();
    for t in &tokens {
        assert!(found_tokens.contains(t), "missing token {t}");
    }
}

#[test]
fn discover_skips_corrupt_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let good = sample_token(10);
    let corrupt = sample_token(20);

    write_checkpoint_metadata(
        dir.path(),
        &good,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );
    write_checkpoint_metadata(
        dir.path(),
        &corrupt,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    // Corrupt the second checkpoint's index file so reading fails
    let idx_path = dir
        .path()
        .join("checkpoints")
        .join(corrupt.to_string())
        .join("index.recovery.json");
    fs::write(&idx_path, b"NOT JSON").unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let infos = mgr.discover_checkpoints().unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].token, good);
}

// ═══════════════════════════════════════════════════════════════════
// 11. Recovery plan step generation
// ═══════════════════════════════════════════════════════════════════

#[test]
fn recovery_plan_fold_over_has_correct_steps() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(110);
    let log_info = fold_over_log_info();

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &log_info,
        &sample_sessions(),
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    assert_eq!(plan.steps.len(), 3); // LoadIndex + RecoverLog + RecoverSessions

    // Step 1: LoadIndex
    match &plan.steps[0] {
        RecoveryStep::LoadIndex {
            path,
            expected_buckets,
        } => {
            assert!(path.ends_with("index.recovery.json"));
            assert_eq!(*expected_buckets, sample_index_info().num_buckets);
        }
        other => panic!("expected LoadIndex, got {other:?}"),
    }

    // Step 2: RecoverLog with fold-over type uses final_address
    match &plan.steps[1] {
        RecoveryStep::RecoverLog {
            checkpoint_type,
            begin_address,
            until_address,
        } => {
            assert_eq!(*checkpoint_type, CheckpointType::FoldOver);
            assert_eq!(*begin_address, log_info.begin_address);
            assert_eq!(*until_address, log_info.final_address);
        }
        other => panic!("expected RecoverLog, got {other:?}"),
    }

    // Step 3: RecoverSessions
    match &plan.steps[2] {
        RecoveryStep::RecoverSessions { count } => {
            assert_eq!(*count, 2);
        }
        other => panic!("expected RecoverSessions, got {other:?}"),
    }
}

#[test]
fn recovery_plan_snapshot_uses_snapshot_final_address() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(111);
    let log_info = snapshot_log_info();

    write_checkpoint_metadata(dir.path(), &token, &sample_index_info(), &log_info, &[]);

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    // No sessions → only 2 steps
    assert_eq!(plan.steps.len(), 2);

    match &plan.steps[1] {
        RecoveryStep::RecoverLog {
            checkpoint_type,
            until_address,
            ..
        } => {
            assert_eq!(*checkpoint_type, CheckpointType::Snapshot);
            assert_eq!(*until_address, log_info.snapshot_final_address);
        }
        other => panic!("expected RecoverLog, got {other:?}"),
    }
}

#[test]
fn recovery_plan_no_sessions_omits_session_step() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(112);

    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &[],
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    assert_eq!(plan.steps.len(), 2);
    assert!(
        !plan
            .steps
            .iter()
            .any(|s| matches!(s, RecoveryStep::RecoverSessions { .. }))
    );
}

// ═══════════════════════════════════════════════════════════════════
// 12. Log recovery engine — fold-over with empty log
// ═══════════════════════════════════════════════════════════════════

#[test]
fn log_recovery_fold_over_empty_log() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(120);

    let log_info = LogRecoveryInfo {
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
    };

    let plan = RecoveryPlan {
        token,
        checkpoint_type: CheckpointType::FoldOver,
        index_info: IndexRecoveryInfo::default(),
        log_info,
        session_infos: vec![],
        steps: vec![],
    };

    let engine = LogRecoveryEngine::new();
    let result = engine.recover_fold_over(&plan, dir.path()).unwrap();

    assert_eq!(result.begin_address, LogicalAddress::ZERO);
    assert_eq!(result.tail_address, LogicalAddress::ZERO);
    assert_eq!(result.head_address, LogicalAddress::ZERO);
    assert_eq!(result.records_scanned, 0);
}

#[test]
fn log_recovery_fold_over_rejects_snapshot_type() {
    let plan = RecoveryPlan {
        token: sample_token(121),
        checkpoint_type: CheckpointType::Snapshot,
        index_info: IndexRecoveryInfo::default(),
        log_info: LogRecoveryInfo {
            checkpoint_type: CheckpointType::Snapshot,
            ..LogRecoveryInfo::default()
        },
        session_infos: vec![],
        steps: vec![],
    };

    let engine = LogRecoveryEngine::new();
    let result = engine.recover_fold_over(&plan, Path::new("/nonexistent"));
    assert!(result.is_err(), "fold_over should reject snapshot type");
}

#[test]
fn log_recovery_snapshot_rejects_fold_over_type() {
    let plan = RecoveryPlan {
        token: sample_token(122),
        checkpoint_type: CheckpointType::FoldOver,
        index_info: IndexRecoveryInfo::default(),
        log_info: LogRecoveryInfo {
            checkpoint_type: CheckpointType::FoldOver,
            use_snapshot_file: false,
            ..LogRecoveryInfo::default()
        },
        session_infos: vec![],
        steps: vec![],
    };

    let engine = LogRecoveryEngine::new();
    let result = engine.recover_snapshot(&plan, Path::new("/nonexistent"));
    assert!(result.is_err(), "snapshot should reject fold-over type");
}

// ═══════════════════════════════════════════════════════════════════
// 13. Metadata round-trip serialization integrity
// ═══════════════════════════════════════════════════════════════════

#[test]
fn metadata_round_trip_preserves_all_fields() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(130);
    let index_info = sample_index_info();
    let log_info = snapshot_log_info();
    let sessions = sample_sessions();

    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    store
        .write_checkpoint_metadata(&token, &index_info, &log_info, &sessions)
        .unwrap();

    let meta = store.read_checkpoint_metadata(&token).unwrap();

    assert_eq!(meta.token, token);
    assert_eq!(meta.checkpoint_type, CheckpointType::Snapshot);
    assert_eq!(meta.index_info, index_info);
    assert_eq!(meta.log_info, log_info);
    assert_eq!(meta.session_infos.len(), 2);
    assert_eq!(meta.session_infos[0], sessions[0]);
    assert_eq!(meta.session_infos[1], sessions[1]);
    assert!(!meta.created_at.is_empty());
}

#[test]
fn metadata_store_list_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());

    // Initially empty
    let tokens = store.list_checkpoints().unwrap();
    assert!(tokens.is_empty());

    // Write 3 checkpoints
    for val in [10, 20, 30] {
        let token = sample_token(val);
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &fold_over_log_info(), &[])
            .unwrap();
    }

    let tokens = store.list_checkpoints().unwrap();
    assert_eq!(tokens.len(), 3);

    // Tokens sorted by value
    assert_eq!(tokens[0].as_u128(), 10);
    assert_eq!(tokens[1].as_u128(), 20);
    assert_eq!(tokens[2].as_u128(), 30);
}

#[test]
fn metadata_store_delete_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    let token = sample_token(140);

    store
        .write_checkpoint_metadata(&token, &sample_index_info(), &fold_over_log_info(), &[])
        .unwrap();

    assert_eq!(store.list_checkpoints().unwrap().len(), 1);

    store.delete_checkpoint(&token).unwrap();

    assert_eq!(store.list_checkpoints().unwrap().len(), 0);
    assert!(store.read_checkpoint_metadata(&token).is_err());
}

// ═══════════════════════════════════════════════════════════════════
// 14. Session recovery edge cases
// ═══════════════════════════════════════════════════════════════════

#[test]
fn session_recovery_rejects_duplicate_session_ids() {
    let plan = RecoveryPlan {
        token: sample_token(140),
        checkpoint_type: CheckpointType::FoldOver,
        index_info: IndexRecoveryInfo::default(),
        log_info: LogRecoveryInfo::default(),
        session_infos: vec![
            SessionRecoveryInfo {
                session_id: 1,
                serial_number: 100,
                excluded_serial_numbers: vec![],
            },
            SessionRecoveryInfo {
                session_id: 1,
                serial_number: 200,
                excluded_serial_numbers: vec![],
            },
        ],
        steps: vec![],
    };

    let engine = SessionRecoveryEngine::new();
    let err = engine.recover_sessions(&plan).unwrap_err();
    match &err {
        RecoveryError::CorruptMetadata(msg) => {
            assert!(msg.contains("duplicate session ID"), "got: {msg}");
        }
        other => panic!("expected CorruptMetadata, got {other:?}"),
    }
}

#[test]
fn session_recovery_rejects_zero_serial_with_excluded() {
    let engine = SessionRecoveryEngine::new();
    let info = SessionRecoveryInfo {
        session_id: 10,
        serial_number: 0,
        excluded_serial_numbers: vec![1, 2, 3],
    };
    let err = engine.recover_session(&info).unwrap_err();
    match &err {
        RecoveryError::CorruptMetadata(msg) => {
            assert!(msg.contains("serial_number is 0"), "got: {msg}");
        }
        other => panic!("expected CorruptMetadata, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════
// 15. RecoveryError display formatting
// ═══════════════════════════════════════════════════════════════════

#[test]
fn recovery_error_display_formatting() {
    let err = RecoveryError::NoCheckpointsFound;
    assert_eq!(format!("{err}"), "no completed checkpoints found");

    let err = RecoveryError::CheckpointNotFound(42);
    let msg = format!("{err}");
    assert!(msg.contains("checkpoint not found"), "got: {msg}");

    let err = RecoveryError::CorruptMetadata("bad json".into());
    assert!(format!("{err}").contains("bad json"));

    let err = RecoveryError::ValidationFailed(vec!["issue1".into(), "issue2".into()]);
    let msg = format!("{err}");
    assert!(msg.contains("issue1"));
    assert!(msg.contains("issue2"));

    let err = RecoveryError::IoError(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "test error",
    ));
    let msg = format!("{err}");
    assert!(msg.contains("test error"));
}

// ═══════════════════════════════════════════════════════════════════
// 16. Full checkpoint → recovery → verify cycle (metadata layer)
// ═══════════════════════════════════════════════════════════════════

#[test]
fn full_cycle_write_discover_select_validate_recover_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(160);
    let sessions = vec![
        SessionRecoveryInfo {
            session_id: 0,
            serial_number: 5000,
            excluded_serial_numbers: vec![4999],
        },
        SessionRecoveryInfo {
            session_id: 1,
            serial_number: 3000,
            excluded_serial_numbers: vec![],
        },
        SessionRecoveryInfo {
            session_id: 2,
            serial_number: 1000,
            excluded_serial_numbers: vec![998, 999],
        },
    ];

    // 1. Write checkpoint metadata
    write_checkpoint_metadata(
        dir.path(),
        &token,
        &sample_index_info(),
        &fold_over_log_info(),
        &sessions,
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());

    // 2. Discover
    let infos = mgr.discover_checkpoints().unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].token, token);
    assert_eq!(infos[0].session_count, 3);

    // 3. Select
    let plan = mgr.select_checkpoint(None).unwrap();
    assert_eq!(plan.token, token);

    // 4. Validate
    let validation = mgr.validate_checkpoint(&token).unwrap();
    assert!(validation.is_valid);
    assert!(validation.issues.is_empty());

    // 5. Recover sessions
    let engine = SessionRecoveryEngine::new();
    let recovered = engine.recover_sessions(&plan).unwrap();
    assert_eq!(recovered.len(), 3);

    // Session 0: needs replay (1 pending)
    assert!(recovered[0].needs_replay);
    assert_eq!(recovered[0].pending_count, 1);

    // Session 1: clean
    assert!(!recovered[1].needs_replay);

    // Session 2: needs replay (2 pending)
    assert!(recovered[2].needs_replay);
    assert_eq!(recovered[2].pending_count, 2);
}

// ═══════════════════════════════════════════════════════════════════
// 17. Full index checkpoint → recovery → verify all entries
// ═══════════════════════════════════════════════════════════════════

#[test]
fn full_index_cycle_checkpoint_recover_verify_all_entries() {
    let log2: u32 = 10; // 1024 buckets
    let base = tempfile::tempdir().unwrap();
    let token = CheckpointToken::new(170);
    let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
    fs::create_dir_all(&ckpt_dir).unwrap();

    // Phase 1: Create and populate an index
    let original = HashIndex::new(log2);
    let thread = original.register_thread().unwrap();
    let _guard = thread.protect();

    let num_entries = 500u64;
    let hashes: Vec<KeyHash> = (0..num_entries)
        .map(|i| KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
        .collect();

    let mut addresses = Vec::new();
    for (i, &hash) in hashes.iter().enumerate() {
        let r = original.find_or_create(hash, LogicalAddress::INVALID);
        if r.created {
            let addr = LogicalAddress::from_raw(i as u64 + 1);
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            original.update(r.slot, r.entry, committed);
            addresses.push((i, hash, addr));
        }
    }

    let entry_count = original.entry_count();
    original.set_version(5);

    // Phase 2: Checkpoint the index
    let mut writer =
        faster_core::checkpoint::index_writer::IndexCheckpointWriter::new(&ckpt_dir, &token)
            .unwrap();
    writer
        .write_index(
            original.checkpoint_bucket_slice(),
            original.log2_buckets() as u8,
            5,
            entry_count,
        )
        .unwrap();

    drop(_guard);
    drop(thread);

    // Phase 3: Recover from checkpoint
    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            version: 5,
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

    // Phase 4: Verify ALL entries
    let thread2 = recovered.register_thread().unwrap();
    let _guard2 = thread2.protect();

    for (i, hash, expected_addr) in &addresses {
        let result = recovered.find(*hash);
        assert!(result.is_some(), "entry {i} not found after recovery");
        let (entry, _) = result.unwrap();
        assert_eq!(
            entry.address(),
            *expected_addr,
            "address mismatch for entry {i}",
        );
        assert!(!entry.is_tentative(), "entry {i} should not be tentative");
    }

    assert_eq!(recovered.version(), 5);
    assert_eq!(recovered.scan_entry_count(), entry_count);
}

// ═══════════════════════════════════════════════════════════════════
// 18. Multiple metadata checkpoints with different session states
// ═══════════════════════════════════════════════════════════════════

#[test]
fn multiple_checkpoints_different_session_snapshots() {
    let dir = tempfile::tempdir().unwrap();

    // Checkpoint 1: session at serial 50
    let t1 = sample_token(181);
    write_checkpoint_metadata(
        dir.path(),
        &t1,
        &sample_index_info(),
        &fold_over_log_info(),
        &[SessionRecoveryInfo {
            session_id: 0,
            serial_number: 50,
            excluded_serial_numbers: vec![],
        }],
    );

    // Checkpoint 2: session at serial 150 with pending ops
    let t2 = sample_token(182);
    write_checkpoint_metadata(
        dir.path(),
        &t2,
        &sample_index_info(),
        &fold_over_log_info(),
        &[SessionRecoveryInfo {
            session_id: 0,
            serial_number: 150,
            excluded_serial_numbers: vec![149],
        }],
    );

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let engine = SessionRecoveryEngine::new();

    // Recover from checkpoint 1: clean session
    let plan1 = mgr.select_checkpoint(Some(t1)).unwrap();
    let sessions1 = engine.recover_sessions(&plan1).unwrap();
    assert_eq!(sessions1[0].serial_number, 50);
    assert!(!sessions1[0].needs_replay);

    // Recover from checkpoint 2: session needs replay
    let plan2 = mgr.select_checkpoint(Some(t2)).unwrap();
    let sessions2 = engine.recover_sessions(&plan2).unwrap();
    assert_eq!(sessions2[0].serial_number, 150);
    assert!(sessions2[0].needs_replay);
}

// ═══════════════════════════════════════════════════════════════════
// 19. Checkpoint metadata store overwrite
// ═══════════════════════════════════════════════════════════════════

#[test]
fn metadata_store_overwrite_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    let token = sample_token(190);

    // Write initial version
    let sessions_v1 = vec![SessionRecoveryInfo {
        session_id: 0,
        serial_number: 100,
        excluded_serial_numbers: vec![],
    }];
    store
        .write_checkpoint_metadata(
            &token,
            &sample_index_info(),
            &fold_over_log_info(),
            &sessions_v1,
        )
        .unwrap();

    // Overwrite with updated sessions
    let sessions_v2 = vec![SessionRecoveryInfo {
        session_id: 0,
        serial_number: 200,
        excluded_serial_numbers: vec![199],
    }];
    store
        .write_checkpoint_metadata(
            &token,
            &sample_index_info(),
            &fold_over_log_info(),
            &sessions_v2,
        )
        .unwrap();

    // Read back — should have v2 data
    let meta = store.read_checkpoint_metadata(&token).unwrap();
    assert_eq!(meta.session_infos.len(), 1);
    assert_eq!(meta.session_infos[0].serial_number, 200);
    assert_eq!(meta.session_infos[0].excluded_serial_numbers, vec![199]);
}

// ═══════════════════════════════════════════════════════════════════
// 20. Index recovery with epoch sharing
// ═══════════════════════════════════════════════════════════════════

#[test]
fn index_recovery_with_shared_epoch() {
    let log2: u32 = 8;
    let (base, token, inserted) = write_index_checkpoint(log2, 50, 2);

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
            version: 2,
            table_size: 1 << log2,
            num_ht_bytes: (1u64 << log2) * 64,
            num_ofb_bytes: 0,
            num_buckets: 1 << log2,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        },
    );

    // Use a shared epoch table (as FasterKv would during real recovery)
    let epoch = std::sync::Arc::new(faster_core::epoch::EpochTable::new());

    let engine = IndexRecoveryEngine::new();
    let recovered = engine
        .recover_index_with_epoch(&plan, base.path(), epoch)
        .unwrap();

    assert_eq!(recovered.num_buckets(), 1u64 << log2);
    assert_eq!(recovered.scan_entry_count(), inserted);
    assert_eq!(recovered.version(), 2);
}

// ═══════════════════════════════════════════════════════════════════
// 21. Empty index checkpoint and recovery
// ═══════════════════════════════════════════════════════════════════

#[test]
fn empty_index_checkpoint_recovery() {
    let log2: u32 = 4; // 16 buckets
    let (base, token, inserted) = write_index_checkpoint(log2, 0, 0);
    assert_eq!(inserted, 0);

    let plan = make_index_plan(
        token,
        IndexRecoveryInfo {
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
    let recovered = engine.recover_index(&plan, base.path()).unwrap();

    assert_eq!(recovered.num_buckets(), 1u64 << log2);
    assert_eq!(recovered.entry_count(), 0);
    assert_eq!(recovered.scan_entry_count(), 0);
    assert_eq!(recovered.version(), 0);
}

// ═══════════════════════════════════════════════════════════════════
// 22. CheckpointInfo fields from discovery
// ═══════════════════════════════════════════════════════════════════

#[test]
fn checkpoint_info_populated_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let token = sample_token(220);
    let index_info = sample_index_info();
    let log_info = snapshot_log_info();
    let sessions = sample_sessions();

    write_checkpoint_metadata(dir.path(), &token, &index_info, &log_info, &sessions);

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let infos = mgr.discover_checkpoints().unwrap();

    assert_eq!(infos.len(), 1);
    let info = &infos[0];
    assert_eq!(info.token, token);
    assert_eq!(info.checkpoint_type, CheckpointType::Snapshot);
    assert_eq!(info.session_count, 2);
    assert_eq!(info.index_info, index_info);
    assert_eq!(info.log_info, log_info);
    assert!(!info.created_at.is_empty());
}

// ═══════════════════════════════════════════════════════════════════
// 23. Metadata store latest_checkpoint
// ═══════════════════════════════════════════════════════════════════

#[test]
fn metadata_store_latest_checkpoint_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    let latest = store.latest_checkpoint().unwrap();
    assert!(latest.is_none());
}

#[test]
fn metadata_store_latest_returns_most_recent() {
    let dir = tempfile::tempdir().unwrap();
    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());

    let t1 = sample_token(231);
    let t2 = sample_token(232);

    store
        .write_checkpoint_metadata(&t1, &sample_index_info(), &fold_over_log_info(), &[])
        .unwrap();

    store
        .write_checkpoint_metadata(&t2, &sample_index_info(), &fold_over_log_info(), &[])
        .unwrap();

    // Patch t2's descriptor to have a definitively later timestamp
    let desc_path = dir
        .path()
        .join("checkpoints")
        .join(t2.to_string())
        .join("checkpoint.json");
    let raw = fs::read_to_string(&desc_path).unwrap();
    let patched = raw.replace(
        &raw.lines()
            .find(|l| l.contains("created_at"))
            .unwrap()
            .to_string(),
        r#"  "created_at": "2099-12-31T23:59:59Z""#,
    );
    fs::write(&desc_path, patched).unwrap();

    let latest = store.latest_checkpoint().unwrap();
    assert!(latest.is_some());
    assert_eq!(latest.unwrap(), t2);
}
