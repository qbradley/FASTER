//! Tests for write-path pending I/O completion (copy-to-tail on disk records).
//!
//! These tests verify that upsert, RMW, and delete operations on records that
//! have been evicted to disk are properly completed when the async I/O finishes.
//!
//! **Performance note:** Pages are 32 MiB (compile-time constant). To trigger
//! eviction quickly, we use a 4 KiB padded value type so each record fills
//! ~4 KiB instead of 24 bytes. This means ~8K records per page instead of
//! ~1.4M, reducing the fill from 7M records (~18s) to ~40K records (<1s).

use faster_core::InMemoryDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::record::Value;
use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

// ── Padded value type for fast page filling ──────────────────────────

const PAD_SIZE: usize = 4096;

/// A 4 KiB value that carries a `u64` payload in its first 8 bytes.
/// The padding ensures each record fills ~4 KiB, so pages fill with
/// far fewer records than tiny `u64` values.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct BigVal {
    payload: u64,
    _pad: [u8; PAD_SIZE - 8],
}

impl Default for BigVal {
    fn default() -> Self {
        Self {
            payload: 0,
            _pad: [0u8; PAD_SIZE - 8],
        }
    }
}

impl std::fmt::Debug for BigVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BigVal")
            .field("payload", &self.payload)
            .finish()
    }
}

impl BigVal {
    fn new(v: u64) -> Self {
        Self {
            payload: v,
            _pad: [0u8; PAD_SIZE - 8],
        }
    }
}

impl Value for BigVal {
    fn serialized_size(&self) -> usize {
        PAD_SIZE
    }

    fn serialize(&self, buf: &mut [u8]) -> usize {
        buf[..8].copy_from_slice(&self.payload.to_le_bytes());
        buf[8..PAD_SIZE].fill(0);
        PAD_SIZE
    }

    fn deserialize(buf: &[u8]) -> Self {
        let payload = u64::from_le_bytes(buf[..8].try_into().unwrap());
        Self::new(payload)
    }
}

/// Number of records to fill enough pages and force eviction.
/// Each page = 32 MiB, each record ≈ 4 KiB → ~8192 records/page.
/// With buffer_size_pages=4 and max_in_memory_pages=2, ~3 pages triggers eviction.
const RECORD_COUNT: u64 = 25_000;

/// Create an in-memory-device-backed store with BigVal values.
fn test_store() -> FasterKv<SimpleFunctions<u64, BigVal>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 16, // 64K buckets
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 2,
            eviction_batch_size: 2,
        },
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}

/// Insert records then force eviction via maintenance.
fn fill_then_evict(
    store: &FasterKv<SimpleFunctions<u64, BigVal>>,
    session: &mut faster_core::store::FasterSession<SimpleFunctions<u64, BigVal>>,
    count: u64,
) {
    for i in 0..count {
        let _ = store.upsert(session, &i, &BigVal::new(i * 7), ());
    }
    // With InMemoryDevice, flush completes synchronously. Each maintenance
    // cycle shifts RO, flushes sealed pages (instantly), and evicts flushed.
    for _ in 0..20 {
        store.maintenance();
    }
}

/// Find a key that is currently evicted (on-disk).
fn find_evicted_key(
    store: &FasterKv<SimpleFunctions<u64, BigVal>>,
    session: &mut faster_core::store::FasterSession<SimpleFunctions<u64, BigVal>>,
    range: std::ops::Range<u64>,
) -> Option<u64> {
    for i in range {
        let mut out: Option<BigVal> = None;
        let status = store.read(session, &i, &BigVal::default(), &mut out, ());
        if status == OperationStatus::Pending {
            // Drain this pending read so it doesn't interfere.
            let _ = store.try_complete_pending(session, true);
            return Some(i);
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════
// 1. Upsert on evicted (on-disk) record
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_upsert_on_disk_record() {
    let store = test_store();
    let mut s = store.new_session();

    // Fill then evict.
    fill_then_evict(&store, &mut s, RECORD_COUNT);

    // Find an evicted key (scan early keys — they're the oldest).
    let target_key = find_evicted_key(&store, &mut s, 0..RECORD_COUNT)
        .expect("expected at least one evicted key");

    // Upsert a new value to the evicted key.
    let new_value = BigVal::new(999_999);
    let status = store.upsert(&mut s, &target_key, &new_value, ());
    assert_eq!(
        status,
        OperationStatus::Pending,
        "upsert on evicted key should return Pending, got {status:?}"
    );

    // Complete the pending I/O.
    let result = store.try_complete_pending(&mut s, true);
    assert!(result.is_done(), "all pending ops should be done");

    // Read back — the upserted value must be visible.
    let mut out: Option<BigVal> = None;
    let status = store.read(&mut s, &target_key, &BigVal::default(), &mut out, ());
    match status {
        OperationStatus::Ok => {
            assert_eq!(
                out.map(|v| v.payload),
                Some(999_999),
                "upsert on disk record should have written new value"
            );
        }
        OperationStatus::Pending => {
            let _ = store.try_complete_pending(&mut s, true);
            let mut out2: Option<BigVal> = None;
            let status2 = store.read(&mut s, &target_key, &BigVal::default(), &mut out2, ());
            assert_eq!(status2, OperationStatus::Ok);
            assert_eq!(
                out2.map(|v| v.payload),
                Some(999_999),
                "upsert on disk record: new value after pending read"
            );
        }
        other => panic!("unexpected read status: {other:?}"),
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 2. RMW on evicted (on-disk) record
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_rmw_on_disk_record() {
    let store = test_store();
    let mut s = store.new_session();

    // Fill then evict.
    fill_then_evict(&store, &mut s, RECORD_COUNT);

    // Find an evicted key.
    let target_key = find_evicted_key(&store, &mut s, 0..RECORD_COUNT)
        .expect("expected at least one evicted key");

    // Apply RMW to the evicted key — SimpleFunctions replaces the value.
    let new_value = BigVal::new(42_42_42);
    let mut rmw_out: Option<BigVal> = None;
    let status = store.rmw(&mut s, &target_key, &new_value, &mut rmw_out, ());
    assert_eq!(
        status,
        OperationStatus::Pending,
        "RMW on evicted key should return Pending, got {status:?}"
    );

    // Complete pending.
    let result = store.try_complete_pending(&mut s, true);
    assert!(result.is_done(), "all pending ops should be done");

    // Read back — the RMW should have replaced the value.
    let mut read_out: Option<BigVal> = None;
    let status = store.read(&mut s, &target_key, &BigVal::default(), &mut read_out, ());
    match status {
        OperationStatus::Ok => {
            assert_eq!(
                read_out.map(|v| v.payload),
                Some(42_42_42),
                "RMW on disk record: expected new payload"
            );
        }
        OperationStatus::Pending => {
            let _ = store.try_complete_pending(&mut s, true);
            let mut read_out2: Option<BigVal> = None;
            let status2 = store.read(&mut s, &target_key, &BigVal::default(), &mut read_out2, ());
            assert_eq!(status2, OperationStatus::Ok);
            assert_eq!(
                read_out2.map(|v| v.payload),
                Some(42_42_42),
                "RMW on disk record (after pending read): expected new payload"
            );
        }
        other => panic!("unexpected read status: {other:?}"),
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 3. Delete on evicted (on-disk) record
// ═══════════════════════════════════════════════════════════════════

#[test]
fn test_delete_on_disk_record() {
    let store = test_store();
    let mut s = store.new_session();

    // Fill then evict.
    fill_then_evict(&store, &mut s, RECORD_COUNT);

    // Find an evicted key.
    let target_key = find_evicted_key(&store, &mut s, 0..RECORD_COUNT)
        .expect("expected at least one evicted key");

    // Delete the evicted key.
    let status = store.delete(&mut s, &target_key, ());
    assert_eq!(
        status,
        OperationStatus::Pending,
        "delete on evicted key should return Pending, got {status:?}"
    );

    // Complete pending.
    let result = store.try_complete_pending(&mut s, true);
    assert!(result.is_done(), "all pending ops should be done");

    // Read back — the key should be gone (NotFound) or the tombstone
    // should be visible after completing pending reads.
    let mut out: Option<BigVal> = None;
    let status = store.read(&mut s, &target_key, &BigVal::default(), &mut out, ());
    match status {
        OperationStatus::NotFound => { /* correct */ }
        OperationStatus::Ok => {
            panic!(
                "deleted key {target_key} should be NotFound, got Ok({:?})",
                out.map(|v| v.payload)
            );
        }
        OperationStatus::Pending => {
            let _ = store.try_complete_pending(&mut s, true);
            let mut out2: Option<BigVal> = None;
            let status2 = store.read(&mut s, &target_key, &BigVal::default(), &mut out2, ());
            assert_eq!(
                status2,
                OperationStatus::NotFound,
                "deleted key should be NotFound after completion, got {status2:?} val={:?}",
                out2.map(|v| v.payload)
            );
        }
        other => panic!("unexpected read status: {other:?}"),
    }

    store.dispose_session(s);
}
