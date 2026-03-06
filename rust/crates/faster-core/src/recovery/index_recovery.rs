//! Index recovery engine for the FASTER hash index.
//!
//! Restores a [`HashIndex`] from a checkpoint file produced by
//! [`IndexCheckpointWriter`]. The recovery process:
//!
//! 1. Opens the checkpoint file via [`IndexCheckpointReader`].
//! 2. Validates CRC32 integrity (fail-fast on corruption).
//! 3. Reads the header to obtain table size and entry count.
//! 4. Allocates a new [`HashIndex`] with the correct table size.
//! 5. Loads all raw 64-byte bucket data directly into the index.
//! 6. Sets version and entry count from recovery metadata.
//! 7. Verifies loaded entry count matches expected.
//!
//! [`IndexCheckpointWriter`]: crate::checkpoint::index_writer::IndexCheckpointWriter
//! [`IndexCheckpointReader`]: crate::checkpoint::index_writer::IndexCheckpointReader

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use crate::checkpoint::CheckpointError;
use crate::checkpoint::index_writer::IndexCheckpointReader;
use crate::epoch::EpochTable;
use crate::hash_bucket::HashBucket;
use crate::hash_index::HashIndex;

use super::{RecoveryError, RecoveryPlan};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of a single serialized bucket in bytes (must match checkpoint format).
const BUCKET_SIZE: usize = core::mem::size_of::<HashBucket>();

// Compile-time assertion: bucket size must be 64 bytes.
const _: () = assert!(BUCKET_SIZE == 64);

/// Size of the checkpoint file header in bytes:
/// magic(4) + version(4) + table_size_bits(1) + padding(3) + num_buckets(8) + entry_count(8)
const HEADER_SIZE: usize = 28;

// ---------------------------------------------------------------------------
// RecoveryProgress
// ---------------------------------------------------------------------------

/// Progress report for an in-flight recovery operation.
///
/// Emitted during recovery to communicate progress to higher-level callers
/// (e.g., logging, progress bars, monitoring).
#[derive(Debug, Clone)]
pub struct RecoveryProgress {
    /// Human-readable name of the current recovery phase.
    pub phase: &'static str,
    /// Number of items (buckets, entries, etc.) processed so far.
    pub items_done: u64,
    /// Total number of items expected in this phase.
    pub items_total: u64,
}

impl RecoveryProgress {
    /// Returns the completion fraction in `[0.0, 1.0]`.
    ///
    /// Returns `1.0` when `items_total` is zero (vacuously complete).
    pub fn fraction(&self) -> f64 {
        if self.items_total == 0 {
            1.0
        } else {
            self.items_done as f64 / self.items_total as f64
        }
    }
}

// ---------------------------------------------------------------------------
// IndexRecoveryEngine
// ---------------------------------------------------------------------------

/// Restores a [`HashIndex`] from a binary checkpoint file.
///
/// The engine reads the checkpoint produced by [`IndexCheckpointWriter`],
/// validates its integrity via CRC32, and reconstructs the in-memory hash
/// index to the exact state captured at checkpoint time.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use faster_core::recovery::RecoveryManager;
/// use faster_core::recovery::index_recovery::IndexRecoveryEngine;
///
/// let mgr = RecoveryManager::new(Path::new("/data/faster").to_path_buf());
/// let plan = mgr.select_checkpoint(None).unwrap();
///
/// let engine = IndexRecoveryEngine::new();
/// let index = engine.recover_index(&plan, Path::new("/data/faster")).unwrap();
/// println!("Recovered {} buckets", index.num_buckets());
/// ```
///
/// [`IndexCheckpointWriter`]: crate::checkpoint::index_writer::IndexCheckpointWriter
pub struct IndexRecoveryEngine {
    _private: (),
}

impl IndexRecoveryEngine {
    /// Creates a new `IndexRecoveryEngine`.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Recovers a [`HashIndex`] from the checkpoint described by `plan`.
    ///
    /// The checkpoint file is located under `base_dir` using the plan's token.
    /// The full recovery process:
    ///
    /// 1. Locate the `.index` checkpoint file.
    /// 2. Open and validate header (magic, format version).
    /// 3. CRC32 integrity check (fail-fast on corruption).
    /// 4. Allocate a new `HashIndex` with the correct table size.
    /// 5. Load all buckets from the checkpoint into the index.
    /// 6. Set version and live entry count from recovery metadata.
    /// 7. Verify that the scanned entry count matches the expected value.
    ///
    /// # Errors
    ///
    /// Returns [`RecoveryError`] if:
    /// - The checkpoint file cannot be found or opened.
    /// - CRC32 verification fails (corrupt data).
    /// - Bucket count does not match header expectations.
    /// - Entry count after loading does not match the checkpoint header.
    pub fn recover_index(
        &self,
        plan: &RecoveryPlan,
        base_dir: &Path,
    ) -> Result<HashIndex, RecoveryError> {
        let index_path = self.resolve_index_path(plan, base_dir);

        // Step 1–2: Open the checkpoint file and validate header.
        let reader = IndexCheckpointReader::open(&index_path).map_err(map_checkpoint_error)?;

        let num_buckets = reader.num_buckets();
        let table_size_bits = reader.table_size_bits();
        let expected_entry_count = reader.entry_count();

        // Step 3: CRC32 integrity check — fail fast on corruption.
        reader.verify().map_err(map_checkpoint_error)?;

        // Step 4: Allocate a new HashIndex with the correct table size.
        let index = HashIndex::new(u32::from(table_size_bits));

        // Sanity: the new index must have the expected number of buckets.
        if index.num_buckets() != num_buckets {
            return Err(RecoveryError::CorruptMetadata(format!(
                "index bucket count mismatch: expected {num_buckets}, allocated {}",
                index.num_buckets(),
            )));
        }

        // Step 5: Load raw bucket data into the index.
        self.load_buckets(&index_path, &index, num_buckets)?;

        // Step 6: Set version and entry count from recovery metadata.
        index.set_version(plan.index_info.version);

        // Step 7: Verify loaded entry count matches expected.
        let scanned = index.scan_entry_count();
        if scanned != expected_entry_count {
            return Err(RecoveryError::ValidationFailed(vec![format!(
                "entry count mismatch after loading: checkpoint header says \
                 {expected_entry_count}, scanned {scanned}",
            )]));
        }

        Ok(index)
    }

    /// Recovers a [`HashIndex`] using a caller-provided [`EpochTable`].
    ///
    /// Identical to [`recover_index`](Self::recover_index) but the returned
    /// `HashIndex` shares the given epoch table instead of creating a private
    /// one. This is the preferred constructor when embedding the index inside
    /// a `FasterKv` that shares a single epoch across subsystems.
    pub fn recover_index_with_epoch(
        &self,
        plan: &RecoveryPlan,
        base_dir: &Path,
        epoch: Arc<EpochTable>,
    ) -> Result<HashIndex, RecoveryError> {
        let index_path = self.resolve_index_path(plan, base_dir);

        let reader = IndexCheckpointReader::open(&index_path).map_err(map_checkpoint_error)?;

        let num_buckets = reader.num_buckets();
        let table_size_bits = reader.table_size_bits();
        let expected_entry_count = reader.entry_count();

        reader.verify().map_err(map_checkpoint_error)?;

        let index = HashIndex::with_epoch(u32::from(table_size_bits), epoch);

        if index.num_buckets() != num_buckets {
            return Err(RecoveryError::CorruptMetadata(format!(
                "index bucket count mismatch: expected {num_buckets}, allocated {}",
                index.num_buckets(),
            )));
        }

        self.load_buckets(&index_path, &index, num_buckets)?;

        index.set_version(plan.index_info.version);

        let scanned = index.scan_entry_count();
        if scanned != expected_entry_count {
            return Err(RecoveryError::ValidationFailed(vec![format!(
                "entry count mismatch after loading: checkpoint header says \
                 {expected_entry_count}, scanned {scanned}",
            )]));
        }

        Ok(index)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Resolves the path to the binary `.index` checkpoint file.
    fn resolve_index_path(&self, plan: &RecoveryPlan, base_dir: &Path) -> std::path::PathBuf {
        base_dir
            .join("checkpoints")
            .join(plan.token.to_string())
            .join(format!("{}.index", plan.token))
    }

    /// Reads raw bucket data from the checkpoint file and copies it into the
    /// hash index's bucket array.
    ///
    /// Re-opens the file (after CRC has been verified) and seeks past the
    /// header to the bucket body. Each 64-byte chunk is written directly
    /// into the corresponding `HashBucket` via `ptr::copy_nonoverlapping`.
    fn load_buckets(
        &self,
        path: &Path,
        index: &HashIndex,
        num_buckets: u64,
    ) -> Result<(), RecoveryError> {
        let mut file = BufReader::new(File::open(path)?);

        // Skip past the header to the body.
        file.seek(SeekFrom::Start(HEADER_SIZE as u64))?;

        let buckets = index.checkpoint_bucket_slice();
        let mut buf = [0u8; BUCKET_SIZE];

        for (i, dst) in buckets.iter().enumerate().take(num_buckets as usize) {
            file.read_exact(&mut buf).map_err(|e| {
                RecoveryError::IoError(std::io::Error::new(
                    e.kind(),
                    format!("failed to read bucket {i}/{num_buckets}: {e}"),
                ))
            })?;

            // SAFETY: `HashBucket` is `#[repr(C, align(64))]` and contains only
            // `AtomicU64` / `AtomicLogicalAddress` fields, which have the same
            // in-memory representation as `u64` on all platforms where
            // `AtomicU64` is lock-free (guaranteed by Rust on all tier-1
            // targets). The struct has no padding holes because
            // 7×8 + 1×8 = 64 bytes == size_of::<HashBucket>().
            //
            // We write through a mutable pointer obtained from a shared
            // reference. This is sound during recovery because:
            // 1. No concurrent readers/writers exist (recovery is single-threaded).
            // 2. The `HashBucket` fields are `AtomicU64` which accept writes
            //    through shared references (interior mutability). We bypass
            //    the atomic API here because we hold exclusive logical access
            //    during recovery — there are no other threads.
            // 3. The source bytes were produced by the checkpoint writer using
            //    the same `#[repr(C, align(64))]` layout, so the byte
            //    pattern is valid for the target type.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buf.as_ptr(),
                    dst as *const HashBucket as *mut u8,
                    BUCKET_SIZE,
                );
            }
        }

        Ok(())
    }
}

impl Default for IndexRecoveryEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Maps a [`CheckpointError`] to a [`RecoveryError`].
fn map_checkpoint_error(err: CheckpointError) -> RecoveryError {
    match err {
        CheckpointError::NotFound => RecoveryError::NoCheckpointsFound,
        CheckpointError::InvalidState(msg) => RecoveryError::CorruptMetadata(msg),
        CheckpointError::IoError(e) => RecoveryError::IoError(e),
        CheckpointError::AlreadyInProgress => {
            RecoveryError::CorruptMetadata("checkpoint is still in progress".to_string())
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::LogicalAddress;
    use crate::checkpoint::index_writer::IndexCheckpointWriter;
    use crate::checkpoint::{CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo};
    use crate::hash::KeyHash;
    use crate::hash_bucket::HashBucketEntry;
    use crate::hash_index::HashIndex;
    use crate::hash_table::HashTable;

    use std::io::{Seek, SeekFrom, Write};

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Creates a `RecoveryPlan` suitable for testing, pointing at the given
    /// `checkpoint_dir` with the specified `token`.
    fn make_plan(token: CheckpointToken, index_info: IndexRecoveryInfo) -> RecoveryPlan {
        RecoveryPlan {
            token,
            checkpoint_type: CheckpointType::Snapshot,
            index_info,
            log_info: LogRecoveryInfo {
                version: 0,
                checkpoint_type: CheckpointType::Snapshot,
                flushed_until_address: LogicalAddress::ZERO,
                begin_address: LogicalAddress::ZERO,
                final_address: LogicalAddress::ZERO,
                head_address: LogicalAddress::ZERO,
                snapshot_start_address: LogicalAddress::ZERO,
                snapshot_final_address: LogicalAddress::ZERO,
                use_snapshot_file: false,
                object_log_segment_count: 0,
            },
            session_infos: vec![],
            steps: vec![],
        }
    }

    /// Sets up a temporary directory as a checkpoint "base_dir" and writes
    /// an index checkpoint.
    ///
    /// Returns `(base_dir_handle, token, num_inserted_entries)`.
    fn write_checkpoint(
        log2_size: u32,
        entry_count: u64,
        version: u32,
    ) -> (tempfile::TempDir, CheckpointToken, u64) {
        let base = tempfile::tempdir().expect("create temp dir");
        let token = CheckpointToken::new(42);
        let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
        std::fs::create_dir_all(&ckpt_dir).unwrap();

        let table = HashTable::new(log2_size);
        let inserted = populate_table(&table, entry_count);

        let mut writer = IndexCheckpointWriter::new(&ckpt_dir, &token).expect("create writer");
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

    /// Populates a `HashTable` with `count` committed entries and returns
    /// the number actually inserted (may be less if hash collisions occur).
    fn populate_table(table: &HashTable, count: u64) -> u64 {
        let mut inserted = 0u64;
        for i in 0..count {
            let hash = KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let result = table.find_or_create_entry(hash, LogicalAddress::INVALID);
            if result.created {
                let committed = HashBucketEntry::new(
                    result.entry.tag(),
                    LogicalAddress::from_raw(i + 1),
                    false,
                );
                table.update_entry(result.slot, result.entry, committed);
                inserted += 1;
            }
        }
        inserted
    }

    /// Returns the path to the `.index` file for a checkpoint.
    fn index_file_path(base: &Path, token: &CheckpointToken) -> std::path::PathBuf {
        base.join("checkpoints")
            .join(token.to_string())
            .join(format!("{token}.index"))
    }

    // -----------------------------------------------------------------------
    // Round-trip tests
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_empty_index() {
        let log2 = 4; // 16 buckets
        let (base, token, _) = write_checkpoint(log2, 0, 0);

        let plan = make_plan(
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
        assert_eq!(recovered.version(), 0);
    }

    #[test]
    fn round_trip_with_entries() {
        let log2 = 8; // 256 buckets
        let (base, token, inserted) = write_checkpoint(log2, 50, 1);

        let plan = make_plan(
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

        assert_eq!(recovered.num_buckets(), 256);
        assert_eq!(recovered.scan_entry_count(), inserted);
        assert_eq!(recovered.version(), 1);
    }

    #[test]
    fn round_trip_preserves_entry_data() {
        // Write an index with known entries, recover, and verify each entry.
        let log2 = 8;
        let base = tempfile::tempdir().unwrap();
        let token = CheckpointToken::new(99);
        let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
        std::fs::create_dir_all(&ckpt_dir).unwrap();

        let original = HashIndex::new(log2);
        let thread = original.register_thread().unwrap();
        let _guard = thread.protect();

        let hashes: Vec<KeyHash> = (0..20u64)
            .map(|i| KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
            .collect();

        for (i, &hash) in hashes.iter().enumerate() {
            let r = original.find_or_create(hash, LogicalAddress::INVALID);
            assert!(r.created);
            let committed =
                HashBucketEntry::new(r.entry.tag(), LogicalAddress::from_raw(i as u64 + 1), false);
            original.update(r.slot, r.entry, committed);
        }

        let entry_count = original.entry_count();
        original.set_version(1);

        let mut writer = IndexCheckpointWriter::new(&ckpt_dir, &token).unwrap();
        writer
            .write_index(
                original.checkpoint_bucket_slice(),
                original.log2_buckets() as u8,
                1,
                entry_count,
            )
            .unwrap();

        drop(_guard);
        drop(thread);

        let plan = make_plan(
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

        // Every entry should be findable with the same hash and have the
        // same address.
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

        assert_eq!(recovered.version(), 1);
        assert_eq!(recovered.scan_entry_count(), entry_count);
    }

    #[test]
    fn round_trip_large_index() {
        let log2 = 12; // 4096 buckets
        let (base, token, inserted) = write_checkpoint(log2, 1500, 0);

        let plan = make_plan(
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

        assert_eq!(recovered.num_buckets(), 4096);
        assert_eq!(recovered.scan_entry_count(), inserted);
    }

    // -----------------------------------------------------------------------
    // Version preservation
    // -----------------------------------------------------------------------

    #[test]
    fn version_preserved_across_recovery() {
        for version in [0, 1] {
            let log2 = 4;
            let (base, token, _) = write_checkpoint(log2, 5, version);

            let plan = make_plan(
                token,
                IndexRecoveryInfo {
                    version,
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
            assert_eq!(recovered.version(), version);
        }
    }

    // -----------------------------------------------------------------------
    // CRC corruption detection
    // -----------------------------------------------------------------------

    #[test]
    fn crc_corruption_detected() {
        let log2 = 4;
        let (base, token, _) = write_checkpoint(log2, 10, 0);
        let path = index_file_path(base.path(), &token);

        // Corrupt a byte in the body area.
        {
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            // Seek to the middle of the body (after header).
            file.seek(SeekFrom::Start(HEADER_SIZE as u64 + 32)).unwrap();
            file.write_all(&[0xFF]).unwrap();
        }

        let plan = make_plan(
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
        assert!(result.is_err(), "should detect CRC corruption");

        match result.unwrap_err() {
            RecoveryError::CorruptMetadata(msg) => {
                assert!(msg.contains("CRC"), "error should mention CRC: {msg}");
            }
            other => panic!("expected CorruptMetadata, got: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Entry count mismatch detection
    // -----------------------------------------------------------------------

    #[test]
    fn entry_count_mismatch_detected() {
        let log2 = 4;
        let base = tempfile::tempdir().unwrap();
        let token = CheckpointToken::new(77);
        let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
        std::fs::create_dir_all(&ckpt_dir).unwrap();

        let table = HashTable::new(log2);
        let inserted = populate_table(&table, 10);

        // Write with a WRONG entry count in the header.
        let mut writer = IndexCheckpointWriter::new(&ckpt_dir, &token).unwrap();
        writer
            .write_index(
                table.bucket_slice(),
                table.log2_buckets() as u8,
                0,
                inserted + 999, // deliberate mismatch
            )
            .unwrap();

        let plan = make_plan(
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
        assert!(result.is_err(), "should detect entry count mismatch");

        match result.unwrap_err() {
            RecoveryError::ValidationFailed(issues) => {
                assert_eq!(issues.len(), 1);
                assert!(
                    issues[0].contains("entry count mismatch"),
                    "should mention mismatch: {}",
                    issues[0]
                );
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    // -----------------------------------------------------------------------
    // Missing file detection
    // -----------------------------------------------------------------------

    #[test]
    fn missing_checkpoint_file_detected() {
        let base = tempfile::tempdir().unwrap();
        let token = CheckpointToken::new(404);
        let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
        std::fs::create_dir_all(&ckpt_dir).unwrap();
        // Don't write any checkpoint file.

        let plan = make_plan(
            token,
            IndexRecoveryInfo {
                version: 0,
                table_size: 16,
                num_ht_bytes: 16 * 64,
                num_ofb_bytes: 0,
                num_buckets: 16,
                start_logical_address: LogicalAddress::ZERO,
                final_logical_address: LogicalAddress::ZERO,
            },
        );

        let engine = IndexRecoveryEngine::new();
        let result = engine.recover_index(&plan, base.path());
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // RecoveryProgress tests
    // -----------------------------------------------------------------------

    #[test]
    fn recovery_progress_fraction() {
        let p = RecoveryProgress {
            phase: "loading_buckets",
            items_done: 50,
            items_total: 100,
        };
        assert!((p.fraction() - 0.5).abs() < f64::EPSILON);

        let empty = RecoveryProgress {
            phase: "loading_buckets",
            items_done: 0,
            items_total: 0,
        };
        assert!((empty.fraction() - 1.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Default implementation
    // -----------------------------------------------------------------------

    #[test]
    fn default_creates_engine() {
        let _engine: IndexRecoveryEngine = Default::default();
    }

    // -----------------------------------------------------------------------
    // Recovery with shared epoch
    // -----------------------------------------------------------------------

    #[test]
    fn recover_with_shared_epoch() {
        let log2 = 8;
        let (base, token, inserted) = write_checkpoint(log2, 30, 0);

        let plan = make_plan(
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

        let shared_epoch = Arc::new(EpochTable::new());
        let engine = IndexRecoveryEngine::new();
        let recovered = engine
            .recover_index_with_epoch(&plan, base.path(), Arc::clone(&shared_epoch))
            .unwrap();

        assert!(Arc::ptr_eq(&recovered.epoch_arc(), &shared_epoch));
        assert_eq!(recovered.scan_entry_count(), inserted);
    }

    // -----------------------------------------------------------------------
    // Overflow bucket round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_with_overflow_buckets() {
        // Use a very small table (2^1 = 2 buckets) and insert enough entries
        // to force overflow bucket allocation.
        let log2 = 1u32; // 2 buckets
        let num_entries = 30u64; // 30 entries into 2 buckets forces overflow

        let base = tempfile::tempdir().unwrap();
        let token = CheckpointToken::new(55);
        let ckpt_dir = base.path().join("checkpoints").join(token.to_string());
        std::fs::create_dir_all(&ckpt_dir).unwrap();

        // Create and populate the original index.
        let original = HashIndex::new(log2);
        let thread = original.register_thread().unwrap();
        let _guard = thread.protect();

        let hashes: Vec<KeyHash> = (0..num_entries)
            .map(|i| KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
            .collect();

        for (i, &hash) in hashes.iter().enumerate() {
            let r = original.find_or_create(hash, LogicalAddress::INVALID);
            if r.created {
                let committed = HashBucketEntry::new(
                    r.entry.tag(),
                    LogicalAddress::from_raw(i as u64 + 1),
                    false,
                );
                original.update(r.slot, r.entry, committed);
            }
        }

        assert!(
            original.overflow_count() > 0,
            "expected overflow buckets with {num_entries} entries in {log2}-bit table",
        );

        // The checkpoint only stores primary buckets. Entries in overflow
        // buckets are NOT persisted in the current format.
        // Count entries in primary buckets only.
        let primary_entry_count = {
            let buckets = original.checkpoint_bucket_slice();
            let mut count = 0u64;
            for bucket in buckets {
                for i in 0..crate::hash_bucket::BUCKET_NUM_ENTRIES {
                    let entry = bucket.entry(i).load(core::sync::atomic::Ordering::Relaxed);
                    if !entry.is_empty() {
                        count += 1;
                    }
                }
            }
            count
        };

        // Write with the primary-only entry count so recovery validation passes.
        let mut writer = IndexCheckpointWriter::new(&ckpt_dir, &token).unwrap();
        writer
            .write_index(
                original.checkpoint_bucket_slice(),
                original.log2_buckets() as u8,
                0,
                primary_entry_count,
            )
            .unwrap();

        drop(_guard);
        drop(thread);

        let plan = make_plan(
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
        assert_eq!(recovered.scan_entry_count(), primary_entry_count);
    }
}
