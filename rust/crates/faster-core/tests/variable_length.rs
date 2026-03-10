//! Variable-length value integration tests for FasterKv.
//!
//! These tests exercise the full store stack with `Vec<u8>` values
//! (variable-length, length-prefixed serialization) to catch bugs that
//! only manifest when record sizes vary at runtime — exactly the class
//! of bug that fixed-size-only test suites miss.
//!
//! # Background
//!
//! A critical bug in `layout_for_fixed` used `mem::size_of::<Vec<u8>>()`
//! (24 bytes — the Rust struct size) instead of the actual serialized size,
//! causing alignment panics. None of the 1,280+ existing tests caught it
//! because they all used fixed-size types (u64, i64, etc.).
//!
//! This file exists to close that gap permanently.

use std::sync::Arc;
use std::thread;

use faster_core::InMemoryDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{
    FasterKv, FasterKvConfig, Functions, ReadInfo, RmwInPlaceResult, RmwInfo, UpsertInfo,
};

// ═══════════════════════════════════════════════════════════════════
// VarLenFunctions — Functions impl for u64 keys + Vec<u8> values
// ═══════════════════════════════════════════════════════════════════

/// Functions implementation for variable-length values.
///
/// - **Key** = `u64`
/// - **Value** = `Vec<u8>` (variable-length, length-prefixed)
/// - **Input** = `Vec<u8>` (data to write)
/// - **Output** = `Option<Vec<u8>>` (read result)
/// - **Context** = `()`
///
/// Upsert replaces the value wholesale. RMW appends input to existing
/// value (demonstrating in-place growth that triggers `NeedsNewRecord`).
struct VarLenFunctions;

impl Functions for VarLenFunctions {
    type Key = u64;
    type Value = Vec<u8>;
    type Input = Vec<u8>;
    type Output = Option<Vec<u8>>;
    type Context = ();

    fn read(
        &self,
        _key: &u64,
        value: &Vec<u8>,
        _input: &Vec<u8>,
        output: &mut Option<Vec<u8>>,
        _info: &ReadInfo,
    ) {
        *output = Some(value.clone());
    }

    fn upsert(
        &self,
        _key: &u64,
        value: &mut Vec<u8>,
        input: &Vec<u8>,
        _old_value: Option<&Vec<u8>>,
        _output: &mut Option<Vec<u8>>,
        _info: &UpsertInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_initial(
        &self,
        _key: &u64,
        input: &Vec<u8>,
        value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        *value = input.clone();
    }

    fn rmw_in_place(
        &self,
        _key: &u64,
        _input: &Vec<u8>,
        _value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        // Append semantics: the value will grow, so we always need a new
        // record. Do NOT modify value here — rmw_copy_update will be
        // called next with the unmodified old value.
        RmwInPlaceResult::NeedsNewRecord
    }

    fn rmw_copy_update(
        &self,
        _key: &u64,
        input: &Vec<u8>,
        old_value: &Vec<u8>,
        new_value: &mut Vec<u8>,
        output: &mut Option<Vec<u8>>,
        _info: &RmwInfo,
    ) {
        let mut merged = old_value.clone();
        merged.extend_from_slice(input);
        *new_value = merged.clone();
        *output = Some(merged);
    }
}

// ═══════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════

/// Create a small in-memory store with variable-length values.
fn varlen_store() -> FasterKv<VarLenFunctions> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 10, // 1 024 buckets
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, VarLenFunctions, InMemoryDevice::new())
}

/// Create a larger store for high-volume tests.
fn large_varlen_store() -> FasterKv<VarLenFunctions> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 16, // 64K buckets
        buffer_size_pages: 64,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, VarLenFunctions, InMemoryDevice::new())
}

/// Create a store with tiny buffer to force page sealing.
fn tiny_buffer_varlen_store() -> FasterKv<VarLenFunctions> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 14,
        buffer_size_pages: 4,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 2,
        },
        auto_compact: false,
        lossy: false,
    };
    FasterKv::new(config, VarLenFunctions, InMemoryDevice::new())
}

/// Generate a deterministic byte vector of given length.
fn make_value(key: u64, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    for i in 0..len {
        // Mix key and position for a deterministic but varied pattern.
        v.push(((key.wrapping_mul(31).wrapping_add(i as u64)) & 0xFF) as u8);
    }
    v
}

/// Verify a read returns the expected value.
fn assert_read(
    store: &FasterKv<VarLenFunctions>,
    session: &mut faster_core::FasterSession<VarLenFunctions>,
    key: u64,
    expected: &[u8],
) {
    let mut out: Option<Vec<u8>> = None;
    let status = store.read(session, &key, &vec![], &mut out, ());
    assert_eq!(
        status,
        OperationStatus::Ok,
        "read key {key} returned {status:?}"
    );
    assert_eq!(
        out.as_deref(),
        Some(expected),
        "value mismatch at key {key}: expected len={}, got len={}",
        expected.len(),
        out.as_ref().map_or(0, |v| v.len()),
    );
}

// ═══════════════════════════════════════════════════════════════════
// 1. Basic CRUD with Vec<u8> values
// ═══════════════════════════════════════════════════════════════════

#[test]
fn varlen_upsert_and_read() {
    let store = varlen_store();
    let mut s = store.new_session();

    let value = b"hello, variable-length world!".to_vec();
    let status = store.upsert(&mut s, &1u64, &value, ());
    assert!(status.is_success(), "upsert failed: {status:?}");

    assert_read(&store, &mut s, 1, &value);
    store.dispose_session(s);
}

#[test]
fn varlen_upsert_overwrite() {
    let store = varlen_store();
    let mut s = store.new_session();

    let v1 = b"first-version".to_vec();
    let v2 = b"second-version-which-is-longer-than-first".to_vec();
    let v3 = b"v3".to_vec(); // shorter than both

    let _ = store.upsert(&mut s, &1u64, &v1, ());
    assert_read(&store, &mut s, 1, &v1);

    let _ = store.upsert(&mut s, &1u64, &v2, ());
    assert_read(&store, &mut s, 1, &v2);

    let _ = store.upsert(&mut s, &1u64, &v3, ());
    assert_read(&store, &mut s, 1, &v3);

    store.dispose_session(s);
}

#[test]
fn varlen_delete() {
    let store = varlen_store();
    let mut s = store.new_session();

    let value = b"to-be-deleted".to_vec();
    let _ = store.upsert(&mut s, &42u64, &value, ());
    assert_read(&store, &mut s, 42, &value);

    let status = store.delete(&mut s, &42u64, ());
    assert_eq!(status, OperationStatus::Deleted);

    let mut out: Option<Vec<u8>> = None;
    let status = store.read(&mut s, &42u64, &vec![], &mut out, ());
    assert_eq!(status, OperationStatus::NotFound);
    assert_eq!(out, None);

    store.dispose_session(s);
}

#[test]
fn varlen_rmw_initial_and_append() {
    let store = varlen_store();
    let mut s = store.new_session();

    // RMW on a non-existent key → rmw_initial sets value to input.
    let initial = b"base-data".to_vec();
    let mut out: Option<Vec<u8>> = None;
    let status = store.rmw(&mut s, &1u64, &initial, &mut out, ());
    assert!(status.is_success(), "rmw initial failed: {status:?}");

    assert_read(&store, &mut s, 1, &initial);

    // RMW again → rmw_copy_update appends (since in_place returns NeedsNewRecord).
    let append = b"-appended".to_vec();
    let mut out2: Option<Vec<u8>> = None;
    let status = store.rmw(&mut s, &1u64, &append, &mut out2, ());
    assert!(status.is_success(), "rmw append failed: {status:?}");

    let expected: Vec<u8> = b"base-data-appended".to_vec();
    assert_read(&store, &mut s, 1, &expected);

    store.dispose_session(s);
}

#[test]
fn varlen_empty_value() {
    let store = varlen_store();
    let mut s = store.new_session();

    // Empty Vec<u8> — serialized as 4-byte length prefix (0) + no data.
    let empty = vec![];
    let _ = store.upsert(&mut s, &1u64, &empty, ());
    assert_read(&store, &mut s, 1, &empty);

    store.dispose_session(s);
}

#[test]
fn varlen_single_byte_value() {
    let store = varlen_store();
    let mut s = store.new_session();

    let one_byte = vec![0xAB];
    let _ = store.upsert(&mut s, &1u64, &one_byte, ());
    assert_read(&store, &mut s, 1, &one_byte);

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 2. Mixed-size records in the same store
// ═══════════════════════════════════════════════════════════════════

#[test]
fn mixed_size_records() {
    let store = varlen_store();
    let mut s = store.new_session();

    let small = vec![0x01; 8]; // 8 bytes
    let medium = vec![0x02; 1024]; // 1 KB
    let large = vec![0x03; 4096]; // 4 KB

    let _ = store.upsert(&mut s, &1u64, &small, ());
    let _ = store.upsert(&mut s, &2u64, &medium, ());
    let _ = store.upsert(&mut s, &3u64, &large, ());

    // Verify all three are correctly stored and retrievable.
    assert_read(&store, &mut s, 1, &small);
    assert_read(&store, &mut s, 2, &medium);
    assert_read(&store, &mut s, 3, &large);

    store.dispose_session(s);
}

#[test]
fn interleaved_sizes() {
    let store = varlen_store();
    let mut s = store.new_session();

    // Write records of alternating sizes to stress layout computation.
    let sizes = [8, 1024, 3, 4096, 1, 512, 7, 2048, 15, 256];
    for (i, &size) in sizes.iter().enumerate() {
        let key = i as u64;
        let value = make_value(key, size);
        let _ = store.upsert(&mut s, &key, &value, ());
    }

    // Read them all back.
    for (i, &size) in sizes.iter().enumerate() {
        let key = i as u64;
        let expected = make_value(key, size);
        assert_read(&store, &mut s, key, &expected);
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 3. Hash chain traversal with variable-length records
// ═══════════════════════════════════════════════════════════════════

#[test]
fn version_chain_variable_length() {
    let store = varlen_store();
    let mut s = store.new_session();

    // Write multiple versions of the same key with different sizes.
    // Each upsert creates a new record, forming a version chain.
    let key = 42u64;
    for version in 0u64..20 {
        // Each version has a different size (10 + version*5 bytes).
        let size = 10 + (version as usize) * 5;
        let value = make_value(version, size);
        let _ = store.upsert(&mut s, &key, &value, ());
    }

    // Read should return the latest version (version 19).
    let expected = make_value(19, 10 + 19 * 5);
    assert_read(&store, &mut s, key, &expected);

    store.dispose_session(s);
}

#[test]
fn many_keys_same_bucket_variable_length() {
    // Use a very small hash table to force collisions → long chains.
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 4, // 16 buckets — extreme collisions
        buffer_size_pages: 16,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, VarLenFunctions, InMemoryDevice::new());
    let mut s = store.new_session();

    // Write 200 keys, each with a different-sized value.
    for i in 0u64..200 {
        let size = 10 + (i as usize % 50) * 3;
        let value = make_value(i, size);
        let _ = store.upsert(&mut s, &i, &value, ());
    }

    // Read all back — chain traversal must handle variable-length records.
    for i in 0u64..200 {
        let size = 10 + (i as usize % 50) * 3;
        let expected = make_value(i, size);
        assert_read(&store, &mut s, i, &expected);
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 4. Alignment stress test
// ═══════════════════════════════════════════════════════════════════

#[test]
fn alignment_awkward_sizes() {
    let store = varlen_store();
    let mut s = store.new_session();

    // Deliberately awkward sizes that are NOT multiples of 8.
    // These stress the alignment padding in record layout.
    let awkward_sizes: &[usize] = &[
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 31, 33, 63, 65, 127, 128,
        129, 255, 256, 257, 511, 512, 513, 1023, 1024, 1025,
    ];

    for (i, &size) in awkward_sizes.iter().enumerate() {
        let key = i as u64;
        let value = make_value(key, size);
        let status = store.upsert(&mut s, &key, &value, ());
        assert!(
            status.is_success(),
            "upsert failed for size {size}: {status:?}"
        );
    }

    // Verify all stored correctly (no alignment panics).
    for (i, &size) in awkward_sizes.iter().enumerate() {
        let key = i as u64;
        let expected = make_value(key, size);
        assert_read(&store, &mut s, key, &expected);
    }

    store.dispose_session(s);
}

#[test]
fn alignment_power_of_two_minus_one() {
    let store = varlen_store();
    let mut s = store.new_session();

    // Sizes that are 2^N - 1 (e.g., 7, 15, 31, ...) — maximally unaligned.
    for n in 1u32..=11 {
        let size = (1usize << n) - 1;
        let key = n as u64;
        let value = make_value(key, size);
        let _ = store.upsert(&mut s, &key, &value, ());
    }

    for n in 1u32..=11 {
        let size = (1usize << n) - 1;
        let key = n as u64;
        let expected = make_value(key, size);
        assert_read(&store, &mut s, key, &expected);
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 5. High-volume variable-length
// ═══════════════════════════════════════════════════════════════════

#[test]
fn high_volume_variable_length() {
    let store = large_varlen_store();
    let mut s = store.new_session();

    const N: u64 = 10_000;

    // Write 10K records with varied sizes (16–528 bytes).
    for i in 0..N {
        let size = 16 + (i as usize % 512);
        let value = make_value(i, size);
        let status = store.upsert(&mut s, &i, &value, ());
        assert!(
            status.is_success(),
            "upsert key {i} (size {size}) failed: {status:?}"
        );
    }

    // Read them all back.
    for i in 0..N {
        let size = 16 + (i as usize % 512);
        let expected = make_value(i, size);
        assert_read(&store, &mut s, i, &expected);
    }

    store.dispose_session(s);
}

#[test]
fn high_volume_with_updates() {
    let store = large_varlen_store();
    let mut s = store.new_session();

    const N: u64 = 5_000;

    // Phase 1: insert.
    for i in 0..N {
        let size = 32 + (i as usize % 256);
        let value = make_value(i, size);
        let _ = store.upsert(&mut s, &i, &value, ());
    }

    // Phase 2: update every other key with a different size.
    for i in (0..N).step_by(2) {
        let new_size = 64 + (i as usize % 128);
        let new_value = make_value(i + N, new_size); // Different pattern
        let _ = store.upsert(&mut s, &i, &new_value, ());
    }

    // Phase 3: verify.
    for i in 0..N {
        if i % 2 == 0 {
            // Updated key.
            let size = 64 + (i as usize % 128);
            let expected = make_value(i + N, size);
            assert_read(&store, &mut s, i, &expected);
        } else {
            // Original key.
            let size = 32 + (i as usize % 256);
            let expected = make_value(i, size);
            assert_read(&store, &mut s, i, &expected);
        }
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 6. Delete and re-insert with different sizes
// ═══════════════════════════════════════════════════════════════════

#[test]
fn delete_reinsert_different_size() {
    let store = varlen_store();
    let mut s = store.new_session();

    let key = 1u64;

    // Insert a large value.
    let large = vec![0xAA; 2048];
    let _ = store.upsert(&mut s, &key, &large, ());
    assert_read(&store, &mut s, key, &large);

    // Delete.
    let _ = store.delete(&mut s, &key, ());
    let mut out: Option<Vec<u8>> = None;
    let status = store.read(&mut s, &key, &vec![], &mut out, ());
    assert_eq!(status, OperationStatus::NotFound);

    // Re-insert with a small value.
    let small = vec![0xBB; 8];
    let _ = store.upsert(&mut s, &key, &small, ());
    assert_read(&store, &mut s, key, &small);

    store.dispose_session(s);
}

#[test]
fn bulk_delete_variable_length() {
    let store = varlen_store();
    let mut s = store.new_session();

    // Insert 100 records with varied sizes.
    for i in 0u64..100 {
        let size = 16 + (i as usize) * 4;
        let value = make_value(i, size);
        let _ = store.upsert(&mut s, &i, &value, ());
    }

    // Delete the first 50.
    for i in 0u64..50 {
        let _ = store.delete(&mut s, &i, ());
    }

    // Verify: first 50 gone, last 50 intact.
    for i in 0u64..50 {
        let mut out: Option<Vec<u8>> = None;
        let status = store.read(&mut s, &i, &vec![], &mut out, ());
        assert_eq!(
            status,
            OperationStatus::NotFound,
            "key {i} should be deleted"
        );
    }
    for i in 50u64..100 {
        let size = 16 + (i as usize) * 4;
        let expected = make_value(i, size);
        assert_read(&store, &mut s, i, &expected);
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 7. Page boundary crossing
// ═══════════════════════════════════════════════════════════════════

#[test]
fn page_boundary_with_variable_length() {
    // Use a tiny buffer so records cross page boundaries quickly.
    let store = tiny_buffer_varlen_store();
    let mut s = store.new_session();

    // Write enough data to span multiple pages.
    const N: u64 = 500;
    for i in 0..N {
        let size = 100 + (i as usize % 200); // 100–299 bytes per record
        let value = make_value(i, size);
        let _ = store.upsert(&mut s, &i, &value, ());
    }

    // Recent keys should still be readable.
    let recent = N - 1;
    let size = 100 + (recent as usize % 200);
    let expected = make_value(recent, size);

    let mut out: Option<Vec<u8>> = None;
    let status = store.read(&mut s, &recent, &vec![], &mut out, ());
    assert!(
        status == OperationStatus::Ok || status == OperationStatus::Pending,
        "read after page seal returned {status:?}"
    );
    if status == OperationStatus::Ok {
        assert_eq!(out.as_deref(), Some(expected.as_slice()));
    }

    store.dispose_session(s);
}

#[test]
fn maintenance_with_variable_length() {
    let store = tiny_buffer_varlen_store();
    let mut s = store.new_session();

    // Write records to fill pages and trigger maintenance.
    for i in 0u64..1_000 {
        let size = 50 + (i as usize % 300);
        let value = make_value(i, size);
        let _ = store.upsert(&mut s, &i, &value, ());
    }

    let head_before = store.head_address();
    store.maintenance();
    store.maintenance();
    let head_after = store.head_address();

    // Head must not regress.
    assert!(
        head_after.raw() >= head_before.raw(),
        "head must not regress: before={}, after={}",
        head_before.raw(),
        head_after.raw(),
    );

    // Recent keys should still be accessible.
    let recent = 999u64;
    let size = 50 + (recent as usize % 300);
    let expected = make_value(recent, size);
    let mut out: Option<Vec<u8>> = None;
    let status = store.read(&mut s, &recent, &vec![], &mut out, ());
    assert!(
        status == OperationStatus::Ok || status == OperationStatus::Pending,
        "recent key {recent}: {status:?}"
    );
    if status == OperationStatus::Ok {
        assert_eq!(out.as_deref(), Some(expected.as_slice()));
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 8. Concurrent variable-length operations
// ═══════════════════════════════════════════════════════════════════

#[test]
fn concurrent_varlen_upsert_disjoint_keys() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 16,
        buffer_size_pages: 32,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::new(
        config,
        VarLenFunctions,
        InMemoryDevice::new(),
    ));

    const THREADS: u64 = 8;
    const KEYS_PER_THREAD: u64 = 500;

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut s = store.new_session();
                let base = t * KEYS_PER_THREAD;

                // Each thread writes disjoint keys with varied sizes.
                for i in 0..KEYS_PER_THREAD {
                    let key = base + i;
                    let size = 16 + (key as usize % 256);
                    let value = make_value(key, size);
                    let _ = store.upsert(&mut s, &key, &value, ());
                }

                // Read back own keys.
                for i in 0..KEYS_PER_THREAD {
                    let key = base + i;
                    let size = 16 + (key as usize % 256);
                    let expected = make_value(key, size);
                    let mut out: Option<Vec<u8>> = None;
                    let status = store.read(&mut s, &key, &vec![], &mut out, ());
                    assert_eq!(status, OperationStatus::Ok, "t{t}: read key {key} failed");
                    assert_eq!(
                        out.as_deref(),
                        Some(expected.as_slice()),
                        "t{t}: key {key} mismatch"
                    );
                }

                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker thread panicked");
    }
}

#[test]
fn concurrent_varlen_mixed_workload() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 14,
        buffer_size_pages: 16,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::new(
        config,
        VarLenFunctions,
        InMemoryDevice::new(),
    ));

    // Phase 1: populate shared keys.
    {
        let mut s = store.new_session();
        for i in 0u64..200 {
            let value = make_value(i, 64 + (i as usize % 64));
            let _ = store.upsert(&mut s, &i, &value, ());
        }
        store.dispose_session(s);
    }

    const THREADS: u64 = 4;
    const OPS: u64 = 200;

    // Phase 2: concurrent reads, upserts, deletes.
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut s = store.new_session();
                for i in 0..OPS {
                    let key = i;
                    match (t + i) % 3 {
                        0 => {
                            let mut out: Option<Vec<u8>> = None;
                            let _ = store.read(&mut s, &key, &vec![], &mut out, ());
                        }
                        1 => {
                            let size = 32 + (t as usize * 16);
                            let value = make_value(key + t * 1000, size);
                            let _ = store.upsert(&mut s, &key, &value, ());
                        }
                        _ => {
                            let _ = store.delete(&mut s, &key, ());
                        }
                    }
                }
                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("worker panicked");
    }

    // Verify no panics — reads may return Ok, NotFound, or Pending.
    let mut s = store.new_session();
    for i in 0u64..OPS {
        let mut out: Option<Vec<u8>> = None;
        let status = store.read(&mut s, &i, &vec![], &mut out, ());
        assert!(
            status == OperationStatus::Ok || status == OperationStatus::NotFound,
            "key {i}: unexpected status {status:?}",
        );
    }
    store.dispose_session(s);
}

#[test]
fn concurrent_maintenance_with_varlen() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 14,
        buffer_size_pages: 8,
        mutable_fraction: 0.5,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 8,
            eviction_batch_size: 2,
        },
        auto_compact: false,
        lossy: false,
    };
    let store = Arc::new(FasterKv::new(
        config,
        VarLenFunctions,
        InMemoryDevice::new(),
    ));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Writer threads.
    let workers: Vec<_> = (0..4u64)
        .map(|t| {
            let store = Arc::clone(&store);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut s = store.new_session();
                let mut i = 0u64;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let key = t * 100_000 + i;
                    let size = 32 + (i as usize % 128);
                    let value = make_value(key, size);
                    let _ = store.upsert(&mut s, &key, &value, ());
                    i += 1;
                    if i > 2_000 {
                        break;
                    }
                }
                store.dispose_session(s);
                i
            })
        })
        .collect();

    // Maintenance thread.
    let store_m = Arc::clone(&store);
    let done_m = Arc::clone(&done);
    let maint = thread::spawn(move || {
        let mut cycles = 0u32;
        while !done_m.load(std::sync::atomic::Ordering::Relaxed) && cycles < 50 {
            store_m.maintenance();
            cycles += 1;
            thread::yield_now();
        }
        cycles
    });

    let mut total_ops = 0u64;
    for w in workers {
        total_ops += w.join().expect("worker panicked");
    }

    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let cycles = maint.join().expect("maintenance panicked");

    assert!(total_ops > 0, "workers should have completed some ops");
    assert!(cycles > 0, "maintenance should have run at least once");
}

// ═══════════════════════════════════════════════════════════════════
// 9. Session lifecycle with variable-length values
// ═══════════════════════════════════════════════════════════════════

#[test]
fn session_reuse_with_varlen() {
    let store = varlen_store();

    // Create and dispose sessions, writing variable-length data each time.
    for round in 0u64..10 {
        let mut s = store.new_session();
        let value = make_value(round, 50 + (round as usize) * 10);
        let _ = store.upsert(&mut s, &round, &value, ());
        assert_read(&store, &mut s, round, &value);
        store.dispose_session(s);
    }

    // All data persists.
    let mut s = store.new_session();
    for round in 0u64..10 {
        let expected = make_value(round, 50 + (round as usize) * 10);
        assert_read(&store, &mut s, round, &expected);
    }
    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 10. Large individual values
// ═══════════════════════════════════════════════════════════════════

#[test]
fn large_value_8kb() {
    let store = large_varlen_store();
    let mut s = store.new_session();

    let large = make_value(1, 8192);
    let _ = store.upsert(&mut s, &1u64, &large, ());
    assert_read(&store, &mut s, 1, &large);

    store.dispose_session(s);
}

#[test]
fn large_value_32kb() {
    let store = large_varlen_store();
    let mut s = store.new_session();

    let large = make_value(1, 32768);
    let _ = store.upsert(&mut s, &1u64, &large, ());
    assert_read(&store, &mut s, 1, &large);

    store.dispose_session(s);
}

#[test]
fn multiple_large_values() {
    let store = large_varlen_store();
    let mut s = store.new_session();

    // Write 50 records, each 4 KB.
    for i in 0u64..50 {
        let value = make_value(i, 4096);
        let _ = store.upsert(&mut s, &i, &value, ());
    }

    for i in 0u64..50 {
        let expected = make_value(i, 4096);
        assert_read(&store, &mut s, i, &expected);
    }

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 11. RMW growth chain
// ═══════════════════════════════════════════════════════════════════

#[test]
fn rmw_growing_value() {
    let store = varlen_store();
    let mut s = store.new_session();

    let key = 1u64;

    // Each RMW appends 10 bytes → value grows from 10 to 100 bytes.
    for iteration in 0u64..10 {
        let chunk = make_value(iteration, 10);
        let mut out: Option<Vec<u8>> = None;
        let status = store.rmw(&mut s, &key, &chunk, &mut out, ());
        assert!(
            status.is_success(),
            "rmw iteration {iteration} failed: {status:?}"
        );
    }

    // Final value should be the concatenation of all 10 chunks.
    let mut expected = Vec::new();
    for iteration in 0u64..10 {
        expected.extend_from_slice(&make_value(iteration, 10));
    }
    assert_read(&store, &mut s, key, &expected);

    store.dispose_session(s);
}

// ═══════════════════════════════════════════════════════════════════
// 12. Small hash table + variable-length = correctness under collisions
// ═══════════════════════════════════════════════════════════════════

#[test]
fn tiny_hash_table_varlen_stress() {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 3, // 8 buckets — extreme collisions
        buffer_size_pages: 16,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
        lossy: false,
    };
    let store = FasterKv::new(config, VarLenFunctions, InMemoryDevice::new());
    let mut s = store.new_session();

    for i in 0u64..100 {
        let size = 8 + (i as usize % 100);
        let value = make_value(i, size);
        let _ = store.upsert(&mut s, &i, &value, ());
    }

    for i in 0u64..100 {
        let size = 8 + (i as usize % 100);
        let expected = make_value(i, size);
        assert_read(&store, &mut s, i, &expected);
    }

    store.dispose_session(s);
}
