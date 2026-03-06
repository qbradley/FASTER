//! Log recovery engine for the FASTER hybrid log (fold-over mode).
//!
//! Restores the hybrid log's address boundaries from a fold-over checkpoint.
//! In fold-over mode, all data up to the checkpoint tail has been flushed to
//! the main log file on disk. Recovery reads the persisted
//! [`LogRecoveryInfo`] metadata, validates the on-disk log file, and produces
//! a [`LogRecoveryResult`] describing the restored state.
//!
//! # Fold-over recovery protocol
//!
//! 1. Load [`LogRecoveryInfo`] from the [`RecoveryPlan`].
//! 2. Validate the log file exists and its size is consistent with the
//!    checkpoint tail address.
//! 3. Compute restored address boundaries:
//!    - `begin_address` = checkpoint begin
//!    - `head_address`  = checkpoint head (or begin if loading everything)
//!    - `tail_address`  = checkpoint tail (`final_address`)
//!    - `read_only_address` = tail (everything is read-only after recovery)
//!    - `flushed_until` = tail (everything up to tail is on disk)
//! 4. Optionally scan records to verify chain integrity.
//!
//! [`LogRecoveryInfo`]: crate::checkpoint::LogRecoveryInfo

use std::fmt;
use std::fs;
use std::path::Path;

use crate::address::{LogicalAddress, OFFSET_BITS};
use crate::checkpoint::{CheckpointType, LogRecoveryInfo};

use super::{RecoveryError, RecoveryPlan};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Page size in bytes (2^OFFSET_BITS, default 32 MiB).
const PAGE_SIZE: u64 = 1u64 << OFFSET_BITS;

// ---------------------------------------------------------------------------
// LogRecoveryResult
// ---------------------------------------------------------------------------

/// Result of a successful log recovery operation.
///
/// Contains the restored address boundaries and summary statistics about the
/// recovery process (records scanned, pages loaded). Consumers use this to
/// reconfigure the [`HybridLogAllocator`] to match the checkpointed state.
///
/// [`HybridLogAllocator`]: crate::hybrid_log::HybridLogAllocator
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecoveryResult {
    /// Earliest valid address in the recovered log.
    pub begin_address: LogicalAddress,
    /// Start of the in-memory region after recovery.
    pub head_address: LogicalAddress,
    /// Tail (exclusive upper-bound) of the recovered log.
    pub tail_address: LogicalAddress,
    /// Read-only boundary after recovery — equal to `tail_address` for
    /// fold-over because all recovered data is immutable.
    pub read_only_address: LogicalAddress,
    /// How far the log has been flushed to disk. For fold-over this equals
    /// `tail_address` since the checkpoint guarantees all pages are on disk.
    pub flushed_until: LogicalAddress,
    /// Number of records scanned during integrity verification.
    pub records_scanned: u64,
    /// Number of pages that should be loaded into memory.
    pub pages_loaded: u32,
}

impl fmt::Display for LogRecoveryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "LogRecoveryResult {{ begin={:?}, head={:?}, tail={:?}, \
             read_only={:?}, flushed_until={:?}, records_scanned={}, \
             pages_loaded={} }}",
            self.begin_address,
            self.head_address,
            self.tail_address,
            self.read_only_address,
            self.flushed_until,
            self.records_scanned,
            self.pages_loaded,
        )
    }
}

// ---------------------------------------------------------------------------
// LogRecoveryEngine
// ---------------------------------------------------------------------------

/// Recovers the hybrid log state from a fold-over checkpoint.
///
/// The engine is stateless — all per-recovery state lives in the
/// [`RecoveryPlan`] and the returned [`LogRecoveryResult`].
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use faster_core::recovery::RecoveryManager;
/// use faster_core::recovery::log_recovery::LogRecoveryEngine;
///
/// let mgr = RecoveryManager::new(Path::new("/data/faster").to_path_buf());
/// let plan = mgr.select_checkpoint(None).unwrap();
///
/// let engine = LogRecoveryEngine::new();
/// let result = engine
///     .recover_fold_over(&plan, Path::new("/data/faster"))
///     .unwrap();
/// println!("Recovered log: {result}");
/// ```
pub struct LogRecoveryEngine {
    _private: (),
}

impl LogRecoveryEngine {
    /// Creates a new `LogRecoveryEngine`.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Recover the hybrid log from a fold-over checkpoint.
    ///
    /// Validates the on-disk log file against the metadata in `plan` and
    /// returns a [`LogRecoveryResult`] describing the restored address
    /// boundaries.
    ///
    /// # Errors
    ///
    /// - [`RecoveryError::CorruptMetadata`] if the plan's log info is not a
    ///   fold-over checkpoint.
    /// - [`RecoveryError::ValidationFailed`] if the log file is missing,
    ///   truncated, or has an inconsistent size.
    /// - [`RecoveryError::IoError`] on underlying I/O failures.
    pub fn recover_fold_over(
        &self,
        plan: &RecoveryPlan,
        base_dir: &Path,
    ) -> Result<LogRecoveryResult, RecoveryError> {
        let log_info = &plan.log_info;

        // Step 1: Validate this is a fold-over checkpoint.
        validate_fold_over_type(log_info)?;

        // Step 2: Validate address consistency in the metadata.
        validate_address_consistency(log_info)?;

        // Step 3: Validate the on-disk log file.
        let records_scanned = validate_log_file(base_dir, log_info)?;

        // Step 4: Compute restored address boundaries.
        let begin_address = log_info.begin_address;
        let tail_address = log_info.final_address;
        let head_address = log_info.head_address;

        // In fold-over, after recovery everything up to tail is read-only.
        let read_only_address = tail_address;
        // Everything up to tail is flushed (the whole point of fold-over).
        let flushed_until = tail_address;

        // Compute pages to load: pages between head and tail.
        let pages_loaded = pages_between_addresses(head_address, tail_address);

        Ok(LogRecoveryResult {
            begin_address,
            head_address,
            tail_address,
            read_only_address,
            flushed_until,
            records_scanned,
            pages_loaded,
        })
    }
}

impl Default for LogRecoveryEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal validation helpers
// ---------------------------------------------------------------------------

/// Verify the recovery info describes a fold-over checkpoint.
fn validate_fold_over_type(log_info: &LogRecoveryInfo) -> Result<(), RecoveryError> {
    if log_info.checkpoint_type != CheckpointType::FoldOver {
        return Err(RecoveryError::CorruptMetadata(format!(
            "expected FoldOver checkpoint, got {:?}",
            log_info.checkpoint_type
        )));
    }
    Ok(())
}

/// Verify the address boundaries in the recovery metadata are consistent.
///
/// The invariant is: `begin <= head <= final` and `flushed_until <= final`.
fn validate_address_consistency(log_info: &LogRecoveryInfo) -> Result<(), RecoveryError> {
    let mut issues = Vec::new();

    if log_info.begin_address > log_info.head_address {
        issues.push(format!(
            "begin_address ({:?}) > head_address ({:?})",
            log_info.begin_address, log_info.head_address
        ));
    }

    if log_info.head_address > log_info.final_address {
        issues.push(format!(
            "head_address ({:?}) > final_address ({:?})",
            log_info.head_address, log_info.final_address
        ));
    }

    if log_info.flushed_until_address > log_info.final_address {
        issues.push(format!(
            "flushed_until_address ({:?}) > final_address ({:?})",
            log_info.flushed_until_address, log_info.final_address
        ));
    }

    if !issues.is_empty() {
        return Err(RecoveryError::ValidationFailed(issues));
    }
    Ok(())
}

/// Validate the on-disk log file against the recovery metadata.
///
/// For fold-over checkpoints the main log file (`log.0`) must exist and its
/// size must be at least as large as the byte offset implied by the tail
/// address. The device uses a segmented file layout (`log.0`, `log.1`, …)
/// but for small logs segment 0 is sufficient.
///
/// Returns the number of records "scanned" (currently 0 — full record-chain
/// scanning is reserved for a future enhancement).
fn validate_log_file(
    base_dir: &Path,
    log_info: &LogRecoveryInfo,
) -> Result<u64, RecoveryError> {
    let tail = log_info.final_address;

    // An empty log (tail at zero) is trivially valid — no file required.
    if tail == LogicalAddress::ZERO {
        return Ok(0);
    }

    // Compute the minimum expected file size. The log file stores pages
    // sequentially starting at byte offset 0 for page 0.
    let expected_min_size = logical_address_to_byte_offset(tail);

    // The device writes segments named `log.{n}`. Determine which segments
    // we expect and check them.
    let segment_size = default_segment_size();
    let num_segments = if expected_min_size == 0 {
        0u64
    } else {
        expected_min_size.div_ceil(segment_size)
    };

    if num_segments == 0 {
        // Nothing to verify — the log is empty.
        return Ok(0);
    }

    let mut total_size: u64 = 0;
    for seg_idx in 0..num_segments {
        let seg_path = base_dir.join(format!("log.{seg_idx}"));
        if !seg_path.exists() {
            return Err(RecoveryError::ValidationFailed(vec![format!(
                "log segment file missing: {}",
                seg_path.display()
            )]));
        }
        let meta = fs::metadata(&seg_path)?;
        total_size += meta.len();
    }

    if total_size < expected_min_size {
        return Err(RecoveryError::ValidationFailed(vec![format!(
            "log file too small: expected at least {expected_min_size} bytes \
             for tail address {:?}, but total segment size is {total_size} bytes",
            tail
        )]));
    }

    Ok(0)
}

/// Convert a [`LogicalAddress`] to a byte offset in the log file.
///
/// `byte_offset = page * PAGE_SIZE + offset`
fn logical_address_to_byte_offset(addr: LogicalAddress) -> u64 {
    (addr.page().0 as u64) * PAGE_SIZE + (addr.offset().0 as u64)
}

/// Default segment size used by [`SyncFileDevice`].
///
/// This must match the device configuration. The default is 1 GiB.
///
/// [`SyncFileDevice`]: crate::SyncFileDevice
fn default_segment_size() -> u64 {
    1u64 << 30 // 1 GiB
}

/// Compute the number of pages between two addresses.
///
/// Returns 0 when `to <= from`.
fn pages_between_addresses(from: LogicalAddress, to: LogicalAddress) -> u32 {
    if to <= from {
        return 0;
    }
    let from_page = from.page().0;
    let to_page = to.page().0;
    if from_page == to_page {
        if to.offset().0 > from.offset().0 { 1 } else { 0 }
    } else {
        let base = to_page - from_page;
        if to.offset().0 > 0 { base + 1 } else { base }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};
    use crate::checkpoint::{
        CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo,
        LogRecoveryInfo,
    };
    use std::fs;

    // -- Test helpers -------------------------------------------------------

    fn sample_token() -> CheckpointToken {
        CheckpointToken::new(0xDEAD_BEEF)
    }

    fn sample_index_info() -> IndexRecoveryInfo {
        IndexRecoveryInfo {
            version: 1,
            table_size: 1 << 16,
            num_ht_bytes: (1 << 16) * 64,
            num_ofb_bytes: 4096,
            num_buckets: (1 << 16) + 64,
            start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
            final_logical_address: LogicalAddress::new(Page(2), Offset(512)),
        }
    }

    fn fold_over_log_info(
        begin: LogicalAddress,
        head: LogicalAddress,
        tail: LogicalAddress,
    ) -> LogRecoveryInfo {
        LogRecoveryInfo {
            version: 1,
            checkpoint_type: CheckpointType::FoldOver,
            begin_address: begin,
            flushed_until_address: tail,
            final_address: tail,
            head_address: head,
            snapshot_start_address: LogicalAddress::ZERO,
            snapshot_final_address: LogicalAddress::ZERO,
            use_snapshot_file: false,
            object_log_segment_count: 0,
        }
    }

    /// Write checkpoint metadata to disk so the recovery plan can be loaded.
    fn write_checkpoint(
        base_dir: &Path,
        token: &CheckpointToken,
        log_info: &LogRecoveryInfo,
    ) {
        let store = CheckpointMetadataStore::new(base_dir.to_path_buf());
        store
            .write_checkpoint_metadata(token, &sample_index_info(), log_info, &[])
            .expect("write checkpoint metadata");
    }

    /// Create a fake log segment file of the given size.
    fn create_log_file(base_dir: &Path, segment: u32, size: u64) {
        let path = base_dir.join(format!("log.{segment}"));
        let data = vec![0u8; size as usize];
        fs::write(path, data).expect("create log segment file");
    }

    /// Build a `RecoveryPlan` from log info (delegates to RecoveryManager).
    fn build_plan(
        base_dir: &Path,
        token: &CheckpointToken,
        log_info: &LogRecoveryInfo,
    ) -> RecoveryPlan {
        use crate::recovery::RecoveryManager;
        write_checkpoint(base_dir, token, log_info);
        let mgr = RecoveryManager::new(base_dir.to_path_buf());
        mgr.select_checkpoint(Some(*token))
            .expect("build recovery plan")
    }

    // -- Tests: basic fold-over recovery ------------------------------------

    #[test]
    fn recover_fold_over_basic() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Log spanning pages 0..2, tail at page 2 offset 512.
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(0), Offset(0));
        let tail = LogicalAddress::new(Page(2), Offset(512));

        let log_info = fold_over_log_info(begin, head, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Create a log file large enough.
        let needed = logical_address_to_byte_offset(tail);
        create_log_file(dir.path(), 0, needed);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_fold_over(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        assert_eq!(result.head_address, head);
        assert_eq!(result.tail_address, tail);
        assert_eq!(result.read_only_address, tail);
        assert_eq!(result.flushed_until, tail);
        // Pages: 0, 1, 2 (3 pages loaded from head to tail)
        assert_eq!(result.pages_loaded, 3);
    }

    #[test]
    fn recover_fold_over_restores_correct_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(1), Offset(64));
        let head = LogicalAddress::new(Page(3), Offset(0));
        let tail = LogicalAddress::new(Page(5), Offset(1024));

        let log_info = fold_over_log_info(begin, head, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        let needed = logical_address_to_byte_offset(tail);
        create_log_file(dir.path(), 0, needed);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_fold_over(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        assert_eq!(result.head_address, head);
        assert_eq!(result.tail_address, tail);
        assert_eq!(result.read_only_address, tail);
        assert_eq!(result.flushed_until, tail);
        // Pages 3, 4, 5 → 3 pages
        assert_eq!(result.pages_loaded, 3);
    }

    // -- Tests: empty log recovery ------------------------------------------

    #[test]
    fn recover_fold_over_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let zero = LogicalAddress::ZERO;
        let log_info = fold_over_log_info(zero, zero, zero);
        let plan = build_plan(dir.path(), &token, &log_info);

        // No log file needed for an empty log.
        let engine = LogRecoveryEngine::new();
        let result = engine.recover_fold_over(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, zero);
        assert_eq!(result.head_address, zero);
        assert_eq!(result.tail_address, zero);
        assert_eq!(result.read_only_address, zero);
        assert_eq!(result.flushed_until, zero);
        assert_eq!(result.records_scanned, 0);
        assert_eq!(result.pages_loaded, 0);
    }

    // -- Tests: log file validation -----------------------------------------

    #[test]
    fn recover_fold_over_missing_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(0), Offset(0));
        let tail = LogicalAddress::new(Page(1), Offset(256));

        let log_info = fold_over_log_info(begin, begin, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Don't create the log file.
        let engine = LogRecoveryEngine::new();
        let err = engine
            .recover_fold_over(&plan, dir.path())
            .unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert_eq!(issues.len(), 1);
                assert!(
                    issues[0].contains("missing"),
                    "expected 'missing' in error: {}",
                    issues[0]
                );
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    #[test]
    fn recover_fold_over_truncated_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(0), Offset(0));
        let tail = LogicalAddress::new(Page(2), Offset(512));

        let log_info = fold_over_log_info(begin, begin, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Create a log file that is too small.
        let needed = logical_address_to_byte_offset(tail);
        create_log_file(dir.path(), 0, needed / 2);

        let engine = LogRecoveryEngine::new();
        let err = engine
            .recover_fold_over(&plan, dir.path())
            .unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert_eq!(issues.len(), 1);
                assert!(
                    issues[0].contains("too small"),
                    "expected 'too small' in error: {}",
                    issues[0]
                );
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    // -- Tests: checkpoint type validation ----------------------------------

    #[test]
    fn recover_fold_over_rejects_snapshot_type() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let mut log_info =
            fold_over_log_info(LogicalAddress::ZERO, LogicalAddress::ZERO, LogicalAddress::ZERO);
        log_info.checkpoint_type = CheckpointType::Snapshot;

        // Write checkpoint metadata manually with FoldOver type in the descriptor
        // but Snapshot in the log_info (simulating corruption).
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &log_info, &[])
            .expect("write");

        // Build a plan — the plan faithfully reflects the metadata.
        let mgr = crate::recovery::RecoveryManager::new(dir.path().to_path_buf());
        let plan = mgr.select_checkpoint(Some(token)).expect("plan");

        let engine = LogRecoveryEngine::new();
        let err = engine
            .recover_fold_over(&plan, dir.path())
            .unwrap_err();

        match err {
            RecoveryError::CorruptMetadata(msg) => {
                assert!(
                    msg.contains("FoldOver"),
                    "expected 'FoldOver' in error: {msg}"
                );
            }
            other => panic!("expected CorruptMetadata, got: {other}"),
        }
    }

    // -- Tests: address consistency validation -------------------------------

    #[test]
    fn recover_fold_over_inconsistent_begin_head() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // begin > head — invalid
        let begin = LogicalAddress::new(Page(5), Offset(0));
        let head = LogicalAddress::new(Page(2), Offset(0));
        let tail = LogicalAddress::new(Page(10), Offset(0));

        let log_info = fold_over_log_info(begin, head, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        let engine = LogRecoveryEngine::new();
        let err = engine
            .recover_fold_over(&plan, dir.path())
            .unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
                assert!(issues[0].contains("begin_address"));
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    #[test]
    fn recover_fold_over_inconsistent_head_tail() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // head > tail — invalid
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(10), Offset(0));
        let tail = LogicalAddress::new(Page(5), Offset(0));

        let log_info = fold_over_log_info(begin, head, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        let engine = LogRecoveryEngine::new();
        let err = engine
            .recover_fold_over(&plan, dir.path())
            .unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
                assert!(issues[0].contains("head_address"));
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    // -- Tests: multi-page recovery -----------------------------------------

    #[test]
    fn recover_fold_over_multiple_pages() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Log spanning 10 full pages.
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(0), Offset(0));
        let tail = LogicalAddress::new(Page(10), Offset(0));

        let log_info = fold_over_log_info(begin, head, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        let needed = logical_address_to_byte_offset(tail);
        create_log_file(dir.path(), 0, needed);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_fold_over(&plan, dir.path()).unwrap();

        assert_eq!(result.tail_address, tail);
        // Pages 0..9 → 10 pages
        assert_eq!(result.pages_loaded, 10);
    }

    #[test]
    fn recover_fold_over_head_not_at_begin() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Head is ahead of begin (some pages have been evicted before checkpoint).
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(5), Offset(0));
        let tail = LogicalAddress::new(Page(10), Offset(0));

        let log_info = fold_over_log_info(begin, head, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        let needed = logical_address_to_byte_offset(tail);
        create_log_file(dir.path(), 0, needed);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_fold_over(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        assert_eq!(result.head_address, head);
        // Pages 5..9 → 5 pages loaded
        assert_eq!(result.pages_loaded, 5);
    }

    // -- Tests: record count verification -----------------------------------

    #[test]
    fn recover_fold_over_records_scanned_is_zero_without_scan() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(0), Offset(0));
        let tail = LogicalAddress::new(Page(1), Offset(256));

        let log_info = fold_over_log_info(begin, begin, tail);
        let plan = build_plan(dir.path(), &token, &log_info);

        let needed = logical_address_to_byte_offset(tail);
        create_log_file(dir.path(), 0, needed);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_fold_over(&plan, dir.path()).unwrap();

        // Record scanning is not yet implemented, so count should be 0.
        assert_eq!(result.records_scanned, 0);
    }

    // -- Tests: Display implementation --------------------------------------

    #[test]
    fn log_recovery_result_display() {
        let result = LogRecoveryResult {
            begin_address: LogicalAddress::ZERO,
            head_address: LogicalAddress::ZERO,
            tail_address: LogicalAddress::new(Page(1), Offset(256)),
            read_only_address: LogicalAddress::new(Page(1), Offset(256)),
            flushed_until: LogicalAddress::new(Page(1), Offset(256)),
            records_scanned: 42,
            pages_loaded: 2,
        };
        let s = format!("{result}");
        assert!(s.contains("records_scanned=42"));
        assert!(s.contains("pages_loaded=2"));
    }

    // -- Tests: Default trait -----------------------------------------------

    #[test]
    fn log_recovery_engine_default() {
        let engine = LogRecoveryEngine::default();
        // Just ensure it constructs without panic.
        let _ = engine;
    }

    // -- Tests: internal helpers ---------------------------------------------

    #[test]
    fn logical_address_to_byte_offset_zero() {
        assert_eq!(logical_address_to_byte_offset(LogicalAddress::ZERO), 0);
    }

    #[test]
    fn logical_address_to_byte_offset_page_boundary() {
        let addr = LogicalAddress::new(Page(3), Offset(0));
        assert_eq!(logical_address_to_byte_offset(addr), 3 * PAGE_SIZE);
    }

    #[test]
    fn logical_address_to_byte_offset_with_offset() {
        let addr = LogicalAddress::new(Page(2), Offset(512));
        assert_eq!(
            logical_address_to_byte_offset(addr),
            2 * PAGE_SIZE + 512
        );
    }

    #[test]
    fn pages_between_addresses_same() {
        let a = LogicalAddress::new(Page(5), Offset(100));
        assert_eq!(pages_between_addresses(a, a), 0);
    }

    #[test]
    fn pages_between_addresses_same_page() {
        let a = LogicalAddress::new(Page(5), Offset(0));
        let b = LogicalAddress::new(Page(5), Offset(100));
        assert_eq!(pages_between_addresses(a, b), 1);
    }

    #[test]
    fn pages_between_addresses_multi_page() {
        let a = LogicalAddress::new(Page(3), Offset(0));
        let b = LogicalAddress::new(Page(7), Offset(200));
        // Pages 3, 4, 5, 6, 7 → 5
        assert_eq!(pages_between_addresses(a, b), 5);
    }

    #[test]
    fn pages_between_addresses_to_page_boundary() {
        let a = LogicalAddress::new(Page(3), Offset(0));
        let b = LogicalAddress::new(Page(7), Offset(0));
        // Pages 3, 4, 5, 6 → 4 (to is at page boundary, no partial page 7)
        assert_eq!(pages_between_addresses(a, b), 4);
    }

    #[test]
    fn pages_between_addresses_reverse() {
        let a = LogicalAddress::new(Page(7), Offset(0));
        let b = LogicalAddress::new(Page(3), Offset(0));
        assert_eq!(pages_between_addresses(a, b), 0);
    }
}
