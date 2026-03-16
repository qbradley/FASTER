//! Simulated device with fault injection for deterministic testing.
//!
//! [`SimulatedStorage`] holds the raw byte buffer behind an `Arc` so that it
//! survives device (and store) drops — enabling crash-recovery tests.
//! [`SimulatedDevice`] implements [`Device`] on top of that storage and
//! consults a [`FaultConfig`] (+ seeded PRNG) to decide when to inject
//! faults.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use faster_core::{Device, IoCompletionCallback, IoRequestResult, IoStatus};
use rand::SeedableRng;
use rand::rngs::SmallRng;

use crate::fault::FaultConfig;
use crate::task::TaskId;
use crate::trace::IoOp;

// ── SimulatedStorage ───────────────────────────────────────────────────────

/// Persistent, shared backing store for [`SimulatedDevice`].
///
/// Wrap in `Arc` and hand copies to successive devices so the data survives
/// store drops (simulated crashes).
#[derive(Debug)]
pub struct SimulatedStorage {
    pub(crate) data: RwLock<Vec<u8>>,
}

impl SimulatedStorage {
    /// Create a new, empty storage wrapped in an `Arc`.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            data: RwLock::new(Vec::new()),
        })
    }

    /// Snapshot the current contents (useful for pre-crash saves).
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.read().expect("storage lock poisoned").clone()
    }

    /// Replace the contents wholesale (useful for restoring a pre-crash state).
    pub fn restore(&self, data: Vec<u8>) {
        *self.data.write().expect("storage lock poisoned") = data;
    }

    /// Current byte-length of the storage.
    pub fn len(&self) -> u64 {
        self.data.read().expect("storage lock poisoned").len() as u64
    }

    /// Returns `true` if the storage contains no data.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn ensure_capacity(buf: &mut Vec<u8>, end: usize) {
        if end > buf.len() {
            buf.resize(end, 0);
        }
    }
}

// ── SimulatedDevice ────────────────────────────────────────────────────────

/// An in-memory [`Device`] with configurable fault injection.
///
/// Data lives in the shared [`SimulatedStorage`]; faults are decided by the
/// seeded PRNG and the [`FaultConfig`].
pub struct SimulatedDevice {
    storage: Arc<SimulatedStorage>,
    fault_config: FaultConfig,
    rng: Mutex<SmallRng>,
    write_count: AtomicU64,
    read_count: AtomicU64,
    sector_size: u32,
    segment_size: u64,
}

impl SimulatedDevice {
    /// Create a fault-free device backed by `storage` with the given PRNG seed.
    pub fn new(storage: Arc<SimulatedStorage>, seed: u64) -> Self {
        Self {
            storage,
            fault_config: FaultConfig::none(),
            rng: Mutex::new(SmallRng::seed_from_u64(seed)),
            write_count: AtomicU64::new(0),
            read_count: AtomicU64::new(0),
            sector_size: 512,
            segment_size: 1 << 30,
        }
    }

    /// Builder: attach a fault configuration.
    #[must_use]
    pub fn with_faults(mut self, config: FaultConfig) -> Self {
        self.fault_config = config;
        self
    }

    /// Builder: override the sector size (default 512).
    #[must_use]
    pub fn with_sector_size(mut self, size: u32) -> Self {
        self.sector_size = size;
        self
    }

    /// Builder: override the segment size (default 1 GiB).
    #[must_use]
    pub fn with_segment_size(mut self, size: u64) -> Self {
        self.segment_size = size;
        self
    }

    /// Number of write operations dispatched so far.
    pub fn write_count(&self) -> u64 {
        self.write_count.load(Ordering::Relaxed)
    }

    /// Number of read operations dispatched so far.
    pub fn read_count(&self) -> u64 {
        self.read_count.load(Ordering::Relaxed)
    }

    // ── fault decision helpers ─────────────────────────────────────────

    fn should_fail_write(&self) -> bool {
        let count = self.write_count.load(Ordering::Relaxed);
        if let Some(limit) = self.fault_config.fail_after_n_writes {
            if count > limit {
                return true;
            }
        }
        if self.fault_config.write_error_rate > 0.0 {
            let val: f64 = {
                let mut rng = self.rng.lock().expect("rng lock poisoned");
                rand_f64(&mut rng)
            };
            return val < self.fault_config.write_error_rate;
        }
        false
    }

    fn should_fail_read(&self) -> bool {
        let count = self.read_count.load(Ordering::Relaxed);
        if let Some(limit) = self.fault_config.fail_after_n_reads {
            if count > limit {
                return true;
            }
        }
        if self.fault_config.read_error_rate > 0.0 {
            let val: f64 = {
                let mut rng = self.rng.lock().expect("rng lock poisoned");
                rand_f64(&mut rng)
            };
            return val < self.fault_config.read_error_rate;
        }
        false
    }

    fn should_partial_write(&self) -> bool {
        if self.fault_config.partial_write_rate > 0.0 {
            let val: f64 = {
                let mut rng = self.rng.lock().expect("rng lock poisoned");
                rand_f64(&mut rng)
            };
            return val < self.fault_config.partial_write_rate;
        }
        false
    }

    /// How many bytes to actually write when a partial write is triggered.
    fn partial_write_len(&self, full_len: u32) -> u32 {
        if full_len <= 1 {
            return 0;
        }
        let mut rng = self.rng.lock().expect("rng lock poisoned");
        rand_range(&mut rng, 1, full_len)
    }
}

impl fmt::Debug for SimulatedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimulatedDevice")
            .field("write_count", &self.write_count.load(Ordering::Relaxed))
            .field("read_count", &self.read_count.load(Ordering::Relaxed))
            .field("sector_size", &self.sector_size)
            .field("segment_size", &self.segment_size)
            .field("fault_config", &self.fault_config)
            .finish()
    }
}

// ── Device impl ────────────────────────────────────────────────────────────

impl Device for SimulatedDevice {
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
        self.read_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail_read() {
            // SAFETY: caller guarantees `context` is valid.
            unsafe {
                callback(context, IoStatus::Error(-1), 0);
            }
            return IoRequestResult::CompletedSync;
        }

        let start = offset as usize;
        let end = start + len as usize;
        let guard = self.storage.data.read().expect("storage lock poisoned");

        // SAFETY: caller guarantees `dest` is valid for `len` bytes.
        unsafe {
            if end <= guard.len() {
                core::ptr::copy_nonoverlapping(guard[start..end].as_ptr(), dest, len as usize);
            } else {
                core::ptr::write_bytes(dest, 0, len as usize);
                if start < guard.len() {
                    let avail = guard.len() - start;
                    core::ptr::copy_nonoverlapping(guard[start..].as_ptr(), dest, avail);
                }
            }
        }
        drop(guard);

        // SAFETY: caller guarantees `context` is valid.
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
        self.write_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail_write() {
            // SAFETY: caller guarantees `context` is valid.
            unsafe {
                callback(context, IoStatus::Error(-1), 0);
            }
            return IoRequestResult::CompletedSync;
        }

        let actual_len = if self.should_partial_write() {
            self.partial_write_len(len)
        } else {
            len
        };

        let start = offset as usize;
        let end = start + actual_len as usize;

        let mut guard = self.storage.data.write().expect("storage lock poisoned");
        SimulatedStorage::ensure_capacity(&mut guard, end);

        // SAFETY: caller guarantees `source` is valid for `len` bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(
                source,
                guard[start..end].as_mut_ptr(),
                actual_len as usize,
            );
        }
        drop(guard);

        // Report the *requested* length even for partial writes — the caller
        // thinks the write succeeded, simulating a torn page.
        // SAFETY: caller guarantees `context` is valid.
        unsafe {
            callback(context, IoStatus::Success, len);
        }
        IoRequestResult::CompletedSync
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        self.read_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail_read() {
            return Err(io::Error::other("simulated read error"));
        }

        let start = offset as usize;
        let end = start + dest.len();
        let guard = self.storage.data.read().expect("storage lock poisoned");

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
        self.write_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail_write() {
            return Err(io::Error::other("simulated write error"));
        }

        let start = offset as usize;
        let end = start + source.len();
        let mut guard = self.storage.data.write().expect("storage lock poisoned");
        SimulatedStorage::ensure_capacity(&mut guard, end);
        guard[start..end].copy_from_slice(source);
        Ok(source.len() as u32)
    }

    fn truncate_until(&self, offset: u64) {
        let mut guard = self.storage.data.write().expect("storage lock poisoned");
        let off = offset as usize;
        if off <= guard.len() {
            guard[..off].fill(0);
        }
    }

    fn size(&self) -> u64 {
        self.storage.len()
    }

    fn close(&self) {
        // No-op — data lives in the Arc<SimulatedStorage>.
    }
}

// ── IoCompletionEvent ──────────────────────────────────────────────────────

/// Descriptor returned by [`SimulatedDevice::pending_completions`].
///
/// In Phase 2 the device is still fully synchronous, so this type exists as a
/// forward-compatible integration point for Phase 3 where I/O completions
/// will be delivered through the scheduler.
#[derive(Debug, Clone)]
pub struct IoCompletionEvent {
    /// Which task initiated this I/O (if known).
    pub task_id: Option<TaskId>,
    /// The operation that completed.
    pub op: IoOp,
    /// Whether the operation succeeded.
    pub result: IoStatus,
}

impl SimulatedDevice {
    /// Return pending I/O completions ready for scheduler delivery.
    ///
    /// **Phase 2**: always returns an empty `Vec` because all I/O is
    /// synchronous.  Phase 3 will make this functional.
    pub fn pending_completions(&self) -> Vec<IoCompletionEvent> {
        Vec::new()
    }
}

// ── Portable PRNG helpers ──────────────────────────────────────────────────
// These avoid naming-conflict issues with the `gen` keyword in Rust 2024.

/// Generate a uniform `f64` in `[0, 1)`.
fn rand_f64(rng: &mut SmallRng) -> f64 {
    use rand::RngCore;
    // Use upper 53 bits of a u64 for a uniform double.
    let bits = rng.next_u64() >> 11;
    (bits as f64) * (1.0 / (1u64 << 53) as f64)
}

/// Generate a uniform `u32` in `[lo, hi)`.
fn rand_range(rng: &mut SmallRng, lo: u32, hi: u32) -> u32 {
    use rand::RngCore;
    debug_assert!(lo < hi);
    let range = hi - lo;
    lo + (rng.next_u32() % range)
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_write_read_roundtrip() {
        let storage = SimulatedStorage::new();
        let device = SimulatedDevice::new(Arc::clone(&storage), 1);

        let data = b"hello, FASTER!";
        device
            .write_sync(0, data)
            .expect("write_sync should succeed");

        let mut buf = vec![0u8; data.len()];
        device
            .read_sync(0, &mut buf)
            .expect("read_sync should succeed");
        assert_eq!(&buf, data);
    }

    #[test]
    fn storage_persists_after_device_drop() {
        let storage = SimulatedStorage::new();

        // Write through one device, then drop it.
        {
            let device = SimulatedDevice::new(Arc::clone(&storage), 1);
            device
                .write_sync(0, b"persist")
                .expect("write should succeed");
        }

        // Read through a fresh device.
        let device = SimulatedDevice::new(Arc::clone(&storage), 2);
        let mut buf = vec![0u8; 7];
        device.read_sync(0, &mut buf).expect("read should succeed");
        assert_eq!(&buf, b"persist");
    }

    #[test]
    fn write_limit_triggers_error() {
        let storage = SimulatedStorage::new();
        let device = SimulatedDevice::new(Arc::clone(&storage), 1)
            .with_faults(FaultConfig::none().with_write_limit(2));

        // First two writes succeed (counts 1, 2; limit is 2 so >2 hasn't triggered).
        assert!(device.write_sync(0, b"a").is_ok());
        assert!(device.write_sync(1, b"b").is_ok());

        // Third write: count=3, 3>2 → should fail.
        assert!(device.write_sync(2, b"c").is_err());
        assert!(device.write_sync(3, b"d").is_err());
    }

    #[test]
    fn read_limit_triggers_error() {
        let storage = SimulatedStorage::new();
        let device = SimulatedDevice::new(Arc::clone(&storage), 1)
            .with_faults(FaultConfig::none().with_read_limit(1));

        device.write_sync(0, b"data").expect("write should succeed");

        let mut buf = [0u8; 4];
        // First read succeeds (count=1, 1>1 is false).
        assert!(device.read_sync(0, &mut buf).is_ok());

        // Second read: count=2, 2>1 → should fail.
        assert!(device.read_sync(0, &mut buf).is_err());
    }

    #[test]
    fn snapshot_and_restore() {
        let storage = SimulatedStorage::new();
        let device = SimulatedDevice::new(Arc::clone(&storage), 1);

        device.write_sync(0, b"before").unwrap();
        let snap = storage.snapshot();

        device.write_sync(0, b"AFTER!").unwrap();
        let mut buf = [0u8; 6];
        device.read_sync(0, &mut buf).unwrap();
        assert_eq!(&buf, b"AFTER!");

        storage.restore(snap);
        device.read_sync(0, &mut buf).unwrap();
        assert_eq!(&buf, b"before");
    }

    #[test]
    fn size_tracks_writes() {
        let storage = SimulatedStorage::new();
        let device = SimulatedDevice::new(Arc::clone(&storage), 1);

        assert_eq!(device.size(), 0);
        device.write_sync(100, b"hi").unwrap();
        assert_eq!(device.size(), 102);
    }
}
