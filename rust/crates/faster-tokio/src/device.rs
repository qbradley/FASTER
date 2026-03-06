//! Tokio-backed storage device for FASTER's hybrid log.
//!
//! [`TokioFileDevice`] implements the [`Device`](faster_core::device::Device)
//! trait using [`tokio::task::spawn_blocking`] for asynchronous I/O.
//! Instead of maintaining a dedicated thread pool (like
//! [`SyncFileDevice`](faster_core::SyncFileDevice)), it leverages Tokio's
//! managed blocking thread pool — sharing resources with the rest of the
//! application.
//!
//! # Architecture
//!
//! ```text
//! FASTER core                    TokioFileDevice
//! ┌─────────┐  read_async()     ┌──────────────────┐
//! │ Session  │ ─────────────▶  │ spawn_blocking {  │
//! │          │                  │   std::fs I/O     │
//! │          │  ◀── callback ── │   invoke callback │
//! └─────────┘                  └──────────────────┘
//! ```
//!
//! Sync operations (`read_sync`, `write_sync`) execute directly on the
//! calling thread, bypassing the Tokio runtime entirely.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use faster_core::device::{Device, IoCompletionCallback, IoRequestResult, IoStatus};

// ---------------------------------------------------------------------------
// Platform-specific positional I/O
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn platform_read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(unix)]
fn platform_write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(not(unix))]
fn platform_read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read(buf)
}

#[cfg(not(unix))]
fn platform_write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.write(buf)
}

/// Read exactly `buf.len()` bytes, zero-filling on early EOF.
fn read_complete_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut pos = 0usize;
    while pos < buf.len() {
        match platform_read_at(file, &mut buf[pos..], offset + pos as u64) {
            Ok(0) => {
                buf[pos..].fill(0);
                return Ok(());
            }
            Ok(n) => pos += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write all of `buf`, retrying on partial writes and interrupts.
fn write_complete_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    let mut pos = 0usize;
    while pos < buf.len() {
        match platform_write_at(file, &buf[pos..], offset + pos as u64) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(n) => pos += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SegmentRegistry
// ---------------------------------------------------------------------------

/// Manages open file handles for segment files.
struct SegmentRegistry {
    segments: RwLock<HashMap<u64, Arc<File>>>,
    base_path: PathBuf,
    prefix: String,
}

impl SegmentRegistry {
    fn new(base_path: PathBuf, prefix: String) -> Self {
        Self {
            segments: RwLock::new(HashMap::new()),
            base_path,
            prefix,
        }
    }

    fn segment_path(&self, segment_index: u64) -> PathBuf {
        self.base_path
            .join(format!("{}{}", self.prefix, segment_index))
    }

    fn get_or_open(&self, segment_index: u64) -> io::Result<Arc<File>> {
        // Fast path: read lock.
        {
            let guard = self.segments.read().expect("segment registry poisoned");
            if let Some(f) = guard.get(&segment_index) {
                return Ok(Arc::clone(f));
            }
        }

        // Slow path: write lock with double-check.
        let mut guard = self.segments.write().expect("segment registry poisoned");
        if let Some(f) = guard.get(&segment_index) {
            return Ok(Arc::clone(f));
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.segment_path(segment_index))?;

        let file = Arc::new(file);
        guard.insert(segment_index, Arc::clone(&file));
        Ok(file)
    }

    fn remove_segment(&self, segment_index: u64) {
        {
            let mut guard = self.segments.write().expect("segment registry poisoned");
            guard.remove(&segment_index);
        }
        let _ = fs::remove_file(self.segment_path(segment_index));
    }

    fn close_all(&self) {
        self.segments
            .write()
            .expect("segment registry poisoned")
            .clear();
    }
}

// ---------------------------------------------------------------------------
// TokioFileDevice
// ---------------------------------------------------------------------------

/// A file-backed [`Device`] using Tokio's blocking thread pool.
///
/// Follows the same segmented file layout as
/// [`SyncFileDevice`](faster_core::SyncFileDevice):
///
/// ```text
/// base_dir/{prefix}0    ← segment 0
/// base_dir/{prefix}1    ← segment 1
/// …
/// ```
///
/// Async operations (`read_async`, `write_async`) are dispatched via
/// [`tokio::task::spawn_blocking`]. Sync operations execute directly.
pub struct TokioFileDevice {
    registry: Arc<SegmentRegistry>,
    handle: tokio::runtime::Handle,
    sector_size: u32,
    segment_size: u64,
    high_water: AtomicU64,
    closed: AtomicBool,
}

impl TokioFileDevice {
    /// Create a new `TokioFileDevice`.
    ///
    /// `base_path` is created if it does not exist. The Tokio runtime handle
    /// is captured from the current context.
    ///
    /// # Panics
    ///
    /// - `sector_size` must be a power of two.
    /// - `segment_size` must be > 0.
    /// - Must be called from within a Tokio runtime context.
    pub fn new(
        base_path: impl AsRef<Path>,
        prefix: &str,
        sector_size: u32,
        segment_size: u64,
    ) -> io::Result<Self> {
        assert!(
            sector_size.is_power_of_two(),
            "sector_size must be a power of two"
        );
        assert!(segment_size > 0, "segment_size must be > 0");

        let base = base_path.as_ref().to_path_buf();
        fs::create_dir_all(&base)?;

        let handle = tokio::runtime::Handle::current();

        Ok(Self {
            registry: Arc::new(SegmentRegistry::new(base, prefix.to_owned())),
            handle,
            sector_size,
            segment_size,
            high_water: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        })
    }

    /// Create a new `TokioFileDevice` with an explicit runtime handle.
    ///
    /// Use this when constructing the device outside of a Tokio runtime
    /// context (e.g., during setup before the runtime starts).
    pub fn with_handle(
        base_path: impl AsRef<Path>,
        prefix: &str,
        sector_size: u32,
        segment_size: u64,
        handle: tokio::runtime::Handle,
    ) -> io::Result<Self> {
        assert!(
            sector_size.is_power_of_two(),
            "sector_size must be a power of two"
        );
        assert!(segment_size > 0, "segment_size must be > 0");

        let base = base_path.as_ref().to_path_buf();
        fs::create_dir_all(&base)?;

        Ok(Self {
            registry: Arc::new(SegmentRegistry::new(base, prefix.to_owned())),
            handle,
            sector_size,
            segment_size,
            high_water: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        })
    }

    /// Perform a segmented read, splitting across segment boundaries.
    fn do_read(
        registry: &SegmentRegistry,
        segment_size: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> io::Result<()> {
        let mut remaining = buf.len();
        let mut buf_pos = 0usize;
        let mut cur = offset;

        while remaining > 0 {
            let seg_idx = cur / segment_size;
            let file_off = cur % segment_size;
            let chunk = remaining.min((segment_size - file_off) as usize);

            let file = registry.get_or_open(seg_idx)?;
            read_complete_at(&file, &mut buf[buf_pos..buf_pos + chunk], file_off)?;

            buf_pos += chunk;
            remaining -= chunk;
            cur += chunk as u64;
        }
        Ok(())
    }

    /// Perform a segmented write, splitting across segment boundaries.
    fn do_write(
        registry: &SegmentRegistry,
        segment_size: u64,
        offset: u64,
        buf: &[u8],
    ) -> io::Result<()> {
        let mut remaining = buf.len();
        let mut buf_pos = 0usize;
        let mut cur = offset;

        while remaining > 0 {
            let seg_idx = cur / segment_size;
            let file_off = cur % segment_size;
            let chunk = remaining.min((segment_size - file_off) as usize);

            let file = registry.get_or_open(seg_idx)?;
            write_complete_at(&file, &buf[buf_pos..buf_pos + chunk], file_off)?;

            buf_pos += chunk;
            remaining -= chunk;
            cur += chunk as u64;
        }
        Ok(())
    }

    fn validate_alignment(&self, offset: u64, len: u32) -> Result<(), io::Error> {
        if offset % u64::from(self.sector_size) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if u64::from(len) % u64::from(self.sector_size) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "len {len} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "len must be > 0",
            ));
        }
        Ok(())
    }
}

impl Device for TokioFileDevice {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn segment_size(&self) -> u64 {
        self.segment_size
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
        if let Err(e) = self.validate_alignment(offset, len) {
            return IoRequestResult::Error(e);
        }

        let registry = Arc::clone(&self.registry);
        let segment_size = self.segment_size;
        // Cast raw pointers to usize so the closure is Send
        // (raw pointers are !Send, usize is Send).
        let dest_addr = dest as usize;
        let ctx_addr = context as usize;

        self.handle.spawn_blocking(move || {
            // SAFETY: Device trait contract guarantees `dest` is valid for
            // `len` bytes until the callback fires. We read into it and
            // then immediately invoke the callback.
            let buf = unsafe { std::slice::from_raw_parts_mut(dest_addr as *mut u8, len as usize) };
            let result = Self::do_read(&registry, segment_size, offset, buf);

            let (status, bytes) = match result {
                Ok(()) => (IoStatus::Success, len),
                Err(e) => (IoStatus::Error(e.raw_os_error().unwrap_or(-1)), 0),
            };

            // SAFETY: Device trait contract guarantees `context` is valid
            // until the callback fires. We invoke it exactly once.
            unsafe {
                (callback)(ctx_addr as *mut u8, status, bytes);
            }
        });

        IoRequestResult::Submitted
    }

    unsafe fn write_async(
        &self,
        source: *const u8,
        offset: u64,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        if let Err(e) = self.validate_alignment(offset, len) {
            return IoRequestResult::Error(e);
        }

        self.high_water
            .fetch_max(offset + u64::from(len), Ordering::Relaxed);

        let registry = Arc::clone(&self.registry);
        let segment_size = self.segment_size;
        // Cast raw pointers to usize so the closure is Send.
        let src_addr = source as usize;
        let ctx_addr = context as usize;

        self.handle.spawn_blocking(move || {
            // SAFETY: Device trait contract guarantees `source` is valid for
            // `len` bytes until the callback fires. We read from it and
            // then immediately invoke the callback.
            let buf = unsafe { std::slice::from_raw_parts(src_addr as *const u8, len as usize) };
            let result = Self::do_write(&registry, segment_size, offset, buf);

            let (status, bytes) = match result {
                Ok(()) => (IoStatus::Success, len),
                Err(e) => (IoStatus::Error(e.raw_os_error().unwrap_or(-1)), 0),
            };

            // SAFETY: Device trait contract guarantees `context` is valid
            // until the callback fires. We invoke it exactly once.
            unsafe {
                (callback)(ctx_addr as *mut u8, status, bytes);
            }
        });

        IoRequestResult::Submitted
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        let total = dest.len();
        self.validate_alignment(offset, total as u32)?;
        Self::do_read(&self.registry, self.segment_size, offset, dest)?;
        Ok(total as u32)
    }

    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32> {
        let total = source.len();
        self.validate_alignment(offset, total as u32)?;
        Self::do_write(&self.registry, self.segment_size, offset, source)?;
        self.high_water
            .fetch_max(offset + total as u64, Ordering::Relaxed);
        Ok(total as u32)
    }

    fn truncate_until(&self, offset: u64) {
        let threshold = offset / self.segment_size;
        let to_remove: Vec<u64> = {
            let guard = self
                .registry
                .segments
                .read()
                .expect("segment registry poisoned");
            guard
                .keys()
                .copied()
                .filter(|&idx| idx < threshold)
                .collect()
        };
        for idx in to_remove {
            self.registry.remove_segment(idx);
        }
    }

    fn size(&self) -> u64 {
        self.high_water.load(Ordering::Relaxed)
    }

    fn close(&self) {
        if self
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        self.registry.close_all();
    }
}

impl Drop for TokioFileDevice {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faster_core::device::TypedIoContext;
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};

    // -- Callback helper --------------------------------------------------

    struct CallbackState {
        completed: AtomicBool,
        status: std::sync::Mutex<Option<IoStatus>>,
        bytes: AtomicU32,
    }

    impl CallbackState {
        fn new() -> Self {
            Self {
                completed: AtomicBool::new(false),
                status: std::sync::Mutex::new(None),
                bytes: AtomicU32::new(0),
            }
        }

        fn wait(&self, timeout: Duration) {
            let start = Instant::now();
            while !self.completed.load(Ordering::Acquire) {
                assert!(
                    start.elapsed() < timeout,
                    "I/O callback did not fire within {timeout:?}",
                );
                std::thread::yield_now();
            }
        }
    }

    /// # Safety
    ///
    /// `context` must be a valid pointer to `TypedIoContext<Arc<CallbackState>>`.
    unsafe fn test_callback(context: *mut u8, status: IoStatus, bytes: u32) {
        // SAFETY: Caller guarantees `context` originated from
        // `TypedIoContext::<Arc<CallbackState>>::as_raw()`.
        let state: Box<Arc<CallbackState>> =
            unsafe { TypedIoContext::<Arc<CallbackState>>::from_raw(context) };
        *state.status.lock().unwrap() = Some(status);
        state.bytes.store(bytes, Ordering::Release);
        state.completed.store(true, Ordering::Release);
    }

    // -- Tests ------------------------------------------------------------

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let device = TokioFileDevice::new(dir.path(), "log.", 512, 1 << 30).unwrap();

        let sector = 512u32;
        let mut write_buf = vec![0u8; sector as usize];
        write_buf[0..8].copy_from_slice(&0xDEAD_BEEF_u64.to_le_bytes());

        // Async write
        let state = Arc::new(CallbackState::new());
        let io_ctx = TypedIoContext::new(Arc::clone(&state));
        let raw_ctx = io_ctx.as_raw();
        // SAFETY: write_buf is valid for sector bytes. raw_ctx is valid
        // until callback fires.
        unsafe {
            device.write_async(write_buf.as_ptr(), 0, sector, test_callback, raw_ctx);
        }
        state.wait(Duration::from_secs(5));
        assert_eq!(*state.status.lock().unwrap(), Some(IoStatus::Success));

        // Async read
        let mut read_buf = vec![0u8; sector as usize];
        let state2 = Arc::new(CallbackState::new());
        let io_ctx2 = TypedIoContext::new(Arc::clone(&state2));
        let raw_ctx2 = io_ctx2.as_raw();
        // SAFETY: read_buf is valid for sector bytes. raw_ctx2 is valid
        // until callback fires.
        unsafe {
            device.read_async(0, read_buf.as_mut_ptr(), sector, test_callback, raw_ctx2);
        }
        state2.wait(Duration::from_secs(5));
        assert_eq!(*state2.status.lock().unwrap(), Some(IoStatus::Success));

        assert_eq!(
            u64::from_le_bytes(read_buf[0..8].try_into().unwrap()),
            0xDEAD_BEEF_u64
        );

        device.close();
    }

    #[tokio::test]
    async fn sync_write_then_sync_read() {
        let dir = tempfile::tempdir().unwrap();
        let device = TokioFileDevice::new(dir.path(), "log.", 512, 1 << 30).unwrap();

        let sector = 512u32;
        let mut write_buf = vec![0u8; sector as usize];
        write_buf[0..8].copy_from_slice(&42u64.to_le_bytes());

        let bytes_written = device.write_sync(0, &write_buf).unwrap();
        assert_eq!(bytes_written, sector);

        let mut read_buf = vec![0u8; sector as usize];
        let bytes_read = device.read_sync(0, &mut read_buf).unwrap();
        assert_eq!(bytes_read, sector);

        assert_eq!(
            u64::from_le_bytes(read_buf[0..8].try_into().unwrap()),
            42u64
        );

        device.close();
    }

    #[tokio::test]
    async fn alignment_validation() {
        let dir = tempfile::tempdir().unwrap();
        let device = TokioFileDevice::new(dir.path(), "log.", 512, 1 << 30).unwrap();

        // Misaligned offset for sync read
        let mut buf = vec![0u8; 512];
        let result = device.read_sync(1, &mut buf);
        assert!(result.is_err());

        // Misaligned length for sync write
        let buf = vec![0u8; 100];
        let result = device.write_sync(0, &buf);
        assert!(result.is_err());

        // Zero length
        let buf: Vec<u8> = vec![];
        let result = device.write_sync(0, &buf);
        assert!(result.is_err());

        device.close();
    }

    #[tokio::test]
    async fn cross_segment_io() {
        let dir = tempfile::tempdir().unwrap();
        // Small segment size: 1024 bytes
        let device = TokioFileDevice::new(dir.path(), "log.", 512, 1024).unwrap();

        let sector = 512u32;
        // Write at offset 512 (within segment 0)
        let buf1 = vec![0xAAu8; sector as usize];
        device.write_sync(512, &buf1).unwrap();

        // Write at offset 1024 (segment 1)
        let buf2 = vec![0xBBu8; sector as usize];
        device.write_sync(1024, &buf2).unwrap();

        // Read back
        let mut read1 = vec![0u8; sector as usize];
        device.read_sync(512, &mut read1).unwrap();
        assert!(read1.iter().all(|&b| b == 0xAA));

        let mut read2 = vec![0u8; sector as usize];
        device.read_sync(1024, &mut read2).unwrap();
        assert!(read2.iter().all(|&b| b == 0xBB));

        device.close();
    }

    #[tokio::test]
    async fn size_tracks_writes() {
        let dir = tempfile::tempdir().unwrap();
        let device = TokioFileDevice::new(dir.path(), "log.", 512, 1 << 30).unwrap();

        assert_eq!(device.size(), 0);

        let buf = vec![0u8; 512];
        device.write_sync(0, &buf).unwrap();
        assert_eq!(device.size(), 512);

        device.write_sync(1024, &buf).unwrap();
        assert_eq!(device.size(), 1536);

        device.close();
    }

    #[tokio::test]
    async fn truncate_removes_old_segments() {
        let dir = tempfile::tempdir().unwrap();
        let device = TokioFileDevice::new(dir.path(), "log.", 512, 1024).unwrap();

        // Write to segments 0 and 1
        let buf = vec![0u8; 512];
        device.write_sync(0, &buf).unwrap();
        device.write_sync(1024, &buf).unwrap();

        // Segment 0 file should exist
        assert!(dir.path().join("log.0").exists());

        // Truncate everything before segment 1
        device.truncate_until(1024);

        // Segment 0 file should be removed
        assert!(!dir.path().join("log.0").exists());
        // Segment 1 should still exist
        assert!(dir.path().join("log.1").exists());

        device.close();
    }

    /// Simple throughput comparison: TokioFileDevice vs SyncFileDevice.
    ///
    /// This is a basic sanity check, not a rigorous benchmark. It verifies
    /// that TokioFileDevice achieves reasonable throughput relative to
    /// SyncFileDevice for sequential sync writes.
    #[tokio::test]
    async fn throughput_comparison_vs_sync_device() {
        use faster_core::SyncFileDevice;

        let dir = tempfile::tempdir().unwrap();
        let tokio_dir = dir.path().join("tokio");
        let sync_dir = dir.path().join("sync");

        let tokio_device = TokioFileDevice::new(&tokio_dir, "log.", 512, 1 << 30).unwrap();
        let sync_device = SyncFileDevice::new(&sync_dir, "log.", 512, 1 << 30, 2).unwrap();

        let iterations = 100u32;
        let sector = 512u32;
        let buf = vec![0xCCu8; sector as usize];

        // Tokio device sync writes
        let start = Instant::now();
        for i in 0..iterations {
            tokio_device
                .write_sync(u64::from(i) * u64::from(sector), &buf)
                .unwrap();
        }
        let tokio_elapsed = start.elapsed();

        // SyncFileDevice sync writes
        let start = Instant::now();
        for i in 0..iterations {
            sync_device
                .write_sync(u64::from(i) * u64::from(sector), &buf)
                .unwrap();
        }
        let sync_elapsed = start.elapsed();

        // Verify correctness: read back from tokio device
        for i in 0..iterations {
            let mut read_buf = vec![0u8; sector as usize];
            tokio_device
                .read_sync(u64::from(i) * u64::from(sector), &mut read_buf)
                .unwrap();
            assert!(
                read_buf.iter().all(|&b| b == 0xCC),
                "data mismatch at offset {}",
                i * sector
            );
        }

        // Log throughput (not a hard assertion — just informational)
        let tokio_ops_per_sec = f64::from(iterations) / tokio_elapsed.as_secs_f64();
        let sync_ops_per_sec = f64::from(iterations) / sync_elapsed.as_secs_f64();
        eprintln!("TokioFileDevice: {tokio_ops_per_sec:.0} ops/s ({tokio_elapsed:?})");
        eprintln!("SyncFileDevice:  {sync_ops_per_sec:.0} ops/s ({sync_elapsed:?})");

        // Sanity: both should handle at least 100 ops
        assert!(
            tokio_ops_per_sec > 10.0,
            "TokioFileDevice throughput too low: {tokio_ops_per_sec:.0} ops/s"
        );

        tokio_device.close();
        sync_device.close();
    }
}
