//! File-backed storage device using synchronous I/O with a background thread pool.
//!
//! [`SyncFileDevice`] maps a single linear address space onto multiple segment
//! files on disk. Async operations are dispatched to a fixed-size thread pool
//! that performs blocking file I/O and invokes the completion callback when done.
//! Sync operations run directly on the calling thread.
//!
//! # Segmented File Layout
//!
//! The address space is divided into fixed-size segments. Each segment
//! corresponds to a separate file on disk named `{prefix}{segment_index}`:
//!
//! ```text
//! base_dir/log.0    ← segment 0: offsets [0, segment_size)
//! base_dir/log.1    ← segment 1: offsets [segment_size, 2*segment_size)
//! …
//! ```
//!
//! Segment files are created on demand when first written.

use crate::sync::channel::{Receiver, Sender, channel};
use crate::sync::thread::{self, JoinHandle};
use crate::sync::{Arc, AtomicBool, AtomicU64, Mutex, Ordering, RwLock};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::device::{Device, IoCompletionCallback, IoRequestResult, IoStatus};

// ---------------------------------------------------------------------------
// Platform-specific positional I/O
// ---------------------------------------------------------------------------

/// Read from a file at a specific offset without altering the seek position.
#[cfg(unix)]
fn platform_read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

/// Write to a file at a specific offset without altering the seek position.
#[cfg(unix)]
fn platform_write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

/// Fallback: clone the handle, seek, then read.
#[cfg(not(unix))]
fn platform_read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = file.try_clone()?;
    f.seek(SeekFrom::Start(offset))?;
    f.read(buf)
}

/// Fallback: clone the handle, seek, then write.
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
                // EOF — zero-fill the remainder (consistent with NullDevice /
                // InMemoryDevice behaviour for unwritten regions).
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
///
/// Each segment is a separate file on disk named `{prefix}{segment_index}`
/// under the base directory. Files are opened lazily on first access and
/// cached in a [`HashMap`] behind an [`RwLock`].
struct SegmentRegistry {
    /// Map from segment index to open file handle.
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

    /// Returns the filesystem path for the given segment index.
    fn segment_path(&self, segment_index: u64) -> PathBuf {
        self.base_path
            .join(format!("{}{}", self.prefix, segment_index))
    }

    /// Get (or lazily open) the file handle for `segment_index`.
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

    /// Remove a segment from the cache and delete its file from disk.
    fn remove_segment(&self, segment_index: u64) {
        {
            let mut guard = self.segments.write().expect("segment registry poisoned");
            guard.remove(&segment_index);
        }
        // Best-effort removal — ignore errors (e.g. already deleted).
        let _ = fs::remove_file(self.segment_path(segment_index));
    }

    /// Close all cached file handles.
    fn close_all(&self) {
        self.segments
            .write()
            .expect("segment registry poisoned")
            .clear();
    }
}

// ---------------------------------------------------------------------------
// I/O request types
// ---------------------------------------------------------------------------

/// Discriminant for [`IoRequest`].
enum IoRequestKind {
    /// Read data from storage into a buffer.
    Read,
    /// Write data from a buffer to storage.
    Write,
}

/// A pending I/O operation to be executed by a worker thread.
struct IoRequest {
    kind: IoRequestKind,
    offset: u64,
    /// For reads: destination buffer. For writes: source buffer (stored as
    /// `*mut u8` for uniform representation; workers only read from it via
    /// `*const u8` on the write path).
    buffer: *mut u8,
    len: u32,
    callback: IoCompletionCallback,
    context: *mut u8,
}

// SAFETY: The raw pointers in `IoRequest` (`buffer` and `context`) are
// guaranteed valid by the `Device` trait contract until the completion
// callback fires. Each worker thread accesses them exactly once and does
// not retain them afterward.
unsafe impl Send for IoRequest {}

// ---------------------------------------------------------------------------
// I/O execution helper
// ---------------------------------------------------------------------------

/// Execute a single I/O request, splitting across segment boundaries if needed.
fn execute_request(
    request: &IoRequest,
    registry: &SegmentRegistry,
    segment_size: u64,
) -> io::Result<()> {
    let mut remaining = request.len as usize;
    let mut buf_pos = 0usize;
    let mut offset = request.offset;

    while remaining > 0 {
        let seg_idx = offset / segment_size;
        let file_off = offset % segment_size;
        let space_in_segment = segment_size - file_off;
        let chunk = remaining.min(space_in_segment as usize);

        let file = registry.get_or_open(seg_idx)?;

        match request.kind {
            IoRequestKind::Read => {
                // SAFETY: `request.buffer` is valid for `request.len` bytes
                // (Device trait contract). `buf_pos + chunk <= request.len`,
                // so `buffer.add(buf_pos)` is in-bounds.
                let buf =
                    unsafe { std::slice::from_raw_parts_mut(request.buffer.add(buf_pos), chunk) };
                read_complete_at(&file, buf, file_off)?;
            }
            IoRequestKind::Write => {
                // SAFETY: Same bounds guarantee as the read path. The pointer
                // originated from `*const u8` (write source) cast to `*mut u8`
                // for storage; we only read through it here via `cast_const`.
                let buf = unsafe {
                    std::slice::from_raw_parts(request.buffer.cast_const().add(buf_pos), chunk)
                };
                write_complete_at(&file, buf, file_off)?;
            }
        }

        buf_pos += chunk;
        remaining -= chunk;
        offset += chunk as u64;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Worker thread entry point
// ---------------------------------------------------------------------------

/// Main loop for an I/O worker thread.
///
/// Receives requests from the shared channel, performs the file I/O, and
/// invokes the completion callback. Exits when the channel is closed (all
/// senders dropped).
fn worker_loop(
    receiver: Arc<Mutex<Receiver<IoRequest>>>,
    registry: Arc<SegmentRegistry>,
    segment_size: u64,
) {
    loop {
        let request = {
            let rx = receiver.lock().expect("worker receiver poisoned");
            match rx.recv() {
                Ok(r) => r,
                Err(_) => return, // Channel closed — shut down.
            }
        };

        let result = execute_request(&request, &registry, segment_size);

        let (status, bytes) = match result {
            Ok(()) => (IoStatus::Success, request.len),
            Err(e) => (IoStatus::Error(e.raw_os_error().unwrap_or(-1)), 0),
        };

        // SAFETY: `request.context` is valid until this callback fires
        // (Device trait contract). The callback is invoked exactly once.
        unsafe {
            (request.callback)(request.context, status, bytes);
        }
    }
}

// ---------------------------------------------------------------------------
// SyncFileDevice
// ---------------------------------------------------------------------------

/// A cross-platform, file-backed [`Device`] using synchronous I/O.
///
/// Async operations are dispatched to a background thread pool that performs
/// blocking file I/O and invokes completion callbacks. Sync operations execute
/// directly on the calling thread. The linear address space is partitioned
/// into fixed-size segment files managed by an internal [`SegmentRegistry`].
pub struct SyncFileDevice {
    registry: Arc<SegmentRegistry>,
    sender: Mutex<Option<Sender<IoRequest>>>,
    threads: Mutex<Vec<Option<JoinHandle<()>>>>,
    sector_size: u32,
    segment_size: u64,
    max_outstanding: u32,
    high_water: AtomicU64,
    closed: AtomicBool,
}

impl SyncFileDevice {
    /// Create a new `SyncFileDevice`.
    ///
    /// `base_path` is created if it does not exist. Worker threads are spawned
    /// immediately.
    ///
    /// # Panics
    ///
    /// Panics if `sector_size` is not a power of two, or if `segment_size` or
    /// `num_io_threads` is zero.
    pub fn new(
        base_path: impl AsRef<Path>,
        prefix: &str,
        sector_size: u32,
        segment_size: u64,
        num_io_threads: usize,
    ) -> io::Result<Self> {
        assert!(
            sector_size.is_power_of_two(),
            "sector_size must be a power of two"
        );
        assert!(segment_size > 0, "segment_size must be > 0");
        assert!(num_io_threads > 0, "num_io_threads must be > 0");

        let base = base_path.as_ref().to_path_buf();
        fs::create_dir_all(&base)?;

        let registry = Arc::new(SegmentRegistry::new(base, prefix.to_owned()));
        let (tx, rx) = channel::<IoRequest>();
        let rx = Arc::new(Mutex::new(rx));

        let mut threads = Vec::with_capacity(num_io_threads);
        for i in 0..num_io_threads {
            let rx_clone = Arc::clone(&rx);
            let reg_clone = Arc::clone(&registry);
            let seg = segment_size;
            let h = thread::Builder::new()
                .name(format!("sync-io-{i}"))
                .spawn(move || worker_loop(rx_clone, reg_clone, seg))
                .map_err(io::Error::other)?;
            threads.push(Some(h));
        }

        Ok(Self {
            registry,
            sender: Mutex::new(Some(tx)),
            threads: Mutex::new(threads),
            sector_size,
            segment_size,
            max_outstanding: 1024,
            high_water: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        })
    }

    /// Submit an I/O request to the background thread pool.
    fn submit(&self, request: IoRequest) -> IoRequestResult {
        let guard = self.sender.lock().expect("sender lock poisoned");
        match guard.as_ref() {
            Some(tx) => match tx.send(request) {
                Ok(()) => IoRequestResult::Submitted,
                Err(_) => IoRequestResult::Error(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "I/O thread pool shut down",
                )),
            },
            None => IoRequestResult::Error(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "device is closed",
            )),
        }
    }
}

impl Device for SyncFileDevice {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn segment_size(&self) -> u64 {
        self.segment_size
    }

    fn max_outstanding_io(&self) -> u32 {
        self.max_outstanding
    }

    // H1/C-1: Alignment checks promoted to release assertions — misaligned I/O
    // causes EINVAL on O_DIRECT or silent corruption. These are safety guards
    // that must never be stripped in release builds.
    unsafe fn read_async(
        &self,
        offset: u64,
        dest: *mut u8,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        if offset % u64::from(self.sector_size) != 0 {
            return IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if u64::from(len) % u64::from(self.sector_size) != 0 {
            return IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "len {len} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if len == 0 {
            return IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "len must be > 0",
            ));
        }

        self.submit(IoRequest {
            kind: IoRequestKind::Read,
            offset,
            buffer: dest,
            len,
            callback,
            context,
        })
    }

    unsafe fn write_async(
        &self,
        source: *const u8,
        offset: u64,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        if offset % u64::from(self.sector_size) != 0 {
            return IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if u64::from(len) % u64::from(self.sector_size) != 0 {
            return IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "len {len} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if len == 0 {
            return IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "len must be > 0",
            ));
        }

        // Eagerly update the high-water mark (consistent with NullDevice).
        self.high_water
            .fetch_max(offset + u64::from(len), Ordering::Relaxed);

        self.submit(IoRequest {
            kind: IoRequestKind::Write,
            offset,
            buffer: source.cast_mut(),
            len,
            callback,
            context,
        })
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        let total = dest.len();
        if offset % u64::from(self.sector_size) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if total % self.sector_size as usize != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "len {total} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if total == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "len must be > 0",
            ));
        }

        let mut remaining = total;
        let mut buf_pos = 0usize;
        let mut cur = offset;

        while remaining > 0 {
            let seg_idx = cur / self.segment_size;
            let file_off = cur % self.segment_size;
            let chunk = remaining.min((self.segment_size - file_off) as usize);

            let file = self.registry.get_or_open(seg_idx)?;
            read_complete_at(&file, &mut dest[buf_pos..buf_pos + chunk], file_off)?;

            buf_pos += chunk;
            remaining -= chunk;
            cur += chunk as u64;
        }

        Ok(total as u32)
    }

    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32> {
        let total = source.len();
        if offset % u64::from(self.sector_size) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if total % self.sector_size as usize != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "len {total} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if total == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "len must be > 0",
            ));
        }

        let mut remaining = total;
        let mut buf_pos = 0usize;
        let mut cur = offset;

        while remaining > 0 {
            let seg_idx = cur / self.segment_size;
            let file_off = cur % self.segment_size;
            let chunk = remaining.min((self.segment_size - file_off) as usize);

            let file = self.registry.get_or_open(seg_idx)?;
            write_complete_at(&file, &source[buf_pos..buf_pos + chunk], file_off)?;

            buf_pos += chunk;
            remaining -= chunk;
            cur += chunk as u64;
        }

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
            return; // Already closed.
        }

        // Drop the sender to close the channel; workers will drain remaining
        // requests and then exit when `recv` returns `Err`.
        {
            let mut guard = self.sender.lock().expect("sender lock poisoned");
            guard.take();
        }

        // Join all worker threads.
        {
            let mut threads = self.threads.lock().expect("threads lock poisoned");
            for slot in threads.iter_mut() {
                if let Some(h) = slot.take() {
                    let _ = h.join();
                }
            }
        }

        self.registry.close_all();
    }
}

impl Drop for SyncFileDevice {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    // -- Callback helper --------------------------------------------------

    /// Shared state for I/O completion callbacks in tests.
    struct CallbackState {
        completed: AtomicBool,
        status: Mutex<Option<IoStatus>>,
        bytes: AtomicU32,
    }

    impl CallbackState {
        fn new() -> Self {
            Self {
                completed: AtomicBool::new(false),
                status: Mutex::new(None),
                bytes: AtomicU32::new(0),
            }
        }

        /// Spin-wait until the callback fires, or panic on timeout.
        fn wait(&self, timeout: Duration) {
            let start = Instant::now();
            while !self.completed.load(Ordering::Acquire) {
                assert!(
                    start.elapsed() < timeout,
                    "I/O callback did not fire within {timeout:?}",
                );
                thread::yield_now();
            }
        }

        fn assert_success(&self, expected_bytes: u32) {
            let guard = self.status.lock().unwrap();
            assert_eq!(*guard, Some(IoStatus::Success));
            assert_eq!(self.bytes.load(Ordering::Acquire), expected_bytes);
        }
    }

    /// Generic completion callback used by all async tests.
    ///
    /// # Safety
    ///
    /// `context` must point to a valid [`CallbackState`] that outlives the
    /// I/O operation.
    unsafe fn test_callback(context: *mut u8, status: IoStatus, bytes: u32) {
        // SAFETY: Caller guarantees `context` is a valid `*const CallbackState`.
        let state = unsafe { &*context.cast::<CallbackState>() };
        *state.status.lock().unwrap() = Some(status);
        state.bytes.store(bytes, Ordering::Release);
        state.completed.store(true, Ordering::Release);
    }

    const SECTOR: u32 = 512;
    const TIMEOUT: Duration = Duration::from_secs(5);

    fn make_device(dir: &TempDir, segment_size: u64) -> SyncFileDevice {
        SyncFileDevice::new(dir.path(), "log.", SECTOR, segment_size, 4).unwrap()
    }

    // -- Tests ------------------------------------------------------------

    #[test]
    fn create_device_and_verify_config() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(&dir, 1 << 30);
        assert_eq!(dev.sector_size(), 512);
        assert_eq!(dev.segment_size(), 1 << 30);
        assert_eq!(dev.max_outstanding_io(), 1024);
        assert_eq!(dev.size(), 0);
    }

    #[test]
    fn sync_write_and_read_back() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(&dir, 1 << 20);

        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let n = dev.write_sync(0, &data).unwrap();
        assert_eq!(n, 4096);

        let mut buf = vec![0u8; 4096];
        let n = dev.read_sync(0, &mut buf).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(buf, data);
    }

    #[test]
    fn async_write_and_read_back() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(&dir, 1 << 20);

        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

        // Async write.
        let w_state = CallbackState::new();
        let result = {
            // SAFETY: `data` and `w_state` live until after `w_state.wait()`.
            unsafe {
                dev.write_async(
                    data.as_ptr(),
                    0,
                    4096,
                    test_callback,
                    std::ptr::addr_of!(w_state) as *mut u8,
                )
            }
        };
        assert!(matches!(result, IoRequestResult::Submitted));
        w_state.wait(TIMEOUT);
        w_state.assert_success(4096);

        // Async read.
        let mut buf = vec![0u8; 4096];
        let r_state = CallbackState::new();
        let result = {
            // SAFETY: `buf` and `r_state` live until after `r_state.wait()`.
            unsafe {
                dev.read_async(
                    0,
                    buf.as_mut_ptr(),
                    4096,
                    test_callback,
                    std::ptr::addr_of!(r_state) as *mut u8,
                )
            }
        };
        assert!(matches!(result, IoRequestResult::Submitted));
        r_state.wait(TIMEOUT);
        r_state.assert_success(4096);

        assert_eq!(buf, data);
    }

    #[test]
    fn multiple_segments() {
        let dir = TempDir::new().unwrap();
        let seg_sz = 4096u64;
        let dev = make_device(&dir, seg_sz);

        let d0 = vec![0xAAu8; 512];
        let d1 = vec![0xBBu8; 512];
        let d2 = vec![0xCCu8; 512];

        dev.write_sync(0, &d0).unwrap();
        dev.write_sync(seg_sz, &d1).unwrap();
        dev.write_sync(seg_sz * 2, &d2).unwrap();

        let mut buf = vec![0u8; 512];

        dev.read_sync(0, &mut buf).unwrap();
        assert_eq!(buf, d0);

        dev.read_sync(seg_sz, &mut buf).unwrap();
        assert_eq!(buf, d1);

        dev.read_sync(seg_sz * 2, &mut buf).unwrap();
        assert_eq!(buf, d2);

        // Segment files should exist on disk.
        assert!(dir.path().join("log.0").exists());
        assert!(dir.path().join("log.1").exists());
        assert!(dir.path().join("log.2").exists());
    }

    #[test]
    fn truncate_removes_segments() {
        let dir = TempDir::new().unwrap();
        let seg_sz = 4096u64;
        let dev = make_device(&dir, seg_sz);

        let data = vec![0xDDu8; 512];
        dev.write_sync(0, &data).unwrap();
        dev.write_sync(seg_sz, &data).unwrap();
        dev.write_sync(seg_sz * 2, &data).unwrap();

        assert!(dir.path().join("log.0").exists());
        assert!(dir.path().join("log.1").exists());
        assert!(dir.path().join("log.2").exists());

        // Truncate segments with index < 2 (i.e. segments 0 and 1).
        dev.truncate_until(seg_sz * 2);

        assert!(
            !dir.path().join("log.0").exists(),
            "segment 0 should be removed"
        );
        assert!(
            !dir.path().join("log.1").exists(),
            "segment 1 should be removed"
        );
        assert!(
            dir.path().join("log.2").exists(),
            "segment 2 should survive"
        );
    }

    #[test]
    fn concurrent_io() {
        let dir = TempDir::new().unwrap();
        let dev = Arc::new(make_device(&dir, 1 << 20));

        let mut handles = Vec::new();
        for t in 0..4u8 {
            let dev = Arc::clone(&dev);
            handles.push(thread::spawn(move || {
                let offset = u64::from(t) * 4096;
                let data = vec![t.wrapping_add(1); 4096];
                dev.write_sync(offset, &data).unwrap();

                let mut buf = vec![0u8; 4096];
                dev.read_sync(offset, &mut buf).unwrap();
                assert_eq!(buf, data, "thread {t} read mismatch");
            }));
        }

        for h in handles {
            h.join().expect("worker thread panicked");
        }
    }

    #[test]
    fn large_write_read() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(&dir, 2 << 20); // 2 MiB segment

        let size = 1024 * 1024; // 1 MiB
        let data: Vec<u8> = (0..size as u32).map(|i| (i % 251) as u8).collect();

        dev.write_sync(0, &data).unwrap();

        let mut buf = vec![0u8; size];
        dev.read_sync(0, &mut buf).unwrap();
        assert_eq!(buf, data);
    }

    #[test]
    fn device_close_cleanup() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(&dir, 1 << 20);

        let data = vec![0xEEu8; 512];
        dev.write_sync(0, &data).unwrap();

        // Close explicitly — threads should be joined.
        dev.close();

        // Double-close is a safe no-op.
        dev.close();

        // All thread handles should have been consumed.
        let threads = dev.threads.lock().unwrap();
        assert!(
            threads.iter().all(Option::is_none),
            "all worker handles should be joined",
        );
    }

    // -- H1: Alignment validation tests -----------------------------------

    #[test]
    fn read_sync_rejects_misaligned_offset() {
        let dir = TempDir::new().unwrap();
        let dev = SyncFileDevice::new(dir.path(), "test", 512, 1 << 20, 2).unwrap();
        let mut buf = vec![0u8; 512];
        let result = dev.read_sync(1, &mut buf); // offset 1 is not 512-aligned
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn read_sync_rejects_misaligned_len() {
        let dir = TempDir::new().unwrap();
        let dev = SyncFileDevice::new(dir.path(), "test", 512, 1 << 20, 2).unwrap();
        let mut buf = vec![0u8; 100]; // 100 is not 512-aligned
        let result = dev.read_sync(0, &mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn read_sync_rejects_zero_len() {
        let dir = TempDir::new().unwrap();
        let dev = SyncFileDevice::new(dir.path(), "test", 512, 1 << 20, 2).unwrap();
        let mut buf = vec![];
        let result = dev.read_sync(0, &mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn write_sync_rejects_misaligned_offset() {
        let dir = TempDir::new().unwrap();
        let dev = SyncFileDevice::new(dir.path(), "test", 512, 1 << 20, 2).unwrap();
        let buf = vec![0u8; 512];
        let result = dev.write_sync(3, &buf); // offset 3 is not aligned
        assert!(result.is_err());
    }

    #[test]
    fn write_sync_rejects_misaligned_len() {
        let dir = TempDir::new().unwrap();
        let dev = SyncFileDevice::new(dir.path(), "test", 512, 1 << 20, 2).unwrap();
        let buf = vec![0u8; 100];
        let result = dev.write_sync(0, &buf);
        assert!(result.is_err());
    }

    #[test]
    fn write_sync_rejects_zero_len() {
        let dir = TempDir::new().unwrap();
        let dev = SyncFileDevice::new(dir.path(), "test", 512, 1 << 20, 2).unwrap();
        let buf = vec![];
        let result = dev.write_sync(0, &buf);
        assert!(result.is_err());
    }
}
