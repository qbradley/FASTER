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

use crate::sync::{AtomicU64, Ordering, RwLock};
use std::io;
use std::mem::ManuallyDrop;

#[cfg(all(debug_assertions, not(loom)))]
type GenerationCounter = AtomicU64;

#[cfg(all(debug_assertions, loom))]
type GenerationCounter = AtomicU64;

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
// TypedIoContext — safe wrapper for callback context lifecycle (CORE-03)
// ---------------------------------------------------------------------------

/// Sentinel stamped into every I/O context envelope at allocation time.
///
/// On consumption via [`TypedIoContext::from_raw`] or [`TypedIoContext::reclaim`],
/// this is atomically swapped to [`IO_CONTEXT_SENTINEL_CONSUMED`]. A mismatch
/// causes a panic *before* `Box::from_raw` can corrupt the heap.
const IO_CONTEXT_SENTINEL_ALIVE: u64 = 0x4641_5354_4552_494F; // "FASTERIO" ASCII

/// Sentinel written after successful consumption (aids post-mortem debugging).
const IO_CONTEXT_SENTINEL_CONSUMED: u64 = 0xDEAD_DEAD_DEAD_DEAD;

/// Global generation counter for `TypedIoContext` instances (debug builds only).
///
/// Each context is tagged with a unique generation number at allocation time.
/// On reconstruction, the generation is validated for richer diagnostics.
#[cfg(all(debug_assertions, not(loom)))]
static IO_CONTEXT_GENERATION: GenerationCounter = GenerationCounter::new(0);

// loom atomics aren't const-constructible — use LazyLock for static initialization.
#[cfg(all(debug_assertions, loom))]
static IO_CONTEXT_GENERATION: std::sync::LazyLock<GenerationCounter> =
    std::sync::LazyLock::new(|| GenerationCounter::new(0));

/// Type-safe wrapper for I/O completion callback contexts.
///
/// Manages the `Box → raw pointer → typed reconstruct` lifecycle that all
/// device callback contexts follow:
///
/// 1. **Allocation**: [`TypedIoContext::new`] heap-allocates the context inside
///    an [`IoContextEnvelope`] with an atomic sentinel.
/// 2. **Submission**: [`as_raw()`](TypedIoContext::as_raw) provides the
///    `*mut u8` for [`Device::read_async`] / [`Device::write_async`].
/// 3. **Completion**: The callback calls [`TypedIoContext::from_raw`] to
///    reconstruct the `Box<T>` (still `unsafe` — this is the FFI boundary).
///    The sentinel is atomically swapped; a second call panics.
/// 4. **Error paths**: If the device rejects the I/O (QueueFull / Error),
///    [`reclaim()`](TypedIoContext::reclaim) safely recovers the allocation
///    without any `unsafe` at the call site.
///
/// # Double-Free Protection (P2-A)
///
/// In **all builds** (debug and release), the context is wrapped in an
/// [`IoContextEnvelope`] carrying an atomic sentinel. Both [`from_raw`] and
/// [`reclaim`] atomically swap the sentinel from `ALIVE` → `CONSUMED`.
/// If the swap observes any value other than `ALIVE`, the call panics
/// *before* `Box::from_raw` can cause heap corruption. This catches:
///
/// - **Concurrent double-callback**: two threads racing to consume the same
///   context — only one will observe `ALIVE`.
/// - **Sequential double-free**: a device bug invoking the callback twice —
///   the second call sees `CONSUMED` and panics.
///
/// # Lifetime Contract
///
/// The `context` pointer passed to [`Device::read_async`] / [`Device::write_async`]
/// **must** remain valid until the completion callback fires. In debug builds,
/// a generation counter provides additional diagnostics.
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
    /// Generation tag stamped at allocation time (debug builds only).
    #[cfg(debug_assertions)]
    generation: u64,
}

/// Envelope wrapping every I/O context for double-free detection.
///
/// Present in **all** builds (not just debug). The `sentinel` field is
/// atomically swapped from [`IO_CONTEXT_SENTINEL_ALIVE`] to
/// [`IO_CONTEXT_SENTINEL_CONSUMED`] on reconstruction. A mismatch panics
/// before `Box::from_raw` can cause heap corruption.
///
/// In debug builds, an additional `generation` counter cross-checks against
/// a global monotonic counter for richer use-after-free diagnostics.
struct IoContextEnvelope<T> {
    sentinel: AtomicU64,
    #[cfg(debug_assertions)]
    generation: u64,
    data: ManuallyDrop<T>,
}

impl<T> TypedIoContext<T> {
    /// Heap-allocate an I/O context.
    ///
    /// The context is wrapped in an [`IoContextEnvelope`] with an atomic
    /// sentinel for double-free detection. In debug builds, a monotonic
    /// generation counter is also stamped for richer diagnostics.
    pub fn new(data: T) -> Self {
        #[cfg(debug_assertions)]
        let gen_id = IO_CONTEXT_GENERATION.fetch_add(1, Ordering::Relaxed);

        let envelope = IoContextEnvelope {
            sentinel: AtomicU64::new(IO_CONTEXT_SENTINEL_ALIVE),
            #[cfg(debug_assertions)]
            generation: gen_id,
            data: ManuallyDrop::new(data),
        };
        Self {
            ptr: Box::into_raw(Box::new(envelope)) as *mut T,
            #[cfg(debug_assertions)]
            generation: gen_id,
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
    ///
    /// # Panics
    ///
    /// Panics if the sentinel has already been consumed (indicates a logic
    /// error where the callback fired despite the I/O being rejected).
    pub fn reclaim(self) -> T {
        // SAFETY: `ptr` was created by `Box::into_raw` in `new()` and points
        // to an `IoContextEnvelope<T>`.
        let mut envelope = unsafe { Box::from_raw(self.ptr as *mut IoContextEnvelope<T>) };

        let old = envelope
            .sentinel
            .swap(IO_CONTEXT_SENTINEL_CONSUMED, Ordering::AcqRel);
        assert_eq!(
            old, IO_CONTEXT_SENTINEL_ALIVE,
            "TypedIoContext::reclaim: context already consumed or corrupted \
             (sentinel was {old:#018x}, expected {IO_CONTEXT_SENTINEL_ALIVE:#018x})"
        );

        #[cfg(debug_assertions)]
        debug_assert_eq!(
            envelope.generation, self.generation,
            "TypedIoContext generation mismatch on reclaim"
        );

        // SAFETY: `data` was initialised in `new()` and the sentinel confirms
        // this is the first (and only) consumption.
        unsafe { ManuallyDrop::take(&mut envelope.data) }
        // `envelope` drops here: AtomicU64 + u64 are trivial; ManuallyDrop
        // suppresses T's destructor (data was already moved out).
    }

    /// Reconstruct the `Box<T>` from a raw callback pointer.
    ///
    /// # Safety
    ///
    /// - `ptr` must have originated from [`TypedIoContext::<T>::as_raw`].
    /// - Must be called **exactly once** per context (double-free otherwise).
    /// - The context must not have been reclaimed via [`reclaim`](Self::reclaim).
    ///
    /// # Panics
    ///
    /// Panics if the atomic sentinel has already been consumed, preventing
    /// heap corruption from a double-free.
    pub unsafe fn from_raw(ptr: *mut u8) -> Box<T> {
        let envelope_ptr = ptr as *mut IoContextEnvelope<T>;

        // SAFETY: The caller guarantees `ptr` originated from `as_raw()`,
        // so `envelope_ptr` points to a live `IoContextEnvelope<T>`.
        // The atomic swap ensures exactly one caller observes ALIVE.
        let old = unsafe { &(*envelope_ptr).sentinel }
            .swap(IO_CONTEXT_SENTINEL_CONSUMED, Ordering::AcqRel);
        assert_eq!(
            old, IO_CONTEXT_SENTINEL_ALIVE,
            "TypedIoContext::from_raw: double-free or corrupted I/O context \
             (sentinel was {old:#018x}, expected {IO_CONTEXT_SENTINEL_ALIVE:#018x})"
        );

        #[cfg(debug_assertions)]
        {
            // SAFETY: envelope is still live (we haven't freed it yet).
            let gen_id = unsafe { (*envelope_ptr).generation };
            debug_assert!(
                gen_id <= IO_CONTEXT_GENERATION.load(Ordering::Relaxed),
                "TypedIoContext: corrupted context pointer (generation overflow)"
            );
        }

        // Take ownership of the envelope and extract the inner data.
        // SAFETY: `envelope_ptr` was created by `Box::into_raw(Box::new(…))`
        // in `new()`, and the sentinel confirms this is the first consumption.
        let mut envelope = unsafe { Box::from_raw(envelope_ptr) };
        // SAFETY: `data` was initialised in `new()` and the sentinel confirms
        // this is the first (and only) consumption — no double-take.
        let data = unsafe { ManuallyDrop::take(&mut envelope.data) };
        // `envelope` drops: AtomicU64 + u64 are trivial; ManuallyDrop is inert.
        drop(envelope);

        Box::new(data)
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
/// # Lifetime Contract (CORE-03)
///
/// The `context` pointer passed to [`read_async`](Device::read_async) and
/// [`write_async`](Device::write_async) **must remain valid** until the
/// completion callback fires. Buffer pointers must also remain valid.
/// See [`TypedIoContext`] documentation for details.
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

    /// Poll for completed I/O operations without blocking.
    ///
    /// Returns the number of completions processed. Implementations that
    /// run callbacks synchronously ([`NullDevice`], [`InMemoryDevice`])
    /// return 0. Implementations with background worker threads
    /// ([`SyncFileDevice`]) yield the current thread to give I/O workers
    /// CPU time. io_uring-based devices can drain their completion queue.
    ///
    /// The default returns 0, which is correct for **synchronous** device
    /// implementations where I/O callbacks fire inline during the write
    /// call. Asynchronous device implementations **must** override this
    /// method; otherwise, I/O completions will silently be lost.
    ///
    /// This method is called from [`maintenance()`](crate::store::FasterKv::maintenance)
    /// and from the writer-side retry loop to help drain the flush pipeline
    /// when the buffer is under pressure.
    fn poll_completions(&self) -> u32 {
        0
    }
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

    fn truncate_until(&self, _offset: u64) {
        // No-op: data below begin_address is never read in lossy mode,
        // so zeroing is unnecessary. The previous implementation held the
        // write lock while zeroing guard[..offset], which is O(total_flushed)
        // and grows linearly over time. Under sustained load, this eventually
        // blocks all concurrent write_async (flush) calls for seconds,
        // starving the flush pipeline and causing a permanent throughput
        // collapse. See: lossy-eviction deadlock fix.
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

        // Truncate first 512 bytes. For InMemoryDevice, truncate_until is a
        // no-op (data below begin_address is never read in normal operation).
        // Verify it doesn't panic and the un-truncated region is intact.
        dev.truncate_until(512);

        let mut second_half = vec![0u8; 512];
        dev.read_sync(512, &mut second_half).unwrap();
        assert!(
            second_half.iter().all(|&b| b == 0xAB),
            "un-truncated region should be intact"
        );

        // Size should be unchanged.
        assert_eq!(dev.size(), 1024);
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

    // -- TypedIoContext double-free guard tests --------------------------------

    #[test]
    fn typed_io_context_from_raw_round_trip() {
        let ctx = TypedIoContext::new(42u64);
        let raw = ctx.as_raw();
        // SAFETY: `raw` was obtained from `ctx.as_raw()` on the line above and
        // has not been consumed yet, so it points to a valid IoContextEnvelope.
        let boxed = unsafe { TypedIoContext::<u64>::from_raw(raw) };
        assert_eq!(*boxed, 42);
    }

    #[test]
    fn typed_io_context_reclaim_round_trip() {
        let ctx = TypedIoContext::new("hello".to_string());
        let data = ctx.reclaim();
        assert_eq!(data, "hello");
    }

    #[test]
    #[should_panic(expected = "double-free or corrupted I/O context")]
    fn typed_io_context_double_from_raw_panics() {
        // Construct an envelope with an already-consumed sentinel to
        // test the guard without accessing freed memory.
        use std::mem::ManuallyDrop;
        let envelope = IoContextEnvelope {
            sentinel: AtomicU64::new(IO_CONTEXT_SENTINEL_CONSUMED),
            #[cfg(debug_assertions)]
            generation: 0,
            data: ManuallyDrop::new(42u64),
        };
        let ptr = Box::into_raw(Box::new(envelope)) as *mut u8;
        // The sentinel is CONSUMED — from_raw must panic.
        // SAFETY: `ptr` points to a valid IoContextEnvelope allocated via
        // `Box::into_raw` above; the consumed sentinel triggers the expected panic.
        let _ = unsafe { TypedIoContext::<u64>::from_raw(ptr) };
    }

    #[test]
    #[should_panic(expected = "context already consumed or corrupted")]
    fn typed_io_context_reclaim_after_from_raw_panics() {
        // Construct an envelope with an already-consumed sentinel.
        use std::mem::ManuallyDrop;
        let envelope = IoContextEnvelope {
            sentinel: AtomicU64::new(IO_CONTEXT_SENTINEL_CONSUMED),
            #[cfg(debug_assertions)]
            generation: 0,
            data: ManuallyDrop::new(99u64),
        };
        let raw = Box::into_raw(Box::new(envelope));
        let fake_ctx = TypedIoContext::<u64> {
            ptr: raw as *mut u64,
            #[cfg(debug_assertions)]
            generation: 0,
        };
        // Sentinel is CONSUMED — reclaim must panic.
        let _ = fake_ctx.reclaim();
    }

    #[test]
    fn typed_io_context_from_raw_with_drop_type() {
        let ctx = TypedIoContext::new(vec![1u32, 2, 3]);
        let raw = ctx.as_raw();
        // SAFETY: `raw` was obtained from `ctx.as_raw()` on the line above and
        // has not been consumed yet, so it points to a valid IoContextEnvelope.
        let boxed = unsafe { TypedIoContext::<Vec<u32>>::from_raw(raw) };
        assert_eq!(*boxed, vec![1, 2, 3]);
        // Vec drops correctly — no leak or double-free.
    }
}
