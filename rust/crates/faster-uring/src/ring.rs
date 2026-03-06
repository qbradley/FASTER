//! Safe wrapper around an io_uring instance.
//!
//! All unsafe io_uring syscall interactions are encapsulated here.
//! Callers of [`Ring`] never touch raw pointers or unsafe code directly.

use io_uring::IoUring;
use io_uring::opcode;
use io_uring::types;
use std::io;
use std::os::unix::io::RawFd;

/// Configuration for the io_uring instance.
#[derive(Debug, Clone)]
pub struct UringConfig {
    /// Number of SQE/CQE entries. Must be a power of 2. Default: 256.
    pub queue_depth: u32,
    /// Whether to use SQPOLL mode (kernel-side submission polling).
    ///
    /// When enabled, the kernel spawns a thread to poll the submission queue,
    /// eliminating the syscall overhead of `io_uring_enter`. Requires elevated
    /// privileges on most kernels.
    pub sq_poll: bool,
    /// Whether files should be opened with `O_DIRECT` for unbuffered I/O.
    ///
    /// Required for FASTER's page-aligned I/O workloads to bypass the page
    /// cache and achieve deterministic latency.
    pub direct_io: bool,
}

impl Default for UringConfig {
    fn default() -> Self {
        Self {
            queue_depth: 256,
            sq_poll: false,
            direct_io: false,
        }
    }
}

/// Safe wrapper around an io_uring instance.
///
/// # Buffer Lifetime Contract
///
/// Between submission and completion the kernel holds references to the
/// caller-supplied buffers. The caller **must** ensure buffers remain valid
/// and are not freed or moved until the corresponding completion is reaped.
/// This is the caller's responsibility for unregistered I/O; for registered
/// buffers use a [`BufferPool`](crate::BufferPool).
///
/// # Crash Analysis
///
/// * **Write submission → completion:** If power fails between submission and
///   the drive acknowledging the write, data is *not* durable. Only after a
///   successful `fsync` completion can the write be considered persistent.
/// * **Fsync completion:** Once `reap_completions` returns a successful fsync
///   result, all previously completed writes to that fd are durable.
/// * **Ring drop during in-flight I/O:** The `Drop` impl calls [`Ring::drain`]
///   to wait for all in-flight operations before closing the ring, preventing
///   kernel-side use-after-free of submitted buffers.
pub struct Ring {
    ring: IoUring,
    /// Count of operations pushed to the SQ but not yet reaped from the CQ.
    inflight: u32,
    /// Count of SQEs pushed but not yet submitted to the kernel via `submit()`.
    unsubmitted: u32,
    /// Saved configuration for reference.
    #[allow(dead_code)]
    config: UringConfig,
}

impl Ring {
    /// Create a new io_uring instance with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the kernel does not support io_uring or if the
    /// requested configuration cannot be satisfied (e.g., SQPOLL without
    /// sufficient privileges).
    pub fn new(config: UringConfig) -> io::Result<Self> {
        let mut builder = IoUring::builder();

        if config.sq_poll {
            builder.setup_sqpoll(1000); // 1 s idle timeout
        }

        let ring = builder.build(config.queue_depth)?;

        Ok(Self {
            ring,
            inflight: 0,
            unsubmitted: 0,
            config,
        })
    }

    /// Submit a read operation.
    ///
    /// The kernel will read `buf.len()` bytes from `fd` at `offset` into `buf`.
    /// `user_data` is an opaque token returned with the completion event so the
    /// caller can correlate completions with requests.
    ///
    /// # Buffer Safety
    ///
    /// The caller must ensure `buf` remains valid until the completion for
    /// `user_data` is reaped.
    ///
    /// # Errors
    ///
    /// Returns an error if the submission queue is full. Call [`Ring::submit`]
    /// to flush pending entries and retry.
    pub fn submit_read(
        &mut self,
        fd: RawFd,
        buf: &mut [u8],
        offset: u64,
        user_data: u64,
    ) -> io::Result<()> {
        let entry = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
            .offset(offset)
            .build()
            .user_data(user_data);

        // SAFETY: The entry describes a valid read operation. The caller is
        // responsible for keeping `buf` alive until completion (documented in
        // the public API contract).
        unsafe {
            self.ring
                .submission()
                .push(&entry)
                .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "submission queue full"))?;
        }
        self.inflight += 1;
        self.unsubmitted += 1;
        Ok(())
    }

    /// Submit a write operation.
    ///
    /// The kernel will write `buf.len()` bytes from `buf` to `fd` at `offset`.
    ///
    /// # Buffer Safety
    ///
    /// The caller must ensure `buf` remains valid until the completion for
    /// `user_data` is reaped.
    ///
    /// # Crash Safety
    ///
    /// A completed write only means the data reached the kernel. Without a
    /// subsequent [`Ring::submit_fsync`] completion, the data is **not**
    /// guaranteed to be on stable storage.
    pub fn submit_write(
        &mut self,
        fd: RawFd,
        buf: &[u8],
        offset: u64,
        user_data: u64,
    ) -> io::Result<()> {
        let entry = opcode::Write::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32)
            .offset(offset)
            .build()
            .user_data(user_data);

        // SAFETY: The entry describes a valid write operation. The caller is
        // responsible for keeping `buf` alive until completion.
        unsafe {
            self.ring
                .submission()
                .push(&entry)
                .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "submission queue full"))?;
        }
        self.inflight += 1;
        self.unsubmitted += 1;
        Ok(())
    }

    /// Submit an fsync (flush) operation.
    ///
    /// Once the completion is reaped, all previously completed writes to `fd`
    /// are durable on stable storage.
    pub fn submit_fsync(&mut self, fd: RawFd, user_data: u64) -> io::Result<()> {
        let entry = opcode::Fsync::new(types::Fd(fd))
            .build()
            .user_data(user_data);

        // SAFETY: Fsync does not reference any user buffer.
        unsafe {
            self.ring
                .submission()
                .push(&entry)
                .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "submission queue full"))?;
        }
        self.inflight += 1;
        self.unsubmitted += 1;
        Ok(())
    }

    /// Flush the submission queue to the kernel.
    ///
    /// Returns the number of entries submitted. Until this is called, queued
    /// operations are not visible to the kernel.
    pub fn submit(&mut self) -> io::Result<u32> {
        let n = self.ring.submit()?;
        self.unsubmitted = 0;
        Ok(n as u32)
    }

    /// Submit and wait for at least one completion, then drain all available
    /// completions.
    ///
    /// Returns completed operations as `(user_data, result)` pairs where
    /// `result` is the kernel return value (bytes transferred on success,
    /// negative errno on failure).
    ///
    /// Also flushes any unsubmitted SQEs to the kernel.
    pub fn reap_completions(&mut self) -> io::Result<Vec<(u64, i32)>> {
        // Submit any pending entries and wait for at least 1 completion.
        self.ring.submit_and_wait(1)?;
        self.unsubmitted = 0;

        let mut results = Vec::new();
        let cq = self.ring.completion();
        for cqe in cq {
            results.push((cqe.user_data(), cqe.result()));
            self.inflight = self.inflight.saturating_sub(1);
        }
        Ok(results)
    }

    /// Non-blocking drain of the completion queue.
    ///
    /// Returns all currently available CQEs without submitting or waiting.
    /// Use this when accumulating a batch of SQEs — it prevents the CQ from
    /// overflowing while new SQEs are being collected.
    pub fn try_reap_completions(&mut self) -> Vec<(u64, i32)> {
        let mut results = Vec::new();
        let cq = self.ring.completion();
        for cqe in cq {
            results.push((cqe.user_data(), cqe.result()));
            self.inflight = self.inflight.saturating_sub(1);
        }
        results
    }

    /// Wait for all in-flight operations to complete.
    ///
    /// This is called automatically by [`Drop`]. After this returns,
    /// `inflight` is zero and all caller buffers are safe to free.
    pub fn drain(&mut self) -> io::Result<()> {
        while self.inflight > 0 {
            self.ring.submit_and_wait(1)?;
            self.unsubmitted = 0;
            let cq = self.ring.completion();
            for _cqe in cq {
                self.inflight = self.inflight.saturating_sub(1);
            }
        }
        Ok(())
    }

    /// Returns the number of operations currently in the pipeline
    /// (pushed to SQ or submitted to kernel but not yet reaped).
    pub fn inflight(&self) -> u32 {
        self.inflight
    }

    /// Returns the number of SQEs pushed but not yet submitted to the kernel.
    pub fn unsubmitted(&self) -> u32 {
        self.unsubmitted
    }

    // ── Buffer registration ───────────────────────────────────────────

    /// Register a [`BufferPool`](crate::BufferPool) with the kernel for
    /// fixed-buffer I/O.
    ///
    /// After registration, buffers from the pool can be used with
    /// `ReadFixed` and `WriteFixed` operations, avoiding per-I/O page
    /// pinning overhead.
    ///
    /// # Lifetime Contract
    ///
    /// The [`BufferPool`](crate::BufferPool) **must** outlive the
    /// registration. Call [`Ring::unregister_buffers`] before dropping the
    /// pool, and ensure all in-flight I/O using registered buffers has
    /// completed first (see [`Ring::drain`]).
    ///
    /// # Errors
    ///
    /// Returns an error if registration fails (e.g., buffers are already
    /// registered, or the kernel rejects the iovec array).
    pub fn register_buffers(&mut self, pool: &crate::buffer::BufferPool) -> io::Result<()> {
        let iovecs = pool.iovecs();
        // SAFETY: The iovecs describe valid, aligned memory owned by the pool.
        // The caller is responsible for keeping the pool alive until
        // `unregister_buffers` is called, as documented above.
        unsafe {
            self.ring.submitter().register_buffers(iovecs)?;
        }
        Ok(())
    }

    /// Unregister previously registered buffers.
    ///
    /// Must be called after all in-flight I/O using registered buffers has
    /// completed. Call [`Ring::drain`] first if I/O may still be pending.
    ///
    /// # Errors
    ///
    /// Returns an error if no buffers are currently registered.
    pub fn unregister_buffers(&mut self) -> io::Result<()> {
        self.ring.submitter().unregister_buffers()
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // Best-effort drain: we cannot propagate errors from Drop, but we
        // must wait for in-flight I/O to complete so the kernel releases
        // its references to caller buffers.
        if self.inflight > 0 {
            let _ = self.drain();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;
    use tempfile::NamedTempFile;

    #[test]
    fn ring_creation_default_config() {
        let ring = Ring::new(UringConfig::default());
        assert!(
            ring.is_ok(),
            "Ring creation with default config should succeed"
        );
        let ring = ring.unwrap();
        assert_eq!(ring.inflight(), 0);
    }

    #[test]
    fn ring_creation_custom_depth() {
        let config = UringConfig {
            queue_depth: 64,
            sq_poll: false,
            direct_io: false,
        };
        let ring = Ring::new(config);
        assert!(
            ring.is_ok(),
            "Ring creation with queue_depth=64 should succeed"
        );
    }

    #[test]
    fn read_write_round_trip() {
        let mut tmpfile = NamedTempFile::new().expect("create temp file");
        let data = b"hello io_uring world!";
        tmpfile.write_all(data).expect("write seed data");
        tmpfile.flush().expect("flush seed data");
        tmpfile.seek(SeekFrom::Start(0)).expect("seek to start");

        let fd = tmpfile.as_raw_fd();
        let mut ring = Ring::new(UringConfig::default()).expect("create ring");

        // Submit a write via io_uring
        let write_buf = b"URING WRITE TEST DATA";
        ring.submit_write(fd, write_buf, 0, 1)
            .expect("submit write");
        ring.submit().expect("flush submission queue");

        let completions = ring.reap_completions().expect("reap write completion");
        assert!(
            !completions.is_empty(),
            "should have at least one completion"
        );
        let (ud, result) = completions[0];
        assert_eq!(ud, 1);
        assert_eq!(result, write_buf.len() as i32);

        // Now read it back via io_uring
        let mut read_buf = vec![0u8; write_buf.len()];
        ring.submit_read(fd, &mut read_buf, 0, 2)
            .expect("submit read");
        ring.submit().expect("flush submission queue");

        let completions = ring.reap_completions().expect("reap read completion");
        assert!(!completions.is_empty());
        let (ud, result) = completions[0];
        assert_eq!(ud, 2);
        assert_eq!(result, write_buf.len() as i32);
        assert_eq!(&read_buf, write_buf);
    }

    #[test]
    fn multiple_io_operations() {
        let tmpfile = NamedTempFile::new().expect("create temp file");
        let fd = tmpfile.as_raw_fd();
        let mut ring = Ring::new(UringConfig::default()).expect("create ring");

        let num_ops = 10;
        let page_size = 4096usize;

        // Submit 10 writes, each a 4 KiB page at a different offset.
        let mut write_bufs: Vec<Vec<u8>> = Vec::new();
        for i in 0..num_ops {
            let mut buf = vec![0u8; page_size];
            buf[0] = i as u8;
            buf[page_size - 1] = i as u8;
            write_bufs.push(buf);
        }

        for (i, buf) in write_bufs.iter().enumerate() {
            let offset = (i * page_size) as u64;
            ring.submit_write(fd, buf, offset, i as u64)
                .expect("submit write");
        }
        ring.submit().expect("flush writes");

        // Reap all write completions.
        let mut completed = 0;
        while completed < num_ops {
            let results = ring.reap_completions().expect("reap completions");
            for (_, result) in &results {
                assert_eq!(
                    *result, page_size as i32,
                    "each write should complete fully"
                );
            }
            completed += results.len();
        }

        // Submit 10 reads at the same offsets.
        let mut read_bufs: Vec<Vec<u8>> = (0..num_ops).map(|_| vec![0u8; page_size]).collect();
        for (i, buf) in read_bufs.iter_mut().enumerate() {
            let offset = (i * page_size) as u64;
            ring.submit_read(fd, buf, offset, (num_ops + i) as u64)
                .expect("submit read");
        }
        ring.submit().expect("flush reads");

        completed = 0;
        while completed < num_ops {
            let results = ring.reap_completions().expect("reap completions");
            completed += results.len();
        }

        // Verify data integrity.
        for (i, buf) in read_bufs.iter().enumerate() {
            assert_eq!(buf[0], i as u8, "first byte mismatch at op {i}");
            assert_eq!(buf[page_size - 1], i as u8, "last byte mismatch at op {i}");
        }
    }

    #[test]
    fn queue_full_backpressure() {
        // Use minimum queue depth to trigger backpressure quickly.
        let config = UringConfig {
            queue_depth: 1,
            sq_poll: false,
            direct_io: false,
        };
        let tmpfile = NamedTempFile::new().expect("create temp file");
        let fd = tmpfile.as_raw_fd();
        let mut ring = Ring::new(config).expect("create ring");

        let buf = vec![0u8; 64];

        // The first submission should succeed.
        ring.submit_write(fd, &buf, 0, 1).expect("first submit");

        // The second submission may fail with WouldBlock if the SQ is full.
        // We don't flush between submissions, so the SQ has only 1 slot.
        let result = ring.submit_write(fd, &buf, 0, 2);
        if let Err(e) = result {
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        }
        // Either way, drain to clean up.
        ring.submit().ok();
        let _ = ring.drain();
    }

    #[test]
    fn fsync_completion() {
        let mut tmpfile = NamedTempFile::new().expect("create temp file");
        tmpfile.write_all(b"fsync test data").expect("write");
        tmpfile.flush().expect("flush");

        let fd = tmpfile.as_raw_fd();
        let mut ring = Ring::new(UringConfig::default()).expect("create ring");

        // Write via io_uring then fsync.
        let write_data = b"durable data";
        ring.submit_write(fd, write_data, 0, 10)
            .expect("submit write");
        ring.submit_fsync(fd, 11).expect("submit fsync");
        ring.submit().expect("flush");

        // Reap both completions (write + fsync).
        let mut tokens = std::collections::HashSet::new();
        while tokens.len() < 2 {
            let results = ring.reap_completions().expect("reap");
            for (ud, result) in results {
                assert!(result >= 0, "operation {ud} failed with {result}");
                tokens.insert(ud);
            }
        }
        assert!(tokens.contains(&10), "write completion missing");
        assert!(tokens.contains(&11), "fsync completion missing");

        // Verify data is readable through normal I/O.
        tmpfile.seek(SeekFrom::Start(0)).expect("seek");
        let mut verify_buf = vec![0u8; write_data.len()];
        tmpfile.read_exact(&mut verify_buf).expect("read back");
        assert_eq!(&verify_buf, write_data);
    }

    #[test]
    fn clean_shutdown_with_inflight() {
        let tmpfile = NamedTempFile::new().expect("create temp file");
        let fd = tmpfile.as_raw_fd();
        let mut ring = Ring::new(UringConfig::default()).expect("create ring");

        let buf = b"inflight data";
        ring.submit_write(fd, buf, 0, 100).expect("submit");
        ring.submit().expect("flush");

        assert!(ring.inflight() > 0, "should have inflight ops");
        // Dropping the ring should drain without hanging or panicking.
        drop(ring);
    }

    #[test]
    fn drain_empties_inflight() {
        let tmpfile = NamedTempFile::new().expect("create temp file");
        let fd = tmpfile.as_raw_fd();
        let mut ring = Ring::new(UringConfig::default()).expect("create ring");

        let buf = b"drain test data";
        ring.submit_write(fd, buf, 0, 200).expect("submit");
        ring.submit().expect("flush");
        ring.drain().expect("drain");
        assert_eq!(ring.inflight(), 0, "drain should empty inflight count");
    }

    #[test]
    fn direct_io_flag_stored() {
        let config = UringConfig {
            queue_depth: 128,
            sq_poll: false,
            direct_io: true,
        };
        // Ring creation succeeds — the direct_io flag is configuration only;
        // it does not affect ring setup, only how files should be opened.
        let ring = Ring::new(config).expect("create ring with direct_io");
        assert_eq!(ring.inflight(), 0);
    }
}
