//! Integration tests: cross-module interactions.
//!
//! These tests verify that the Phase 1 foundation modules (address, hash,
//! hash_bucket, record, epoch, allocator) compose correctly. Individual
//! module correctness is covered by unit tests; here we test the *seams*.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use faster_core::address::LogicalAddress;
use faster_core::allocator::MallocFixedPageSize;
use faster_core::epoch::EpochTable;
use faster_core::hash::Hashable;
use faster_core::hash_bucket::{BUCKET_NUM_ENTRIES, HashBucket, HashBucketEntry};
use faster_core::record::{
    RecordInfo, RecordLayout, read_key, read_record_info, read_value, write_record,
};
use faster_core::status::OperationStatus;

// ===========================================================================
// 1. LogicalAddress → RecordInfo round-trip
// ===========================================================================

#[test]
fn address_to_record_info_round_trip() {
    // A LogicalAddress can be embedded in a RecordInfo's previous-address field
    // and recovered identically. This is the version-chain pointer pattern.
    let addrs = [
        LogicalAddress::ZERO,
        LogicalAddress::INVALID,
        LogicalAddress::MIN_VALID,
        common::addr(0, 100),
        common::addr(42, 1024),
        common::addr(8_388_607, 33_554_431), // MAX_PAGE, MAX_OFFSET
    ];
    for &addr in &addrs {
        for version in [0u16, 1, 42, 4095] {
            for (inv, tomb, fin) in [(false, false, false), (true, true, true)] {
                let info = RecordInfo::new(addr, version, inv, tomb, fin);
                assert_eq!(
                    info.previous_address(),
                    addr,
                    "previous_address mismatch for {addr:?} version={version}"
                );
                assert_eq!(info.checkpoint_version(), version);
                assert_eq!(info.is_invalid(), inv);
                assert_eq!(info.is_tombstone(), tomb);
                assert_eq!(info.is_final(), fin);
            }
        }
    }
}

#[test]
fn record_info_mutation_chain() {
    // Build a RecordInfo incrementally, simulating the lifecycle:
    // new → set version → mark invalid → set tombstone
    let base_addr = common::addr(5, 256);
    let info = RecordInfo::new(base_addr, 1, false, false, false);

    let info = info.with_checkpoint_version(42);
    assert_eq!(info.checkpoint_version(), 42);
    assert_eq!(info.previous_address(), base_addr);

    let info = info.with_invalid();
    assert!(info.is_invalid());
    assert!(!info.is_tombstone());

    let info = info.with_tombstone();
    assert!(info.is_tombstone());
    assert!(info.is_invalid());
    assert_eq!(info.checkpoint_version(), 42);
}

// ===========================================================================
// 2. Allocator → store a record → read it back
// ===========================================================================

/// A 128-byte item (large enough for a small record).
#[repr(C, align(8))]
struct RecordSlot {
    data: [u8; 128],
}

#[test]
fn allocator_store_and_read_record() {
    let alloc = MallocFixedPageSize::<RecordSlot>::new();

    let key: u64 = 0xDEAD_BEEF;
    let value: u64 = 0xCAFE_BABE;
    let layout = RecordLayout::for_kv(&key, &value);
    assert!(layout.total_size() <= 128, "record must fit in slot");

    let addr = alloc.allocate();
    assert!(addr.is_valid());

    // Write a record into the allocated slot.
    {
        let slot = alloc.get(addr);
        // SAFETY: `alloc.get(addr)` returns a valid pointer to a 128-byte slot we just allocated.
        let buf = unsafe { std::slice::from_raw_parts_mut(slot as *const _ as *mut u8, 128) };
        let info = RecordInfo::new(LogicalAddress::ZERO, 7, false, false, false);
        write_record(buf, &info, &key, &value, &layout);
    }

    // Read back from a fresh reference.
    {
        let slot = alloc.get(addr);
        // SAFETY: `alloc.get(addr)` returns a valid pointer to the 128-byte slot allocated above.
        let buf = unsafe { std::slice::from_raw_parts(slot as *const _ as *const u8, 128) };
        let info = read_record_info(buf);
        assert_eq!(info.checkpoint_version(), 7);
        assert_eq!(info.previous_address(), LogicalAddress::ZERO);

        let k: u64 = read_key(buf, &layout);
        let v: u64 = read_value(buf, &layout);
        assert_eq!(k, 0xDEAD_BEEF);
        assert_eq!(v, 0xCAFE_BABE);
    }
}

#[test]
fn allocator_multiple_records_independent() {
    let alloc = MallocFixedPageSize::<RecordSlot>::new();

    let pairs: Vec<(u64, u64)> = (0..100).map(|i| (i, i * 1000)).collect();
    let mut addrs = Vec::new();

    for &(k, v) in &pairs {
        let layout = RecordLayout::for_kv(&k, &v);
        let addr = alloc.allocate();
        addrs.push(addr);

        let slot = alloc.get(addr);
        // SAFETY: `alloc.get(addr)` returns a valid pointer to the freshly-allocated 128-byte slot.
        let buf = unsafe { std::slice::from_raw_parts_mut(slot as *const _ as *mut u8, 128) };
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        write_record(buf, &info, &k, &v, &layout);
    }

    // All addresses should be unique.
    let mut raw_addrs: Vec<u64> = addrs.iter().map(|a| a.raw()).collect();
    raw_addrs.sort();
    raw_addrs.dedup();
    assert_eq!(raw_addrs.len(), 100, "all addresses must be unique");

    // Verify each record reads back correctly.
    for (i, &addr) in addrs.iter().enumerate() {
        let (k, v) = pairs[i];
        let layout = RecordLayout::for_kv(&k, &v);
        let slot = alloc.get(addr);
        // SAFETY: `alloc.get(addr)` returns a valid pointer to the previously-allocated 128-byte slot.
        let buf = unsafe { std::slice::from_raw_parts(slot as *const _ as *const u8, 128) };
        assert_eq!(read_key::<u64>(buf, &layout), k);
        assert_eq!(read_value::<u64>(buf, &layout), v);
    }
}

// ===========================================================================
// 3. Hash key → extract tag → bucket entry → find in bucket
// ===========================================================================

#[test]
fn hash_to_bucket_entry_find_round_trip() {
    let bucket = HashBucket::new();

    // Insert entries for keys 0..7 using their real hash tags.
    let keys: Vec<u64> = (100..107).collect();
    for (slot, &key) in keys.iter().enumerate() {
        let kh = key.hash();
        let tag = kh.tag();
        let fake_addr = common::addr(0, (slot as u32) + 10);
        let entry = HashBucketEntry::new(tag, fake_addr, false);
        bucket.entry(slot).store(entry, Ordering::Release);
    }

    // Each key should be findable by its tag.
    for (expected_slot, &key) in keys.iter().enumerate() {
        let kh = key.hash();
        let tag = kh.tag();
        let result = bucket.find_entry(tag);
        assert!(result.is_some(), "key {key} tag 0x{tag:04x} not found");
        let (found_slot, found_entry) = result.unwrap();
        assert_eq!(found_slot, expected_slot);
        assert_eq!(
            found_entry.address(),
            common::addr(0, (expected_slot as u32) + 10)
        );
    }
}

#[test]
fn hash_tag_determines_bucket_find_miss() {
    let bucket = HashBucket::new();

    // Insert one entry with a known tag.
    let key: u64 = 42;
    let kh = key.hash();
    let tag = kh.tag();
    let addr = common::addr(1, 100);
    bucket
        .entry(0)
        .store(HashBucketEntry::new(tag, addr, false), Ordering::Release);

    // A different key whose tag differs should NOT be found.
    let other_key: u64 = 12345;
    let other_tag = other_key.hash().tag();
    if other_tag != tag {
        assert!(bucket.find_entry(other_tag).is_none());
    }

    // The original tag should be found.
    let (slot, entry) = bucket.find_entry(tag).unwrap();
    assert_eq!(slot, 0);
    assert_eq!(entry.address(), addr);
}

#[test]
fn tentative_entries_invisible_to_find() {
    let bucket = HashBucket::new();
    let tag = 0x1234u16;
    let addr = common::addr(2, 50);

    // A tentative entry should be skipped by find_entry.
    let tentative = HashBucketEntry::new(tag, addr, true);
    bucket.entry(0).store(tentative, Ordering::Release);
    assert!(
        bucket.find_entry(tag).is_none(),
        "tentative should be invisible"
    );

    // Committing (clearing tentative) makes it visible.
    let committed = tentative.without_tentative();
    bucket.entry(0).store(committed, Ordering::Release);
    assert!(bucket.find_entry(tag).is_some());
}

// ===========================================================================
// 4. Epoch + allocator: protect during allocation
// ===========================================================================

#[test]
fn epoch_protects_during_allocation() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");
    let alloc = MallocFixedPageSize::<RecordSlot>::new();

    // Allocate under epoch protection.
    let addr = {
        let _guard = thread.protect();
        let a = alloc.allocate();
        assert!(a.is_valid());
        a
    };
    // After dropping the guard, the address is still valid to access
    // (we haven't freed it).
    let slot = alloc.get(addr);
    // SAFETY: `alloc.get(addr)` returns a valid pointer to the 128-byte slot allocated earlier.
    let buf = unsafe { std::slice::from_raw_parts(slot as *const _ as *const u8, 128) };
    assert_eq!(&buf[..8], &[0u8; 8]);
}

#[test]
fn epoch_drain_fires_after_unprotect() {
    use std::sync::atomic::AtomicBool;

    let table = Arc::new(EpochTable::new());
    let fired = Arc::new(AtomicBool::new(false));
    let fired_clone = Arc::clone(&fired);

    let thread = table.register().expect("register");

    // Enter protection, bump epoch with callback.
    let guard = thread.protect();
    table.bump_current_epoch(move || {
        fired_clone.store(true, Ordering::SeqCst);
    });
    // Callback should NOT have fired yet (we're still protected at the old epoch).
    assert!(!fired.load(Ordering::SeqCst));

    // Drop guard → unprotect → try_drain should fire the callback.
    drop(guard);
    assert!(
        fired.load(Ordering::SeqCst),
        "drain callback should fire after last guard drops"
    );
}

// ===========================================================================
// 5. HashBucket::try_insert — full bucket behavior
// ===========================================================================

#[test]
fn try_insert_fills_bucket_then_fails() {
    let bucket = HashBucket::new();
    let base_addr = common::addr(0, 100);

    // Insert 7 entries (all slots).
    for i in 0..BUCKET_NUM_ENTRIES {
        let tag = (i as u16) + 1;
        let addr = LogicalAddress::from_raw(base_addr.raw() + i as u64);
        let result = bucket.try_insert(tag, addr);
        assert!(result.is_ok(), "insert {i} should succeed");
        assert_eq!(result.unwrap(), i);
    }

    // 8th insert should fail — bucket is full.
    let result = bucket.try_insert(0x3FFF, common::addr(99, 99));
    assert!(result.is_err(), "bucket should be full");

    // Verify all 7 entries are retrievable.
    for i in 0..BUCKET_NUM_ENTRIES {
        let tag = (i as u16) + 1;
        let found = bucket.find_entry(tag);
        assert!(found.is_some(), "tag {tag} should be findable");
    }
}

// ===========================================================================
// 6. Record serialization: variable-length round-trip
// ===========================================================================

#[test]
fn variable_length_record_round_trip() {
    let key: Vec<u8> = b"hello-world".to_vec();
    let value: Vec<u8> = b"some-value-data-longer-than-key".to_vec();
    let layout = RecordLayout::for_kv(&key, &value);
    let prev = common::addr(3, 999);
    let info = RecordInfo::new(prev, 100, false, true, false);

    let mut buf = vec![0u8; layout.total_size()];
    write_record(&mut buf, &info, &key, &value, &layout);

    let ri = read_record_info(&buf);
    assert_eq!(ri.previous_address(), prev);
    assert_eq!(ri.checkpoint_version(), 100);
    assert!(ri.is_tombstone());
    assert!(!ri.is_invalid());

    let k: Vec<u8> = read_key(&buf, &layout);
    let v: Vec<u8> = read_value(&buf, &layout);
    assert_eq!(k, b"hello-world");
    assert_eq!(v, b"some-value-data-longer-than-key");
}

// ===========================================================================
// 7. OperationStatus + error type interaction
// ===========================================================================

#[test]
fn operation_status_in_result_context() {
    use faster_core::error::FasterError;

    // Simulate the pattern: operation returns Result<OperationStatus, FasterError>
    fn simulated_read(key: u64) -> Result<OperationStatus, FasterError> {
        if key == 0 {
            return Err(FasterError::InvalidOperation("zero key".into()));
        }
        if key == 999 {
            return Ok(OperationStatus::NotFound);
        }
        Ok(OperationStatus::Ok)
    }

    assert!(simulated_read(0).is_err());
    assert_eq!(simulated_read(999).unwrap(), OperationStatus::NotFound);
    assert!(simulated_read(42).unwrap().is_success());
}

// ===========================================================================
// 8. Full pipeline: hash → bucket → allocator → record
// ===========================================================================

#[test]
fn full_pipeline_hash_bucket_alloc_record() {
    let alloc = MallocFixedPageSize::<RecordSlot>::new();
    let bucket = HashBucket::new();

    let key: u64 = 0xBEEF_CAFE;
    let value: u64 = 0x1234_5678_9ABC_DEF0;
    let kh = key.hash();
    let tag = kh.tag();

    // 1. Allocate a slot for the record.
    let record_addr = alloc.allocate();

    // 2. Write the record into the allocator slot.
    let layout = RecordLayout::for_kv(&key, &value);
    {
        let slot = alloc.get(record_addr);
        // SAFETY: `alloc.get(record_addr)` returns a valid pointer to a 128-byte slot we allocated.
        let buf = unsafe { std::slice::from_raw_parts_mut(slot as *const _ as *mut u8, 128) };
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        write_record(buf, &info, &key, &value, &layout);
    }

    // 3. Insert the (tag, record_addr) into the hash bucket.
    let insert_result = bucket.try_insert(tag, record_addr);
    assert!(insert_result.is_ok());

    // 4. Look up via the tag → get back the address → read the record.
    let (_, found_entry) = bucket.find_entry(tag).expect("entry should be found");
    assert_eq!(found_entry.address(), record_addr);

    let slot = alloc.get(found_entry.address());
    // SAFETY: `alloc.get()` returns a valid pointer to the 128-byte slot written above.
    let buf = unsafe { std::slice::from_raw_parts(slot as *const _ as *const u8, 128) };
    let k: u64 = read_key(buf, &layout);
    let v: u64 = read_value(buf, &layout);
    assert_eq!(k, 0xBEEF_CAFE);
    assert_eq!(v, 0x1234_5678_9ABC_DEF0);
}

// ===========================================================================
// 9. Epoch table + hash bucket: protected insert pattern
// ===========================================================================

#[test]
fn epoch_protected_hash_insert() {
    let table = Arc::new(EpochTable::new());
    let thread = table.register().expect("register");
    let bucket = HashBucket::new();

    let key: u64 = 42;
    let tag = key.hash().tag();
    let addr = common::addr(1, 50);

    // Insert under epoch protection — the real FASTER pattern.
    {
        let _guard = thread.protect();
        let _ = bucket.try_insert(tag, addr);
    }

    // Verify findable after protection ends.
    let result = bucket.find_entry(tag);
    assert!(result.is_some());
    assert_eq!(result.unwrap().1.address(), addr);
}

// ===========================================================================
// 10. Allocator free + reuse: addresses remain valid after free
// ===========================================================================

#[test]
fn allocator_free_and_reuse() {
    let alloc = MallocFixedPageSize::<RecordSlot>::new();

    let addr1 = alloc.allocate();
    alloc.free(addr1);

    let addr2 = alloc.allocate();
    // The reused address should come from the free list.
    assert!(addr2.is_valid());

    // Both addresses are valid pointers (even after free, the page still exists).
    let _ = alloc.get(addr1);
    let _ = alloc.get(addr2);
}
