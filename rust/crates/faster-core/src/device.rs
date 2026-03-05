//! Storage device abstraction.
//!
//! The [`Device`] trait defines the interface for storage backends used by
//! the hybrid log. Implementations live in the `faster-device` crate.

use std::io;

/// A storage device for hybrid log pages.
///
/// This trait abstracts over physical storage (files, block devices,
/// in-memory buffers) so the hybrid log can be tested and deployed
/// with different backends.
pub trait Device: Send + Sync + 'static {
    /// Reads `len` bytes from offset `source` into `dest`.
    fn read_async(
        &self,
        source: u64,
        dest: &mut [u8],
        callback: impl FnOnce(io::Result<()>) + Send + 'static,
    );

    /// Writes `src` bytes to offset `dest`.
    fn write_async(
        &self,
        dest: u64,
        src: &[u8],
        callback: impl FnOnce(io::Result<()>) + Send + 'static,
    );

    /// Returns the sector size (minimum alignment for I/O).
    fn sector_size(&self) -> u32;
}

/// A null device that discards writes and returns zeros on read.
///
/// Useful for testing hash index operations without a real storage backend.
#[derive(Debug, Default, Clone)]
pub struct NullDevice;

impl Device for NullDevice {
    fn read_async(
        &self,
        _source: u64,
        dest: &mut [u8],
        callback: impl FnOnce(io::Result<()>) + Send + 'static,
    ) {
        dest.fill(0);
        callback(Ok(()));
    }

    fn write_async(
        &self,
        _dest: u64,
        _src: &[u8],
        callback: impl FnOnce(io::Result<()>) + Send + 'static,
    ) {
        callback(Ok(()));
    }

    fn sector_size(&self) -> u32 {
        512
    }
}
