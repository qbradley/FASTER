//! Shared test helpers for integration tests.

use faster_core::address::{LogicalAddress, Offset, Page};
use faster_core::hash::Hashable;
use faster_core::hash_bucket::{HashBucket, HashBucketEntry};
use std::sync::atomic::Ordering;

/// Create a valid `LogicalAddress` from simple numeric inputs.
/// Panics if page or offset exceed their limits.
pub fn addr(page: u32, offset: u32) -> LogicalAddress {
    LogicalAddress::new(Page(page), Offset(offset))
}

/// Fill all 7 bucket entries with distinct tags, using sequential addresses.
pub fn fill_bucket(bucket: &HashBucket, base_tag: u16, base_addr: LogicalAddress) {
    for i in 0..7 {
        let tag = base_tag.wrapping_add(i as u16) & 0x3FFF;
        let a = LogicalAddress::from_raw(base_addr.raw() + i as u64);
        let entry = HashBucketEntry::new(tag, a, false);
        bucket.entry(i).store(entry, Ordering::Relaxed);
    }
}

/// Hash a u64 key and return (tag, hash).
pub fn hash_key(key: u64) -> (u16, u64) {
    let kh = key.hash();
    (kh.tag(), kh.value())
}
