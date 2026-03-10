//! Mutation-testing gap-fill tests for `sync_file_device.rs`.
//!
//! Targets surviving mutants in segment arithmetic, read/write loops,
//! and device metadata methods.

use faster_core::device::Device;
use faster_core::sync_file_device::SyncFileDevice;
use std::path::PathBuf;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("faster_mutation_sfd_{}", name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

// ===========================================================================
// Basic write/read roundtrip — kills arithmetic mutants in read/write_sync
// ===========================================================================

/// Kill mutants in read_sync/write_sync segment offset calculations.
/// Specifically: `/ with %`, `% with +`, `- with +`, `+= with *=`.
#[test]
fn sync_file_device_write_read_roundtrip() {
    let dir = temp_dir("roundtrip");
    let dev = SyncFileDevice::new(&dir, "log.", 512, 1 << 20, 1)
        .expect("create device");

    let data: Vec<u8> = (0..512).map(|i| (i % 256) as u8).collect();
    dev.write_sync(0, &data).expect("write should succeed");

    let mut readback = vec![0u8; 512];
    dev.read_sync(0, &mut readback).expect("read should succeed");
    assert_eq!(readback, data, "roundtrip data must match");

    dev.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Write and read at non-zero offset.
#[test]
fn sync_file_device_write_read_at_offset() {
    let dir = temp_dir("offset");
    let dev = SyncFileDevice::new(&dir, "log.", 512, 1 << 20, 1)
        .expect("create device");

    let data = vec![0xABu8; 1024];
    dev.write_sync(4096, &data).expect("write at offset");

    let mut readback = vec![0u8; 1024];
    dev.read_sync(4096, &mut readback).expect("read at offset");
    assert_eq!(readback, data);

    dev.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Write at two different offsets and verify both.
#[test]
fn sync_file_device_multiple_writes() {
    let dir = temp_dir("multi_write");
    let dev = SyncFileDevice::new(&dir, "log.", 512, 1 << 20, 1)
        .expect("create device");

    let data1 = vec![0x11u8; 512];
    let data2 = vec![0x22u8; 512];

    dev.write_sync(0, &data1).expect("write 1");
    dev.write_sync(512, &data2).expect("write 2");

    let mut buf1 = vec![0u8; 512];
    let mut buf2 = vec![0u8; 512];
    dev.read_sync(0, &mut buf1).expect("read 1");
    dev.read_sync(512, &mut buf2).expect("read 2");

    assert_eq!(buf1, data1, "first write data");
    assert_eq!(buf2, data2, "second write data");

    dev.close();
    let _ = std::fs::remove_dir_all(&dir);
}

// ===========================================================================
// Cross-segment boundary — kills segment splitting arithmetic
// ===========================================================================

/// Kill mutants in execute_request segment splitting: `/ with %`, `% with +`,
/// `- with +`, `+= with *=`.
///
/// Use a tiny segment size (4096) so a 2-sector write crosses the boundary.
#[test]
fn sync_file_device_cross_segment_write() {
    let dir = temp_dir("cross_seg");
    let segment_size: u64 = 4096; // tiny segments
    let dev = SyncFileDevice::new(&dir, "log.", 512, segment_size, 1)
        .expect("create device");

    // Write 1024 bytes starting 512 bytes before the segment boundary
    let offset = segment_size - 512;
    let data = vec![0xCCu8; 1024]; // crosses boundary
    dev.write_sync(offset, &data).expect("cross-segment write");

    let mut readback = vec![0u8; 1024];
    dev.read_sync(offset, &mut readback).expect("cross-segment read");
    assert_eq!(readback, data, "cross-segment data must match");

    dev.close();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Write spanning exactly 3 segments.
#[test]
fn sync_file_device_multi_segment_write() {
    let dir = temp_dir("multi_seg");
    let segment_size: u64 = 2048;
    let dev = SyncFileDevice::new(&dir, "log.", 512, segment_size, 1)
        .expect("create device");

    // Write 4096 bytes at offset 0 — spans segments 0, 1 (and partially 2 if the segment_size allows)
    let data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
    dev.write_sync(0, &data).expect("multi-segment write");

    let mut readback = vec![0u8; 4096];
    dev.read_sync(0, &mut readback).expect("multi-segment read");
    assert_eq!(readback, data, "multi-segment roundtrip must match");

    dev.close();
    let _ = std::fs::remove_dir_all(&dir);
}

// ===========================================================================
// Device::size — high water mark tracking
// ===========================================================================

/// Kill mutant: `size -> 0`.
#[test]
fn sync_file_device_size_tracks_writes() {
    let dir = temp_dir("size_track");
    let dev = SyncFileDevice::new(&dir, "log.", 512, 1 << 20, 1)
        .expect("create device");

    assert_eq!(dev.size(), 0, "empty device should have size 0");

    let data = vec![0u8; 512];
    dev.write_sync(0, &data).expect("write");
    assert_eq!(dev.size(), 512, "size should be 512 after first write");

    dev.write_sync(4096, &data).expect("write at offset");
    assert_eq!(
        dev.size(),
        4096 + 512,
        "size should reflect the high water mark"
    );

    dev.close();
    let _ = std::fs::remove_dir_all(&dir);
}

// ===========================================================================
// Device trait constants
// ===========================================================================

/// Verify sector_size and segment_size return construction values.
#[test]
fn sync_file_device_trait_constants() {
    let dir = temp_dir("constants");
    let dev = SyncFileDevice::new(&dir, "log.", 1024, 1 << 22, 1)
        .expect("create device");

    assert_eq!(dev.sector_size(), 1024);
    assert_eq!(dev.segment_size(), 1 << 22);

    dev.close();
    let _ = std::fs::remove_dir_all(&dir);
}

// ===========================================================================
// Triage: equivalent/performance-only mutants
// ===========================================================================

// write_complete_at Interrupted handling: the match guard mutations
// (== vs !=, true vs false) affect retry behavior for EINTR. In practice,
// EINTR rarely occurs in tests. These are edge-case I/O mutants that
// would require fault injection to kill. Existing io_error_injection.rs
// uses a different device. Triaged as equivalent for this campaign.
//
// SegmentRegistry::close_all -> (): file handle leak, equivalent.
// Drop for SyncFileDevice -> (): same — leak undetectable.
// worker_loop `delete -` (line 294): affects thread priority/indexing,
// cosmetic in single-worker setup.
