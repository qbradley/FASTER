//! Async-completion I/O device for DST v2.0.
//!
//! [`SimDeviceV2`] implements the [`Device`] trait with **deferred callbacks**.
//! Instead of completing I/O synchronously (like [`SimulatedDevice`]), writes
//! and reads are enqueued with a simulated latency derived from the
//! [`SimulatedClock`]. Completions fire only when [`Device::poll_completions`]
//! is called and the clock has advanced past the operation's completion time.
//!
//! Key properties:
//! - **Deterministic** — same [`SimIoConfig::seed`] ⇒ identical latency
//!   sequence ⇒ identical completion order. Zero flakes.
//! - **Queue pressure** — bounded `queue_depth` returns [`IoRequestResult::QueueFull`]
//!   when the pending queue is saturated.
//! - **Fault injection** — configurable write error rate.
//! - **BTreeMap ordering** — pending I/O keyed by `(complete_at_ns, completion_id)`
//!   for deterministic iteration.
//!
//! [`SimulatedDevice`]: crate::device::SimulatedDevice

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use faster_core::{Device, IoCompletionCallback, IoRequestResult, IoStatus};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::clock::SimulatedClock;
use crate::device::SimulatedStorage;

// ── Configuration ──────────────────────────────────────────────────────────

/// Configuration for [`SimDeviceV2`] I/O simulation.
#[derive(Debug, Clone)]
pub struct SimIoConfig {
    /// Maximum pending I/O operations (2–1024, default 16).
    pub queue_depth: usize,
    /// Base I/O latency in simulated nanoseconds.
    pub base_latency_ns: u64,
    /// Random jitter range (±jitter) added to base latency.
    pub latency_jitter_ns: u64,
    /// Virtual cost per byte for `truncate_until` (models O(n) zeroing).
    pub truncate_ns_per_byte: u64,
    /// If `true`, reads complete synchronously (Phase 1 compatibility).
    pub read_sync: bool,
    /// Probability `[0.0, 1.0]` that a write callback signals an error.
    pub write_fault_rate: f64,
    /// RNG seed — determinism anchor.
    pub seed: u64,
}

impl Default for SimIoConfig {
    fn default() -> Self {
        Self {
            queue_depth: 16,
            base_latency_ns: 1_000, // 1 µs
            latency_jitter_ns: 100, // ±100 ns
            truncate_ns_per_byte: 1,
            read_sync: true,
            write_fault_rate: 0.0,
            seed: 0,
        }
    }
}

// ── Pending I/O entry ──────────────────────────────────────────────────────

/// A deferred I/O completion waiting in the pending queue.
struct PendingIo {
    callback: IoCompletionCallback,
    context: *mut u8,
    status: IoStatus,
    bytes_transferred: u32,
}

// SAFETY: The `context` pointer's validity is guaranteed by the `Device` trait
// contract — callers must keep it valid until the callback fires, and we only
// access it from `poll_completions` which fires the callback exactly once.
unsafe impl Send for PendingIo {}

// ── SimDeviceV2 ────────────────────────────────────────────────────────────

/// Async-completion simulated device for DST v2.0.
///
/// Defers I/O callbacks until [`poll_completions`](Device::poll_completions)
/// drains operations whose simulated completion time has been reached.
pub struct SimDeviceV2 {
    config: SimIoConfig,
    clock: Arc<SimulatedClock>,
    rng: Mutex<ChaCha8Rng>,
    /// `(complete_at_ns, completion_id)` → deferred callback.
    /// BTreeMap guarantees deterministic iteration order.
    pending: Mutex<BTreeMap<(u64, u64), PendingIo>>,
    storage: Arc<SimulatedStorage>,
    next_completion_id: AtomicU64,
    // Metrics
    total_submitted: AtomicU64,
    total_completed: AtomicU64,
    total_queue_full: AtomicU64,
    // Device geometry
    sector_size: u32,
    segment_size: u64,
}

impl SimDeviceV2 {
    /// Create a new async-completion device.
    pub fn new(
        config: SimIoConfig,
        clock: Arc<SimulatedClock>,
        storage: Arc<SimulatedStorage>,
    ) -> Self {
        let rng = ChaCha8Rng::seed_from_u64(config.seed);
        Self {
            config,
            clock,
            rng: Mutex::new(rng),
            pending: Mutex::new(BTreeMap::new()),
            storage,
            next_completion_id: AtomicU64::new(0),
            total_submitted: AtomicU64::new(0),
            total_completed: AtomicU64::new(0),
            total_queue_full: AtomicU64::new(0),
            sector_size: 512,
            segment_size: 1 << 30,
        }
    }

    /// Builder: override sector size (default 512).
    #[must_use]
    pub fn with_sector_size(mut self, size: u32) -> Self {
        self.sector_size = size;
        self
    }

    /// Builder: override segment size (default 1 GiB).
    #[must_use]
    pub fn with_segment_size(mut self, size: u64) -> Self {
        self.segment_size = size;
        self
    }

    // ── Metrics ────────────────────────────────────────────────────────

    /// Total I/O operations submitted (writes + async reads).
    pub fn total_submitted(&self) -> u64 {
        self.total_submitted.load(Ordering::Relaxed)
    }

    /// Total I/O completions fired via `poll_completions`.
    pub fn total_completed(&self) -> u64 {
        self.total_completed.load(Ordering::Relaxed)
    }

    /// Total times a submission was rejected due to queue pressure.
    pub fn total_queue_full(&self) -> u64 {
        self.total_queue_full.load(Ordering::Relaxed)
    }

    /// Number of I/O operations currently in the pending queue.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().expect("pending lock poisoned").len()
    }

    // ── Internal helpers ───────────────────────────────────────────────

    /// Compute completion time: `now + base_latency ± jitter`.
    fn completion_time(&self) -> u64 {
        let base = self.config.base_latency_ns;
        let jitter_range = self.config.latency_jitter_ns;
        let jitter = if jitter_range > 0 {
            let mut rng = self.rng.lock().expect("rng lock poisoned");
            // Uniform in [0, 2*jitter_range], then subtract jitter_range → [-jitter, +jitter]
            let raw = rand_u64(&mut rng) % (2 * jitter_range + 1);
            raw as i64 - jitter_range as i64
        } else {
            0
        };
        let latency = (base as i64 + jitter).max(0) as u64;
        self.clock.now_nanos() + latency
    }

    /// Allocate a unique, monotonically increasing completion ID.
    fn next_id(&self) -> u64 {
        self.next_completion_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Roll the fault die. Returns `true` if this write should fail.
    fn should_fail_write(&self) -> bool {
        if self.config.write_fault_rate <= 0.0 {
            return false;
        }
        if self.config.write_fault_rate >= 1.0 {
            return true;
        }
        let val: f64 = {
            let mut rng = self.rng.lock().expect("rng lock poisoned");
            rand_f64(&mut rng)
        };
        val < self.config.write_fault_rate
    }

    /// Enqueue a deferred completion. Returns `false` if queue is full.
    fn enqueue(&self, io: PendingIo, complete_at: u64) -> bool {
        let mut pending = self.pending.lock().expect("pending lock poisoned");
        if pending.len() >= self.config.queue_depth {
            return false;
        }
        let id = self.next_id();
        pending.insert((complete_at, id), io);
        true
    }
}

impl std::fmt::Debug for SimDeviceV2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimDeviceV2")
            .field("config", &self.config)
            .field("total_submitted", &self.total_submitted.load(Ordering::Relaxed))
            .field("total_completed", &self.total_completed.load(Ordering::Relaxed))
            .field("total_queue_full", &self.total_queue_full.load(Ordering::Relaxed))
            .field("pending_count", &self.pending_count())
            .finish()
    }
}

// ── Device trait ───────────────────────────────────────────────────────────

impl Device for SimDeviceV2 {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn segment_size(&self) -> u64 {
        self.segment_size
    }

    fn max_outstanding_io(&self) -> u32 {
        self.config.queue_depth as u32
    }

    unsafe fn read_async(
        &self,
        offset: u64,
        dest: *mut u8,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        // Copy data to dest immediately (storage is always available).
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

        if self.config.read_sync {
            // Phase 1 compatibility: complete synchronously.
            // SAFETY: caller guarantees `context` is valid.
            unsafe {
                callback(context, IoStatus::Success, len);
            }
            return IoRequestResult::CompletedSync;
        }

        // Async path: defer the callback.
        let complete_at = self.completion_time();
        let io = PendingIo {
            callback,
            context,
            status: IoStatus::Success,
            bytes_transferred: len,
        };
        if self.enqueue(io, complete_at) {
            self.total_submitted.fetch_add(1, Ordering::Relaxed);
            IoRequestResult::Submitted
        } else {
            self.total_queue_full.fetch_add(1, Ordering::Relaxed);
            IoRequestResult::QueueFull
        }
    }

    unsafe fn write_async(
        &self,
        source: *const u8,
        offset: u64,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        // Check queue capacity first (before any side effects).
        {
            let pending = self.pending.lock().expect("pending lock poisoned");
            if pending.len() >= self.config.queue_depth {
                self.total_queue_full.fetch_add(1, Ordering::Relaxed);
                return IoRequestResult::QueueFull;
            }
        }

        // Decide whether this write faults.
        let fault = self.should_fail_write();

        if !fault {
            // Copy data to storage immediately.
            let start = offset as usize;
            let end = start + len as usize;
            let mut guard = self.storage.data.write().expect("storage lock poisoned");
            SimulatedStorage::ensure_capacity(&mut guard, end);
            // SAFETY: caller guarantees `source` is valid for `len` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source,
                    guard[start..end].as_mut_ptr(),
                    len as usize,
                );
            }
        }

        // Schedule the deferred completion.
        let complete_at = self.completion_time();
        let status = if fault {
            IoStatus::Error(-1)
        } else {
            IoStatus::Success
        };
        let bytes = if fault { 0 } else { len };

        let io = PendingIo {
            callback,
            context,
            status,
            bytes_transferred: bytes,
        };

        // We already checked capacity above, but another thread could have
        // raced (unlikely in single-threaded DST, but safe to handle).
        let mut pending = self.pending.lock().expect("pending lock poisoned");
        if pending.len() >= self.config.queue_depth {
            drop(pending);
            self.total_queue_full.fetch_add(1, Ordering::Relaxed);
            return IoRequestResult::QueueFull;
        }
        let id = self.next_id();
        pending.insert((complete_at, id), io);
        drop(pending);

        self.total_submitted.fetch_add(1, Ordering::Relaxed);
        IoRequestResult::Submitted
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
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
        let start = offset as usize;
        let end = start + source.len();
        let mut guard = self.storage.data.write().expect("storage lock poisoned");
        SimulatedStorage::ensure_capacity(&mut guard, end);
        guard[start..end].copy_from_slice(source);
        Ok(source.len() as u32)
    }

    fn truncate_until(&self, offset: u64) {
        // Charge virtual time proportional to offset (models O(n) zeroing).
        self.clock
            .charge(offset.saturating_mul(self.config.truncate_ns_per_byte));

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

    fn poll_completions(&self) -> u32 {
        let now = self.clock.now_nanos();
        let mut pending = self.pending.lock().expect("pending lock poisoned");

        // Collect all entries whose completion time has been reached.
        // split_off gives us everything >= the split key; we want everything < (now+1, 0).
        let ready = pending.split_off(&(now + 1, 0));
        // `pending` now contains entries with key < (now+1, 0), i.e. complete_at <= now.
        // `ready` contains entries with key >= (now+1, 0), i.e. still pending.
        // Swap so `pending` holds the still-pending items.
        let completed = std::mem::replace(&mut *pending, ready);
        drop(pending);

        let count = completed.len() as u32;

        // Fire callbacks in completion-time order (BTreeMap iteration order).
        for (_key, io) in completed {
            // SAFETY: The Device trait contract guarantees `context` is valid
            // until the callback fires. We fire each callback exactly once.
            unsafe {
                (io.callback)(io.context, io.status, io.bytes_transferred);
            }
        }

        self.total_completed
            .fetch_add(count as u64, Ordering::Relaxed);
        count
    }
}

// ── Portable PRNG helpers ──────────────────────────────────────────────────

/// Generate a uniform `u64`.
fn rand_u64(rng: &mut ChaCha8Rng) -> u64 {
    use rand::RngCore;
    rng.next_u64()
}

/// Generate a uniform `f64` in `[0, 1)`.
fn rand_f64(rng: &mut ChaCha8Rng) -> f64 {
    use rand::RngCore;
    let bits = rng.next_u64() >> 11;
    (bits as f64) * (1.0 / (1u64 << 53) as f64)
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    /// Helper: create a device + clock + storage with the given config.
    fn make_device(config: SimIoConfig) -> (SimDeviceV2, Arc<SimulatedClock>, Arc<SimulatedStorage>) {
        let clock = Arc::new(SimulatedClock::new());
        let storage = SimulatedStorage::new();
        let device = SimDeviceV2::new(config, Arc::clone(&clock), Arc::clone(&storage));
        (device, clock, storage)
    }

    /// Noop callback used when we don't care about the completion.
    unsafe fn noop_callback(_ctx: *mut u8, _status: IoStatus, _bytes: u32) {}

    /// Callback that increments an AtomicU32 counter.
    unsafe fn counting_callback(ctx: *mut u8, _status: IoStatus, _bytes: u32) {
        let counter = unsafe { &*(ctx as *const AtomicU32) };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Callback that records the status it was called with.
    unsafe fn status_callback(ctx: *mut u8, status: IoStatus, _bytes: u32) {
        let slot = unsafe { &*(ctx as *const Mutex<Vec<IoStatus>>) };
        slot.lock().unwrap().push(status);
    }

    // ── Test 1: Queue depth enforcement ────────────────────────────────

    #[test]
    fn queue_depth_enforced() {
        let config = SimIoConfig {
            queue_depth: 4,
            base_latency_ns: 1_000,
            latency_jitter_ns: 0,
            ..Default::default()
        };
        let (device, _clock, _storage) = make_device(config);

        let data = [0u8; 512];
        let mut results = Vec::new();

        for i in 0..5u64 {
            let result = unsafe {
                device.write_async(
                    data.as_ptr(),
                    i * 512,
                    512,
                    noop_callback,
                    std::ptr::null_mut(),
                )
            };
            results.push(result);
        }

        // First 4 should be Submitted.
        for r in &results[..4] {
            assert!(matches!(r, IoRequestResult::Submitted), "expected Submitted, got {:?}", r);
        }
        // 5th should be QueueFull.
        assert!(
            matches!(results[4], IoRequestResult::QueueFull),
            "expected QueueFull, got {:?}",
            results[4]
        );

        assert_eq!(device.total_submitted(), 4);
        assert_eq!(device.total_queue_full(), 1);
    }

    // ── Test 2: poll_completions fires callbacks in order ──────────────

    #[test]
    fn poll_completions_fires_in_order() {
        let config = SimIoConfig {
            queue_depth: 16,
            base_latency_ns: 0,
            latency_jitter_ns: 0,
            ..Default::default()
        };
        let (device, clock, _storage) = make_device(config);

        // Submit 3 writes with manually controlled completion times by
        // advancing the clock between submissions (latency=0 means complete_at = now).
        let counter = AtomicU32::new(0);
        let ctx = &counter as *const AtomicU32 as *mut u8;
        let data = [0u8; 512];

        // Write 1 at t=0
        let r = unsafe { device.write_async(data.as_ptr(), 0, 512, counting_callback, ctx) };
        assert!(matches!(r, IoRequestResult::Submitted));

        // Write 2 at t=100
        clock.advance(Duration::from_nanos(100));
        let r = unsafe { device.write_async(data.as_ptr(), 512, 512, counting_callback, ctx) };
        assert!(matches!(r, IoRequestResult::Submitted));

        // Write 3 at t=200
        clock.advance(Duration::from_nanos(100));
        let r = unsafe { device.write_async(data.as_ptr(), 1024, 512, counting_callback, ctx) };
        assert!(matches!(r, IoRequestResult::Submitted));

        // Now poll — clock is at 200, all three should complete.
        let n = device.poll_completions();
        assert_eq!(n, 3);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        assert_eq!(device.total_completed(), 3);
    }

    // ── Test 3: Clock advancement triggers completions ─────────────────

    #[test]
    fn clock_advancement_triggers_completions() {
        let config = SimIoConfig {
            queue_depth: 16,
            base_latency_ns: 1_000,
            latency_jitter_ns: 0,
            ..Default::default()
        };
        let (device, clock, _storage) = make_device(config);

        let counter = AtomicU32::new(0);
        let ctx = &counter as *const AtomicU32 as *mut u8;
        let data = [0u8; 512];

        // Submit at t=0, completes at t=1000.
        let r = unsafe { device.write_async(data.as_ptr(), 0, 512, counting_callback, ctx) };
        assert!(matches!(r, IoRequestResult::Submitted));

        // Poll at t=0 — nothing ready.
        assert_eq!(device.poll_completions(), 0);
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        // Advance to t=500 — still not ready.
        clock.advance(Duration::from_nanos(500));
        assert_eq!(device.poll_completions(), 0);

        // Advance to t=1000 — should fire.
        clock.advance(Duration::from_nanos(500));
        assert_eq!(device.poll_completions(), 1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        // Subsequent poll returns 0 (already drained).
        assert_eq!(device.poll_completions(), 0);
    }

    // ── Test 4: Determinism — same seed = same latencies ───────────────

    #[test]
    fn deterministic_with_same_seed() {
        fn run_and_collect(seed: u64) -> Vec<u64> {
            let config = SimIoConfig {
                queue_depth: 64,
                base_latency_ns: 1_000,
                latency_jitter_ns: 500,
                seed,
                ..Default::default()
            };
            let clock = Arc::new(SimulatedClock::new());
            let storage = SimulatedStorage::new();
            let device = SimDeviceV2::new(config, Arc::clone(&clock), Arc::clone(&storage));

            let data = [0u8; 512];
            let mut completion_times = Vec::new();

            for i in 0..10u64 {
                let _ = unsafe {
                    device.write_async(
                        data.as_ptr(),
                        i * 512,
                        512,
                        noop_callback,
                        std::ptr::null_mut(),
                    )
                };
            }

            // Peek at the pending queue to get completion times.
            let pending = device.pending.lock().unwrap();
            for &(complete_at, _id) in pending.keys() {
                completion_times.push(complete_at);
            }
            completion_times
        }

        let run1 = run_and_collect(42);
        let run2 = run_and_collect(42);
        let run3 = run_and_collect(99); // different seed

        assert_eq!(run1, run2, "Same seed must produce identical latencies");
        assert_ne!(run1, run3, "Different seeds should produce different latencies");
    }

    // ── Test 5: truncate_until charges virtual time ────────────────────

    #[test]
    fn truncate_charges_virtual_time() {
        let config = SimIoConfig {
            truncate_ns_per_byte: 2,
            ..Default::default()
        };
        let (device, clock, _storage) = make_device(config);

        // Write some data so truncate has something to zero.
        device.write_sync(0, &[0xAA; 1024]).unwrap();

        let before = clock.now_nanos();
        device.truncate_until(512);
        let after = clock.now_nanos();

        // Should have charged 512 * 2 = 1024 ns.
        assert_eq!(after - before, 1024);
    }

    // ── Test 6: Fault injection on writes ──────────────────────────────

    #[test]
    fn write_fault_injection() {
        let config = SimIoConfig {
            queue_depth: 64,
            base_latency_ns: 0,
            latency_jitter_ns: 0,
            write_fault_rate: 1.0, // 100% failure
            ..Default::default()
        };
        let (device, _clock, _storage) = make_device(config);

        let statuses: Mutex<Vec<IoStatus>> = Mutex::new(Vec::new());
        let ctx = &statuses as *const Mutex<Vec<IoStatus>> as *mut u8;
        let data = [0u8; 512];

        for i in 0..5u64 {
            let _ = unsafe {
                device.write_async(data.as_ptr(), i * 512, 512, status_callback, ctx)
            };
        }

        let n = device.poll_completions();
        assert_eq!(n, 5);

        let results = statuses.lock().unwrap();
        for s in results.iter() {
            assert_eq!(*s, IoStatus::Error(-1), "All writes should have failed");
        }
    }

    #[test]
    fn write_fault_rate_partial() {
        // With a moderate fault rate, we should get a mix of successes and failures.
        let config = SimIoConfig {
            queue_depth: 256,
            base_latency_ns: 0,
            latency_jitter_ns: 0,
            write_fault_rate: 0.5,
            seed: 12345,
            ..Default::default()
        };
        let (device, _clock, _storage) = make_device(config);

        let statuses: Mutex<Vec<IoStatus>> = Mutex::new(Vec::new());
        let ctx = &statuses as *const Mutex<Vec<IoStatus>> as *mut u8;
        let data = [0u8; 512];

        for i in 0..100u64 {
            let _ = unsafe {
                device.write_async(data.as_ptr(), i * 512, 512, status_callback, ctx)
            };
        }

        device.poll_completions();

        let results = statuses.lock().unwrap();
        let errors = results.iter().filter(|s| matches!(s, IoStatus::Error(_))).count();
        let successes = results.iter().filter(|s| matches!(s, IoStatus::Success)).count();

        // With 50% rate over 100 trials, should get both (extremely unlikely to be all one).
        assert!(errors > 0, "Expected some errors with 50% fault rate");
        assert!(successes > 0, "Expected some successes with 50% fault rate");
    }

    // ── Test 7: Metrics counters ───────────────────────────────────────

    #[test]
    fn metrics_correct_after_mixed_ops() {
        let config = SimIoConfig {
            queue_depth: 4,
            base_latency_ns: 100,
            latency_jitter_ns: 0,
            ..Default::default()
        };
        let (device, clock, _storage) = make_device(config);

        let data = [0u8; 512];

        // Submit 4 writes (fills queue).
        for i in 0..4u64 {
            let r = unsafe {
                device.write_async(
                    data.as_ptr(),
                    i * 512,
                    512,
                    noop_callback,
                    std::ptr::null_mut(),
                )
            };
            assert!(matches!(r, IoRequestResult::Submitted));
        }

        // 5th should be QueueFull.
        let r = unsafe {
            device.write_async(
                data.as_ptr(),
                2048,
                512,
                noop_callback,
                std::ptr::null_mut(),
            )
        };
        assert!(matches!(r, IoRequestResult::QueueFull));

        assert_eq!(device.total_submitted(), 4);
        assert_eq!(device.total_queue_full(), 1);
        assert_eq!(device.total_completed(), 0);

        // Advance clock and drain.
        clock.advance(Duration::from_nanos(200));
        let n = device.poll_completions();
        assert_eq!(n, 4);
        assert_eq!(device.total_completed(), 4);

        // Submit 2 more and drain.
        for i in 0..2u64 {
            let r = unsafe {
                device.write_async(
                    data.as_ptr(),
                    (i + 5) * 512,
                    512,
                    noop_callback,
                    std::ptr::null_mut(),
                )
            };
            assert!(matches!(r, IoRequestResult::Submitted));
        }
        clock.advance(Duration::from_nanos(200));
        device.poll_completions();

        assert_eq!(device.total_submitted(), 6);
        assert_eq!(device.total_completed(), 6);
        assert_eq!(device.total_queue_full(), 1);
    }

    // ── Test 8: Read sync mode ─────────────────────────────────────────

    #[test]
    fn read_sync_mode_returns_completed_sync() {
        let config = SimIoConfig {
            read_sync: true,
            ..Default::default()
        };
        let (device, _clock, _storage) = make_device(config);

        // Write some data first.
        device.write_sync(0, b"hello DST v2!").unwrap();

        let mut buf = [0u8; 13];
        let counter = AtomicU32::new(0);
        let ctx = &counter as *const AtomicU32 as *mut u8;

        let r = unsafe {
            device.read_async(0, buf.as_mut_ptr(), 13, counting_callback, ctx)
        };
        assert!(matches!(r, IoRequestResult::CompletedSync));
        // Callback already fired synchronously.
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert_eq!(&buf, b"hello DST v2!");
    }

    // ── Test 9: Read async mode ────────────────────────────────────────

    #[test]
    fn read_async_mode_defers_callback() {
        let config = SimIoConfig {
            read_sync: false,
            queue_depth: 16,
            base_latency_ns: 500,
            latency_jitter_ns: 0,
            ..Default::default()
        };
        let (device, clock, _storage) = make_device(config);

        device.write_sync(0, b"async read!").unwrap();

        let mut buf = [0u8; 11];
        let counter = AtomicU32::new(0);
        let ctx = &counter as *const AtomicU32 as *mut u8;

        let r = unsafe {
            device.read_async(0, buf.as_mut_ptr(), 11, counting_callback, ctx)
        };
        assert!(matches!(r, IoRequestResult::Submitted));
        // Callback NOT yet fired.
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        // But data is already in the buffer.
        assert_eq!(&buf, b"async read!");

        // Advance and poll.
        clock.advance(Duration::from_nanos(500));
        assert_eq!(device.poll_completions(), 1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    // ── Test 10: Data roundtrip through write_async + read_sync ────────

    #[test]
    fn write_async_data_readable_immediately() {
        let config = SimIoConfig {
            queue_depth: 16,
            base_latency_ns: 10_000,
            latency_jitter_ns: 0,
            ..Default::default()
        };
        let (device, _clock, _storage) = make_device(config);

        let data = b"written via write_async";
        let _ = unsafe {
            device.write_async(
                data.as_ptr(),
                0,
                data.len() as u32,
                noop_callback,
                std::ptr::null_mut(),
            )
        };

        // Data should be in storage immediately (even before callback fires).
        let mut buf = vec![0u8; data.len()];
        device.read_sync(0, &mut buf).unwrap();
        assert_eq!(&buf, data);
    }

    // ── Test 11: now_nanos and charge on SimulatedClock ────────────────

    #[test]
    fn clock_now_nanos_and_charge() {
        let clock = SimulatedClock::new();
        assert_eq!(clock.now_nanos(), 0);

        clock.advance(Duration::from_nanos(500));
        assert_eq!(clock.now_nanos(), 500);

        clock.charge(250);
        assert_eq!(clock.now_nanos(), 750);

        // charge(0) is a no-op.
        clock.charge(0);
        assert_eq!(clock.now_nanos(), 750);
    }
}
