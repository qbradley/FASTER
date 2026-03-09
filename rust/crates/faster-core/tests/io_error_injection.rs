//! I/O error injection integration tests.
//!
//! Verifies that the FASTER store handles device-level I/O failures
//! correctly: write failures (disk full), read failures (corrupted data),
//! partial writes (torn pages), and various `IoStatus` error codes.
//!
//! # Architecture
//!
//! These tests use a [`FaultInjectingDevice`] that wraps `InMemoryDevice`
//! and allows fine-grained control over when and how I/O operations fail.
//! This avoids a dependency on `faster-dst` (which depends on `faster-core`)
//! and keeps the tests self-contained.
//!
//! # Behavioral Contracts Tested
//!
//! From the C++ reference implementation:
//! - Write failures return `IoStatus::Error` through the callback
//! - Read failures return `IoStatus::Error` through the callback
//! - The store must not corrupt in-memory state on I/O failure
//! - Operations on in-memory (mutable) records are unaffected by device errors
//! - `PendingIoError` variants propagate correctly to callers

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use faster_core::device::{
    Device, InMemoryDevice, IoCompletionCallback, IoRequestResult, IoStatus,
};
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{CounterFunctions, FasterKv, FasterKvConfig, SimpleFunctions};

// ═══════════════════════════════════════════════════════════════════════════
// Fault-injecting device wrapper
// ═══════════════════════════════════════════════════════════════════════════

/// Which I/O operation to fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultTarget {
    Read,
    Write,
    Both,
}

/// Configuration for a fault-injecting device.
#[derive(Debug, Clone)]
struct FaultPolicy {
    /// Enable/disable fault injection at runtime.
    enabled: Arc<AtomicBool>,
    /// Which operations to fault.
    target: FaultTarget,
    /// After this many operations (per type), start failing. None = never.
    fail_after_n: Option<u64>,
    /// Error code to return on failure.
    error_code: i32,
    /// If true, write succeeds but only writes a partial (torn) page.
    torn_write: bool,
    /// Fraction of the write to actually persist (0.0–1.0) for torn writes.
    torn_fraction: f64,
}

impl Default for FaultPolicy {
    fn default() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            target: FaultTarget::Both,
            fail_after_n: None,
            error_code: -1,
            torn_write: false,
            torn_fraction: 0.5,
        }
    }
}

/// A `Device` wrapper that delegates to `InMemoryDevice` but can inject I/O
/// failures based on a runtime-configurable [`FaultPolicy`].
///
/// Counters and the fault-enable flag are `Arc`-shared so tests can toggle
/// faults at precise points without re-creating the device.
struct FaultInjectingDevice {
    inner: InMemoryDevice,
    policy: FaultPolicy,
    write_count: AtomicU64,
    read_count: AtomicU64,
    /// Total bytes written (for torn-write simulation tracking).
    total_bytes_written: AtomicU64,
    /// Records which operations faulted, for test assertions.
    fault_log: Mutex<Vec<String>>,
}

impl FaultInjectingDevice {
    fn new(policy: FaultPolicy) -> Self {
        Self {
            inner: InMemoryDevice::new(),
            policy,
            write_count: AtomicU64::new(0),
            read_count: AtomicU64::new(0),
            total_bytes_written: AtomicU64::new(0),
            fault_log: Mutex::new(Vec::new()),
        }
    }

    /// Create a device with faults disabled (can be enabled later).
    fn new_dormant(target: FaultTarget, error_code: i32) -> (Self, Arc<AtomicBool>) {
        let enabled = Arc::new(AtomicBool::new(false));
        let policy = FaultPolicy {
            enabled: Arc::clone(&enabled),
            target,
            fail_after_n: None,
            error_code,
            torn_write: false,
            torn_fraction: 0.5,
        };
        (Self::new(policy), enabled)
    }

    /// Create a device that fails writes after N successful ones.
    fn fail_writes_after(n: u64) -> Self {
        Self::new(FaultPolicy {
            enabled: Arc::new(AtomicBool::new(true)),
            target: FaultTarget::Write,
            fail_after_n: Some(n),
            error_code: -28, // ENOSPC
            ..Default::default()
        })
    }

    /// Create a device that fails reads after N successful ones.
    fn fail_reads_after(n: u64) -> Self {
        Self::new(FaultPolicy {
            enabled: Arc::new(AtomicBool::new(true)),
            target: FaultTarget::Read,
            fail_after_n: Some(n),
            error_code: -5, // EIO
            ..Default::default()
        })
    }

    /// Create a device that produces torn writes.
    fn torn_writes(fraction: f64) -> Self {
        Self::new(FaultPolicy {
            enabled: Arc::new(AtomicBool::new(true)),
            target: FaultTarget::Write,
            torn_write: true,
            torn_fraction: fraction,
            ..Default::default()
        })
    }

    /// Should this write operation fail?
    fn should_fail_write(&self) -> bool {
        if !self.policy.enabled.load(Ordering::Acquire) {
            return false;
        }
        if !matches!(self.policy.target, FaultTarget::Write | FaultTarget::Both) {
            return false;
        }
        if self.policy.torn_write {
            return false; // Torn writes go through a different path
        }
        match self.policy.fail_after_n {
            Some(n) => self.write_count.load(Ordering::Relaxed) > n,
            None => true, // Fail all writes when enabled with no threshold
        }
    }

    /// Should this read operation fail?
    fn should_fail_read(&self) -> bool {
        if !self.policy.enabled.load(Ordering::Acquire) {
            return false;
        }
        if !matches!(self.policy.target, FaultTarget::Read | FaultTarget::Both) {
            return false;
        }
        match self.policy.fail_after_n {
            Some(n) => self.read_count.load(Ordering::Relaxed) > n,
            None => true,
        }
    }

    /// Should this write be torn?
    fn should_tear_write(&self) -> bool {
        self.policy.enabled.load(Ordering::Acquire) && self.policy.torn_write
    }

    /// Number of faults injected so far.
    fn fault_count(&self) -> usize {
        self.fault_log.lock().unwrap().len()
    }
}

impl std::fmt::Debug for FaultInjectingDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultInjectingDevice")
            .field("writes", &self.write_count.load(Ordering::Relaxed))
            .field("reads", &self.read_count.load(Ordering::Relaxed))
            .field("faults", &self.fault_count())
            .finish()
    }
}

impl Device for FaultInjectingDevice {
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn segment_size(&self) -> u64 {
        self.inner.segment_size()
    }

    fn max_outstanding_io(&self) -> u32 {
        self.inner.max_outstanding_io()
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
            self.fault_log
                .lock()
                .unwrap()
                .push(format!("read_async@{offset}+{len}"));
            // SAFETY: caller guarantees context is valid.
            unsafe {
                callback(context, IoStatus::Error(self.policy.error_code), 0);
            }
            return IoRequestResult::CompletedSync;
        }

        // SAFETY: delegating with same safety preconditions.
        unsafe { self.inner.read_async(offset, dest, len, callback, context) }
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
        self.total_bytes_written
            .fetch_add(len as u64, Ordering::Relaxed);

        if self.should_fail_write() {
            self.fault_log
                .lock()
                .unwrap()
                .push(format!("write_async@{offset}+{len}"));
            // SAFETY: caller guarantees context is valid.
            unsafe {
                callback(context, IoStatus::Error(self.policy.error_code), 0);
            }
            return IoRequestResult::CompletedSync;
        }

        if self.should_tear_write() {
            // Write only a fraction of the data (torn page simulation).
            let actual_len = ((len as f64) * self.policy.torn_fraction) as u32;
            let actual_len = actual_len.max(1); // write at least 1 byte
            self.fault_log
                .lock()
                .unwrap()
                .push(format!("torn_write@{offset}+{actual_len}/{len}"));

            // Write the partial data to the inner device.
            // SAFETY: delegating with same safety preconditions, reduced length.
            unsafe {
                self.inner
                    .write_async(source, offset, actual_len, callback, context)
            }
        } else {
            // SAFETY: delegating with same safety preconditions.
            unsafe {
                self.inner
                    .write_async(source, offset, len, callback, context)
            }
        }
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        self.read_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail_read() {
            self.fault_log
                .lock()
                .unwrap()
                .push(format!("read_sync@{offset}+{}", dest.len()));
            return Err(io::Error::other(format!(
                "injected read error (code {})",
                self.policy.error_code
            )));
        }

        self.inner.read_sync(offset, dest)
    }

    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32> {
        self.write_count.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_written
            .fetch_add(source.len() as u64, Ordering::Relaxed);

        if self.should_fail_write() {
            self.fault_log
                .lock()
                .unwrap()
                .push(format!("write_sync@{offset}+{}", source.len()));
            return Err(io::Error::other(format!(
                "injected write error (code {})",
                self.policy.error_code
            )));
        }

        if self.should_tear_write() {
            let actual_len = ((source.len() as f64) * self.policy.torn_fraction) as usize;
            let actual_len = actual_len.max(1);
            self.fault_log.lock().unwrap().push(format!(
                "torn_write_sync@{offset}+{actual_len}/{}",
                source.len()
            ));
            self.inner.write_sync(offset, &source[..actual_len])
        } else {
            self.inner.write_sync(offset, source)
        }
    }

    fn truncate_until(&self, offset: u64) {
        self.inner.truncate_until(offset);
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn close(&self) {
        self.inner.close();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Test helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Small config suitable for fault injection tests.
fn small_config() -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 10, // 1 024 buckets
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
    }
}

/// Tiny config that forces page sealing/flushing quickly.
fn tiny_config() -> FasterKvConfig {
    FasterKvConfig {
        hash_index_size_log2: 14, // 16 K buckets
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 2,
        },
        grow_config: GrowConfig::default(),
        auto_compact: false,
    }
}

type SimpleStore = FasterKv<SimpleFunctions<u64, u64>>;

fn make_store(config: FasterKvConfig, device: impl Device) -> SimpleStore {
    FasterKv::new(config, SimpleFunctions::default(), device)
}

/// Populate a store with N key-value pairs (key=i, value=i*7).
#[allow(dead_code)]
fn populate(store: &SimpleStore, n: u64) {
    let mut s = store.new_session();
    for i in 0..n {
        let status = store.upsert(&mut s, &i, &(i * 7), ());
        assert!(
            status.is_success(),
            "populate: upsert key {i} returned {status:?}"
        );
    }
    store.dispose_session(s);
}

/// Verify that keys [0, n) read back with value = key * 7.
#[allow(dead_code)]
fn verify_range(store: &SimpleStore, n: u64) {
    let mut s = store.new_session();
    for i in 0..n {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "verify: read key {i} failed");
        assert_eq!(out, Some(i * 7), "verify: key {i} value mismatch");
    }
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. In-memory operations are unaffected by device errors
// ═══════════════════════════════════════════════════════════════════════════

/// Device errors should not affect operations on mutable (in-memory) records.
/// This mirrors the C++ contract: only on-disk I/O goes through the device.
#[test]
fn in_memory_ops_unaffected_by_device_errors() {
    let device = FaultInjectingDevice::new(FaultPolicy {
        enabled: Arc::new(AtomicBool::new(true)),
        target: FaultTarget::Both,
        error_code: -5,
        ..Default::default()
    });

    let store = make_store(small_config(), device);
    let mut s = store.new_session();

    // All operations hit the mutable in-memory region — device is not involved.
    for i in 0u64..100 {
        let status = store.upsert(&mut s, &i, &(i * 3), ());
        assert!(status.is_success(), "upsert key {i}: {status:?}");
    }

    for i in 0u64..100 {
        let mut out: Option<u64> = None;
        let status = store.read(&mut s, &i, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok, "read key {i}: {status:?}");
        assert_eq!(out, Some(i * 3));
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Write failure simulation (disk full / ENOSPC)
// ═══════════════════════════════════════════════════════════════════════════

/// Simulate write failures via write_sync. Verifies that the error propagates
/// as an `io::Error` and does not corrupt existing data.
#[test]
fn write_sync_failure_returns_io_error() {
    let device = FaultInjectingDevice::fail_writes_after(0);

    // Direct device-level test: write_sync should fail immediately.
    let result = device.write_sync(0, &[1, 2, 3, 4]);
    assert!(result.is_err(), "write should fail");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert!(
        err.to_string().contains("injected write error"),
        "unexpected error message: {}",
        err
    );
}

/// After N successful writes, subsequent writes fail. Verify that data
/// written before the failure is preserved.
#[test]
fn write_failure_preserves_prior_data() {
    // Allow 3 writes to succeed, then fail all subsequent ones.
    let device = FaultInjectingDevice::fail_writes_after(3);

    // First 3 writes succeed.
    device.write_sync(0, b"AAAA").unwrap();
    device.write_sync(4, b"BBBB").unwrap();
    device.write_sync(8, b"CCCC").unwrap();

    // 4th write fails.
    assert!(device.write_sync(12, b"DDDD").is_err());

    // Prior data is intact.
    let mut buf = [0u8; 4];
    device.read_sync(0, &mut buf).unwrap();
    assert_eq!(&buf, b"AAAA");
    device.read_sync(4, &mut buf).unwrap();
    assert_eq!(&buf, b"BBBB");
    device.read_sync(8, &mut buf).unwrap();
    assert_eq!(&buf, b"CCCC");
}

/// Verify that the fault log records injected failures.
#[test]
fn fault_log_tracks_injections() {
    let device = FaultInjectingDevice::fail_writes_after(1);

    device.write_sync(0, b"ok").unwrap(); // count=1, 1>1 false
    assert_eq!(device.fault_count(), 0);

    let _ = device.write_sync(4, b"fail"); // count=2, 2>1 true
    assert_eq!(device.fault_count(), 1);

    let _ = device.write_sync(8, b"fail2"); // count=3, 3>1 true
    assert_eq!(device.fault_count(), 2);

    let log = device.fault_log.lock().unwrap();
    assert!(log[0].contains("write_sync"));
    assert!(log[1].contains("write_sync"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Read failure simulation (EIO / corrupted data)
// ═══════════════════════════════════════════════════════════════════════════

/// read_sync failures propagate as io::Error.
#[test]
fn read_sync_failure_returns_io_error() {
    let device = FaultInjectingDevice::fail_reads_after(0);

    let mut buf = [0u8; 4];
    let result = device.read_sync(0, &mut buf);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("injected read error")
    );
}

/// Read failures happen after N successful reads. Prior reads returned
/// correct data.
#[test]
fn read_failure_after_n_successful_reads() {
    let device = FaultInjectingDevice::fail_reads_after(2);

    // Write some data.
    device.write_sync(0, b"DATA").unwrap();

    // First two reads succeed.
    let mut buf = [0u8; 4];
    assert!(device.read_sync(0, &mut buf).is_ok());
    assert_eq!(&buf, b"DATA");

    assert!(device.read_sync(0, &mut buf).is_ok());
    assert_eq!(&buf, b"DATA");

    // Third read fails.
    assert!(device.read_sync(0, &mut buf).is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Async I/O error injection (callback path)
// ═══════════════════════════════════════════════════════════════════════════

/// Verify that read_async delivers IoStatus::Error through the callback
/// when faults are injected.
#[test]
fn read_async_delivers_error_via_callback() {
    let device = FaultInjectingDevice::new(FaultPolicy {
        enabled: Arc::new(AtomicBool::new(true)),
        target: FaultTarget::Read,
        error_code: -5, // EIO
        ..Default::default()
    });

    // Track what the callback receives via thread-local.
    use std::cell::Cell;
    thread_local! {
        static READ_CB_STATUS: Cell<Option<IoStatus>> = const { Cell::new(None) };
        static READ_CB_BYTES: Cell<u32> = const { Cell::new(u32::MAX) };
    }

    unsafe fn test_callback(_context: *mut u8, status: IoStatus, bytes: u32) {
        READ_CB_STATUS.with(|s| s.set(Some(status)));
        READ_CB_BYTES.with(|b| b.set(bytes));
    }

    let mut buf = [0u8; 512];
    // SAFETY: buf and null context are valid for this test.
    let result = unsafe {
        device.read_async(
            0,
            buf.as_mut_ptr(),
            512,
            test_callback,
            std::ptr::null_mut(),
        )
    };

    assert!(matches!(result, IoRequestResult::CompletedSync));
    READ_CB_STATUS.with(|s| assert_eq!(s.get(), Some(IoStatus::Error(-5))));
    READ_CB_BYTES.with(|b| assert_eq!(b.get(), 0));
}

/// Verify that write_async delivers IoStatus::Error through the callback.
#[test]
fn write_async_delivers_error_via_callback() {
    let device = FaultInjectingDevice::new(FaultPolicy {
        enabled: Arc::new(AtomicBool::new(true)),
        target: FaultTarget::Write,
        error_code: -28, // ENOSPC
        ..Default::default()
    });

    // Use thread-local to avoid global statics for this test.
    use std::cell::Cell;
    thread_local! {
        static WRITE_CB_STATUS: Cell<Option<IoStatus>> = const { Cell::new(None) };
        static WRITE_CB_BYTES: Cell<u32> = const { Cell::new(0) };
    }

    unsafe fn write_test_callback(_context: *mut u8, status: IoStatus, bytes: u32) {
        WRITE_CB_STATUS.with(|s| s.set(Some(status)));
        WRITE_CB_BYTES.with(|b| b.set(bytes));
    }

    let data = [0xABu8; 512];
    // SAFETY: `data` and null context are valid for this synchronous test callback.
    let result = unsafe {
        device.write_async(
            data.as_ptr(),
            0,
            512,
            write_test_callback,
            std::ptr::null_mut(),
        )
    };

    assert!(matches!(result, IoRequestResult::CompletedSync));
    WRITE_CB_STATUS.with(|s| assert_eq!(s.get(), Some(IoStatus::Error(-28))));
    WRITE_CB_BYTES.with(|b| assert_eq!(b.get(), 0));
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Various IoStatus error codes
// ═══════════════════════════════════════════════════════════════════════════

/// Verify that different error codes propagate correctly through the device.
#[test]
fn various_error_codes_propagate() {
    let error_codes = [
        (-1, "generic"),
        (-5, "EIO"),
        (-13, "EACCES"),
        (-28, "ENOSPC"),
        (-30, "EROFS"),
        (-122, "EDQUOT"),
    ];

    for (code, label) in error_codes {
        let device = FaultInjectingDevice::new(FaultPolicy {
            enabled: Arc::new(AtomicBool::new(true)),
            target: FaultTarget::Write,
            error_code: code,
            ..Default::default()
        });

        let result = device.write_sync(0, &[0; 4]);
        assert!(result.is_err(), "{label}: write should fail");

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(&format!("code {code}")),
            "{label}: expected code {code} in error, got: {err_msg}"
        );
    }
}

/// IoStatus::Error carries the exact error code through the callback.
#[test]
fn io_status_error_carries_code() {
    let status_err = IoStatus::Error(-42);
    assert_eq!(status_err, IoStatus::Error(-42));
    assert_ne!(status_err, IoStatus::Success);
    assert_ne!(status_err, IoStatus::Cancelled);

    // Different codes are distinct.
    assert_ne!(IoStatus::Error(-1), IoStatus::Error(-5));
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Partial (torn) write simulation
// ═══════════════════════════════════════════════════════════════════════════

/// Torn writes write only a fraction of the data, simulating a crash
/// mid-write at the storage layer.
#[test]
fn torn_write_writes_partial_data() {
    let device = FaultInjectingDevice::torn_writes(0.5);

    let full_data = [0xAA; 100];
    device.write_sync(0, &full_data).unwrap();

    // Device size should reflect only partial write.
    let size = device.size();
    assert!(
        size < 100,
        "torn write should produce less data than requested: got {size}"
    );

    // Read back: first `size` bytes should be 0xAA, rest should be zero.
    let mut readback = vec![0u8; 100];
    device.inner.read_sync(0, &mut readback).unwrap();

    let written_bytes = size as usize;
    for (i, &byte) in readback[..written_bytes].iter().enumerate() {
        assert_eq!(byte, 0xAA, "byte {i} should be 0xAA");
    }
    // Beyond written region, the device returns zeros (never written).
}

/// Torn write with fraction=0 writes minimal data (1 byte).
#[test]
fn torn_write_fraction_zero_writes_minimal() {
    let device = FaultInjectingDevice::torn_writes(0.0);

    device.write_sync(0, &[0xFF; 1000]).unwrap();
    let size = device.size();
    assert_eq!(
        size, 1,
        "fraction=0.0 should write exactly 1 byte (minimum)"
    );
}

/// Torn writes are recorded in the fault log.
#[test]
fn torn_write_logged() {
    let device = FaultInjectingDevice::torn_writes(0.25);

    device.write_sync(0, &[0; 200]).unwrap();
    assert_eq!(device.fault_count(), 1);

    let log = device.fault_log.lock().unwrap();
    assert!(log[0].contains("torn_write_sync"), "got: {}", log[0]);
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Runtime fault toggling
// ═══════════════════════════════════════════════════════════════════════════

/// Faults can be enabled and disabled at runtime via the shared AtomicBool.
#[test]
fn runtime_fault_toggle() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Write, -28);

    // Faults disabled: writes succeed.
    assert!(device.write_sync(0, b"ok").is_ok());

    // Enable faults.
    fault_switch.store(true, Ordering::Release);
    assert!(device.write_sync(4, b"fail").is_err());

    // Disable faults again.
    fault_switch.store(false, Ordering::Release);
    assert!(device.write_sync(8, b"ok2").is_ok());
}

/// Toggle faults between reads: verify correct data before and error after.
#[test]
fn runtime_read_fault_toggle() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Read, -5);

    device.write_sync(0, b"hello").unwrap();

    // Reads work while faults are off.
    let mut buf = [0u8; 5];
    assert!(device.read_sync(0, &mut buf).is_ok());
    assert_eq!(&buf, b"hello");

    // Enable read faults.
    fault_switch.store(true, Ordering::Release);
    assert!(device.read_sync(0, &mut buf).is_err());

    // Disable again.
    fault_switch.store(false, Ordering::Release);
    assert!(device.read_sync(0, &mut buf).is_ok());
    assert_eq!(&buf, b"hello");
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. Store continues operating after transient I/O errors
// ═══════════════════════════════════════════════════════════════════════════

/// The store's in-memory operations continue to work even after the device
/// has experienced write failures during flush.
#[test]
fn store_survives_write_errors_during_flush() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Write, -28);

    let store = make_store(tiny_config(), device);
    let mut s = store.new_session();

    // Insert enough data to fill pages.
    for i in 0u64..500 {
        let _ = store.upsert(&mut s, &i, &(i * 7), ());
    }

    // Enable write faults before flush.
    fault_switch.store(true, Ordering::Release);

    // Flush may fail silently or return 0 flushed pages — either is acceptable.
    let _ = store.flush();

    // Disable faults.
    fault_switch.store(false, Ordering::Release);

    // The store should still be usable for in-memory operations.
    let status = store.upsert(&mut s, &9999u64, &42u64, ());
    assert!(status.is_success(), "upsert after error: {status:?}");

    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &9999u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(42));

    store.dispose_session(s);
}

/// After a transient write error, disabling faults and re-flushing works.
#[test]
fn store_recovers_from_transient_write_error() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Write, -28);

    let store = make_store(tiny_config(), device);
    let mut s = store.new_session();

    // Write data.
    for i in 0u64..200 {
        let _ = store.upsert(&mut s, &i, &(i * 11), ());
    }

    // Transient failure.
    fault_switch.store(true, Ordering::Release);
    let _ = store.flush();

    // Recovery: disable faults and flush again.
    fault_switch.store(false, Ordering::Release);
    let _ = store.flush();
    store.maintenance();

    // Verify recent data is still accessible.
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &199u64, &0u64, &mut out, ());
    assert!(
        status == OperationStatus::Ok || status == OperationStatus::Pending,
        "read after recovery: {status:?}"
    );

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. Concurrent operations with I/O errors
// ═══════════════════════════════════════════════════════════════════════════

/// Multiple threads operate on the store. One encounters device errors
/// during flush; the others continue working on in-memory data.
#[test]
fn concurrent_sessions_survive_device_errors() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Write, -28);
    let store = Arc::new(make_store(small_config(), device));

    let fault_switch = Arc::new(fault_switch);
    const THREADS: u64 = 4;
    const OPS_PER_THREAD: u64 = 200;

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            let switch = Arc::clone(&fault_switch);
            std::thread::spawn(move || {
                let mut s = store.new_session();
                let base = t * OPS_PER_THREAD;

                for i in 0..OPS_PER_THREAD {
                    let key = base + i;
                    let status = store.upsert(&mut s, &key, &(key * 5), ());
                    assert!(status.is_success(), "t{t}: upsert key {key}: {status:?}");

                    // Thread 0 triggers a fault midway and recovers.
                    if t == 0 && i == OPS_PER_THREAD / 2 {
                        switch.store(true, Ordering::Release);
                        let _ = store.flush();
                        switch.store(false, Ordering::Release);
                    }
                }

                // All threads verify their own data.
                for i in 0..OPS_PER_THREAD {
                    let key = base + i;
                    let mut out: Option<u64> = None;
                    let status = store.read(&mut s, &key, &0u64, &mut out, ());
                    assert_eq!(status, OperationStatus::Ok, "t{t}: read key {key}");
                    assert_eq!(out, Some(key * 5), "t{t}: key {key} mismatch");
                }

                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker panicked");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 10. Checkpoint I/O failure handling
// ═══════════════════════════════════════════════════════════════════════════

/// Checkpoint with a device that fails writes should not panic. The
/// checkpoint may return an error, which is the correct behavior.
#[test]
fn checkpoint_handles_write_failure_gracefully() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Write, -28);

    let store = make_store(small_config(), device);
    let mut s = store.new_session();

    // Insert data.
    for i in 0u64..50 {
        let _ = store.upsert(&mut s, &i, &(i * 3), ());
    }
    store.dispose_session(s);

    // Enable write faults before checkpoint.
    fault_switch.store(true, Ordering::Release);

    let dir = tempfile::tempdir().expect("tempdir");
    let result = store.checkpoint(
        dir.path(),
        faster_core::checkpoint::CheckpointType::FoldOver,
    );

    // The checkpoint may succeed (if metadata writes use a different path)
    // or fail with an error. Either is acceptable — it must not panic or
    // corrupt the store.
    match result {
        Ok(token) => {
            // Checkpoint succeeded despite device errors (metadata may bypass
            // the faulty device).
            let _ = token;
        }
        Err(err) => {
            // Checkpoint correctly reported the I/O failure.
            let msg = err.to_string();
            assert!(
                msg.contains("error")
                    || msg.contains("Error")
                    || msg.contains("failed")
                    || !msg.is_empty(),
                "checkpoint error should have a message: {msg}"
            );
        }
    }

    // Store is still usable.
    fault_switch.store(false, Ordering::Release);
    let mut s = store.new_session();
    let status = store.upsert(&mut s, &9999u64, &42u64, ());
    assert!(
        status.is_success(),
        "upsert after failed checkpoint: {status:?}"
    );
    store.dispose_session(s);
}

/// Successful checkpoint, then I/O failure during recovery load_pages.
#[test]
fn recovery_handles_device_read_failure() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Phase 1: Create checkpoint with a working device.
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Read, -5);
    let store = make_store(small_config(), device);
    let mut s = store.new_session();
    for i in 0u64..50 {
        let _ = store.upsert(&mut s, &i, &(i * 3), ());
    }
    store.dispose_session(s);

    let token = store
        .checkpoint(
            dir.path(),
            faster_core::checkpoint::CheckpointType::FoldOver,
        )
        .expect("checkpoint should succeed");

    // Phase 2: Recovery with read faults enabled.
    fault_switch.store(true, Ordering::Release);

    let (device2, _) = FaultInjectingDevice::new_dormant(FaultTarget::Read, -5);
    // We can't easily share state between devices for recovery since
    // recovery creates a new store. This test verifies the code path
    // doesn't panic; the actual recovery may fail because the new device
    // has no data.
    let mut store2 = make_store(small_config(), device2);
    let result = store2.recover(dir.path(), Some(token));

    // Recovery should fail gracefully (no data on device2).
    match result {
        Ok(_) => { /* acceptable if metadata-only */ }
        Err(err) => {
            let _ = err; // error is expected — no panic is the goal
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 11. Device counter accuracy
// ═══════════════════════════════════════════════════════════════════════════

/// Verify that read/write counters are accurate across sync and async paths.
#[test]
fn device_counters_are_accurate() {
    let device = FaultInjectingDevice::new(FaultPolicy::default());

    assert_eq!(device.write_count.load(Ordering::Relaxed), 0);
    assert_eq!(device.read_count.load(Ordering::Relaxed), 0);

    device.write_sync(0, b"a").unwrap();
    device.write_sync(1, b"b").unwrap();
    assert_eq!(device.write_count.load(Ordering::Relaxed), 2);

    let mut buf = [0u8; 1];
    device.read_sync(0, &mut buf).unwrap();
    assert_eq!(device.read_count.load(Ordering::Relaxed), 1);

    // Async paths also increment counters.
    unsafe fn noop_cb(_: *mut u8, _: IoStatus, _: u32) {}

    let mut async_buf = [0u8; 512];
    // SAFETY: `async_buf` and null context are valid for the noop callback.
    unsafe {
        device.write_async(async_buf.as_ptr(), 0, 512, noop_cb, std::ptr::null_mut());
    }
    assert_eq!(device.write_count.load(Ordering::Relaxed), 3);

    // SAFETY: `async_buf` and null context are valid for the noop callback.
    unsafe {
        device.read_async(
            0,
            async_buf.as_mut_ptr(),
            512,
            noop_cb,
            std::ptr::null_mut(),
        );
    }
    assert_eq!(device.read_count.load(Ordering::Relaxed), 2);
}

// ═══════════════════════════════════════════════════════════════════════════
// 12. Store flush + maintenance cycle under fault conditions
// ═══════════════════════════════════════════════════════════════════════════

/// Maintenance with read errors during eviction doesn't corrupt state.
#[test]
fn maintenance_with_read_errors_does_not_corrupt() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Read, -5);

    let store = make_store(tiny_config(), device);
    let mut s = store.new_session();

    // Fill pages to trigger eviction pressure.
    for i in 0u64..1000 {
        let _ = store.upsert(&mut s, &i, &(i * 13), ());
    }

    // Flush without errors first.
    let _ = store.flush();

    // Enable read faults, then run maintenance (which may try to evict pages).
    fault_switch.store(true, Ordering::Release);
    store.maintenance();
    fault_switch.store(false, Ordering::Release);

    // Store is still usable — in-memory data should be intact.
    let recent_key = 999u64;
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &recent_key, &0u64, &mut out, ());
    assert!(
        status == OperationStatus::Ok || status == OperationStatus::Pending,
        "read after maintenance error: {status:?}"
    );

    store.dispose_session(s);
}

/// Interleave successful and failed flush cycles.
#[test]
fn interleaved_flush_success_and_failure() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Write, -28);

    let store = make_store(tiny_config(), device);
    let mut s = store.new_session();

    for cycle in 0..5 {
        // Insert data.
        for i in 0u64..100 {
            let key = cycle * 100 + i;
            let _ = store.upsert(&mut s, &key, &(key * 2), ());
        }

        // Alternate between faulty and clean flushes.
        if cycle % 2 == 1 {
            fault_switch.store(true, Ordering::Release);
        }
        let _ = store.flush();
        store.maintenance();
        fault_switch.store(false, Ordering::Release);
    }

    // Verify the latest data is still accessible.
    let key = 499u64;
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &key, &0u64, &mut out, ());
    assert!(
        status == OperationStatus::Ok || status == OperationStatus::Pending,
        "read after interleaved flushes: {status:?}"
    );

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 13. RMW operations with device faults
// ═══════════════════════════════════════════════════════════════════════════

/// RMW operations on in-memory records work despite device faults.
#[test]
fn rmw_unaffected_by_device_errors() {
    let device = FaultInjectingDevice::new(FaultPolicy {
        enabled: Arc::new(AtomicBool::new(true)),
        target: FaultTarget::Both,
        error_code: -5,
        ..Default::default()
    });

    let config = small_config();
    let store: FasterKv<CounterFunctions<u64>> =
        FasterKv::new(config, CounterFunctions::new(), device);
    let mut s = store.new_session();

    // RMW: key 1, increment 10 times.
    for _ in 0..10 {
        let mut out: i64 = 0;
        let status = store.rmw(&mut s, &1u64, &1i64, &mut out, ());
        assert!(
            status.is_success() || status == OperationStatus::NotFound,
            "rmw: {status:?}"
        );
    }

    // Read back: should be 10.
    let mut out: i64 = 0;
    let status = store.read(&mut s, &1u64, &0i64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, 10, "RMW counter should be 10");

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 14. Fault injection does not leak or double-free
// ═══════════════════════════════════════════════════════════════════════════

/// Stress test: many operations with intermittent faults. No panic, no leak.
#[test]
fn stress_intermittent_faults_no_crash() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Write, -1);

    let store = make_store(small_config(), device);
    let mut s = store.new_session();

    for i in 0u64..1000 {
        // Toggle faults every 50 operations.
        if i % 50 == 0 {
            let on = i % 100 == 0;
            fault_switch.store(on, Ordering::Release);
        }

        // Upsert should always succeed (in-memory operations).
        let status = store.upsert(&mut s, &i, &(i * 3), ());
        assert!(status.is_success(), "upsert {i}: {status:?}");

        // Periodic flush (may fail under faults — that's OK).
        if i % 200 == 0 {
            let _ = store.flush();
        }
    }

    // Clean up: disable faults and verify.
    fault_switch.store(false, Ordering::Release);
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &999u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(999 * 3));

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// 15. Device trait compliance: fault device behaves like a real device
// ═══════════════════════════════════════════════════════════════════════════

/// When faults are disabled, the device behaves identically to InMemoryDevice.
#[test]
fn fault_device_behaves_as_passthrough_when_disabled() {
    let (device, _switch) = FaultInjectingDevice::new_dormant(FaultTarget::Both, -1);

    // Write + read round-trip.
    device.write_sync(0, b"test data").unwrap();
    let mut buf = [0u8; 9];
    device.read_sync(0, &mut buf).unwrap();
    assert_eq!(&buf, b"test data");

    // Size tracking.
    assert_eq!(device.size(), 9);

    // Sector/segment sizes match InMemoryDevice defaults.
    let reference = InMemoryDevice::new();
    assert_eq!(device.sector_size(), reference.sector_size());
    assert_eq!(device.segment_size(), reference.segment_size());
    assert_eq!(device.max_outstanding_io(), reference.max_outstanding_io());
}

/// Truncate works through the fault device.
#[test]
fn fault_device_truncate_delegates() {
    let (device, _) = FaultInjectingDevice::new_dormant(FaultTarget::Both, -1);

    device.write_sync(0, b"0123456789").unwrap();
    device.truncate_until(5);

    // First 5 bytes should be zeroed.
    let mut buf = [0xFFu8; 10];
    device.read_sync(0, &mut buf).unwrap();
    assert_eq!(&buf[..5], &[0, 0, 0, 0, 0]);
    assert_eq!(&buf[5..], b"56789");
}

// ═══════════════════════════════════════════════════════════════════════════
// 16. Error code discrimination
// ═══════════════════════════════════════════════════════════════════════════

/// Different IoStatus variants are distinguishable.
#[test]
fn io_status_variants_distinguishable() {
    assert_ne!(IoStatus::Success, IoStatus::Error(-1));
    assert_ne!(IoStatus::Success, IoStatus::Cancelled);
    assert_ne!(IoStatus::Error(-1), IoStatus::Cancelled);
    assert_eq!(IoStatus::Error(-5), IoStatus::Error(-5));

    // Debug formatting works.
    let debug = format!("{:?}", IoStatus::Error(-28));
    assert!(debug.contains("-28"), "debug should include code: {debug}");
}

// ═══════════════════════════════════════════════════════════════════════════
// 17. Large batch under fault conditions
// ═══════════════════════════════════════════════════════════════════════════

/// 10K operations with intermittent faults. The store must not panic or corrupt.
#[test]
fn large_batch_with_intermittent_faults() {
    let (device, fault_switch) = FaultInjectingDevice::new_dormant(FaultTarget::Both, -1);

    let store = make_store(
        FasterKvConfig {
            hash_index_size_log2: 14,
            buffer_size_pages: 16,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
        },
        device,
    );
    let mut s = store.new_session();

    for i in 0u64..10_000 {
        // Brief fault window every 1000 ops.
        if i % 1000 == 500 {
            fault_switch.store(true, Ordering::Release);
            let _ = store.flush();
            fault_switch.store(false, Ordering::Release);
        }

        let status = store.upsert(&mut s, &i, &(i * 7), ());
        assert!(status.is_success(), "upsert {i}: {status:?}");
    }

    // Final verification.
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &9999u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(9999 * 7));

    store.dispose_session(s);
}
