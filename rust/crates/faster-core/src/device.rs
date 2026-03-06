//! Storage device abstraction for the FASTER hybrid log.
//!
//! The [`Device`] trait defines a completion-based I/O interface for storage
//! backends. Concrete implementations (file-backed, io_uring, etc.) live in
//! the `faster-device` crate; this module provides the trait plus two
//! in-process implementations useful for testing:
//!
//! - [`NullDevice`] — discards writes, returns zeros on read. Used for
//!   benchmarking and hash-index-only tests.
//! - [`InMemoryDevice`] — stores data in a `RwLock<Vec<u8>>`. Used for
//!   integration testing without touching the filesystem.

use std::io;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Callback & status types
// ---------------------------------------------------------------------------

/// Completion callback for async I/O operations.
///
/// Called by the device when a read or write completes.
///
/// # Safety
///
/// `context` must be a valid pointer to the context originally passed to
/// [`Device::read_async`] or [`Device::write_async`]. The callback is
/// responsible for interpreting and freeing the context.
pub type IoCompletionCallback =
    unsafe fn(context: *mut u8, status: IoStatus, bytes_transferred: u32);

/// Status code delivered to an [`IoCompletionCallback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoStatus {
    /// The operation completed successfully.
    Success,
    /// The operation failed with a platform error code.
    Error(i32),
    /// The operation was cancelled.
    Cancelled,
}

/// Result of submitting an I/O request to a [`Device`].
#[derive(Debug)]
pub enum IoRequestResult {
    /// I/O was submitted successfully. The callback will be invoked later.
    Submitted,
    /// I/O completed synchronously. The callback has already been invoked.
    CompletedSync,
    /// I/O queue is full. The caller should retry.
    QueueFull,
    /// Error during submission.
    Error(io::Error),
}

// ---------------------------------------------------------------------------
// TypedIoContext — safe wrapper for callback context lifecycle
// ---------------------------------------------------------------------------

/// Type-safe wrapper for I/O completion callback contexts.
///
/// Manages the `Box → raw pointer → typed reconstruct` lifecycle that all
/// device callback contexts follow:
///
/// 1. **Allocation**: [`TypedIoContext::new`] heap-allocates the context.
/// 2. **Submission**: [`as_raw()`](TypedIoContext::as_raw) provides the
///    `*mut u8` for [`Device::read_async`] / [`Device::write_async`].
/// 3. **Completion**: The callback calls [`TypedIoContext::from_raw`] to
///    reconstruct the `Box<T>` (still `unsafe` — this is the FFI boundary).
/// 4. **Error paths**: If the device rejects the I/O (QueueFull / Error),
///    [`reclaim()`](TypedIoContext::reclaim) safely recovers the allocation
///    without any `unsafe` at the call site.
///
/// # No `Drop` implementation
///
/// This type intentionally has **no** `Drop` impl. On the success path the
/// raw pointer's ownership transfers to the I/O subsystem — the completion
/// callback is responsible for freeing it. Dropping a `TypedIoContext` whose
/// I/O was successfully submitted is a safe no-op (the pointer is consumed
/// elsewhere).
pub struct TypedIoContext<T> {
    ptr: *mut T,
}

impl<T> TypedIoContext<T> {
    /// Heap-allocate an I/O context.
    pub fn new(data: T) -> Self {
        Self {
            ptr: Box::into_raw(Box::new(data)),
        }
    }

    /// Return the type-erased raw pointer for passing to [`Device`] methods.
    ///
    /// The pointer remains valid until either [`reclaim`](Self::reclaim) is
    /// called or the completion callback reconstructs it via [`from_raw`](Self::from_raw).
    pub fn as_raw(&self) -> *mut u8 {
        self.ptr as *mut u8
    }

    /// Safely reclaim the context on error paths.
    ///
    /// Use this when the I/O was **not** submitted (the device returned
    /// `QueueFull` or `Error`) and the completion callback will **not** fire.
    /// Consumes `self` and returns the owned data.
    pub fn reclaim(self) -> T {
        // SAFETY: `ptr` was created by `Box::into_raw` in `new()` and has not
        // been consumed by a callback — the caller guarantees the I/O was never
        // submitted.
        *unsafe { Box::from_raw(self.ptr) }
    }

    /// Reconstruct the `Box<T>` from a raw callback pointer.
    ///
    /// # Safety
    ///
    /// - `ptr` must have originated from [`TypedIoContext::<T>::as_raw`].
    /// - Must be called **exactly once** per context (double-free otherwise).
    pub unsafe fn from_raw(ptr: *mut u8) -> Box<T> {
        // SAFETY: Caller guarantees `ptr` is a valid `*mut T` produced by
        // `Box::into_raw`.
        unsafe { Box::from_raw(ptr as *mut T) }
    }
}

// ---------------------------------------------------------------------------
// Device trait
// ---------------------------------------------------------------------------

/// Abstract storage device for FASTER's hybrid log.
///
/// # Design Principles
///
/// - **Completion-based, not async/await** — callers supply a raw callback
///   rather than awaiting a `Future`.
/// - **Sector-aligned I/O** — all offsets and lengths must be multiples of
///   [`sector_size()`](Device::sector_size).
/// - **Segmented storage** — the device may map a single linear address space
///   onto multiple underlying files/segments.
///
/// # Thread Safety
///
/// All methods must be safe to call from any thread (`Send + Sync`).
pub trait Device: Send + Sync + 'static {
    /// Sector size for aligned I/O (typically 512 or 4096).
    fn sector_size(&self) -> u32;

    /// Segment size in bytes (e.g., 1 GiB).
    fn segment_size(&self) -> u64;

    /// Maximum number of concurrent I/O operations the device supports.
    fn max_outstanding_io(&self) -> u32;

    /// Issue an asynchronous read.
    ///
    /// # Safety
    ///
    /// - `dest` must be valid for writes of `len` bytes and must remain valid
    ///   until the callback fires.
    /// - `context` must remain valid until the callback fires.
    /// - `offset` and `len` must be multiples of [`sector_size()`](Device::sector_size).
    unsafe fn read_async(
        &self,
        offset: u64,
        dest: *mut u8,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult;

    /// Issue an asynchronous write.
    ///
    /// # Safety
    ///
    /// - `source` must be valid for reads of `len` bytes and must remain valid
    ///   until the callback fires.
    /// - `context` must remain valid until the callback fires.
    /// - `offset` and `len` must be multiples of [`sector_size()`](Device::sector_size).
    unsafe fn write_async(
        &self,
        source: *const u8,
        offset: u64,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult;

    /// Synchronous (blocking) read. Used during recovery.
    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32>;

    /// Synchronous (blocking) write. Used during checkpoint metadata writes.
    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32>;

    /// Truncate all data before the given offset.
    fn truncate_until(&self, offset: u64);

    /// Current logical size (highest written offset + length).
    fn size(&self) -> u64;

    /// Close the device and release resources.
    fn close(&self);
}

// ---------------------------------------------------------------------------
// NullDevice
// ---------------------------------------------------------------------------

/// A device that discards writes and returns zeros on read.
///
/// Used for benchmarking and hash-index-only tests where no actual I/O is
/// required. The only mutable state is an [`AtomicU64`] high-water mark
/// returned by [`size()`](Device::size).
#[derive(Debug)]
pub struct NullDevice {
    high_water: AtomicU64,
}

impl NullDevice {
    /// Create a new `NullDevice`.
    pub fn new() -> Self {
        Self {
            high_water: AtomicU64::new(0),
        }
    }
}

impl Default for NullDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl Device for NullDevice {
    fn sector_size(&self) -> u32 {
        512
    }

    fn segment_size(&self) -> u64 {
        1 << 30 // 1 GiB
    }

    fn max_outstanding_io(&self) -> u32 {
        u32::MAX
    }

    unsafe fn read_async(
        &self,
        _offset: u64,
        dest: *mut u8,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        // SAFETY: Caller guarantees `dest` is valid for `len` bytes.
        unsafe {
            core::ptr::write_bytes(dest, 0, len as usize);
        }
        // SAFETY: Caller guarantees `context` is valid.
        unsafe {
            callback(context, IoStatus::Success, len);
        }
        IoRequestResult::CompletedSync
    }

    unsafe fn write_async(
        &self,
        _source: *const u8,
        offset: u64,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        // Track the high-water mark.
        let end = offset + u64::from(len);
        self.high_water.fetch_max(end, Ordering::Relaxed);

        // SAFETY: Caller guarantees `context` is valid.
        unsafe {
            callback(context, IoStatus::Success, len);
        }
        IoRequestResult::CompletedSync
    }

    fn read_sync(&self, _offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        dest.fill(0);
        Ok(dest.len() as u32)
    }

    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32> {
        let end = offset + source.len() as u64;
        self.high_water.fetch_max(end, Ordering::Relaxed);
        Ok(source.len() as u32)
    }

    fn truncate_until(&self, _offset: u64) {
        // No-op: nothing to truncate.
    }

    fn size(&self) -> u64 {
        self.high_water.load(Ordering::Relaxed)
    }

    fn close(&self) {
        // No-op: nothing to release.
    }
}

// ---------------------------------------------------------------------------
// InMemoryDevice
// ---------------------------------------------------------------------------

/// A device backed by a `Vec<u8>` for integration testing without file I/O.
///
/// Data is stored contiguously in memory, protected by a [`RwLock`]. This is
/// not designed for production use — the lock is acceptable for test workloads.
#[derive(Debug)]
pub struct InMemoryDevice {
    data: RwLock<Vec<u8>>,
    sector: u32,
    segment: u64,
}

impl InMemoryDevice {
    /// Create a new, empty `InMemoryDevice`.
    pub fn new() -> Self {
        Self {
            data: RwLock::new(Vec::new()),
            sector: 512,
            segment: 1 << 30,
        }
    }

    /// Create an `InMemoryDevice` with custom sector and segment sizes.
    pub fn with_sizes(sector_size: u32, segment_size: u64) -> Self {
        Self {
            data: RwLock::new(Vec::new()),
            sector: sector_size,
            segment: segment_size,
        }
    }

    /// Ensure the internal buffer is large enough for `end` bytes, zero-filling
    /// any newly allocated region.
    fn ensure_capacity(buf: &mut Vec<u8>, end: usize) {
        if end > buf.len() {
            buf.resize(end, 0);
        }
    }
}

impl Default for InMemoryDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl Device for InMemoryDevice {
    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn segment_size(&self) -> u64 {
        self.segment
    }

    fn max_outstanding_io(&self) -> u32 {
        u32::MAX
    }

    unsafe fn read_async(
        &self,
        offset: u64,
        dest: *mut u8,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        let start = offset as usize;
        let end = start + len as usize;

        let guard = self.data.read().expect("InMemoryDevice lock poisoned");
        // SAFETY: Caller guarantees `dest` is valid for `len` bytes.
        unsafe {
            if end <= guard.len() {
                core::ptr::copy_nonoverlapping(guard[start..end].as_ptr(), dest, len as usize);
            } else {
                // Zero the whole region, then copy whatever is available.
                core::ptr::write_bytes(dest, 0, len as usize);
                if start < guard.len() {
                    let avail = guard.len() - start;
                    core::ptr::copy_nonoverlapping(guard[start..].as_ptr(), dest, avail);
                }
            }
        }
        drop(guard);

        // SAFETY: Caller guarantees `context` is valid.
        unsafe {
            callback(context, IoStatus::Success, len);
        }
        IoRequestResult::CompletedSync
    }

    unsafe fn write_async(
        &self,
        source: *const u8,
        offset: u64,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        let start = offset as usize;
        let end = start + len as usize;

        let mut guard = self.data.write().expect("InMemoryDevice lock poisoned");
        Self::ensure_capacity(&mut guard, end);

        // SAFETY: Caller guarantees `source` is valid for `len` bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(source, guard[start..end].as_mut_ptr(), len as usize);
        }
        drop(guard);

        // SAFETY: Caller guarantees `context` is valid.
        unsafe {
            callback(context, IoStatus::Success, len);
        }
        IoRequestResult::CompletedSync
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        let start = offset as usize;
        let end = start + dest.len();
        let guard = self.data.read().expect("InMemoryDevice lock poisoned");

        if end <= guard.len() {
            dest.copy_from_slice(&guard[start..end]);
        } else {
            dest.fill(0);
            if start < guard.len() {
                let avail = guard.len() - start;
                dest[..avail].copy_from_slice(&guard[start..]);
            }
        }
        Ok(dest.len() as u32)
    }

    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32> {
        let start = offset as usize;
        let end = start + source.len();
        let mut guard = self.data.write().expect("InMemoryDevice lock poisoned");
        Self::ensure_capacity(&mut guard, end);
        guard[start..end].copy_from_slice(source);
        Ok(source.len() as u32)
    }

    fn truncate_until(&self, offset: u64) {
        let mut guard = self.data.write().expect("InMemoryDevice lock poisoned");
        let off = offset as usize;
        if off <= guard.len() {
            // Zero out the truncated region.
            guard[..off].fill(0);
        }
    }

    fn size(&self) -> u64 {
        self.data
            .read()
            .expect("InMemoryDevice lock poisoned")
            .len() as u64
    }

    fn close(&self) {
        // No-op: Drop handles cleanup.
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    // -- NullDevice tests --------------------------------------------------

    #[test]
    fn null_device_read_returns_zeros() {
        let dev = NullDevice::new();
        let mut buf = vec![0xFFu8; 512];
        let n = dev.read_sync(0, &mut buf).unwrap();
        assert_eq!(n, 512);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn null_device_write_then_size() {
        let dev = NullDevice::new();
        assert_eq!(dev.size(), 0);
        let data = vec![1u8; 1024];
        dev.write_sync(4096, &data).unwrap();
        assert_eq!(dev.size(), 4096 + 1024);
    }

    #[test]
    fn null_device_async_read_callback_fires() {
        static FIRED: AtomicU32 = AtomicU32::new(0);

        unsafe fn on_complete(_ctx: *mut u8, status: IoStatus, bytes: u32) {
            assert_eq!(status, IoStatus::Success);
            assert_eq!(bytes, 512);
            FIRED.store(1, Ordering::SeqCst);
        }

        FIRED.store(0, Ordering::SeqCst);
        let dev = NullDevice::new();
        let mut buf = vec![0xFFu8; 512];

        // SAFETY: `buf` is valid for 512 bytes; null context is acceptable
        // because our callback ignores it.
        let result =
            unsafe { dev.read_async(0, buf.as_mut_ptr(), 512, on_complete, core::ptr::null_mut()) };
        assert!(matches!(result, IoRequestResult::CompletedSync));
        assert_eq!(FIRED.load(Ordering::SeqCst), 1);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn null_device_async_write_callback_fires() {
        static FIRED: AtomicU32 = AtomicU32::new(0);

        unsafe fn on_complete(_ctx: *mut u8, status: IoStatus, bytes: u32) {
            assert_eq!(status, IoStatus::Success);
            assert_eq!(bytes, 1024);
            FIRED.store(1, Ordering::SeqCst);
        }

        FIRED.store(0, Ordering::SeqCst);
        let dev = NullDevice::new();
        let data = vec![42u8; 1024];

        // SAFETY: `data` is valid for 1024 bytes; null context is acceptable.
        let result =
            unsafe { dev.write_async(data.as_ptr(), 0, 1024, on_complete, core::ptr::null_mut()) };
        assert!(matches!(result, IoRequestResult::CompletedSync));
        assert_eq!(FIRED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn null_device_sector_alignment() {
        let dev = NullDevice::new();
        assert_eq!(dev.sector_size(), 512);
        assert_eq!(dev.segment_size(), 1 << 30);
        assert_eq!(dev.max_outstanding_io(), u32::MAX);
    }

    // -- InMemoryDevice tests ----------------------------------------------

    #[test]
    fn in_memory_device_write_read_round_trip() {
        let dev = InMemoryDevice::new();
        let write_data: Vec<u8> = (0..=255).cycle().take(4096).collect();
        dev.write_sync(0, &write_data).unwrap();

        let mut read_buf = vec![0u8; 4096];
        dev.read_sync(0, &mut read_buf).unwrap();
        assert_eq!(read_buf, write_data);
    }

    #[test]
    fn in_memory_device_multiple_segments() {
        // Use a tiny segment size so we can test cross-segment writes easily.
        let dev = InMemoryDevice::with_sizes(512, 1024);

        // Write across segment boundary (offset 512, len 1024 spans segments 0 and 1).
        let data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        dev.write_sync(512, &data).unwrap();

        let mut read_buf = vec![0u8; 1024];
        dev.read_sync(512, &mut read_buf).unwrap();
        assert_eq!(read_buf, data);

        // Verify that bytes before offset 512 are still zero.
        let mut prefix = vec![0xFFu8; 512];
        dev.read_sync(0, &mut prefix).unwrap();
        assert!(prefix.iter().all(|&b| b == 0));
    }

    #[test]
    fn in_memory_device_truncate() {
        let dev = InMemoryDevice::new();
        let data = vec![0xABu8; 1024];
        dev.write_sync(0, &data).unwrap();

        // Truncate first 512 bytes.
        dev.truncate_until(512);

        let mut first_half = vec![0xFFu8; 512];
        dev.read_sync(0, &mut first_half).unwrap();
        assert!(
            first_half.iter().all(|&b| b == 0),
            "truncated region should be zeroed"
        );

        let mut second_half = vec![0u8; 512];
        dev.read_sync(512, &mut second_half).unwrap();
        assert!(
            second_half.iter().all(|&b| b == 0xAB),
            "un-truncated region should be intact"
        );
    }

    #[test]
    fn in_memory_device_async_round_trip() {
        static READ_OK: AtomicU32 = AtomicU32::new(0);

        unsafe fn on_read(_ctx: *mut u8, status: IoStatus, _bytes: u32) {
            assert_eq!(status, IoStatus::Success);
            READ_OK.store(1, Ordering::SeqCst);
        }

        unsafe fn on_write(_ctx: *mut u8, status: IoStatus, _bytes: u32) {
            assert_eq!(status, IoStatus::Success);
        }

        let dev = InMemoryDevice::new();
        let write_data = vec![0xCDu8; 512];

        // SAFETY: Valid buffers; null context acceptable for our callbacks.
        unsafe {
            dev.write_async(write_data.as_ptr(), 0, 512, on_write, core::ptr::null_mut());
        }

        let mut read_buf = vec![0u8; 512];
        READ_OK.store(0, Ordering::SeqCst);
        // SAFETY: Valid buffer; null context acceptable.
        unsafe {
            dev.read_async(
                0,
                read_buf.as_mut_ptr(),
                512,
                on_read,
                core::ptr::null_mut(),
            );
        }
        assert_eq!(READ_OK.load(Ordering::SeqCst), 1);
        assert!(read_buf.iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn in_memory_device_size_tracks_writes() {
        let dev = InMemoryDevice::new();
        assert_eq!(dev.size(), 0);
        dev.write_sync(1000, &[1u8; 24]).unwrap();
        assert_eq!(dev.size(), 1024);
    }

    #[test]
    fn in_memory_device_read_beyond_size_returns_zeros() {
        let dev = InMemoryDevice::new();
        let mut buf = vec![0xFFu8; 512];
        dev.read_sync(8192, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }
}
