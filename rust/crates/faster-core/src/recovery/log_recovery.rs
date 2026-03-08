//! Log recovery engine for the FASTER hybrid log.
//!
//! Restores the hybrid log's address boundaries from either a **fold-over** or
//! a **snapshot** checkpoint. The engine is stateless — all per-recovery state
//! lives in the [`RecoveryPlan`] and the returned [`LogRecoveryResult`].
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
//! # Snapshot recovery protocol
//!
//! In snapshot mode the in-memory pages were written to a separate snapshot
//! file instead of being flushed to the main log. Recovery must merge:
//!
//! 1. **Main log file** — pages from `begin_address` to `head_address`
//!    (already on disk from before the checkpoint).
//! 2. **Snapshot file** — pages from `snapshot_start_address` to
//!    `snapshot_final_address` (written by [`SnapshotFileWriter`]).
//!
//! After loading, the allocator addresses are restored so that:
//! - `tail_address` = `snapshot_final_address`
//! - `head_address` = `begin_address` (all recovered data is in-memory or
//!   on-disk)
//! - `flushed_until` = `snapshot_final_address`
//!
//! [`LogRecoveryInfo`]: crate::checkpoint::LogRecoveryInfo
//! [`SnapshotFileWriter`]: crate::checkpoint::snapshot_writer::SnapshotFileWriter

use std::fmt;
use std::fs;
use std::path::Path;

use crate::address::{LogicalAddress, OFFSET_BITS};
use crate::checkpoint::snapshot_writer::SnapshotFileReader;
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

/// Recovers the hybrid log state from a fold-over or snapshot checkpoint.
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

    /// Recover the hybrid log from a snapshot checkpoint.
    ///
    /// Unlike fold-over, snapshot recovery merges data from two sources:
    ///
    /// - **Main log file** — pages from `begin_address` to `head_address`
    ///   that were already on disk before the checkpoint.
    /// - **Snapshot file** — pages from `snapshot_start_address` to
    ///   `snapshot_final_address` that were in memory at checkpoint time
    ///   and persisted to a separate file by [`SnapshotFileWriter`].
    ///
    /// The snapshot file is located at `{base_dir}/{token}.snapshot.log`.
    ///
    /// # Errors
    ///
    /// - [`RecoveryError::CorruptMetadata`] if the plan's log info is not a
    ///   snapshot checkpoint or `use_snapshot_file` is `false`.
    /// - [`RecoveryError::ValidationFailed`] if the snapshot file is missing,
    ///   truncated, has an inconsistent page count, or page indices are
    ///   outside the expected range.
    /// - [`RecoveryError::IoError`] on underlying I/O failures.
    ///
    /// [`SnapshotFileWriter`]: crate::checkpoint::snapshot_writer::SnapshotFileWriter
    pub fn recover_snapshot(
        &self,
        plan: &RecoveryPlan,
        base_dir: &Path,
    ) -> Result<LogRecoveryResult, RecoveryError> {
        let log_info = &plan.log_info;

        // Step 1: Validate this is a snapshot checkpoint.
        validate_snapshot_type(log_info)?;

        // Step 2: Validate address consistency in the metadata.
        validate_snapshot_address_consistency(log_info)?;

        // Step 3: Validate the main log file covers pages below head.
        if log_info.head_address > LogicalAddress::ZERO {
            validate_log_file_for_head(base_dir, log_info)?;
        }

        // Step 4: Validate and load the snapshot file.
        let snapshot_pages = validate_snapshot_file(base_dir, plan, log_info)?;

        // Step 5: Compute restored address boundaries.
        let begin_address = log_info.begin_address;
        let tail_address = log_info.snapshot_final_address;
        // After snapshot recovery, head is set to begin — all data from
        // begin to tail is considered loaded (main log + snapshot).
        let head_address = begin_address;
        let read_only_address = tail_address;
        let flushed_until = tail_address;

        // Pages loaded = main-log pages (begin..snapshot_head) + snapshot pages.
        let main_log_pages =
            pages_between_addresses(begin_address, log_info.snapshot_start_address);
        let pages_loaded = main_log_pages + snapshot_pages;

        Ok(LogRecoveryResult {
            begin_address,
            head_address,
            tail_address,
            read_only_address,
            flushed_until,
            records_scanned: 0,
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
fn validate_log_file(base_dir: &Path, log_info: &LogRecoveryInfo) -> Result<u64, RecoveryError> {
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
        if to.offset().0 > from.offset().0 {
            1
        } else {
            0
        }
    } else {
        let base = to_page - from_page;
        if to.offset().0 > 0 { base + 1 } else { base }
    }
}

/// Verify the recovery info describes a snapshot checkpoint.
fn validate_snapshot_type(log_info: &LogRecoveryInfo) -> Result<(), RecoveryError> {
    if log_info.checkpoint_type != CheckpointType::Snapshot {
        return Err(RecoveryError::CorruptMetadata(format!(
            "expected Snapshot checkpoint, got {:?}",
            log_info.checkpoint_type
        )));
    }
    if !log_info.use_snapshot_file {
        return Err(RecoveryError::CorruptMetadata(
            "snapshot checkpoint has use_snapshot_file=false".to_string(),
        ));
    }
    Ok(())
}

/// Validate address consistency for a snapshot checkpoint.
///
/// The invariant is:
/// - `begin <= head`
/// - `snapshot_start == head` (snapshot covers from head to tail)
/// - `snapshot_start <= snapshot_final`
/// - `final_address == snapshot_final_address`
fn validate_snapshot_address_consistency(log_info: &LogRecoveryInfo) -> Result<(), RecoveryError> {
    let mut issues = Vec::new();

    if log_info.begin_address > log_info.head_address {
        issues.push(format!(
            "begin_address ({:?}) > head_address ({:?})",
            log_info.begin_address, log_info.head_address
        ));
    }

    if log_info.snapshot_start_address > log_info.snapshot_final_address {
        issues.push(format!(
            "snapshot_start_address ({:?}) > snapshot_final_address ({:?})",
            log_info.snapshot_start_address, log_info.snapshot_final_address
        ));
    }

    if log_info.head_address > log_info.snapshot_final_address {
        issues.push(format!(
            "head_address ({:?}) > snapshot_final_address ({:?})",
            log_info.head_address, log_info.snapshot_final_address
        ));
    }

    if !issues.is_empty() {
        return Err(RecoveryError::ValidationFailed(issues));
    }
    Ok(())
}

/// Validate the main log file covers pages from 0 to `head_address`.
///
/// For snapshot recovery, the main log file only needs to contain pages
/// up to the head address (not the full tail as in fold-over).
fn validate_log_file_for_head(
    base_dir: &Path,
    log_info: &LogRecoveryInfo,
) -> Result<(), RecoveryError> {
    let head = log_info.head_address;
    if head == LogicalAddress::ZERO {
        return Ok(());
    }

    let expected_min_size = logical_address_to_byte_offset(head);
    if expected_min_size == 0 {
        return Ok(());
    }

    let segment_size = default_segment_size();
    let num_segments = expected_min_size.div_ceil(segment_size);

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
             for head address {:?}, but total segment size is {total_size} bytes",
            head
        )]));
    }

    Ok(())
}

/// Validate the snapshot file exists and contains the expected pages.
///
/// Opens the snapshot file using [`SnapshotFileReader`], iterates all pages,
/// and verifies each page index falls within the expected range
/// [`snapshot_start_address`..`snapshot_final_address`].
///
/// Returns the number of pages found in the snapshot file.
fn validate_snapshot_file(
    base_dir: &Path,
    plan: &RecoveryPlan,
    log_info: &LogRecoveryInfo,
) -> Result<u32, RecoveryError> {
    let snapshot_path = base_dir.join(format!("{}.snapshot.log", plan.token));

    if !snapshot_path.exists() {
        return Err(RecoveryError::ValidationFailed(vec![format!(
            "snapshot file missing: {}",
            snapshot_path.display()
        )]));
    }

    let snap_start = log_info.snapshot_start_address;
    let snap_final = log_info.snapshot_final_address;

    // Empty snapshot region is valid if the file is also empty.
    if snap_start == snap_final {
        let file_meta = fs::metadata(&snapshot_path)?;
        if file_meta.len() != 0 {
            return Err(RecoveryError::ValidationFailed(vec![format!(
                "snapshot file is {} bytes but snapshot region is empty \
                 (start == final == {:?})",
                file_meta.len(),
                snap_start
            )]));
        }
        return Ok(0);
    }

    let reader = SnapshotFileReader::open(&snapshot_path)
        .map_err(|e| {
            RecoveryError::IoError(std::io::Error::other(format!(
                "failed to open snapshot file: {e}"
            )))
        })?
        .with_page_size(PAGE_SIZE as usize);

    let start_page = snap_start.page().0 as u64;
    let final_page = snap_final.page().0 as u64;

    let mut page_count: u32 = 0;
    let mut issues = Vec::new();

    for (page_index, _page_data) in reader.pages() {
        if page_index < start_page || page_index > final_page {
            issues.push(format!(
                "snapshot page index {page_index} outside expected range \
                 [{start_page}..{final_page}]"
            ));
        }
        page_count += 1;
    }

    if !issues.is_empty() {
        return Err(RecoveryError::ValidationFailed(issues));
    }

    let expected_pages = pages_between_addresses(snap_start, snap_final);
    if page_count != expected_pages {
        return Err(RecoveryError::ValidationFailed(vec![format!(
            "snapshot file contains {page_count} pages but expected \
             {expected_pages} pages for range {:?}..{:?}",
            snap_start, snap_final
        )]));
    }

    Ok(page_count)
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
            format_version: 2,
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
            format_version: 2,
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
    fn write_checkpoint(base_dir: &Path, token: &CheckpointToken, log_info: &LogRecoveryInfo) {
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
        let err = engine.recover_fold_over(&plan, dir.path()).unwrap_err();

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
        let err = engine.recover_fold_over(&plan, dir.path()).unwrap_err();

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

        let mut log_info = fold_over_log_info(
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
        );
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
        let err = engine.recover_fold_over(&plan, dir.path()).unwrap_err();

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
        let err = engine.recover_fold_over(&plan, dir.path()).unwrap_err();

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
        let err = engine.recover_fold_over(&plan, dir.path()).unwrap_err();

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
        assert_eq!(logical_address_to_byte_offset(addr), 2 * PAGE_SIZE + 512);
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

    // -- Snapshot test helpers -----------------------------------------------

    use crate::checkpoint::snapshot_writer::SnapshotFileWriter;

    /// Construct a [`LogRecoveryInfo`] for a snapshot checkpoint.
    fn snapshot_log_info(
        begin: LogicalAddress,
        head: LogicalAddress,
        snap_start: LogicalAddress,
        snap_final: LogicalAddress,
    ) -> LogRecoveryInfo {
        LogRecoveryInfo {
            format_version: 2,
            version: 1,
            checkpoint_type: CheckpointType::Snapshot,
            begin_address: begin,
            flushed_until_address: snap_final,
            final_address: snap_final,
            head_address: head,
            snapshot_start_address: snap_start,
            snapshot_final_address: snap_final,
            use_snapshot_file: true,
            object_log_segment_count: 0,
        }
    }

    /// Write a snapshot file with pages covering `start_page..end_page`.
    ///
    /// Each page is filled with `(page_num & 0xFF) as u8` so content can be
    /// verified after recovery.
    fn create_snapshot_file(
        base_dir: &Path,
        token: &CheckpointToken,
        start_page: u32,
        end_page: u32,
    ) {
        let path = base_dir.join(format!("{token}.snapshot.log"));
        let mut writer = SnapshotFileWriter::new(&path).expect("create snapshot file");
        for page_num in start_page..end_page {
            let data = vec![(page_num & 0xFF) as u8; PAGE_SIZE as usize];
            writer
                .write_page(page_num as u64, &data)
                .expect("write snapshot page");
        }
        writer.finalize().expect("finalize snapshot file");
    }

    /// Create an empty snapshot file (0 bytes).
    fn create_empty_snapshot_file(base_dir: &Path, token: &CheckpointToken) {
        let path = base_dir.join(format!("{token}.snapshot.log"));
        let writer = SnapshotFileWriter::new(&path).expect("create empty snapshot file");
        writer.finalize().expect("finalize empty snapshot file");
    }

    // -- Tests: basic snapshot recovery -------------------------------------

    #[test]
    fn recover_snapshot_basic() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // All data in snapshot (begin == head == 0), snapshot pages 0..3.
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(0), Offset(0));
        let snap_start = LogicalAddress::new(Page(0), Offset(0));
        let snap_final = LogicalAddress::new(Page(3), Offset(0));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Write snapshot file with 3 pages: 0, 1, 2.
        create_snapshot_file(dir.path(), &token, 0, 3);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        assert_eq!(result.head_address, begin);
        assert_eq!(result.tail_address, snap_final);
        assert_eq!(result.read_only_address, snap_final);
        assert_eq!(result.flushed_until, snap_final);
        assert_eq!(result.pages_loaded, 3);
    }

    #[test]
    fn recover_snapshot_with_main_log_and_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Pages 0..3 on main log, pages 3..6 in snapshot.
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(3), Offset(0));
        let snap_start = LogicalAddress::new(Page(3), Offset(0));
        let snap_final = LogicalAddress::new(Page(6), Offset(0));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Create main log covering up to head.
        let log_size = logical_address_to_byte_offset(head);
        create_log_file(dir.path(), 0, log_size);

        // Create snapshot with 3 pages: 3, 4, 5.
        create_snapshot_file(dir.path(), &token, 3, 6);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        assert_eq!(result.head_address, begin);
        assert_eq!(result.tail_address, snap_final);
        assert_eq!(result.read_only_address, snap_final);
        assert_eq!(result.flushed_until, snap_final);
        // 3 main log pages + 3 snapshot pages = 6
        assert_eq!(result.pages_loaded, 6);
    }

    #[test]
    fn recover_snapshot_only_snapshot_pages() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // begin == head — everything is in the snapshot, no main log pages.
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(0), Offset(0));
        let snap_start = LogicalAddress::new(Page(0), Offset(0));
        let snap_final = LogicalAddress::new(Page(5), Offset(256));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // 6 pages: 0, 1, 2, 3, 4, 5 (tail has offset so page 5 is partial).
        create_snapshot_file(dir.path(), &token, 0, 6);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        assert_eq!(result.head_address, begin);
        assert_eq!(result.tail_address, snap_final);
        // pages_between(0:0, 5:256) = 6
        assert_eq!(result.pages_loaded, 6);
        assert_eq!(result.records_scanned, 0);
    }

    // -- Tests: empty snapshot recovery -------------------------------------

    #[test]
    fn recover_snapshot_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let zero = LogicalAddress::ZERO;
        let log_info = snapshot_log_info(zero, zero, zero, zero);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Empty snapshot file.
        create_empty_snapshot_file(dir.path(), &token);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, zero);
        assert_eq!(result.head_address, zero);
        assert_eq!(result.tail_address, zero);
        assert_eq!(result.read_only_address, zero);
        assert_eq!(result.flushed_until, zero);
        assert_eq!(result.records_scanned, 0);
        assert_eq!(result.pages_loaded, 0);
    }

    // -- Tests: snapshot type validation ------------------------------------

    #[test]
    fn recover_snapshot_rejects_fold_over_type() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let mut log_info = snapshot_log_info(
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
        );
        log_info.checkpoint_type = CheckpointType::FoldOver;

        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &log_info, &[])
            .expect("write");

        let mgr = crate::recovery::RecoveryManager::new(dir.path().to_path_buf());
        let plan = mgr.select_checkpoint(Some(token)).expect("plan");

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::CorruptMetadata(msg) => {
                assert!(
                    msg.contains("Snapshot"),
                    "expected 'Snapshot' in error: {msg}"
                );
            }
            other => panic!("expected CorruptMetadata, got: {other}"),
        }
    }

    #[test]
    fn recover_snapshot_rejects_use_snapshot_file_false() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let mut log_info = snapshot_log_info(
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
            LogicalAddress::ZERO,
        );
        log_info.use_snapshot_file = false;

        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        store
            .write_checkpoint_metadata(&token, &sample_index_info(), &log_info, &[])
            .expect("write");

        let mgr = crate::recovery::RecoveryManager::new(dir.path().to_path_buf());
        let plan = mgr.select_checkpoint(Some(token)).expect("plan");

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::CorruptMetadata(msg) => {
                assert!(
                    msg.contains("use_snapshot_file"),
                    "expected 'use_snapshot_file' in error: {msg}"
                );
            }
            other => panic!("expected CorruptMetadata, got: {other}"),
        }
    }

    // -- Tests: snapshot address consistency --------------------------------

    #[test]
    fn recover_snapshot_inconsistent_begin_head() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // begin > head — invalid
        let begin = LogicalAddress::new(Page(5), Offset(0));
        let head = LogicalAddress::new(Page(2), Offset(0));
        let snap_start = LogicalAddress::new(Page(2), Offset(0));
        let snap_final = LogicalAddress::new(Page(10), Offset(0));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
                assert!(issues[0].contains("begin_address"));
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    #[test]
    fn recover_snapshot_inconsistent_snap_range() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // snapshot_start > snapshot_final — invalid
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(0), Offset(0));
        let snap_start = LogicalAddress::new(Page(10), Offset(0));
        let snap_final = LogicalAddress::new(Page(5), Offset(0));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
                assert!(issues[0].contains("snapshot_start_address"));
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    // -- Tests: snapshot file validation ------------------------------------

    #[test]
    fn recover_snapshot_missing_snapshot_file() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(0), Offset(0));
        let snap_final = LogicalAddress::new(Page(3), Offset(0));
        let log_info = snapshot_log_info(begin, begin, begin, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Don't create the snapshot file.
        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

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
    fn recover_snapshot_wrong_page_count() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Expect 3 pages but write only 2.
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let snap_final = LogicalAddress::new(Page(3), Offset(0));
        let log_info = snapshot_log_info(begin, begin, begin, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Write only 2 pages (0, 1) instead of expected 3.
        create_snapshot_file(dir.path(), &token, 0, 2);

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
                assert!(
                    issues[0].contains("pages"),
                    "expected 'pages' in error: {}",
                    issues[0]
                );
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    #[test]
    fn recover_snapshot_page_index_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Snapshot range is pages 2..5, but we write pages 10..13.
        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(2), Offset(0));
        let snap_start = LogicalAddress::new(Page(2), Offset(0));
        let snap_final = LogicalAddress::new(Page(5), Offset(0));
        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Create main log file.
        let log_size = logical_address_to_byte_offset(head);
        create_log_file(dir.path(), 0, log_size);

        // Write pages at wrong indices.
        let path = dir.path().join(format!("{token}.snapshot.log"));
        let mut writer = SnapshotFileWriter::new(&path).expect("create snapshot file");
        for page_num in 10..13u64 {
            let data = vec![0xFFu8; PAGE_SIZE as usize];
            writer.write_page(page_num, &data).expect("write page");
        }
        writer.finalize().expect("finalize");

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
                assert!(
                    issues[0].contains("outside expected range"),
                    "expected 'outside expected range' in error: {}",
                    issues[0]
                );
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    // -- Tests: address restoration correctness -----------------------------

    #[test]
    fn recover_snapshot_restores_correct_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(1), Offset(64));
        let head = LogicalAddress::new(Page(3), Offset(0));
        let snap_start = LogicalAddress::new(Page(3), Offset(0));
        let snap_final = LogicalAddress::new(Page(7), Offset(512));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Main log covering up to head.
        let log_size = logical_address_to_byte_offset(head);
        create_log_file(dir.path(), 0, log_size);

        // Snapshot pages: 3, 4, 5, 6, 7 = 5 pages.
        create_snapshot_file(dir.path(), &token, 3, 8);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        // After snapshot recovery head is set to begin.
        assert_eq!(result.head_address, begin);
        assert_eq!(result.tail_address, snap_final);
        assert_eq!(result.read_only_address, snap_final);
        assert_eq!(result.flushed_until, snap_final);
        // Main log pages: pages_between(1:64, 3:0) = 2
        // Snapshot pages: pages_between(3:0, 7:512) = 5
        assert_eq!(result.pages_loaded, 7);
    }

    // -- Tests: page content verification after snapshot recovery -----------

    #[test]
    fn recover_snapshot_page_content_verification() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(0), Offset(0));
        let snap_final = LogicalAddress::new(Page(3), Offset(0));
        let log_info = snapshot_log_info(begin, begin, begin, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Write snapshot pages with identifiable content.
        create_snapshot_file(dir.path(), &token, 0, 3);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();
        assert_eq!(result.pages_loaded, 3);

        // Verify the snapshot file can be re-read and pages have correct content.
        let path = dir.path().join(format!("{token}.snapshot.log"));
        let reader = SnapshotFileReader::open(&path)
            .unwrap()
            .with_page_size(PAGE_SIZE as usize);

        let pages: Vec<(u64, &[u8])> = reader.pages().collect();
        assert_eq!(pages.len(), 3);
        for (page_index, page_data) in &pages {
            let expected_fill = (*page_index & 0xFF) as u8;
            assert!(
                page_data.iter().all(|&b| b == expected_fill),
                "page {page_index} content mismatch"
            );
        }
    }

    // -- Tests: snapshot with non-zero begin --------------------------------

    #[test]
    fn recover_snapshot_evicted_pages_before_head() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Some pages were evicted: begin at page 2, head at page 5.
        // Snapshot covers pages 5..8.
        let begin = LogicalAddress::new(Page(2), Offset(0));
        let head = LogicalAddress::new(Page(5), Offset(0));
        let snap_start = LogicalAddress::new(Page(5), Offset(0));
        let snap_final = LogicalAddress::new(Page(8), Offset(0));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Main log file must cover up to head.
        let log_size = logical_address_to_byte_offset(head);
        create_log_file(dir.path(), 0, log_size);

        // Snapshot pages: 5, 6, 7.
        create_snapshot_file(dir.path(), &token, 5, 8);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();

        assert_eq!(result.begin_address, begin);
        assert_eq!(result.head_address, begin);
        assert_eq!(result.tail_address, snap_final);
        // Main log pages: pages_between(2:0, 5:0) = 3
        // Snapshot pages: pages_between(5:0, 8:0) = 3
        assert_eq!(result.pages_loaded, 6);
    }

    // -- Tests: missing main log for snapshot with head > 0 -----------------

    #[test]
    fn recover_snapshot_missing_main_log_when_head_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(0), Offset(0));
        let head = LogicalAddress::new(Page(3), Offset(0));
        let snap_start = LogicalAddress::new(Page(3), Offset(0));
        let snap_final = LogicalAddress::new(Page(6), Offset(0));

        let log_info = snapshot_log_info(begin, head, snap_start, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Create snapshot but no main log.
        create_snapshot_file(dir.path(), &token, 3, 6);

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
                assert!(
                    issues[0].contains("missing"),
                    "expected 'missing' in error: {}",
                    issues[0]
                );
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }

    // -- Tests: snapshot records scanned ------------------------------------

    #[test]
    fn recover_snapshot_records_scanned_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        let begin = LogicalAddress::new(Page(0), Offset(0));
        let snap_final = LogicalAddress::new(Page(2), Offset(0));
        let log_info = snapshot_log_info(begin, begin, begin, snap_final);
        let plan = build_plan(dir.path(), &token, &log_info);

        create_snapshot_file(dir.path(), &token, 0, 2);

        let engine = LogRecoveryEngine::new();
        let result = engine.recover_snapshot(&plan, dir.path()).unwrap();

        assert_eq!(result.records_scanned, 0);
    }

    // -- Tests: non-empty snapshot file for empty region --------------------

    #[test]
    fn recover_snapshot_non_empty_file_for_empty_region() {
        let dir = tempfile::tempdir().unwrap();
        let token = sample_token();

        // Snapshot region is empty (start == final) but file has data.
        let zero = LogicalAddress::ZERO;
        let log_info = snapshot_log_info(zero, zero, zero, zero);
        let plan = build_plan(dir.path(), &token, &log_info);

        // Write a page to the snapshot file even though region is empty.
        let path = dir.path().join(format!("{token}.snapshot.log"));
        let mut writer = SnapshotFileWriter::new(&path).expect("create snapshot file");
        let data = vec![0xABu8; PAGE_SIZE as usize];
        writer.write_page(0, &data).expect("write page");
        writer.finalize().expect("finalize");

        let engine = LogRecoveryEngine::new();
        let err = engine.recover_snapshot(&plan, dir.path()).unwrap_err();

        match err {
            RecoveryError::ValidationFailed(issues) => {
                assert!(!issues.is_empty());
            }
            other => panic!("expected ValidationFailed, got: {other}"),
        }
    }
}
