//! io_uring-backed storage device.
//!
//! [`UringDevice`] implements the FASTER [`Device`](faster_core::device::Device)
//! trait using Linux io_uring for true kernel-async I/O. A dedicated I/O thread
//! owns the [`Ring`](crate::Ring) and processes submissions and completions,
//! invoking callbacks when operations finish.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────────┐     mpsc::channel     ┌─────────────────────┐
//! │  caller thread  │ ──────────────────►  │     I/O thread       │
//! │  read_async()   │                      │  owns Ring           │
//! │  write_async()  │                      │  submit SQEs         │
//! └────────────────┘                      │  reap CQEs → callback│
//!                                          └─────────────────────┘
//! ```
//!
//! - **Async I/O** (`read_async` / `write_async`) dispatches commands to the
//!   I/O thread via a lock-free channel. The I/O thread batches submissions,
//!   flushes them to the kernel, and invokes completion callbacks on reap.
//! - **Sync I/O** (`read_sync` / `write_sync`) bypasses the ring entirely,
//!   using `pread`/`pwrite` directly on the calling thread (used for recovery
//!   and metadata writes — cold path).
//!
//! # O_DIRECT Support
//!
//! When [`UringConfig::direct_io`](crate::UringConfig) is `true`, segment files
//! are opened with `O_DIRECT` for async operations, bypassing the page cache.
//! Sync operations use separate non-`O_DIRECT` file handles so that callers
//! are not required to provide sector-aligned buffers on the recovery path.
//!
//! # Cross-Segment I/O
//!
//! Requests that span segment boundaries are automatically split into per-segment
//! chunks. Each chunk is submitted as a separate SQE; the completion callback
//! fires only when all chunks finish (or on first error).
//!
//! # Crash Analysis
//!
//! - **Write `Submitted` → callback**: Data reached the kernel page cache (or
//!   the drive if `O_DIRECT`), but is **not** durable. Power failure may lose it.
//! - **Durability**: Requires an explicit `fsync` via the ring (not yet exposed
//!   through the `Device` trait; planned for the WAL integration).
//! - **Shutdown**: `close()` / `Drop` drops the channel sender, causing the I/O
//!   thread to drain all in-flight operations before exiting.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};

use faster_core::device::{Device, IoCompletionCallback, IoRequestResult, IoStatus};

use crate::ring::{Ring, UringConfig};

// ---------------------------------------------------------------------------
// Segment registry
// ---------------------------------------------------------------------------

/// Manages open file handles for segment files.
///
/// Segment files are named `{prefix}{segment_index}` under `base_path` and
/// opened lazily on first access. Two registries are maintained:
///
/// - **async_segments**: Opened with `O_DIRECT` (if configured) for ring I/O.
/// - **sync_segments**: Always opened without `O_DIRECT` for `pread`/`pwrite`.
struct SegmentRegistry {
    async_segments: RwLock<HashMap<u64, Arc<File>>>,
    sync_segments: RwLock<HashMap<u64, Arc<File>>>,
    base_path: PathBuf,
    prefix: String,
    direct_io: bool,
}

impl SegmentRegistry {
    fn new(base_path: PathBuf, prefix: String, direct_io: bool) -> Self {
        Self {
            async_segments: RwLock::new(HashMap::new()),
            sync_segments: RwLock::new(HashMap::new()),
            base_path,
            prefix,
            direct_io,
        }
    }

    fn segment_path(&self, segment_index: u64) -> PathBuf {
        self.base_path
            .join(format!("{}{}", self.prefix, segment_index))
    }

    /// Get (or lazily open) a file handle for async I/O (may be O_DIRECT).
    fn get_or_open_async(&self, segment_index: u64) -> io::Result<Arc<File>> {
        Self::get_or_open_from(
            &self.async_segments,
            &self.base_path,
            &self.prefix,
            segment_index,
            self.direct_io,
        )
    }

    /// Get (or lazily open) a file handle for sync I/O (never O_DIRECT).
    fn get_or_open_sync(&self, segment_index: u64) -> io::Result<Arc<File>> {
        Self::get_or_open_from(
            &self.sync_segments,
            &self.base_path,
            &self.prefix,
            segment_index,
            false,
        )
    }

    fn get_or_open_from(
        segments: &RwLock<HashMap<u64, Arc<File>>>,
        base_path: &Path,
        prefix: &str,
        segment_index: u64,
        direct_io: bool,
    ) -> io::Result<Arc<File>> {
        // Fast path: read lock.
        {
            let guard = segments.read().expect("segment registry poisoned");
            if let Some(f) = guard.get(&segment_index) {
                return Ok(Arc::clone(f));
            }
        }

        // Slow path: write lock with double-check.
        let mut guard = segments.write().expect("segment registry poisoned");
        if let Some(f) = guard.get(&segment_index) {
            return Ok(Arc::clone(f));
        }

        let path = base_path.join(format!("{prefix}{segment_index}"));
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        if direct_io {
            opts.custom_flags(libc::O_DIRECT);
        }
        let file = Arc::new(opts.open(path)?);
        guard.insert(segment_index, Arc::clone(&file));
        Ok(file)
    }

    /// Remove a segment from both registries and delete the file from disk.
    fn remove_segment(&self, segment_index: u64) {
        {
            let mut guard = self
                .async_segments
                .write()
                .expect("segment registry poisoned");
            guard.remove(&segment_index);
        }
        {
            let mut guard = self
                .sync_segments
                .write()
                .expect("segment registry poisoned");
            guard.remove(&segment_index);
        }
        let _ = fs::remove_file(self.segment_path(segment_index));
    }

    /// Return all segment indices currently tracked (for truncation).
    fn segment_indices(&self) -> Vec<u64> {
        let guard = self
            .async_segments
            .read()
            .expect("segment registry poisoned");
        let sync_guard = self
            .sync_segments
            .read()
            .expect("segment registry poisoned");
        let mut indices: Vec<u64> = guard.keys().copied().collect();
        for &idx in sync_guard.keys() {
            if !indices.contains(&idx) {
                indices.push(idx);
            }
        }
        indices
    }

    /// Close all cached file handles.
    fn close_all(&self) {
        self.async_segments
            .write()
            .expect("segment registry poisoned")
            .clear();
        self.sync_segments
            .write()
            .expect("segment registry poisoned")
            .clear();
    }
}

// ---------------------------------------------------------------------------
// I/O command
// ---------------------------------------------------------------------------

/// Discriminant for read vs write commands.
enum IoCommandKind {
    /// Read from storage into a buffer.
    Read,
    /// Write from a buffer to storage.
    Write,
}

/// A pending I/O operation sent from the device to the I/O thread.
struct IoCommand {
    kind: IoCommandKind,
    offset: u64,
    /// For reads: destination buffer. For writes: source buffer (stored as
    /// `*mut u8` for uniform representation; the write path only reads).
    buffer: *mut u8,
    len: u32,
    callback: IoCompletionCallback,
    context: *mut u8,
}

// SAFETY: The raw pointers (`buffer` and `context`) are guaranteed valid by
// the `Device` trait contract until the completion callback fires. The I/O
// thread creates short-lived slices from `buffer` for ring submission and
// invokes the callback exactly once. No pointer is retained after callback.
unsafe impl Send for IoCommand {}

// ---------------------------------------------------------------------------
// Pending operation tracking (I/O thread state)
// ---------------------------------------------------------------------------

/// Tracks a single SQE awaiting completion on the I/O thread.
enum PendingEntry {
    /// Single-segment I/O — invoke callback directly on completion.
    Direct {
        callback: IoCompletionCallback,
        context: *mut u8,
        len: u32,
        /// Prevents the fd from being closed while the kernel references it.
        _file_guard: Arc<File>,
    },
    /// One chunk of a multi-segment I/O group.
    Chunk {
        group_id: u64,
        /// Prevents the fd from being closed while the kernel references it.
        _file_guard: Arc<File>,
    },
}

/// Tracks a group of chunks for cross-segment I/O.
struct GroupState {
    callback: IoCompletionCallback,
    context: *mut u8,
    total_len: u32,
    remaining: u32,
    error: Option<i32>,
}

// ---------------------------------------------------------------------------
// I/O thread
// ---------------------------------------------------------------------------

/// Main loop for the I/O thread.
///
/// Owns the [`Ring`] and alternates between receiving commands from the channel
/// and reaping completions. Batches naturally: when multiple commands arrive
/// between reaps, they are submitted together.
fn io_thread_main(
    rx: Receiver<IoCommand>,
    config: UringConfig,
    registry: Arc<SegmentRegistry>,
    segment_size: u64,
) {
    let mut ring = match Ring::new(config) {
        Ok(r) => r,
        Err(_) => {
            // Ring creation failed — fail every command that arrives.
            for cmd in rx.iter() {
                // SAFETY: `cmd.context` is valid per the Device trait contract
                // and we invoke the callback exactly once.
                unsafe {
                    (cmd.callback)(cmd.context, IoStatus::Error(libc::EIO), 0);
                }
            }
            return;
        }
    };

    let mut pending: HashMap<u64, PendingEntry> = HashMap::new();
    let mut groups: HashMap<u64, GroupState> = HashMap::new();
    let mut next_id: u64 = 1;
    let mut next_group: u64 = 1;
    let mut shutdown = false;

    loop {
        // ── Phase 1: Collect commands ──────────────────────────────────
        let first_cmd = if ring.inflight() == 0 && !shutdown {
            // Nothing in flight — block on channel to avoid spinning.
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => {
                    shutdown = true;
                    None
                }
            }
        } else {
            match rx.try_recv() {
                Ok(cmd) => Some(cmd),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    shutdown = true;
                    None
                }
            }
        };

        if let Some(cmd) = first_cmd {
            submit_command(
                &mut ring,
                &registry,
                segment_size,
                cmd,
                &mut pending,
                &mut groups,
                &mut next_id,
                &mut next_group,
            );

            // Drain the channel to batch multiple submissions.
            loop {
                match rx.try_recv() {
                    Ok(cmd) => {
                        submit_command(
                            &mut ring,
                            &registry,
                            segment_size,
                            cmd,
                            &mut pending,
                            &mut groups,
                            &mut next_id,
                            &mut next_group,
                        );
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        shutdown = true;
                        break;
                    }
                }
            }
        }

        // ── Phase 2: Reap completions ─────────────────────────────────
        if ring.inflight() > 0 {
            match ring.reap_completions() {
                Ok(completions) => {
                    for (user_data, result) in completions {
                        handle_completion(user_data, result, &mut pending, &mut groups);
                    }
                }
                Err(_) => {
                    // Fatal ring error — fail all pending ops and exit.
                    fail_all_pending(&mut pending, &mut groups);
                    return;
                }
            }
        }

        // ── Phase 3: Exit when shutdown and nothing inflight ──────────
        if shutdown && ring.inflight() == 0 {
            return;
        }
    }
}

/// Submit a single command to the ring, splitting across segments if needed.
#[allow(clippy::too_many_arguments)]
fn submit_command(
    ring: &mut Ring,
    registry: &SegmentRegistry,
    segment_size: u64,
    cmd: IoCommand,
    pending: &mut HashMap<u64, PendingEntry>,
    groups: &mut HashMap<u64, GroupState>,
    next_id: &mut u64,
    next_group: &mut u64,
) {
    let end_offset = cmd.offset + u64::from(cmd.len);
    let start_seg = cmd.offset / segment_size;
    let end_seg = (end_offset - 1) / segment_size;

    if start_seg == end_seg {
        // Common case: single segment.
        submit_single_segment(ring, registry, segment_size, cmd, pending, next_id);
    } else {
        // Rare case: cross-segment I/O.
        submit_cross_segment(
            ring,
            registry,
            segment_size,
            cmd,
            pending,
            groups,
            next_id,
            next_group,
        );
    }
}

/// Submit a command that fits within a single segment.
fn submit_single_segment(
    ring: &mut Ring,
    registry: &SegmentRegistry,
    segment_size: u64,
    cmd: IoCommand,
    pending: &mut HashMap<u64, PendingEntry>,
    next_id: &mut u64,
) {
    let seg_idx = cmd.offset / segment_size;
    let file = match registry.get_or_open_async(seg_idx) {
        Ok(f) => f,
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc::EIO);
            // SAFETY: The Device trait contract guarantees `cmd.context` is
            // valid and the callback may be invoked exactly once.
            unsafe {
                (cmd.callback)(cmd.context, IoStatus::Error(errno), 0);
            }
            return;
        }
    };

    let fd = file.as_raw_fd();
    let file_off = cmd.offset % segment_size;
    let user_data = *next_id;
    *next_id += 1;

    let submit_result = match cmd.kind {
        IoCommandKind::Read => {
            // SAFETY: `cmd.buffer` is valid for `cmd.len` bytes per the
            // Device trait contract and remains valid until the callback fires.
            let buf = unsafe { std::slice::from_raw_parts_mut(cmd.buffer, cmd.len as usize) };
            submit_with_retry(ring, |r| r.submit_read(fd, buf, file_off, user_data))
        }
        IoCommandKind::Write => {
            // SAFETY: Same validity guarantee as read. We create a shared
            // slice — the I/O thread never writes through this pointer.
            let buf =
                unsafe { std::slice::from_raw_parts(cmd.buffer.cast_const(), cmd.len as usize) };
            submit_with_retry(ring, |r| r.submit_write(fd, buf, file_off, user_data))
        }
    };

    match submit_result {
        Ok(()) => {
            pending.insert(
                user_data,
                PendingEntry::Direct {
                    callback: cmd.callback,
                    context: cmd.context,
                    len: cmd.len,
                    _file_guard: file,
                },
            );
        }
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc::EIO);
            // SAFETY: Callback invoked exactly once on submission failure.
            unsafe {
                (cmd.callback)(cmd.context, IoStatus::Error(errno), 0);
            }
        }
    }
}

/// Submit a command that spans multiple segments.
#[allow(clippy::too_many_arguments)]
fn submit_cross_segment(
    ring: &mut Ring,
    registry: &SegmentRegistry,
    segment_size: u64,
    cmd: IoCommand,
    pending: &mut HashMap<u64, PendingEntry>,
    groups: &mut HashMap<u64, GroupState>,
    next_id: &mut u64,
    next_group: &mut u64,
) {
    let group_id = *next_group;
    *next_group += 1;

    let mut chunk_count = 0u32;
    let mut buf_pos = 0usize;
    let mut offset = cmd.offset;
    let mut remaining = cmd.len as usize;
    let mut submit_error: Option<i32> = None;

    while remaining > 0 {
        let seg_idx = offset / segment_size;
        let file_off = offset % segment_size;
        let space = (segment_size - file_off) as usize;
        let chunk = remaining.min(space);

        let file = match registry.get_or_open_async(seg_idx) {
            Ok(f) => f,
            Err(e) => {
                submit_error = Some(e.raw_os_error().unwrap_or(libc::EIO));
                break;
            }
        };

        let fd = file.as_raw_fd();
        let user_data = *next_id;
        *next_id += 1;

        let result = match cmd.kind {
            IoCommandKind::Read => {
                // SAFETY: `cmd.buffer.add(buf_pos)` is within bounds because
                // `buf_pos + chunk <= cmd.len` (loop invariant).
                let buf = unsafe { std::slice::from_raw_parts_mut(cmd.buffer.add(buf_pos), chunk) };
                submit_with_retry(ring, |r| r.submit_read(fd, buf, file_off, user_data))
            }
            IoCommandKind::Write => {
                // SAFETY: Same bounds guarantee as read path.
                let buf = unsafe {
                    std::slice::from_raw_parts(cmd.buffer.cast_const().add(buf_pos), chunk)
                };
                submit_with_retry(ring, |r| r.submit_write(fd, buf, file_off, user_data))
            }
        };

        match result {
            Ok(()) => {
                pending.insert(
                    user_data,
                    PendingEntry::Chunk {
                        group_id,
                        _file_guard: file,
                    },
                );
                chunk_count += 1;
            }
            Err(e) => {
                submit_error = Some(e.raw_os_error().unwrap_or(libc::EIO));
                break;
            }
        }

        buf_pos += chunk;
        remaining -= chunk;
        offset += chunk as u64;
    }

    if chunk_count == 0 {
        // Nothing submitted — fail immediately.
        let errno = submit_error.unwrap_or(libc::EIO);
        // SAFETY: Callback invoked exactly once.
        unsafe {
            (cmd.callback)(cmd.context, IoStatus::Error(errno), 0);
        }
    } else {
        groups.insert(
            group_id,
            GroupState {
                callback: cmd.callback,
                context: cmd.context,
                total_len: cmd.len,
                remaining: chunk_count,
                error: submit_error,
            },
        );
    }
}

/// Submit an operation with one retry on SQ-full.
///
/// If the submission queue is full (`WouldBlock`), flush pending entries and
/// retry once.
fn submit_with_retry<F>(ring: &mut Ring, mut f: F) -> io::Result<()>
where
    F: FnMut(&mut Ring) -> io::Result<()>,
{
    match f(ring) {
        Ok(()) => Ok(()),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            // SQ full — flush to kernel to free entries, then retry.
            ring.submit()?;
            f(ring)
        }
        Err(e) => Err(e),
    }
}

/// Process a single CQE.
fn handle_completion(
    user_data: u64,
    result: i32,
    pending: &mut HashMap<u64, PendingEntry>,
    groups: &mut HashMap<u64, GroupState>,
) {
    let entry = match pending.remove(&user_data) {
        Some(e) => e,
        None => return,
    };

    match entry {
        PendingEntry::Direct {
            callback,
            context,
            len,
            ..
        } => {
            let (status, bytes) = if result >= 0 {
                (IoStatus::Success, len)
            } else {
                (IoStatus::Error(-result), 0)
            };
            // SAFETY: `context` is valid per Device contract. Invoked once.
            unsafe {
                callback(context, status, bytes);
            }
        }
        PendingEntry::Chunk { group_id, .. } => {
            if let Some(group) = groups.get_mut(&group_id) {
                if result < 0 && group.error.is_none() {
                    group.error = Some(-result);
                }
                group.remaining -= 1;

                if group.remaining == 0 {
                    let group = groups.remove(&group_id).unwrap();
                    let (status, bytes) = match group.error {
                        None => (IoStatus::Success, group.total_len),
                        Some(errno) => (IoStatus::Error(errno), 0),
                    };
                    // SAFETY: `context` valid per Device contract. Invoked once.
                    unsafe {
                        (group.callback)(group.context, status, bytes);
                    }
                }
            }
        }
    }
}

/// Fail all pending operations on a fatal ring error.
fn fail_all_pending(
    pending: &mut HashMap<u64, PendingEntry>,
    groups: &mut HashMap<u64, GroupState>,
) {
    for (_, entry) in pending.drain() {
        match entry {
            PendingEntry::Direct {
                callback, context, ..
            } => {
                // SAFETY: `context` valid per Device trait contract.
                unsafe {
                    callback(context, IoStatus::Error(libc::EIO), 0);
                }
            }
            PendingEntry::Chunk { group_id, .. } => {
                if let Some(group) = groups.get_mut(&group_id) {
                    group.error = Some(libc::EIO);
                    group.remaining -= 1;
                    if group.remaining == 0 {
                        let group = groups.remove(&group_id).unwrap();
                        // SAFETY: `context` valid per Device trait contract.
                        unsafe {
                            (group.callback)(group.context, IoStatus::Error(libc::EIO), 0);
                        }
                    }
                }
            }
        }
    }

    // Fail any remaining groups (edge case: all their chunks were already
    // processed above but the group still has remaining > 0 from partial
    // submission).
    for (_, group) in groups.drain() {
        // SAFETY: `context` valid per Device trait contract.
        unsafe {
            (group.callback)(group.context, IoStatus::Error(libc::EIO), 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Platform-specific positional I/O for sync path
// ---------------------------------------------------------------------------

/// Read exactly `buf.len()` bytes via `pread`, zero-filling on early EOF.
fn read_complete_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut pos = 0usize;
    while pos < buf.len() {
        match file.read_at(&mut buf[pos..], offset + pos as u64) {
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

/// Write all of `buf` via `pwrite`, retrying on partial writes.
fn write_complete_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut pos = 0usize;
    while pos < buf.len() {
        match file.write_at(&buf[pos..], offset + pos as u64) {
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
// UringDevice
// ---------------------------------------------------------------------------

/// Configuration for creating a [`UringDevice`].
#[derive(Debug, Clone)]
pub struct UringDeviceConfig {
    /// Base directory for segment files. Created if it does not exist.
    pub base_path: PathBuf,
    /// Prefix for segment file names (e.g., `"log."`).
    pub prefix: String,
    /// Sector size for alignment validation (must be power of two).
    pub sector_size: u32,
    /// Size of each segment file in bytes.
    pub segment_size: u64,
    /// io_uring ring configuration.
    pub ring_config: UringConfig,
}

/// io_uring-backed [`Device`](faster_core::device::Device) implementation.
///
/// See the [module-level documentation](self) for architecture details.
pub struct UringDevice {
    sender: Mutex<Option<Sender<IoCommand>>>,
    io_thread: Mutex<Option<JoinHandle<()>>>,
    registry: Arc<SegmentRegistry>,
    sector_size: u32,
    segment_size: u64,
    queue_depth: u32,
    high_water: AtomicU64,
    closed: AtomicBool,
}

impl UringDevice {
    /// Create a new `UringDevice`.
    ///
    /// Spawns a dedicated I/O thread that owns the io_uring ring.
    /// `config.base_path` is created if it does not exist.
    ///
    /// # Panics
    ///
    /// Panics if `sector_size` is not a power of two, or if `segment_size` is
    /// zero.
    ///
    /// # Errors
    ///
    /// Returns an error if the base directory cannot be created or the I/O
    /// thread cannot be spawned.
    pub fn new(config: UringDeviceConfig) -> io::Result<Self> {
        assert!(
            config.sector_size.is_power_of_two(),
            "sector_size must be a power of two"
        );
        assert!(config.segment_size > 0, "segment_size must be > 0");

        fs::create_dir_all(&config.base_path)?;

        let registry = Arc::new(SegmentRegistry::new(
            config.base_path,
            config.prefix,
            config.ring_config.direct_io,
        ));

        let queue_depth = config.ring_config.queue_depth;
        let segment_size = config.segment_size;
        let ring_config = config.ring_config.clone();
        let reg_clone = Arc::clone(&registry);

        let (tx, rx) = mpsc::channel::<IoCommand>();

        let io_thread = thread::Builder::new()
            .name("uring-io".to_string())
            .spawn(move || {
                io_thread_main(rx, ring_config, reg_clone, segment_size);
            })
            .map_err(io::Error::other)?;

        Ok(Self {
            sender: Mutex::new(Some(tx)),
            io_thread: Mutex::new(Some(io_thread)),
            registry,
            sector_size: config.sector_size,
            segment_size: config.segment_size,
            queue_depth,
            high_water: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        })
    }

    /// Send a command to the I/O thread.
    fn send_command(&self, cmd: IoCommand) -> IoRequestResult {
        let guard = self.sender.lock().expect("sender lock poisoned");
        match guard.as_ref() {
            Some(tx) => match tx.send(cmd) {
                Ok(()) => IoRequestResult::Submitted,
                Err(_) => IoRequestResult::Error(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "I/O thread shut down",
                )),
            },
            None => IoRequestResult::Error(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "device is closed",
            )),
        }
    }

    /// Validate alignment for an I/O operation.
    fn validate_alignment(&self, offset: u64, len: u32) -> Result<(), IoRequestResult> {
        if offset % u64::from(self.sector_size) != 0 {
            return Err(IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            )));
        }
        if u64::from(len) % u64::from(self.sector_size) != 0 {
            return Err(IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "len {len} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            )));
        }
        if len == 0 {
            return Err(IoRequestResult::Error(io::Error::new(
                io::ErrorKind::InvalidInput,
                "len must be > 0",
            )));
        }
        Ok(())
    }

    /// Validate alignment for sync operations (returns `io::Error`).
    fn validate_sync_alignment(&self, offset: u64, len: usize) -> io::Result<()> {
        if offset % u64::from(self.sector_size) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "offset {offset} not sector-aligned (sector_size={})",
                    self.sector_size
                ),
            ));
        }
        if len % self.sector_size as usize != 0 {
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

impl Device for UringDevice {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn segment_size(&self) -> u64 {
        self.segment_size
    }

    fn max_outstanding_io(&self) -> u32 {
        self.queue_depth
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
            return e;
        }

        self.send_command(IoCommand {
            kind: IoCommandKind::Read,
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
        if let Err(e) = self.validate_alignment(offset, len) {
            return e;
        }

        // Eagerly update the high-water mark (consistent with SyncFileDevice).
        self.high_water
            .fetch_max(offset + u64::from(len), Ordering::Relaxed);

        self.send_command(IoCommand {
            kind: IoCommandKind::Write,
            offset,
            buffer: source.cast_mut(),
            len,
            callback,
            context,
        })
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        let total = dest.len();
        self.validate_sync_alignment(offset, total)?;

        let mut remaining = total;
        let mut buf_pos = 0usize;
        let mut cur = offset;

        while remaining > 0 {
            let seg_idx = cur / self.segment_size;
            let file_off = cur % self.segment_size;
            let chunk = remaining.min((self.segment_size - file_off) as usize);

            let file = self.registry.get_or_open_sync(seg_idx)?;
            read_complete_at(&file, &mut dest[buf_pos..buf_pos + chunk], file_off)?;

            buf_pos += chunk;
            remaining -= chunk;
            cur += chunk as u64;
        }

        Ok(total as u32)
    }

    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32> {
        let total = source.len();
        self.validate_sync_alignment(offset, total)?;

        let mut remaining = total;
        let mut buf_pos = 0usize;
        let mut cur = offset;

        while remaining > 0 {
            let seg_idx = cur / self.segment_size;
            let file_off = cur % self.segment_size;
            let chunk = remaining.min((self.segment_size - file_off) as usize);

            let file = self.registry.get_or_open_sync(seg_idx)?;
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
        let to_remove: Vec<u64> = self
            .registry
            .segment_indices()
            .into_iter()
            .filter(|&idx| idx < threshold)
            .collect();
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

        // Drop the sender to close the channel. The I/O thread will drain
        // remaining inflight operations and then exit.
        {
            let mut guard = self.sender.lock().expect("sender lock poisoned");
            guard.take();
        }

        // Join the I/O thread.
        {
            let mut guard = self.io_thread.lock().expect("thread lock poisoned");
            if let Some(h) = guard.take() {
                let _ = h.join();
            }
        }

        self.registry.close_all();
    }
}

impl Drop for UringDevice {
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

    // ── Test helpers ──────────────────────────────────────────────────

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
            let status = self.status.lock().unwrap();
            assert_eq!(*status, Some(IoStatus::Success));
            assert_eq!(self.bytes.load(Ordering::Relaxed), expected_bytes);
        }
    }

    /// Completion callback for tests.
    ///
    /// # Safety
    ///
    /// `context` must point to a valid `Arc<CallbackState>` created by
    /// `Arc::into_raw`.
    unsafe fn test_callback(context: *mut u8, status: IoStatus, bytes: u32) {
        // SAFETY: Caller guarantees `context` is a valid `Arc<CallbackState>`
        // pointer produced by `Arc::into_raw`. We reconstruct the Arc to
        // decrement the reference count after use.
        let state = unsafe { Arc::from_raw(context as *const CallbackState) };
        *state.status.lock().unwrap() = Some(status);
        state.bytes.store(bytes, Ordering::Relaxed);
        state.completed.store(true, Ordering::Release);
    }

    const SECTOR: u32 = 4096;
    const SEGMENT: u64 = 1 << 20; // 1 MiB for tests (small segments)
    const TIMEOUT: Duration = Duration::from_secs(5);

    fn make_device(dir: &Path) -> UringDevice {
        UringDevice::new(UringDeviceConfig {
            base_path: dir.to_path_buf(),
            prefix: "test.".to_string(),
            sector_size: SECTOR,
            segment_size: SEGMENT,
            ring_config: UringConfig {
                queue_depth: 256,
                sq_poll: false,
                direct_io: false,
            },
        })
        .expect("create UringDevice")
    }

    /// Create a callback state and return (Arc, raw_ptr) for use with Device.
    fn make_callback() -> (Arc<CallbackState>, *mut u8) {
        let state = Arc::new(CallbackState::new());
        let ptr = Arc::into_raw(Arc::clone(&state)) as *mut u8;
        (state, ptr)
    }

    // ── Basic tests ──────────────────────────────────────────────────

    #[test]
    fn device_creation_and_properties() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());
        assert_eq!(dev.sector_size(), SECTOR);
        assert_eq!(dev.segment_size(), SEGMENT);
        assert_eq!(dev.size(), 0);
        dev.close();
    }

    #[test]
    fn sync_write_then_read() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());

        let mut data = vec![0u8; SECTOR as usize];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }

        let written = dev.write_sync(0, &data).expect("write_sync");
        assert_eq!(written, SECTOR);
        assert_eq!(dev.size(), u64::from(SECTOR));

        let mut read_buf = vec![0u8; SECTOR as usize];
        let read = dev.read_sync(0, &mut read_buf).expect("read_sync");
        assert_eq!(read, SECTOR);
        assert_eq!(read_buf, data);
    }

    #[test]
    fn async_write_then_read_single_page() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());

        // Write a page
        let mut data = vec![0u8; SECTOR as usize];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 251) as u8; // prime modulus for pattern
        }

        let (w_state, w_ctx) = make_callback();
        // SAFETY: `data.as_ptr()` is valid for SECTOR bytes and lives until
        // callback. `w_ctx` is a valid Arc<CallbackState> pointer.
        unsafe {
            let result = dev.write_async(data.as_ptr(), 0, SECTOR, test_callback, w_ctx);
            assert!(matches!(result, IoRequestResult::Submitted));
        }
        w_state.wait(TIMEOUT);
        w_state.assert_success(SECTOR);

        // Read it back
        let mut read_buf = vec![0u8; SECTOR as usize];
        let (r_state, r_ctx) = make_callback();
        // SAFETY: `read_buf.as_mut_ptr()` is valid for SECTOR bytes and lives
        // until callback.
        unsafe {
            let result = dev.read_async(0, read_buf.as_mut_ptr(), SECTOR, test_callback, r_ctx);
            assert!(matches!(result, IoRequestResult::Submitted));
        }
        r_state.wait(TIMEOUT);
        r_state.assert_success(SECTOR);

        assert_eq!(read_buf, data);
    }

    #[test]
    fn alignment_rejection() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());

        // Misaligned offset
        let mut buf = vec![0u8; SECTOR as usize];
        let (_, ctx) = make_callback();
        // SAFETY: Buffer is valid, we're testing error path.
        let result = unsafe { dev.read_async(1, buf.as_mut_ptr(), SECTOR, test_callback, ctx) };
        assert!(matches!(result, IoRequestResult::Error(_)));

        // Misaligned length
        let (_, ctx2) = make_callback();
        // SAFETY: Buffer is valid, we're testing error path.
        let result = unsafe { dev.read_async(0, buf.as_mut_ptr(), 100, test_callback, ctx2) };
        assert!(matches!(result, IoRequestResult::Error(_)));

        // Zero length
        let (_, ctx3) = make_callback();
        // SAFETY: Buffer is valid, we're testing error path.
        let result = unsafe { dev.read_async(0, buf.as_mut_ptr(), 0, test_callback, ctx3) };
        assert!(matches!(result, IoRequestResult::Error(_)));

        // Reclaim the leaked Arc pointers from error paths
        // SAFETY: These Arcs were created by make_callback and never consumed
        // by the callback since submission failed.
        unsafe {
            drop(Arc::from_raw(ctx as *const CallbackState));
            drop(Arc::from_raw(ctx2 as *const CallbackState));
            drop(Arc::from_raw(ctx3 as *const CallbackState));
        }
    }

    #[test]
    fn write_read_1000_pages() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());
        let page_size = SECTOR as usize;
        let num_pages = 1000;

        // Write 1000 pages asynchronously
        let mut write_data: Vec<Vec<u8>> = Vec::with_capacity(num_pages);
        let mut write_states: Vec<Arc<CallbackState>> = Vec::with_capacity(num_pages);

        for i in 0..num_pages {
            let mut page = vec![0u8; page_size];
            // Unique pattern: first 4 bytes = page index, rest = i % 256
            let idx_bytes = (i as u32).to_le_bytes();
            page[..4].copy_from_slice(&idx_bytes);
            for byte in &mut page[4..] {
                *byte = (i % 256) as u8;
            }
            write_data.push(page);
        }

        for (i, page) in write_data.iter().enumerate() {
            let (state, ctx) = make_callback();
            write_states.push(state);
            let offset = (i * page_size) as u64;
            // SAFETY: `page.as_ptr()` valid for page_size bytes. `ctx` valid.
            unsafe {
                let result =
                    dev.write_async(page.as_ptr(), offset, page_size as u32, test_callback, ctx);
                assert!(
                    matches!(result, IoRequestResult::Submitted),
                    "write {i} submission failed"
                );
            }
        }

        // Wait for all writes
        for (i, state) in write_states.iter().enumerate() {
            state.wait(Duration::from_secs(30));
            state.assert_success(page_size as u32);
            let _ = i;
        }

        // Read all 1000 pages back
        let mut read_bufs: Vec<Vec<u8>> = (0..num_pages).map(|_| vec![0u8; page_size]).collect();
        let mut read_states: Vec<Arc<CallbackState>> = Vec::with_capacity(num_pages);

        for (i, buf) in read_bufs.iter_mut().enumerate() {
            let (state, ctx) = make_callback();
            read_states.push(state);
            let offset = (i * page_size) as u64;
            // SAFETY: `buf.as_mut_ptr()` valid for page_size bytes.
            unsafe {
                let result = dev.read_async(
                    offset,
                    buf.as_mut_ptr(),
                    page_size as u32,
                    test_callback,
                    ctx,
                );
                assert!(
                    matches!(result, IoRequestResult::Submitted),
                    "read {i} submission failed"
                );
            }
        }

        // Wait and verify
        for (i, state) in read_states.iter().enumerate() {
            state.wait(Duration::from_secs(30));
            state.assert_success(page_size as u32);
            assert_eq!(read_bufs[i], write_data[i], "data mismatch at page {i}");
        }
    }

    #[test]
    fn stress_64_concurrent_ops() {
        let dir = TempDir::new().unwrap();
        let dev = Arc::new(make_device(dir.path()));
        let page_size = SECTOR as usize;
        let num_ops = 64usize;

        // Each thread writes a unique page then reads it back.
        let handles: Vec<_> = (0..num_ops)
            .map(|i| {
                let dev = Arc::clone(&dev);
                thread::spawn(move || {
                    let mut page = vec![0u8; page_size];
                    let tag = (i as u32).to_le_bytes();
                    page[..4].copy_from_slice(&tag);
                    page[4..].fill((i % 256) as u8);

                    let offset = (i * page_size) as u64;

                    // Write
                    let (w_state, w_ctx) = make_callback();
                    // SAFETY: page lives until callback fires.
                    unsafe {
                        let r = dev.write_async(
                            page.as_ptr(),
                            offset,
                            page_size as u32,
                            test_callback,
                            w_ctx,
                        );
                        assert!(matches!(r, IoRequestResult::Submitted));
                    }
                    w_state.wait(TIMEOUT);
                    w_state.assert_success(page_size as u32);

                    // Read
                    let mut rbuf = vec![0u8; page_size];
                    let (r_state, r_ctx) = make_callback();
                    // SAFETY: rbuf lives until callback fires.
                    unsafe {
                        let r = dev.read_async(
                            offset,
                            rbuf.as_mut_ptr(),
                            page_size as u32,
                            test_callback,
                            r_ctx,
                        );
                        assert!(matches!(r, IoRequestResult::Submitted));
                    }
                    r_state.wait(TIMEOUT);
                    r_state.assert_success(page_size as u32);

                    assert_eq!(rbuf, page, "data mismatch at op {i}");
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn cross_segment_write_read() {
        let dir = TempDir::new().unwrap();
        // Use a tiny segment size to force cross-segment I/O.
        let dev = UringDevice::new(UringDeviceConfig {
            base_path: dir.path().to_path_buf(),
            prefix: "xseg.".to_string(),
            sector_size: SECTOR,
            segment_size: SECTOR as u64, // 1 page per segment
            ring_config: UringConfig::default(),
        })
        .expect("create device");

        // Write 2 pages at once, spanning 2 segments.
        let page_size = SECTOR as usize;
        let total_len = page_size * 2;
        let mut data = vec![0u8; total_len];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }

        let (w_state, w_ctx) = make_callback();
        // SAFETY: data valid for total_len bytes.
        unsafe {
            let r = dev.write_async(data.as_ptr(), 0, total_len as u32, test_callback, w_ctx);
            assert!(matches!(r, IoRequestResult::Submitted));
        }
        w_state.wait(TIMEOUT);
        w_state.assert_success(total_len as u32);

        // Read it back
        let mut rbuf = vec![0u8; total_len];
        let (r_state, r_ctx) = make_callback();
        // SAFETY: rbuf valid for total_len bytes.
        unsafe {
            let r = dev.read_async(0, rbuf.as_mut_ptr(), total_len as u32, test_callback, r_ctx);
            assert!(matches!(r, IoRequestResult::Submitted));
        }
        r_state.wait(TIMEOUT);
        r_state.assert_success(total_len as u32);

        assert_eq!(rbuf, data);
    }

    #[test]
    fn truncate_removes_segments() {
        let dir = TempDir::new().unwrap();
        let dev = UringDevice::new(UringDeviceConfig {
            base_path: dir.path().to_path_buf(),
            prefix: "trunc.".to_string(),
            sector_size: SECTOR,
            segment_size: SECTOR as u64, // 1 page per segment
            ring_config: UringConfig::default(),
        })
        .expect("create device");

        // Write pages into segments 0, 1, 2
        for i in 0..3u64 {
            let data = vec![0xABu8; SECTOR as usize];
            dev.write_sync(i * u64::from(SECTOR), &data).expect("write");
        }

        // Verify all 3 segment files exist
        assert!(dir.path().join("trunc.0").exists());
        assert!(dir.path().join("trunc.1").exists());
        assert!(dir.path().join("trunc.2").exists());

        // Truncate before segment 2 (offset = 2 * SECTOR)
        dev.truncate_until(2 * u64::from(SECTOR));

        // Segments 0 and 1 should be gone
        assert!(!dir.path().join("trunc.0").exists());
        assert!(!dir.path().join("trunc.1").exists());
        assert!(dir.path().join("trunc.2").exists());
    }

    #[test]
    fn high_water_mark() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());
        assert_eq!(dev.size(), 0);

        // Sync write updates high water
        let data = vec![0u8; SECTOR as usize];
        dev.write_sync(0, &data).unwrap();
        assert_eq!(dev.size(), u64::from(SECTOR));

        // Write at higher offset
        dev.write_sync(u64::from(SECTOR) * 10, &data).unwrap();
        assert_eq!(dev.size(), u64::from(SECTOR) * 11);

        // Write at lower offset doesn't decrease
        dev.write_sync(0, &data).unwrap();
        assert_eq!(dev.size(), u64::from(SECTOR) * 11);
    }

    #[test]
    fn close_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());
        dev.close();
        dev.close(); // second close is a no-op
    }

    #[test]
    fn write_after_close_returns_error() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());
        dev.close();

        let data = vec![0u8; SECTOR as usize];
        let (_, ctx) = make_callback();
        // SAFETY: Testing error path after close.
        let result = unsafe { dev.write_async(data.as_ptr(), 0, SECTOR, test_callback, ctx) };
        assert!(matches!(result, IoRequestResult::Error(_)));

        // Reclaim leaked Arc
        // SAFETY: Arc was not consumed by callback.
        unsafe {
            drop(Arc::from_raw(ctx as *const CallbackState));
        }
    }

    // ── Benchmark-style: sequential throughput ───────────────────────

    #[test]
    fn sequential_write_read_throughput() {
        let dir = TempDir::new().unwrap();
        let dev = make_device(dir.path());
        let page_size = SECTOR as usize;
        let num_pages = 256;

        // Sequential write
        let data = vec![0xCDu8; page_size];
        let start = Instant::now();
        let mut states: Vec<Arc<CallbackState>> = Vec::with_capacity(num_pages);

        for i in 0..num_pages {
            let (state, ctx) = make_callback();
            states.push(state);
            let offset = (i * page_size) as u64;
            // SAFETY: data valid for page_size.
            unsafe {
                dev.write_async(data.as_ptr(), offset, page_size as u32, test_callback, ctx);
            }
        }

        for s in &states {
            s.wait(Duration::from_secs(30));
        }
        let write_elapsed = start.elapsed();

        // Sequential read
        let start = Instant::now();
        let mut read_bufs: Vec<Vec<u8>> = (0..num_pages).map(|_| vec![0u8; page_size]).collect();
        let mut read_states: Vec<Arc<CallbackState>> = Vec::with_capacity(num_pages);

        for (i, buf) in read_bufs.iter_mut().enumerate() {
            let (state, ctx) = make_callback();
            read_states.push(state);
            let offset = (i * page_size) as u64;
            // SAFETY: buf valid for page_size.
            unsafe {
                dev.read_async(
                    offset,
                    buf.as_mut_ptr(),
                    page_size as u32,
                    test_callback,
                    ctx,
                );
            }
        }

        for s in &read_states {
            s.wait(Duration::from_secs(30));
        }
        let read_elapsed = start.elapsed();

        let total_bytes = num_pages * page_size;
        let write_mbps = total_bytes as f64 / write_elapsed.as_secs_f64() / (1024.0 * 1024.0);
        let read_mbps = total_bytes as f64 / read_elapsed.as_secs_f64() / (1024.0 * 1024.0);

        eprintln!("UringDevice sequential write: {write_mbps:.1} MiB/s ({num_pages} pages)");
        eprintln!("UringDevice sequential read:  {read_mbps:.1} MiB/s ({num_pages} pages)");

        // Verify data
        for buf in &read_bufs {
            assert_eq!(buf, &data);
        }
    }
}
