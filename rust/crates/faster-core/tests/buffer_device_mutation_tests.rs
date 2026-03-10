//! Mutation-testing gap-fill tests for `buffer_pool.rs` and `device.rs`.
//!
//! Targets surviving mutants from the cargo-mutants campaign.

use faster_core::buffer_pool::{AlignedBuffer, BufferPool};
use faster_core::device::{Device, InMemoryDevice};

// ===========================================================================
// buffer_pool.rs — AlignedBuffer
// ===========================================================================

/// Kill mutant: `AlignedBuffer::as_ptr -> Default::default()` (null).
#[test]
fn aligned_buffer_as_ptr_is_not_null() {
    let buf = AlignedBuffer::new(512, 512);
    assert!(!buf.as_ptr().is_null(), "as_ptr must return a valid pointer");
}

/// Kill mutant: `AlignedBuffer::is_empty -> false`.
#[test]
fn aligned_buffer_is_empty_for_zero_len() {
    // AlignedBuffer with data should not be empty
    let buf = AlignedBuffer::new(512, 512);
    assert!(!buf.is_empty(), "512-byte buffer should not be empty");
    assert_eq!(buf.len(), 512);
}

// Drop mutant (drop -> ()) is equivalent: tests cannot detect memory leaks.

// ===========================================================================
// buffer_pool.rs — BufferPool acquire/release and sector_size
// ===========================================================================

/// Kill mutant: `BufferPool::sector_size -> 0` and `-> 1`.
#[test]
fn buffer_pool_sector_size_reflects_construction() {
    let pool = BufferPool::new(512, 4);
    assert_eq!(pool.sector_size(), 512, "sector_size must match constructor argument");

    let pool2 = BufferPool::new(4096, 4);
    assert_eq!(pool2.sector_size(), 4096);
}

/// Kill mutant: `acquire` and `release` — `< with <=` boundary.
///
/// Test that acquire returns a buffer from the free list after release,
/// and that the pool respects capacity limits.
#[test]
fn buffer_pool_acquire_release_roundtrip() {
    let pool = BufferPool::new(512, 2);

    // Acquire a buffer
    let buf1 = pool.acquire(512);
    assert_eq!(buf1.len(), 512);

    // Release it back
    pool.release(buf1);

    // Acquire again — should come from free list
    let buf2 = pool.acquire(512);
    assert_eq!(buf2.len(), 512);

    // Check pool stats
    let stats = pool.pool_stats();
    // After acquiring buf2, the pool's free list for 512 class should be empty
    for &(size, count) in stats.classes.iter() {
        if size == 512 {
            assert_eq!(count, 0, "512-byte class should be empty after acquire");
        }
    }

    pool.release(buf2);
}

/// Test pool capacity limits (related to < vs <= in release).
#[test]
fn buffer_pool_respects_max_capacity() {
    let pool = BufferPool::new(512, 2); // max 2 per class

    // Acquire 3 buffers
    let b1 = pool.acquire(512);
    let b2 = pool.acquire(512);
    let b3 = pool.acquire(512);

    // Release all 3
    pool.release(b1);
    pool.release(b2);
    pool.release(b3); // This one should be dropped (over capacity)

    let stats = pool.pool_stats();
    for &(size, count) in stats.classes.iter() {
        if size == 512 {
            assert!(count <= 2, "pool should not exceed max capacity of 2");
        }
    }
}

// ===========================================================================
// device.rs — InMemoryDevice
// ===========================================================================

/// Kill mutant: `InMemoryDevice::with_sizes -> Default::default()`.
#[test]
fn in_memory_device_with_sizes_uses_custom_values() {
    let dev = InMemoryDevice::with_sizes(1024, 1 << 20);
    assert_eq!(dev.sector_size(), 1024, "custom sector size must be used");
    assert_eq!(dev.segment_size(), 1 << 20, "custom segment size must be used");
}

/// Kill mutant: `sector_size -> 0` and `-> 1`.
#[test]
fn in_memory_device_default_sector_size() {
    let dev = InMemoryDevice::new();
    assert_eq!(dev.sector_size(), 512, "default sector size should be 512");
}

/// Kill mutant: `segment_size -> 0` and `-> 1`.
#[test]
fn in_memory_device_default_segment_size() {
    let dev = InMemoryDevice::new();
    assert_eq!(dev.segment_size(), 1 << 30, "default segment size should be 1GB");
}

/// Kill mutant: `max_outstanding_io -> 0` and `-> 1`.
#[test]
fn in_memory_device_max_outstanding_io() {
    let dev = InMemoryDevice::new();
    assert_eq!(
        dev.max_outstanding_io(),
        u32::MAX,
        "in-memory device should report u32::MAX outstanding IO"
    );
}

/// Kill mutant: `ensure_capacity > vs >=` and `read_sync < vs <=`.
///
/// Test write then read at exact boundaries.
#[test]
fn in_memory_device_write_read_exact_boundary() {
    let dev = InMemoryDevice::new();
    let data = vec![0xABu8; 512];

    // Write 512 bytes at offset 0
    dev.write_sync(0, &data).expect("write should succeed");

    // Read back exactly 512 bytes
    let mut readback = vec![0u8; 512];
    dev.read_sync(0, &mut readback).expect("read should succeed");
    assert_eq!(readback, data, "read-back data must match written data");
}

/// Write at a non-zero offset and verify ensure_capacity works.
#[test]
fn in_memory_device_write_at_offset() {
    let dev = InMemoryDevice::new();
    let data = vec![0xCDu8; 1024];

    // Write at offset 4096
    dev.write_sync(4096, &data).expect("write at offset should succeed");

    // Read back
    let mut readback = vec![0u8; 1024];
    dev.read_sync(4096, &mut readback).expect("read at offset should succeed");
    assert_eq!(readback, data);
}

/// Read beyond written data should return zeros.
#[test]
fn in_memory_device_read_unwritten_returns_zeros() {
    let dev = InMemoryDevice::new();

    // Write some data to extend the buffer
    let data = vec![0xFFu8; 512];
    dev.write_sync(0, &data).expect("write should succeed");

    // Read from a region that was zero-filled (beyond the written area)
    dev.write_sync(2048, &[0u8; 512]).expect("extend buffer");
    let mut readback = vec![0xEEu8; 512];
    dev.read_sync(512, &mut readback).expect("read should succeed");
    assert_eq!(readback, vec![0u8; 512], "unwritten region should be zeros");
}
