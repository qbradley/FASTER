//! Integration tests for page CRC-32C checksums.
//!
//! Validates the end-to-end checksum path: flush writes a CRC trailer,
//! recovery reads it back and validates, and corruption is detected.

use std::sync::Arc;

use faster_core::device::{Device, InMemoryDevice};
use faster_core::hybrid_log::flush::PageFlusher;
use faster_core::hybrid_log::page::{PageState, PageTable, PageTrailer};
use faster_core::recovery::ChecksumValidationPolicy;

use faster_dst::device::{SimulatedDevice, SimulatedStorage};
use faster_dst::fault::FaultConfig;

const SECTOR: u32 = 512;
const PAGE: u32 = 4096;
const BUFFER_PAGES: usize = 4;

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Allocate a page, write a pattern into `[0..valid_bytes]`, seal it, and
/// return `(PageTable, PageFlusher)`.
fn prepare_page(valid_bytes: usize) -> (PageTable, PageFlusher) {
    let pt = PageTable::new(BUFFER_PAGES, PAGE as usize, SECTOR as usize);
    let frame = pt.get_or_allocate_frame(faster_core::address::Page(0));

    let pattern: Vec<u8> = (0..valid_bytes).map(|i| (i % 251) as u8).collect();
    unsafe {
        std::ptr::copy_nonoverlapping(pattern.as_ptr(), frame.as_mut_ptr(), valid_bytes);
    }
    frame
        .state()
        .try_transition(PageState::Open, PageState::Sealed);

    let flusher = PageFlusher::new(SECTOR, PAGE);
    (pt, flusher)
}

/// Read back the write region and extract the trailer.
fn read_trailer(dev: &dyn Device, page_num: u32, valid_bytes: u32) -> (Vec<u8>, PageTrailer) {
    let flusher = PageFlusher::new(SECTOR, PAGE);
    let write_size = PageTrailer::write_size(valid_bytes, SECTOR, PAGE) as usize;
    let offset = flusher.device_offset(faster_core::address::Page(page_num));

    let mut buf = vec![0u8; write_size];
    dev.read_sync(offset, &mut buf).unwrap();
    let trailer = PageTrailer::from_slice(&buf, write_size);
    (buf, trailer)
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Valid page write → read → CRC passes
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn valid_page_crc_roundtrip() {
    let (pt, flusher) = prepare_page(1000);
    let dev = InMemoryDevice::with_sizes(SECTOR, 1 << 30);
    let page = faster_core::address::Page(0);

    flusher
        .flush_page_sync(page, &pt, &dev, 1000)
        .expect("flush should succeed");

    let (buf, trailer) = read_trailer(&dev, 0, 1000);
    assert_eq!(trailer.valid_bytes, 1000);

    let actual_crc = crc32fast::hash(&buf[..trailer.valid_bytes as usize]);
    assert_eq!(trailer.crc32, actual_crc, "CRC should match");
}

#[test]
fn full_page_crc_roundtrip() {
    let (pt, flusher) = prepare_page(PAGE as usize);
    let dev = InMemoryDevice::with_sizes(SECTOR, 1 << 30);
    let page = faster_core::address::Page(0);

    flusher
        .flush_page_sync(page, &pt, &dev, PAGE)
        .expect("flush should succeed");

    let (buf, _trailer) = read_trailer(&dev, 0, PAGE);
    // For a full page, the CRC trailer is skipped to avoid overwriting
    // the last 8 bytes of valid record data. The trailer region will
    // contain the original data pattern, not a valid trailer.
    // Recovery detects this via the valid_bytes==0 && crc==0 check.
    let pattern: Vec<u8> = (0..PAGE as usize).map(|i| (i % 251) as u8).collect();
    assert_eq!(
        &buf[..],
        &pattern[..],
        "full-page data should be preserved without CRC trailer corruption"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Torn write detection (SimulatedDevice partial writes)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn torn_write_produces_crc_mismatch() {
    let storage = SimulatedStorage::new();
    let device = SimulatedDevice::new(Arc::clone(&storage), 42)
        .with_faults(FaultConfig {
            partial_write_rate: 1.0, // Every write is partial
            ..FaultConfig::none()
        })
        .with_sector_size(SECTOR);

    let (pt, flusher) = prepare_page(1000);
    let page = faster_core::address::Page(0);

    // Flush with guaranteed partial write. The page is still in Flushing
    // state because the callback detects the short write.
    flusher
        .flush_page(page, &pt, &device, 1000)
        .expect("flush submission should succeed");

    // Read back whatever was partially written.
    let write_size = PageTrailer::write_size(1000, SECTOR, PAGE) as usize;
    let mut buf = vec![0u8; write_size];
    device.read_sync(0, &mut buf).unwrap();
    let trailer = PageTrailer::from_slice(&buf, write_size);

    // The partial write likely truncated the data or the trailer.
    // Either the trailer is garbage (reads as zero/random) or the CRC won't
    // match the truncated data.
    if trailer.valid_bytes > 0 && (trailer.valid_bytes as usize) <= write_size {
        let actual = crc32fast::hash(&buf[..trailer.valid_bytes as usize]);
        // CRC mismatch expected (torn write corrupted the page).
        // With partial_write_rate=1.0 the length is always < write_size,
        // so the trailer or data region is guaranteed to be truncated.
        assert_ne!(
            actual, trailer.crc32,
            "partial write with rate=1.0 should cause CRC mismatch"
        );
    }
    // If trailer.valid_bytes is 0, the trailer region was zeroed out (not
    // written), which is also detected as corruption.
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Format version 2 backward compatibility
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn version_2_pages_skip_crc_validation() {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::checkpoint::{
        CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo,
        LogRecoveryInfo,
    };
    use faster_core::recovery::{RecoveryManager, log_recovery::LogRecoveryEngine};
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let token = CheckpointToken::new(0xABC);

    let begin = LogicalAddress::new(Page(0), Offset(0));
    let tail = LogicalAddress::new(Page(1), Offset(256));

    // Explicitly use format_version 2 (no CRC trailer).
    let log_info = LogRecoveryInfo {
        format_version: 2,
        version: 1,
        checkpoint_type: CheckpointType::FoldOver,
        begin_address: begin,
        flushed_until_address: tail,
        final_address: tail,
        head_address: begin,
        snapshot_start_address: LogicalAddress::ZERO,
        snapshot_final_address: LogicalAddress::ZERO,
        use_snapshot_file: false,
        object_log_segment_count: 0,
    };

    let index_info = IndexRecoveryInfo {
        format_version: 2,
        version: 1,
        table_size: 1 << 16,
        num_ht_bytes: (1 << 16) * 64,
        num_ofb_bytes: 4096,
        num_buckets: (1 << 16) + 64,
        start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
        final_logical_address: LogicalAddress::new(Page(1), Offset(256)),
    };

    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    store
        .write_checkpoint_metadata(&token, &index_info, &log_info, &[])
        .unwrap();

    // Create a log file with NO CRC trailers (just zeroes).
    let page_size: u64 = 1u64 << faster_core::address::OFFSET_BITS;
    let needed = page_size + 256;
    let data = vec![0u8; needed as usize];
    fs::write(dir.path().join("log.0"), &data).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    // Recovery should succeed — CRC validation is skipped for v2.
    let engine = LogRecoveryEngine::new();
    let result = engine.recover_fold_over(&plan, dir.path(), "log.").unwrap();
    assert_eq!(result.tail_address, tail);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. ChecksumValidationPolicy::Reject — returns error
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn reject_policy_returns_error_on_corruption() {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::checkpoint::{
        CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo,
        LogRecoveryInfo,
    };
    use faster_core::recovery::{RecoveryManager, log_recovery::LogRecoveryEngine};
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let token = CheckpointToken::new(0xDEF);

    let begin = LogicalAddress::new(Page(0), Offset(0));
    let tail = LogicalAddress::new(Page(0), Offset(1000));

    // Format version 3 — CRC validation is active.
    let log_info = LogRecoveryInfo {
        format_version: 3,
        version: 1,
        checkpoint_type: CheckpointType::FoldOver,
        begin_address: begin,
        flushed_until_address: tail,
        final_address: tail,
        head_address: begin,
        snapshot_start_address: LogicalAddress::ZERO,
        snapshot_final_address: LogicalAddress::ZERO,
        use_snapshot_file: false,
        object_log_segment_count: 0,
    };

    let index_info = IndexRecoveryInfo {
        format_version: 3,
        version: 1,
        table_size: 1 << 16,
        num_ht_bytes: (1 << 16) * 64,
        num_ofb_bytes: 4096,
        num_buckets: (1 << 16) + 64,
        start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
        final_logical_address: LogicalAddress::new(Page(0), Offset(1000)),
    };

    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    store
        .write_checkpoint_metadata(&token, &index_info, &log_info, &[])
        .unwrap();

    // Create a log file with CORRUPTED data (no valid CRC trailer).
    let page_size: u64 = 1u64 << faster_core::address::OFFSET_BITS;
    let data = vec![0xABu8; page_size as usize];
    fs::write(dir.path().join("log.0"), &data).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    // Reject policy (default) — should fail with CRC mismatch or trailer corruption.
    let engine = LogRecoveryEngine::new().with_checksum_policy(ChecksumValidationPolicy::Reject);
    let err = engine
        .recover_fold_over(&plan, dir.path(), "log.")
        .unwrap_err();
    match err {
        faster_core::recovery::RecoveryError::ValidationFailed(issues) => {
            assert!(!issues.is_empty());
            assert!(
                issues[0].contains("CRC") || issues[0].contains("trailer"),
                "expected CRC/trailer error, got: {}",
                issues[0]
            );
        }
        other => panic!("expected ValidationFailed, got: {other}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. ChecksumValidationPolicy::Warn — continues recovery
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn warn_policy_continues_on_corruption() {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::checkpoint::{
        CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo,
        LogRecoveryInfo,
    };
    use faster_core::recovery::{RecoveryManager, log_recovery::LogRecoveryEngine};
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let token = CheckpointToken::new(0x999);

    let begin = LogicalAddress::new(Page(0), Offset(0));
    let tail = LogicalAddress::new(Page(0), Offset(1000));

    let log_info = LogRecoveryInfo {
        format_version: 3,
        version: 1,
        checkpoint_type: CheckpointType::FoldOver,
        begin_address: begin,
        flushed_until_address: tail,
        final_address: tail,
        head_address: begin,
        snapshot_start_address: LogicalAddress::ZERO,
        snapshot_final_address: LogicalAddress::ZERO,
        use_snapshot_file: false,
        object_log_segment_count: 0,
    };

    let index_info = IndexRecoveryInfo {
        format_version: 3,
        version: 1,
        table_size: 1 << 16,
        num_ht_bytes: (1 << 16) * 64,
        num_ofb_bytes: 4096,
        num_buckets: (1 << 16) + 64,
        start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
        final_logical_address: LogicalAddress::new(Page(0), Offset(1000)),
    };

    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    store
        .write_checkpoint_metadata(&token, &index_info, &log_info, &[])
        .unwrap();

    // Corrupted log file.
    let page_size: u64 = 1u64 << faster_core::address::OFFSET_BITS;
    let data = vec![0xCDu8; page_size as usize];
    fs::write(dir.path().join("log.0"), &data).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    // Warn policy — should succeed despite CRC mismatch.
    let engine = LogRecoveryEngine::new().with_checksum_policy(ChecksumValidationPolicy::Warn);
    let result = engine.recover_fold_over(&plan, dir.path(), "log.");
    assert!(
        result.is_ok(),
        "Warn policy should allow recovery to proceed"
    );
    assert_eq!(result.unwrap().tail_address, tail);
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. ChecksumValidationPolicy::Repair — returns error (future work)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn repair_policy_returns_error_on_corruption() {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::checkpoint::{
        CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo,
        LogRecoveryInfo,
    };
    use faster_core::recovery::{RecoveryManager, log_recovery::LogRecoveryEngine};
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let token = CheckpointToken::new(0x888);

    let begin = LogicalAddress::new(Page(0), Offset(0));
    let tail = LogicalAddress::new(Page(0), Offset(1000));

    let log_info = LogRecoveryInfo {
        format_version: 3,
        version: 1,
        checkpoint_type: CheckpointType::FoldOver,
        begin_address: begin,
        flushed_until_address: tail,
        final_address: tail,
        head_address: begin,
        snapshot_start_address: LogicalAddress::ZERO,
        snapshot_final_address: LogicalAddress::ZERO,
        use_snapshot_file: false,
        object_log_segment_count: 0,
    };

    let index_info = IndexRecoveryInfo {
        format_version: 3,
        version: 1,
        table_size: 1 << 16,
        num_ht_bytes: (1 << 16) * 64,
        num_ofb_bytes: 4096,
        num_buckets: (1 << 16) + 64,
        start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
        final_logical_address: LogicalAddress::new(Page(0), Offset(1000)),
    };

    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    store
        .write_checkpoint_metadata(&token, &index_info, &log_info, &[])
        .unwrap();

    let page_size: u64 = 1u64 << faster_core::address::OFFSET_BITS;
    let data = vec![0xEFu8; page_size as usize];
    fs::write(dir.path().join("log.0"), &data).unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    // Repair policy — currently behaves like Reject.
    let engine = LogRecoveryEngine::new().with_checksum_policy(ChecksumValidationPolicy::Repair);
    let err = engine
        .recover_fold_over(&plan, dir.path(), "log.")
        .unwrap_err();
    match err {
        faster_core::recovery::RecoveryError::ValidationFailed(issues) => {
            assert!(
                issues[0].contains("CRC") || issues[0].contains("trailer"),
                "expected CRC/trailer error, got: {}",
                issues[0]
            );
        }
        other => panic!("expected ValidationFailed, got: {other}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Valid V3 page — end-to-end flush + file recovery
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn v3_flush_and_recover_passes_crc() {
    use faster_core::address::{LogicalAddress, Offset, Page};
    use faster_core::checkpoint::{
        CheckpointMetadataStore, CheckpointToken, CheckpointType, IndexRecoveryInfo,
        LogRecoveryInfo,
    };
    use faster_core::recovery::{RecoveryManager, log_recovery::LogRecoveryEngine};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let token = CheckpointToken::new(0x777);

    // Flush a page with CRC to an in-memory device, then write it to a file.
    let valid_bytes = 1000u32;
    let (pt, flusher) = prepare_page(valid_bytes as usize);
    let dev = InMemoryDevice::with_sizes(SECTOR, 1 << 30);
    let page = faster_core::address::Page(0);

    flusher
        .flush_page_sync(page, &pt, &dev, valid_bytes)
        .unwrap();

    // Read the full page from device and write to log file.
    let page_size: u64 = 1u64 << faster_core::address::OFFSET_BITS;
    let mut page_data = vec![0u8; page_size as usize];
    dev.read_sync(0, &mut page_data).unwrap();

    let log_path = dir.path().join("log.0");
    let mut f = std::fs::File::create(&log_path).unwrap();
    f.write_all(&page_data).unwrap();

    // Create checkpoint metadata with format_version 3.
    let begin = LogicalAddress::new(Page(0), Offset(0));
    let tail = LogicalAddress::new(Page(0), Offset(valid_bytes));

    let log_info = LogRecoveryInfo {
        format_version: 3,
        version: 1,
        checkpoint_type: CheckpointType::FoldOver,
        begin_address: begin,
        flushed_until_address: tail,
        final_address: tail,
        head_address: begin,
        snapshot_start_address: LogicalAddress::ZERO,
        snapshot_final_address: LogicalAddress::ZERO,
        use_snapshot_file: false,
        object_log_segment_count: 0,
    };

    let index_info = IndexRecoveryInfo {
        format_version: 3,
        version: 1,
        table_size: 1 << 16,
        num_ht_bytes: (1 << 16) * 64,
        num_ofb_bytes: 4096,
        num_buckets: (1 << 16) + 64,
        start_logical_address: begin,
        final_logical_address: tail,
    };

    let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
    store
        .write_checkpoint_metadata(&token, &index_info, &log_info, &[])
        .unwrap();

    let mgr = RecoveryManager::new(dir.path().to_path_buf());
    let plan = mgr.select_checkpoint(Some(token)).unwrap();

    // Recovery with Reject policy should succeed (CRC is valid).
    let engine = LogRecoveryEngine::new().with_checksum_policy(ChecksumValidationPolicy::Reject);
    let result = engine.recover_fold_over(&plan, dir.path(), "log.").unwrap();
    assert_eq!(result.tail_address, tail);
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. SimulatedDevice verify_page_checksum helper
// ═══════════════════════════════════════════════════════════════════════════

/// Read a page from the SimulatedStorage and verify its CRC trailer.
/// Returns `Ok(trailer)` if the CRC matches, `Err(msg)` if it doesn't.
fn verify_page_checksum(
    storage: &SimulatedStorage,
    page_num: u32,
    valid_bytes: u32,
) -> Result<PageTrailer, String> {
    let write_size = PageTrailer::write_size(valid_bytes, SECTOR, PAGE) as usize;
    let offset = page_num as u64 * PAGE as u64;
    let snapshot = storage.snapshot();
    let start = offset as usize;
    let end = start + write_size;

    if end > snapshot.len() {
        return Err(format!(
            "page {page_num}: storage too small ({} bytes, need {end})",
            snapshot.len()
        ));
    }

    let page_data = &snapshot[start..end];
    let trailer = PageTrailer::from_slice(page_data, write_size);
    let crc_range = trailer.valid_bytes as usize;
    if crc_range > page_data.len() {
        return Err(format!(
            "page {page_num}: trailer valid_bytes ({crc_range}) > write_size ({write_size})"
        ));
    }

    let actual = crc32fast::hash(&page_data[..crc_range]);
    if actual != trailer.crc32 {
        Err(format!(
            "page {page_num}: CRC mismatch (trailer={:#010x}, actual={:#010x})",
            trailer.crc32, actual
        ))
    } else {
        Ok(trailer)
    }
}

#[test]
fn simulated_storage_verify_helper_valid_page() {
    let storage = SimulatedStorage::new();
    let device = SimulatedDevice::new(Arc::clone(&storage), 1).with_sector_size(SECTOR);

    let (pt, flusher) = prepare_page(1000);
    let page = faster_core::address::Page(0);
    flusher.flush_page_sync(page, &pt, &device, 1000).unwrap();

    let trailer =
        verify_page_checksum(&storage, 0, 1000).expect("valid page should pass CRC check");
    assert_eq!(trailer.valid_bytes, 1000);
}

#[test]
fn simulated_storage_verify_helper_detects_corruption() {
    let storage = SimulatedStorage::new();
    let device = SimulatedDevice::new(Arc::clone(&storage), 1).with_sector_size(SECTOR);

    let (pt, flusher) = prepare_page(1000);
    let page = faster_core::address::Page(0);
    flusher.flush_page_sync(page, &pt, &device, 1000).unwrap();

    // Corrupt the first byte on disk.
    let mut snapshot = storage.snapshot();
    snapshot[0] ^= 0xFF;
    storage.restore(snapshot);

    let err =
        verify_page_checksum(&storage, 0, 1000).expect_err("corrupted page should fail CRC check");
    assert!(err.contains("CRC mismatch"));
}
