//! Pending I/O completion manager for FASTER sessions.
//!
//! **⚠ Pre-alpha scaffolding (SF-13):** This subsystem is structurally
//! complete but not yet integrated into `FasterKv`. The types are exported
//! to support incremental development. Do not rely on the public API —
//! it will change when full async I/O integration lands (see O-2).
//!
//! When a CRUD operation finds a record in the on-disk region, it cannot
//! complete synchronously. The operation is captured as a [`PendingOperation`]
//! and handed to the [`PendingIoManager`], which issues a disk read and tracks
//! its completion so the operation can be retried with the data in memory.
//!
//! # Workflow
//!
//! 1. An operation returns [`OperationStatus::Pending`] and enqueues a
//!    [`PendingOperation`].
//! 2. The session calls [`PendingIoManager::issue_read`] (async) or
//!    [`issue_read_sync`](PendingIoManager::issue_read_sync) (blocking).
//! 3. For the async path, the caller polls [`PendingIoContext::is_completed`]
//!    or calls [`try_complete`](PendingIoContext::try_complete) to obtain a
//!    [`CompletedIo`].
//! 4. The [`CompletedIo`] contains the data buffer and record offset, allowing
//!    the caller to extract the key/value and retry the operation.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::buffer_pool::{AlignedBuffer, BufferPool};
use crate::device::{Device, IoCompletionCallback, IoRequestResult, IoStatus, TypedIoContext};
use crate::record::{RecordInfo, RecordLayout, read_key, read_record_info, read_value};
use crate::store::Functions;

use super::session::PendingOperation;

/// Default read chunk size (8 KiB). Covers most records without reading an
/// entire 32 MiB page.
const DEFAULT_READ_CHUNK: u32 = 8192;

/// Default timeout for pending I/O operations (30 seconds).
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Global counter for assigning unique context IDs.
static NEXT_IO_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Returns a unique context ID for a new pending I/O operation.
fn next_context_id() -> u64 {
    NEXT_IO_CONTEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Configuration for pending I/O behavior.
#[derive(Debug, Clone)]
pub struct PendingIoConfig {
    /// How long to wait before considering a pending I/O timed out.
    pub default_timeout: Duration,
    /// Maximum number of retry attempts for a failed I/O.
    pub max_retries: u32,
    /// Whether to cancel pending I/O when the session is dropped.
    pub cancel_on_drop: bool,
}

impl Default for PendingIoConfig {
    fn default() -> Self {
        Self {
            default_timeout: DEFAULT_IO_TIMEOUT,
            max_retries: 3,
            cancel_on_drop: true,
        }
    }
}

// ── align_to_sector ─────────────────────────────────────────────────

/// Round `value` up to the next multiple of `sector_size`.
///
/// `sector_size` must be a power of two.
#[inline]
fn align_to_sector(value: u32, sector_size: u32) -> u32 {
    debug_assert!(
        sector_size.is_power_of_two(),
        "sector_size must be a power of two"
    );
    (value + sector_size - 1) & !(sector_size - 1)
}

// ── PendingIoError ──────────────────────────────────────────────────

/// Errors that can occur during a pending I/O read.
#[derive(Debug)]
pub enum PendingIoError {
    /// Device read failed with an I/O error.
    IoError(std::io::Error),
    /// Device returned a non-success status.
    DeviceError(IoStatus),
    /// The read-back record data was invalid.
    InvalidRecord,
    /// The pending I/O operation timed out.
    TimedOut {
        /// ID of the context that timed out.
        context_id: u64,
        /// How long the operation had been running.
        elapsed: Duration,
    },
    /// The pending I/O operation was cancelled.
    Cancelled {
        /// ID of the context that was cancelled.
        context_id: u64,
    },
    /// The pending I/O operation failed with a specific device error.
    IoFailed {
        /// ID of the context that failed.
        context_id: u64,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

impl std::fmt::Display for PendingIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingIoError::IoError(e) => write!(f, "I/O error: {e}"),
            PendingIoError::DeviceError(s) => write!(f, "device error: {s:?}"),
            PendingIoError::InvalidRecord => write!(f, "invalid record data"),
            PendingIoError::TimedOut {
                context_id,
                elapsed,
            } => {
                write!(f, "pending I/O {context_id} timed out after {elapsed:?}")
            }
            PendingIoError::Cancelled { context_id } => {
                write!(f, "pending I/O {context_id} cancelled")
            }
            PendingIoError::IoFailed { context_id, source } => {
                write!(f, "pending I/O {context_id} failed: {source}")
            }
        }
    }
}

impl std::error::Error for PendingIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PendingIoError::IoError(e) => Some(e),
            PendingIoError::IoFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

// ── ReadCallbackContext ─────────────────────────────────────────────

/// Context passed through the device's completion callback.
///
/// The `Arc` fields are shared with the [`PendingIoContext`] so the callback
/// (which runs on the device's I/O thread) can signal completion.
struct ReadCallbackContext {
    completed: Arc<AtomicBool>,
    io_status: Arc<Mutex<Option<IoStatus>>>,
    bytes_transferred: Arc<AtomicU32>,
}

/// Completion callback invoked by the device when an async read finishes.
///
/// # Safety
///
/// `context` must be a valid pointer to a `ReadCallbackContext` that was
/// created via [`TypedIoContext::new`]. This function takes ownership of the
/// box and drops it after updating the shared state.
unsafe fn read_completion_callback(context: *mut u8, status: IoStatus, bytes: u32) {
    // SAFETY: The caller (issue_read) created this pointer via
    // `TypedIoContext::new`. We are the sole consumer, so reconstructing
    // the Box is safe.
    let ctx = unsafe { TypedIoContext::<ReadCallbackContext>::from_raw(context) };
    *ctx.io_status.lock().expect("io_status mutex poisoned") = Some(status);
    ctx.bytes_transferred.store(bytes, Ordering::Release);
    ctx.completed.store(true, Ordering::Release);
    // Box drops here, freeing the context allocation.
}

// ── CompletedIo ─────────────────────────────────────────────────────

/// A completed disk read with the data buffer and original operation.
///
/// After the device read finishes, this struct holds everything needed to
/// retry the original operation: the raw data buffer, the offset of the
/// record within that buffer, and the original [`PendingOperation`].
pub struct CompletedIo<F: Functions> {
    /// The original pending operation to retry.
    pub operation: PendingOperation<F>,
    /// Buffer containing the read-back page/sector data.
    pub buffer: AlignedBuffer,
    /// Byte offset within `buffer` where the record starts.
    pub record_offset: usize,
    /// I/O completion status.
    pub status: IoStatus,
}

impl<F: Functions> std::fmt::Debug for CompletedIo<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletedIo")
            .field("record_offset", &self.record_offset)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl<F: Functions> CompletedIo<F> {
    /// Read the [`RecordInfo`] header from the completed I/O buffer.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is too short to contain a record header at
    /// `record_offset`.
    pub fn record_info(&self) -> RecordInfo {
        read_record_info(&self.buffer.as_slice()[self.record_offset..])
    }

    /// Read the key from the completed I/O buffer using the given layout.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is too short for key deserialization.
    pub fn read_key(&self, layout: &RecordLayout) -> F::Key {
        read_key::<F::Key>(&self.buffer.as_slice()[self.record_offset..], layout)
    }

    /// Read the value from the completed I/O buffer using the given layout.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is too short for value deserialization.
    pub fn read_value(&self, layout: &RecordLayout) -> F::Value {
        read_value::<F::Value>(&self.buffer.as_slice()[self.record_offset..], layout)
    }
}

// ── PendingIoContext ────────────────────────────────────────────────

/// Context for tracking a single pending async I/O operation.
///
/// Owns the I/O buffer and shares completion state with the device callback
/// via `Arc`-wrapped atomics. The caller polls [`is_completed`](Self::is_completed)
/// or consumes the context with [`try_complete`](Self::try_complete).
pub struct PendingIoContext<F: Functions> {
    /// Unique identifier for this pending I/O context.
    id: u64,
    /// When this context was created.
    created_at: Instant,
    /// Timeout duration for this I/O operation.
    timeout: Duration,
    /// The original operation to retry after the read completes.
    operation: PendingOperation<F>,
    /// The I/O buffer (owned until completion, then moved into `CompletedIo`).
    buffer: Option<AlignedBuffer>,
    /// Byte offset within the buffer where the target record starts.
    record_offset: usize,
    /// Completion flag, set by the device callback.
    completed: Arc<AtomicBool>,
    /// I/O status written by the callback.
    io_status: Arc<Mutex<Option<IoStatus>>>,
    /// Bytes transferred, written by the callback.
    bytes_transferred: Arc<AtomicU32>,
}

impl<F: Functions> std::fmt::Debug for PendingIoContext<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingIoContext")
            .field("id", &self.id)
            .field("record_offset", &self.record_offset)
            .field("completed", &self.completed.load(Ordering::Relaxed))
            .field("expired", &self.is_expired())
            .finish_non_exhaustive()
    }
}

impl<F: Functions> PendingIoContext<F> {
    /// Create a `PendingIoContext` for testing purposes.
    ///
    /// Allows tests to construct contexts with controlled completion state
    /// without issuing real device I/O.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        operation: PendingOperation<F>,
        buffer: AlignedBuffer,
        record_offset: usize,
        completed: Arc<AtomicBool>,
        io_status: Arc<Mutex<Option<IoStatus>>>,
        bytes_transferred: Arc<AtomicU32>,
    ) -> Self {
        Self {
            id: next_context_id(),
            created_at: Instant::now(),
            timeout: DEFAULT_IO_TIMEOUT,
            operation,
            buffer: Some(buffer),
            record_offset,
            completed,
            io_status,
            bytes_transferred,
        }
    }

    /// Create a `PendingIoContext` for testing with a custom timeout.
    #[cfg(test)]
    pub(crate) fn new_for_test_with_timeout(
        operation: PendingOperation<F>,
        buffer: AlignedBuffer,
        record_offset: usize,
        completed: Arc<AtomicBool>,
        io_status: Arc<Mutex<Option<IoStatus>>>,
        bytes_transferred: Arc<AtomicU32>,
        timeout: Duration,
        created_at: Instant,
    ) -> Self {
        Self {
            id: next_context_id(),
            created_at,
            timeout,
            operation,
            buffer: Some(buffer),
            record_offset,
            completed,
            io_status,
            bytes_transferred,
        }
    }

    /// Returns the unique ID of this context.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the instant when this context was created.
    pub fn created_at(&self) -> Instant {
        self.created_at
    }

    /// Returns the timeout duration for this I/O operation.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns how long this I/O has been pending.
    pub fn elapsed(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Returns `true` if this I/O operation has exceeded its timeout.
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= self.timeout
    }

    /// Check whether the I/O has completed.
    pub fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    /// Returns the number of bytes transferred (valid after completion).
    pub fn bytes_transferred(&self) -> u32 {
        self.bytes_transferred.load(Ordering::Acquire)
    }

    /// Try to consume this context and produce a [`CompletedIo`].
    ///
    /// Returns `Ok(CompletedIo)` if the I/O is done, or `Err(self)` if it
    /// has not yet completed (allowing the caller to retry later).
    #[allow(clippy::result_large_err)]
    pub fn try_complete(mut self) -> Result<CompletedIo<F>, Self> {
        if !self.is_completed() {
            return Err(self);
        }

        let status = self
            .io_status
            .lock()
            .expect("io_status mutex poisoned")
            .take()
            .unwrap_or(IoStatus::Success);

        Ok(CompletedIo {
            operation: self.operation,
            buffer: self
                .buffer
                .take()
                .expect("buffer already consumed — try_complete called twice?"),
            record_offset: self.record_offset,
            status,
        })
    }
}

// ── PendingIoManager ────────────────────────────────────────────────

/// Manages pending disk read operations for FASTER sessions.
///
/// When a CRUD operation hits the on-disk region, the operation is deferred
/// as a [`PendingOperation`]. This manager issues disk reads and tracks their
/// completion so the operations can be retried.
pub struct PendingIoManager {
    /// Buffer pool for allocating sector-aligned I/O buffers.
    buffer_pool: BufferPool,
    /// Page size in bytes (used for computing device offsets).
    page_size: u32,
    /// Sector size in bytes (used for alignment).
    sector_size: u32,
    /// Configuration for pending I/O behavior.
    config: PendingIoConfig,
}

impl PendingIoManager {
    /// Create a new `PendingIoManager`.
    ///
    /// # Arguments
    ///
    /// * `page_size` — the hybrid log page size in bytes (e.g. 32 MiB).
    /// * `sector_size` — the device sector size (e.g. 512 or 4096).
    pub fn new(page_size: u32, sector_size: u32) -> Self {
        Self::with_config(page_size, sector_size, PendingIoConfig::default())
    }

    /// Create a `PendingIoManager` with custom configuration.
    pub fn with_config(page_size: u32, sector_size: u32, config: PendingIoConfig) -> Self {
        Self {
            buffer_pool: BufferPool::new(sector_size as usize, 16),
            page_size,
            sector_size,
            config,
        }
    }

    /// Returns the current configuration.
    pub fn config(&self) -> &PendingIoConfig {
        &self.config
    }

    /// Issue an asynchronous disk read for a pending operation.
    ///
    /// Reads a chunk of the page containing the record from the device.
    /// Returns a [`PendingIoContext`] that can be polled for completion.
    ///
    /// The read covers a sector-aligned region starting at (or before) the
    /// record's offset within the page, large enough to contain most records
    /// (currently [`DEFAULT_READ_CHUNK`] bytes, aligned to sector boundaries).
    pub fn issue_read<F: Functions>(
        &self,
        pending_op: PendingOperation<F>,
        device: &dyn Device,
    ) -> Result<PendingIoContext<F>, PendingIoError> {
        let addr = pending_op.address;
        let page = addr.page();
        let offset_in_page = addr.offset().0;

        // Align the start down to a sector boundary.
        let sector_start = (offset_in_page / self.sector_size) * self.sector_size;

        // Record offset within the read buffer.
        let record_offset = (offset_in_page - sector_start) as usize;

        // Read enough to cover the record, at least DEFAULT_READ_CHUNK.
        let min_read = offset_in_page - sector_start + DEFAULT_READ_CHUNK;
        let read_size = align_to_sector(min_read, self.sector_size);

        // Device offset: page_base + sector_start.
        let device_offset = page.0 as u64 * self.page_size as u64 + sector_start as u64;

        // Allocate a sector-aligned buffer.
        let mut buffer = self.buffer_pool.acquire(read_size as usize);

        // Set up shared completion tracking state.
        let completed = Arc::new(AtomicBool::new(false));
        let io_status = Arc::new(Mutex::new(None));
        let bytes_transferred = Arc::new(AtomicU32::new(0));

        // Heap-allocate the callback context via TypedIoContext.
        let io_ctx = TypedIoContext::new(ReadCallbackContext {
            completed: Arc::clone(&completed),
            io_status: Arc::clone(&io_status),
            bytes_transferred: Arc::clone(&bytes_transferred),
        });

        // SAFETY:
        // - `buffer.as_mut_ptr()` is valid for `read_size` bytes (allocated above).
        // - The buffer is owned by this function and moved into PendingIoContext,
        //   keeping it alive until the callback fires.
        // - `io_ctx.as_raw()` is a valid heap pointer; ownership transfers to the
        //   callback on success, or is reclaimed on error.
        let result = unsafe {
            device.read_async(
                device_offset,
                buffer.as_mut_ptr(),
                read_size,
                read_completion_callback as IoCompletionCallback,
                io_ctx.as_raw(),
            )
        };

        match result {
            IoRequestResult::Submitted | IoRequestResult::CompletedSync => Ok(PendingIoContext {
                id: next_context_id(),
                created_at: Instant::now(),
                timeout: self.config.default_timeout,
                operation: pending_op,
                buffer: Some(buffer),
                record_offset,
                completed,
                io_status,
                bytes_transferred,
            }),
            IoRequestResult::QueueFull => {
                // I/O was never submitted — safely reclaim the context.
                let _ = io_ctx.reclaim();
                Err(PendingIoError::IoError(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "device I/O queue full",
                )))
            }
            IoRequestResult::Error(e) => {
                // I/O was never submitted — safely reclaim the context.
                let _ = io_ctx.reclaim();
                Err(PendingIoError::IoError(e))
            }
        }
    }

    /// Issue a synchronous (blocking) disk read for a pending operation.
    ///
    /// Reads the sector-aligned chunk containing the record and returns a
    /// [`CompletedIo`] ready for retry, or a [`PendingIoError`] on failure.
    pub fn issue_read_sync<F: Functions>(
        &self,
        pending_op: PendingOperation<F>,
        device: &dyn Device,
    ) -> Result<CompletedIo<F>, PendingIoError> {
        let addr = pending_op.address;
        let page = addr.page();
        let offset_in_page = addr.offset().0;

        let sector_start = (offset_in_page / self.sector_size) * self.sector_size;
        let record_offset = (offset_in_page - sector_start) as usize;

        let min_read = offset_in_page - sector_start + DEFAULT_READ_CHUNK;
        let read_size = align_to_sector(min_read, self.sector_size);

        let device_offset = page.0 as u64 * self.page_size as u64 + sector_start as u64;

        let mut buffer = self.buffer_pool.acquire(read_size as usize);

        let bytes = device
            .read_sync(
                device_offset,
                &mut buffer.as_mut_slice()[..read_size as usize],
            )
            .map_err(PendingIoError::IoError)?;

        if bytes == 0 {
            return Err(PendingIoError::InvalidRecord);
        }

        Ok(CompletedIo {
            operation: pending_op,
            buffer,
            record_offset,
            status: IoStatus::Success,
        })
    }

    /// Return a buffer to the pool for reuse.
    pub fn return_buffer(&self, buffer: AlignedBuffer) {
        self.buffer_pool.release(buffer);
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};
    use crate::device::InMemoryDevice;
    use crate::hash::KeyHash;
    use crate::record::{RecordInfo, RecordLayout, write_record};
    use crate::store::SimpleFunctions;
    use crate::store::session::{PendingOpType, PendingOperation};

    type TestFunctions = SimpleFunctions<u64, u64>;

    /// Helper: create a `PendingOperation` for testing.
    fn make_pending_op(page: u32, offset: u32) -> PendingOperation<TestFunctions> {
        PendingOperation {
            op_type: PendingOpType::Read,
            key: 42u64,
            input: Some(0u64),
            context: (),
            address: LogicalAddress::new(Page(page), Offset(offset)),
            record_layout: RecordLayout::compute(8, 8),
            key_hash: KeyHash::new(0xDEAD),
        }
    }

    /// Helper: write a u64/u64 record into the device at the given device offset.
    fn write_test_record(device: &InMemoryDevice, device_offset: u64, key: u64, value: u64) {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);
        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);
        device.write_sync(device_offset, &buf).unwrap();
    }

    // ── align_to_sector ────────────────────────────────────────────

    #[test]
    fn align_to_sector_already_aligned() {
        assert_eq!(align_to_sector(512, 512), 512);
        assert_eq!(align_to_sector(4096, 512), 4096);
        assert_eq!(align_to_sector(0, 512), 0);
    }

    #[test]
    fn align_to_sector_rounds_up() {
        assert_eq!(align_to_sector(1, 512), 512);
        assert_eq!(align_to_sector(513, 512), 1024);
        assert_eq!(align_to_sector(4097, 4096), 8192);
    }

    // ── Synchronous read ───────────────────────────────────────────

    #[test]
    fn issue_sync_read_with_in_memory_device() {
        let page_size: u32 = 32 * 1024; // 32 KiB for testing
        let sector_size: u32 = 512;
        let device = InMemoryDevice::with_sizes(sector_size, 1 << 30);
        let manager = PendingIoManager::new(page_size, sector_size);

        // Place a record at page 1, offset 0.
        let device_offset = page_size as u64;
        write_test_record(&device, device_offset, 42, 999);

        let pending = make_pending_op(1, 0);
        let completed = manager.issue_read_sync(pending, &device).unwrap();

        assert_eq!(completed.status, IoStatus::Success);
        assert_eq!(completed.record_offset, 0);

        let layout = RecordLayout::compute(8, 8);
        assert_eq!(completed.read_key(&layout), 42u64);
        assert_eq!(completed.read_value(&layout), 999u64);

        // Return the buffer.
        manager.return_buffer(completed.buffer);
    }

    // ── Asynchronous read ──────────────────────────────────────────

    #[test]
    fn issue_async_read_with_in_memory_device() {
        let page_size: u32 = 32 * 1024;
        let sector_size: u32 = 512;
        let device = InMemoryDevice::with_sizes(sector_size, 1 << 30);
        let manager = PendingIoManager::new(page_size, sector_size);

        let device_offset = 2u64 * page_size as u64;
        write_test_record(&device, device_offset, 7, 77);

        let pending = make_pending_op(2, 0);
        let ctx = manager.issue_read(pending, &device).unwrap();

        // InMemoryDevice completes synchronously, so it should be done.
        assert!(ctx.is_completed());

        let completed = ctx.try_complete().expect("should be completed");
        assert_eq!(completed.status, IoStatus::Success);

        let layout = RecordLayout::compute(8, 8);
        assert_eq!(completed.read_key(&layout), 7u64);
        assert_eq!(completed.read_value(&layout), 77u64);

        manager.return_buffer(completed.buffer);
    }

    // ── CompletedIo record accessors ───────────────────────────────

    #[test]
    fn completed_io_read_record() {
        let page_size: u32 = 32 * 1024;
        let sector_size: u32 = 512;
        let device = InMemoryDevice::with_sizes(sector_size, 1 << 30);
        let manager = PendingIoManager::new(page_size, sector_size);

        let key: u64 = 0xDEAD_BEEF;
        let value: u64 = 0xCAFE_BABE;
        write_test_record(&device, 0, key, value);

        let pending = make_pending_op(0, 0);
        let completed = manager.issue_read_sync(pending, &device).unwrap();

        // Verify record_info
        let info = completed.record_info();
        assert_eq!(info.checkpoint_version(), 1);
        assert!(!info.is_invalid());
        assert!(!info.is_tombstone());

        // Verify key and value
        let layout = RecordLayout::compute(8, 8);
        assert_eq!(completed.read_key(&layout), key);
        assert_eq!(completed.read_value(&layout), value);
    }

    // ── PendingIoContext::is_completed ──────────────────────────────

    #[test]
    fn pending_io_context_is_completed() {
        let page_size: u32 = 32 * 1024;
        let sector_size: u32 = 512;
        let device = InMemoryDevice::with_sizes(sector_size, 1 << 30);
        let manager = PendingIoManager::new(page_size, sector_size);

        // Write data so the read succeeds.
        write_test_record(&device, 0, 1, 2);

        let pending = make_pending_op(0, 0);
        let ctx = manager.issue_read(pending, &device).unwrap();

        // InMemoryDevice is synchronous — should already be done.
        assert!(ctx.is_completed());

        let completed = ctx.try_complete().unwrap();
        assert_eq!(completed.status, IoStatus::Success);
    }

    // ── try_complete when not ready ────────────────────────────────

    #[test]
    fn try_complete_not_ready() {
        // Manually construct a PendingIoContext with completed=false.
        let pending = make_pending_op(0, 0);
        let buffer = AlignedBuffer::new(512, 512);

        let ctx = PendingIoContext::<TestFunctions> {
            id: next_context_id(),
            created_at: Instant::now(),
            timeout: DEFAULT_IO_TIMEOUT,
            operation: pending,
            buffer: Some(buffer),
            record_offset: 0,
            completed: Arc::new(AtomicBool::new(false)),
            io_status: Arc::new(Mutex::new(None)),
            bytes_transferred: Arc::new(AtomicU32::new(0)),
        };

        assert!(!ctx.is_completed());

        // try_complete should return Err(self) when not ready.
        let ctx = ctx.try_complete().expect_err("should not be completed yet");

        // Still not completed.
        assert!(!ctx.is_completed());
    }

    // ── Buffer pool reuse ──────────────────────────────────────────

    #[test]
    fn buffer_pool_reuse() {
        let page_size: u32 = 32 * 1024;
        let sector_size: u32 = 512;
        let device = InMemoryDevice::with_sizes(sector_size, 1 << 30);
        let manager = PendingIoManager::new(page_size, sector_size);

        write_test_record(&device, 0, 1, 2);

        // Issue a read and return the buffer.
        let pending = make_pending_op(0, 0);
        let completed = manager.issue_read_sync(pending, &device).unwrap();
        let ptr1 = completed.buffer.as_ptr();
        manager.return_buffer(completed.buffer);

        // Issue another read — should reuse the buffer.
        let pending2 = make_pending_op(0, 0);
        let completed2 = manager.issue_read_sync(pending2, &device).unwrap();
        let ptr2 = completed2.buffer.as_ptr();

        assert_eq!(ptr1, ptr2, "buffer should be reused from pool");
        manager.return_buffer(completed2.buffer);
    }

    // ── Callback sets status ───────────────────────────────────────

    #[test]
    fn read_callback_sets_status() {
        let completed = Arc::new(AtomicBool::new(false));
        let io_status = Arc::new(Mutex::new(None));
        let bytes_transferred = Arc::new(AtomicU32::new(0));

        let typed_ctx = TypedIoContext::new(ReadCallbackContext {
            completed: Arc::clone(&completed),
            io_status: Arc::clone(&io_status),
            bytes_transferred: Arc::clone(&bytes_transferred),
        });
        let io_context = typed_ctx.as_raw();

        // Simulate the callback.
        // SAFETY: io_context was created via TypedIoContext::new and the
        // callback reconstructs it via TypedIoContext::from_raw.
        unsafe {
            read_completion_callback(io_context, IoStatus::Success, 4096);
        }

        assert!(completed.load(Ordering::Acquire));
        assert_eq!(*io_status.lock().unwrap(), Some(IoStatus::Success));
        assert_eq!(bytes_transferred.load(Ordering::Acquire), 4096);
    }

    // ── Non-zero offset within page ────────────────────────────────

    #[test]
    fn issue_read_with_nonzero_offset_in_page() {
        let page_size: u32 = 32 * 1024;
        let sector_size: u32 = 512;
        let device = InMemoryDevice::with_sizes(sector_size, 1 << 30);
        let manager = PendingIoManager::new(page_size, sector_size);

        // Place a record at page 0, offset 1024 within the page.
        let record_device_offset = 1024u64;
        write_test_record(&device, record_device_offset, 55, 66);

        let pending = make_pending_op(0, 1024);
        let completed = manager.issue_read_sync(pending, &device).unwrap();

        // record_offset should account for sector alignment.
        // offset_in_page=1024, sector_start=1024 (1024/512*512), record_offset=0
        assert_eq!(completed.record_offset, 0);

        let layout = RecordLayout::compute(8, 8);
        assert_eq!(completed.read_key(&layout), 55u64);
        assert_eq!(completed.read_value(&layout), 66u64);
    }

    #[test]
    fn issue_read_with_non_sector_aligned_offset() {
        let page_size: u32 = 32 * 1024;
        let sector_size: u32 = 512;
        let device = InMemoryDevice::with_sizes(sector_size, 1 << 30);
        let manager = PendingIoManager::new(page_size, sector_size);

        // Record at page 0, offset 600 (not sector-aligned).
        let record_device_offset = 600u64;
        write_test_record(&device, record_device_offset, 88, 99);

        let pending = make_pending_op(0, 600);
        let completed = manager.issue_read_sync(pending, &device).unwrap();

        // sector_start = (600/512)*512 = 512, record_offset = 600-512 = 88
        assert_eq!(completed.record_offset, 88);

        let layout = RecordLayout::compute(8, 8);
        assert_eq!(completed.read_key(&layout), 88u64);
        assert_eq!(completed.read_value(&layout), 99u64);
    }

    // ── Error variant Display ──────────────────────────────────────

    #[test]
    fn pending_io_error_display() {
        let e = PendingIoError::InvalidRecord;
        assert_eq!(e.to_string(), "invalid record data");

        let e = PendingIoError::DeviceError(IoStatus::Cancelled);
        assert!(e.to_string().contains("Cancelled"));

        let e = PendingIoError::IoError(std::io::Error::other("disk failed"));
        assert!(e.to_string().contains("disk failed"));

        let e = PendingIoError::TimedOut {
            context_id: 42,
            elapsed: Duration::from_secs(31),
        };
        assert!(e.to_string().contains("42"));
        assert!(e.to_string().contains("timed out"));

        let e = PendingIoError::Cancelled { context_id: 7 };
        assert!(e.to_string().contains("7"));
        assert!(e.to_string().contains("cancelled"));

        let e = PendingIoError::IoFailed {
            context_id: 99,
            source: std::io::Error::other("bad sector"),
        };
        assert!(e.to_string().contains("99"));
        assert!(e.to_string().contains("bad sector"));
    }

    #[test]
    fn context_not_expired_by_default() {
        let ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(0, 0),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
        );
        assert!(!ctx.is_expired());
        assert!(ctx.timeout() == Duration::from_secs(30));
        assert!(ctx.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn context_expired_with_zero_timeout() {
        let past = Instant::now() - Duration::from_millis(1);
        let ctx = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(0, 0),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Duration::ZERO,
            past,
        );
        assert!(ctx.is_expired());
    }

    #[test]
    fn context_expired_after_timeout_elapses() {
        let past = Instant::now() - Duration::from_secs(2);
        let ctx = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(0, 0),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Duration::from_secs(1),
            past,
        );
        assert!(ctx.is_expired());
        assert!(ctx.elapsed() >= Duration::from_secs(2));
    }

    #[test]
    fn context_not_expired_within_timeout() {
        let ctx = PendingIoContext::<TestFunctions>::new_for_test_with_timeout(
            make_pending_op(0, 0),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
            Duration::from_secs(60),
            Instant::now(),
        );
        assert!(!ctx.is_expired());
    }

    #[test]
    fn context_ids_are_unique() {
        let ctx1 = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(0, 0),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
        );
        let ctx2 = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(0, 0),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
        );
        assert_ne!(ctx1.id(), ctx2.id());
    }

    #[test]
    fn context_debug_includes_id_and_expired() {
        let ctx = PendingIoContext::<TestFunctions>::new_for_test(
            make_pending_op(0, 0),
            AlignedBuffer::new(512, 512),
            0,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicU32::new(0)),
        );
        let debug = format!("{ctx:?}");
        assert!(debug.contains("id"));
        assert!(debug.contains("expired"));
    }

    #[test]
    fn pending_io_config_defaults() {
        let config = PendingIoConfig::default();
        assert_eq!(config.default_timeout, Duration::from_secs(30));
        assert_eq!(config.max_retries, 3);
        assert!(config.cancel_on_drop);
    }

    #[test]
    fn pending_io_config_custom() {
        let config = PendingIoConfig {
            default_timeout: Duration::from_secs(10),
            max_retries: 5,
            cancel_on_drop: false,
        };
        assert_eq!(config.default_timeout, Duration::from_secs(10));
        assert_eq!(config.max_retries, 5);
        assert!(!config.cancel_on_drop);
    }

    #[test]
    fn manager_with_config_uses_custom_timeout() {
        let config = PendingIoConfig {
            default_timeout: Duration::from_secs(5),
            ..PendingIoConfig::default()
        };
        let manager = PendingIoManager::with_config(32 * 1024, 512, config);
        assert_eq!(manager.config().default_timeout, Duration::from_secs(5));
    }

    #[test]
    fn manager_default_config() {
        let manager = PendingIoManager::new(32 * 1024, 512);
        assert_eq!(manager.config().default_timeout, Duration::from_secs(30));
    }

    #[test]
    fn io_failed_error_has_source() {
        use std::error::Error;
        let e = PendingIoError::IoFailed {
            context_id: 1,
            source: std::io::Error::other("disk"),
        };
        assert!(e.source().is_some());
        let e = PendingIoError::TimedOut {
            context_id: 1,
            elapsed: Duration::from_secs(1),
        };
        assert!(e.source().is_none());
        let e = PendingIoError::Cancelled { context_id: 1 };
        assert!(e.source().is_none());
    }

    #[test]
    fn issue_async_read_context_has_id_and_timeout() {
        let config = PendingIoConfig {
            default_timeout: Duration::from_secs(10),
            ..PendingIoConfig::default()
        };
        let manager = PendingIoManager::with_config(32 * 1024, 512, config);
        let device = InMemoryDevice::with_sizes(512, 1 << 30);
        write_test_record(&device, 0, 1, 2);
        let ctx = manager.issue_read(make_pending_op(0, 0), &device).unwrap();
        assert!(ctx.id() > 0);
        assert_eq!(ctx.timeout(), Duration::from_secs(10));
        assert!(!ctx.is_expired());
    }
}
