//! Record read/write operations for the hybrid log.
//!
//! This module bridges the record layout (byte-slice-based) with the hybrid
//! log's page-backed memory. It provides zero-copy accessors for reading and
//! writing records, plus a version chain iterator.
//!
//! # Types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`RecordAccessor`] | Zero-copy read access to a record in page memory |
//! | [`MutableRecordAccessor`] | Write access to records in the mutable region |
//! | [`LogRecordWriter`] | Allocate + write records via the bump allocator |
//! | [`LogRecordReader`] | Read records from in-memory pages |
//! | [`VersionChainIterator`] | Walk the `previous_address` chain |

use crate::address::LogicalAddress;
use crate::record::{
    AtomicRecordInfo, Key, RECORD_HEADER_SIZE, RecordInfo, RecordLayout, Value,
    read_record_info as layout_read_record_info, write_record as layout_write_record,
};

use super::log_allocator::HybridLogAllocator;

// ── RecordAccessor ──────────────────────────────────────────────────

/// Provides typed, zero-copy access to a record in the hybrid log.
///
/// This is a lightweight handle that holds a raw pointer to a record's
/// memory location in a page frame. The caller is responsible for ensuring
/// the record remains valid (i.e., the page frame is in memory and the
/// epoch guard is held).
///
/// # Not `Send` / `Sync`
///
/// `RecordAccessor` holds a raw pointer to page memory that could be
/// evicted; it is intentionally `!Send` and `!Sync` (automatic because
/// `*const u8` is `!Send + !Sync`).
pub struct RecordAccessor {
    /// Raw pointer to the start of the record in page memory.
    ptr: *const u8,
    /// Total record size in bytes.
    record_size: u32,
}

impl RecordAccessor {
    /// Creates a new `RecordAccessor`.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to valid, readable memory of at least `record_size`
    ///   bytes.
    /// - The memory must remain valid for the lifetime of this accessor (the
    ///   page frame must be pinned in memory, typically by holding an epoch
    ///   guard).
    /// - `ptr` must be 8-byte aligned (records always start at 8-byte
    ///   boundaries).
    #[inline]
    pub unsafe fn new(ptr: *const u8, record_size: u32) -> Self {
        debug_assert!(!ptr.is_null(), "RecordAccessor: null pointer");
        debug_assert!(
            ptr as usize % 8 == 0,
            "RecordAccessor: pointer not 8-byte aligned"
        );
        debug_assert!(
            record_size as usize >= RECORD_HEADER_SIZE,
            "RecordAccessor: record_size smaller than header"
        );
        Self { ptr, record_size }
    }

    /// Reads the [`RecordInfo`] header (first 8 bytes of the record).
    #[inline]
    pub fn record_info(&self) -> RecordInfo {
        layout_read_record_info(self.as_slice())
    }

    /// Returns a reference to the header as [`AtomicRecordInfo`] for CAS
    /// operations.
    ///
    /// This is safe because the record starts at an 8-byte aligned position
    /// and `AtomicRecordInfo` is `#[repr(transparent)]` over `AtomicU64`.
    #[inline]
    pub fn atomic_record_info(&self) -> &AtomicRecordInfo {
        // SAFETY: `ptr` is 8-byte aligned (constructor invariant) and points to
        // at least 8 valid bytes. `AtomicRecordInfo` is `#[repr(transparent)]`
        // over `AtomicU64`, which has the same size and alignment as `u64`.
        unsafe { &*(self.ptr as *const AtomicRecordInfo) }
    }

    /// Deserializes the key from this record.
    #[inline]
    pub fn key<K: Key>(&self, layout: &RecordLayout) -> K {
        K::deserialize(&self.as_slice()[layout.key_offset()..])
    }

    /// Deserializes the value from this record.
    #[inline]
    pub fn value<V: Value>(&self, layout: &RecordLayout) -> V {
        V::deserialize(&self.as_slice()[layout.value_offset()..])
    }

    /// Returns the raw key bytes (zero-copy).
    ///
    /// The returned slice spans from `key_offset` to `value_offset`, which
    /// may include alignment padding after the serialized key data.
    #[inline]
    pub fn key_ref(&self, layout: &RecordLayout) -> &[u8] {
        &self.as_slice()[layout.key_offset()..layout.value_offset()]
    }

    /// Returns the raw value bytes (zero-copy).
    ///
    /// The returned slice spans from `value_offset` to the end of the
    /// record, which may include alignment padding after the serialized
    /// value data.
    #[inline]
    pub fn value_ref(&self, layout: &RecordLayout) -> &[u8] {
        &self.as_slice()[layout.value_offset()..self.record_size as usize]
    }

    /// Returns the full record as a byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `record_size` bytes (constructor invariant).
        unsafe { core::slice::from_raw_parts(self.ptr, self.record_size as usize) }
    }

    /// Returns the total record size in bytes.
    #[inline]
    pub fn record_size(&self) -> u32 {
        self.record_size
    }
}

// ── MutableRecordAccessor ───────────────────────────────────────────

/// Mutable access to a record in the mutable region of the hybrid log.
///
/// Like [`RecordAccessor`] but with write capabilities. Used for newly
/// allocated records that need to be populated, or for in-place updates
/// to values in the mutable region.
///
/// # Not `Send` / `Sync`
///
/// `MutableRecordAccessor` holds a raw pointer to page memory that could
/// be evicted; it is intentionally `!Send` and `!Sync`.
pub struct MutableRecordAccessor {
    /// Raw pointer to the start of the record.
    ptr: *mut u8,
    /// Total record size in bytes.
    record_size: u32,
}

impl MutableRecordAccessor {
    /// Creates a new `MutableRecordAccessor`.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to valid, writable memory of at least `record_size`
    ///   bytes.
    /// - The memory must remain valid for the lifetime of this accessor.
    /// - `ptr` must be 8-byte aligned.
    /// - No other references to this memory region may exist concurrently
    ///   (exclusive access).
    #[inline]
    pub unsafe fn new(ptr: *mut u8, record_size: u32) -> Self {
        debug_assert!(!ptr.is_null(), "MutableRecordAccessor: null pointer");
        debug_assert!(
            ptr as usize % 8 == 0,
            "MutableRecordAccessor: pointer not 8-byte aligned"
        );
        debug_assert!(
            record_size as usize >= RECORD_HEADER_SIZE,
            "MutableRecordAccessor: record_size smaller than header"
        );
        Self { ptr, record_size }
    }

    // ── Read methods (delegated to an immutable view) ───────────────

    /// Reads the [`RecordInfo`] header.
    #[inline]
    pub fn record_info(&self) -> RecordInfo {
        self.as_read_accessor().record_info()
    }

    /// Returns a reference to the header as [`AtomicRecordInfo`] for CAS
    /// operations.
    #[inline]
    pub fn atomic_record_info(&self) -> &AtomicRecordInfo {
        // SAFETY: Same guarantees as RecordAccessor::atomic_record_info —
        // `ptr` is 8-byte aligned and points to at least 8 valid bytes.
        unsafe { &*(self.ptr as *const AtomicRecordInfo) }
    }

    /// Deserializes the key from this record.
    #[inline]
    pub fn key<K: Key>(&self, layout: &RecordLayout) -> K {
        self.as_read_accessor().key(layout)
    }

    /// Deserializes the value from this record.
    #[inline]
    pub fn value<V: Value>(&self, layout: &RecordLayout) -> V {
        self.as_read_accessor().value(layout)
    }

    /// Returns the raw key bytes (zero-copy).
    #[inline]
    pub fn key_ref(&self, layout: &RecordLayout) -> &[u8] {
        // SAFETY: `ptr` is valid for `record_size` bytes (constructor invariant).
        let slice = unsafe {
            core::slice::from_raw_parts(self.ptr as *const u8, self.record_size as usize)
        };
        &slice[layout.key_offset()..layout.value_offset()]
    }

    /// Returns the raw value bytes (zero-copy).
    #[inline]
    pub fn value_ref(&self, layout: &RecordLayout) -> &[u8] {
        // SAFETY: `ptr` is valid for `record_size` bytes (constructor invariant).
        let slice = unsafe {
            core::slice::from_raw_parts(self.ptr as *const u8, self.record_size as usize)
        };
        &slice[layout.value_offset()..self.record_size as usize]
    }

    /// Returns the full record as a read-only byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `record_size` bytes (constructor invariant).
        unsafe { core::slice::from_raw_parts(self.ptr as *const u8, self.record_size as usize) }
    }

    /// Returns the total record size in bytes.
    #[inline]
    pub fn record_size(&self) -> u32 {
        self.record_size
    }

    // ── Write methods ───────────────────────────────────────────────

    /// Writes the [`RecordInfo`] header (first 8 bytes).
    #[inline]
    pub fn write_record_info(&self, info: &RecordInfo) {
        self.as_mut_slice()[..RECORD_HEADER_SIZE].copy_from_slice(&info.raw().to_le_bytes());
    }

    /// Serializes and writes a key at the position specified by `layout`.
    #[inline]
    pub fn write_key<K: Key>(&self, key: &K, layout: &RecordLayout) {
        let buf = self.as_mut_slice();
        let key_end = layout.key_offset() + key.serialized_size();
        key.serialize(&mut buf[layout.key_offset()..key_end]);
    }

    /// Serializes and writes a value at the position specified by `layout`.
    #[inline]
    pub fn write_value<V: Value>(&self, value: &V, layout: &RecordLayout) {
        let buf = self.as_mut_slice();
        let value_end = layout.value_offset() + value.serialized_size();
        value.serialize(&mut buf[layout.value_offset()..value_end]);
    }

    /// Writes a complete record (header + key + value) using the existing
    /// [`write_record`](crate::record::write_record) helper.
    #[inline]
    pub fn write_full_record<K: Key, V: Value>(
        &self,
        info: &RecordInfo,
        key: &K,
        value: &V,
        layout: &RecordLayout,
    ) {
        layout_write_record(self.as_mut_slice(), info, key, value, layout);
    }

    /// Returns the full record as a mutable byte slice.
    ///
    /// # Safety note
    ///
    /// This method takes `&self` rather than `&mut self` because the
    /// `MutableRecordAccessor` represents an *owned* mutable view of the
    /// underlying page memory (guaranteed exclusive by the caller at
    /// construction time). The `*mut u8` already carries the mutability
    /// permission.
    ///
    /// # TODO (SF-2)
    ///
    /// This creates `&mut [u8]` from `&self`, which violates Stacked Borrows.
    /// Multiple calls produce overlapping `&mut` references. Refactor to use
    /// raw pointer operations (`ptr::copy_nonoverlapping`) internally, or
    /// change write methods to take `&mut self`. Validate with `cargo +nightly
    /// miri test`.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn as_mut_slice(&self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `record_size` writable bytes and we have
        // exclusive access (constructor invariant).
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.record_size as usize) }
    }

    /// Returns a read-only [`RecordAccessor`] view of this record.
    #[inline]
    fn as_read_accessor(&self) -> RecordAccessor {
        RecordAccessor {
            ptr: self.ptr as *const u8,
            record_size: self.record_size,
        }
    }
}

// ── LogRecordWriter ─────────────────────────────────────────────────

/// Writes records into the hybrid log via the bump allocator.
///
/// This is the primary interface for appending new records to the log.
/// It computes the record layout, allocates space from the
/// [`HybridLogAllocator`], and provides a [`MutableRecordAccessor`] for
/// writing the record data.
pub struct LogRecordWriter<'a> {
    allocator: &'a HybridLogAllocator,
}

impl<'a> LogRecordWriter<'a> {
    /// Creates a new writer backed by the given allocator.
    #[inline]
    pub fn new(allocator: &'a HybridLogAllocator) -> Self {
        Self { allocator }
    }

    /// Allocate space for a record and return a [`MutableRecordAccessor`].
    ///
    /// Computes the [`RecordLayout`] from the key and value, allocates the
    /// required number of bytes, and returns both the logical address and a
    /// mutable accessor to the newly allocated region.
    ///
    /// Returns `None` if the allocator is sealed or if the record would
    /// cross a page boundary. The caller handles page advance and retries.
    pub fn allocate_record<K: Key, V: Value>(
        &self,
        key: &K,
        value: &V,
    ) -> Option<(LogicalAddress, MutableRecordAccessor)> {
        let layout = RecordLayout::for_kv(key, value);
        let size = layout.total_size() as u32;

        let addr = self.allocator.try_allocate(size)?;
        let ptr = self.allocator.get_physical_address(addr)?;

        // SAFETY: The allocator guarantees that:
        // 1. `addr` points to freshly allocated space of `size` bytes within a
        //    valid page frame (get_or_allocate_frame was called by try_allocate).
        // 2. The offset is a multiple of RECORD_ALIGNMENT (8) because all record
        //    sizes are multiples of 8 and allocation is bump-sequential from
        //    page-aligned start.
        // 3. We have exclusive access — the region was just bumped past the old
        //    tail and is not yet visible to readers.
        let accessor = unsafe { MutableRecordAccessor::new(ptr, size) };
        Some((addr, accessor))
    }

    /// Allocate and write a complete record in one step.
    ///
    /// Returns the [`LogicalAddress`] of the newly written record, or `None`
    /// if allocation failed (sealed or page boundary).
    pub fn write_record<K: Key, V: Value>(
        &self,
        info: &RecordInfo,
        key: &K,
        value: &V,
    ) -> Option<LogicalAddress> {
        let layout = RecordLayout::for_kv(key, value);
        let (addr, accessor) = self.allocate_record(key, value)?;
        accessor.write_full_record(info, key, value, &layout);
        Some(addr)
    }
}

// ── LogRecordReader ─────────────────────────────────────────────────

/// Reads records from in-memory pages of the hybrid log.
///
/// All methods return `None` if the target address is not currently in
/// memory (i.e., the page frame has not been allocated or has been evicted).
pub struct LogRecordReader<'a> {
    allocator: &'a HybridLogAllocator,
}

impl<'a> LogRecordReader<'a> {
    /// Creates a new reader backed by the given allocator.
    #[inline]
    pub fn new(allocator: &'a HybridLogAllocator) -> Self {
        Self { allocator }
    }

    /// Get a read-only accessor to a record at the given address.
    ///
    /// `record_size` is the total record size in bytes (as computed by
    /// [`RecordLayout::total_size()`]). The caller must know the record's
    /// key and value types to provide this.
    ///
    /// Returns `None` if the address is not in memory.
    pub fn get_record(&self, addr: LogicalAddress, record_size: u32) -> Option<RecordAccessor> {
        let ptr = self.allocator.get_physical_address(addr)?;

        // SAFETY: The allocator returned a valid pointer within an allocated page
        // frame. The caller guarantees `record_size` matches the actual record at
        // this address. The pointer is 8-byte aligned because record offsets are
        // always multiples of 8.
        Some(unsafe { RecordAccessor::new(ptr as *const u8, record_size) })
    }

    /// Read the [`RecordInfo`] header at the given address.
    ///
    /// This is cheaper than [`get_record`](Self::get_record) when only the
    /// header is needed (e.g., for chain traversal).
    ///
    /// Returns `None` if the address is not in memory.
    pub fn read_record_info(&self, addr: LogicalAddress) -> Option<RecordInfo> {
        let ptr = self.allocator.get_physical_address(addr)?;

        // SAFETY: The allocator returned a valid pointer within a page frame.
        // RecordInfo occupies the first 8 bytes of any record, and all page frames
        // are at least one page in size. Reading `RECORD_HEADER_SIZE` bytes at a
        // valid record start is safe.
        let header_slice =
            unsafe { core::slice::from_raw_parts(ptr as *const u8, RECORD_HEADER_SIZE) };
        Some(layout_read_record_info(header_slice))
    }

    /// Read a key at the given address with the given layout.
    ///
    /// Returns `None` if the address is not in memory.
    pub fn read_key<K: Key>(&self, addr: LogicalAddress, layout: &RecordLayout) -> Option<K> {
        let accessor = self.get_record(addr, layout.total_size() as u32)?;
        Some(accessor.key(layout))
    }

    /// Read a value at the given address with the given layout.
    ///
    /// Returns `None` if the address is not in memory.
    pub fn read_value<V: Value>(&self, addr: LogicalAddress, layout: &RecordLayout) -> Option<V> {
        let accessor = self.get_record(addr, layout.total_size() as u32)?;
        Some(accessor.value(layout))
    }

    /// Read the [`RecordInfo`] header and check key equality in a single
    /// physical-address lookup.
    ///
    /// Returns `None` if the address is not in memory. Otherwise returns
    /// `(record_info, key_matches)`. This is more efficient than calling
    /// [`read_record_info`](Self::read_record_info) followed by
    /// [`key_matches`](Self::key_matches) because the logical→physical
    /// translation is performed only once.
    pub fn read_header_and_match_key<K: Key>(
        &self,
        addr: LogicalAddress,
        key: &K,
        layout: &RecordLayout,
    ) -> Option<(RecordInfo, bool)> {
        let ptr = self.allocator.get_physical_address(addr)?;

        // Read header (first RECORD_HEADER_SIZE bytes).
        // SAFETY: The allocator returned a valid pointer within a page frame.
        // RecordInfo occupies the first `RECORD_HEADER_SIZE` bytes of any
        // record, and all page frames are at least one page in size.
        let header_slice =
            unsafe { core::slice::from_raw_parts(ptr as *const u8, RECORD_HEADER_SIZE) };
        let ri = layout_read_record_info(header_slice);

        // Read key from the same physical pointer via a RecordAccessor.
        let record_size = layout.total_size() as u32;
        // SAFETY: The allocator returned a valid pointer and `record_size`
        // matches the layout. The pointer is 8-byte aligned because record
        // offsets are always multiples of 8.
        let accessor = unsafe { RecordAccessor::new(ptr as *const u8, record_size) };
        let stored_key: K = accessor.key(layout);
        Some((ri, stored_key == *key))
    }

    /// Check if the record at `addr` matches the given key.
    ///
    /// Returns `false` if the address is not in memory or the stored key
    /// does not equal `key`.
    pub fn key_matches<K: Key>(
        &self,
        addr: LogicalAddress,
        key: &K,
        layout: &RecordLayout,
    ) -> bool {
        match self.read_key::<K>(addr, layout) {
            Some(stored_key) => stored_key == *key,
            None => false,
        }
    }

    /// Creates a [`VersionChainIterator`] starting at the given address.
    ///
    /// `record_size` is the expected total size of each record in the chain.
    /// For fixed-size key/value types all records in a version chain share
    /// the same size.
    pub fn version_chain(
        &'a self,
        start: LogicalAddress,
        record_size: u32,
    ) -> VersionChainIterator<'a> {
        VersionChainIterator {
            reader: self,
            current: start,
            record_size,
        }
    }
}

// ── VersionChainIterator ────────────────────────────────────────────

/// Iterator over the version chain for a key, starting from a given
/// address.
///
/// Follows [`RecordInfo::previous_address()`] links until reaching
/// [`LogicalAddress::ZERO`], [`LogicalAddress::INVALID`], or an address
/// whose page is no longer in memory.
pub struct VersionChainIterator<'a> {
    reader: &'a LogRecordReader<'a>,
    current: LogicalAddress,
    record_size: u32,
}

impl<'a> Iterator for VersionChainIterator<'a> {
    type Item = (LogicalAddress, RecordAccessor);

    fn next(&mut self) -> Option<Self::Item> {
        // Stop at sentinel or invalid addresses.
        if !self.current.is_valid() {
            return None;
        }

        let addr = self.current;
        let accessor = self.reader.get_record(addr, self.record_size)?;

        // Advance to the next entry in the version chain.
        self.current = accessor.record_info().previous_address();

        Some((addr, accessor))
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Offset, Page};
    use core::sync::atomic::Ordering;

    const SECTOR_SIZE: usize = 512;
    const BUFFER_PAGES: usize = 4;
    const MUTABLE_FRAC: f64 = 0.5;

    fn make_allocator() -> HybridLogAllocator {
        HybridLogAllocator::new(BUFFER_PAGES, MUTABLE_FRAC, SECTOR_SIZE)
    }

    // 1. Write and read a u64/u64 record.
    #[test]
    fn write_and_read_record_u64() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let key: u64 = 42;
        let value: u64 = 999;
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);

        let addr = writer.write_record(&info, &key, &value).expect("write");

        // Read back via reader.
        let ri = reader.read_record_info(addr).expect("read info");
        assert_eq!(ri.checkpoint_version(), 1);
        assert_eq!(ri.previous_address(), LogicalAddress::ZERO);

        let rk: u64 = reader.read_key(addr, &layout).expect("read key");
        assert_eq!(rk, 42);

        let rv: u64 = reader.read_value(addr, &layout).expect("read value");
        assert_eq!(rv, 999);
    }

    // 2. Variable-length keys and values.
    #[test]
    fn write_and_read_record_string() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let key = String::from("hello");
        let value = String::from("world");
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);

        let addr = writer.write_record(&info, &key, &value).expect("write");

        let rk: String = reader.read_key(addr, &layout).expect("read key");
        assert_eq!(rk, "hello");

        let rv: String = reader.read_value(addr, &layout).expect("read value");
        assert_eq!(rv, "world");
    }

    // 3. key_ref returns the correct raw bytes (zero-copy).
    #[test]
    fn record_accessor_key_ref_zero_copy() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let key: u64 = 0xDEAD_BEEF;
        let value: u64 = 0xCAFE_BABE;
        let layout = RecordLayout::for_kv(&key, &value);

        let addr = writer
            .write_record(
                &RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false),
                &key,
                &value,
            )
            .expect("write");

        let accessor = reader
            .get_record(addr, layout.total_size() as u32)
            .expect("get record");

        // key_ref should contain the LE bytes of 0xDEAD_BEEF.
        assert_eq!(accessor.key_ref(&layout), &0xDEAD_BEEFu64.to_le_bytes());

        // value_ref should contain the LE bytes of 0xCAFE_BABE.
        assert_eq!(accessor.value_ref(&layout), &0xCAFE_BABEu64.to_le_bytes());
    }

    // 4. Write a record, then update the value in place.
    #[test]
    fn mutable_accessor_update_value() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let key: u64 = 1;
        let original: u64 = 100;
        let updated: u64 = 200;
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let layout = RecordLayout::for_kv(&key, &original);

        // Allocate and write the initial record.
        let (addr, accessor) = writer.allocate_record(&key, &original).expect("alloc");
        accessor.write_full_record(&info, &key, &original, &layout);

        // Verify original value.
        let rv: u64 = reader.read_value(addr, &layout).expect("read original");
        assert_eq!(rv, 100);

        // Update value in place via a new mutable accessor.
        let ptr = alloc.get_physical_address(addr).expect("phys addr");
        // SAFETY: ptr is valid, 8-byte aligned, and we control access in this test.
        let mut_acc = unsafe { MutableRecordAccessor::new(ptr, layout.total_size() as u32) };
        mut_acc.write_value(&updated, &layout);

        // Verify updated value.
        let rv2: u64 = reader.read_value(addr, &layout).expect("read updated");
        assert_eq!(rv2, 200);
    }

    // 5. Use atomic_record_info to CAS the header (set tombstone).
    #[test]
    fn atomic_record_info_cas() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let key: u64 = 7;
        let value: u64 = 77;
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);

        let addr = writer.write_record(&info, &key, &value).expect("write");

        let accessor = reader
            .get_record(addr, layout.total_size() as u32)
            .expect("get record");
        let atomic = accessor.atomic_record_info();

        // Initially not a tombstone.
        let before = atomic.load(Ordering::SeqCst);
        assert!(!before.is_tombstone());

        // CAS: set tombstone bit.
        let expected = before;
        let desired = expected.with_tombstone();
        let result = atomic.compare_exchange(expected, desired, Ordering::SeqCst, Ordering::SeqCst);
        assert!(result.is_ok());

        // Read back and verify.
        let after = reader.read_record_info(addr).expect("read info");
        assert!(after.is_tombstone());
        assert_eq!(after.checkpoint_version(), 1);
    }

    // 6. Write 100 records, read them all back.
    #[test]
    fn multiple_records_sequential() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let layout = RecordLayout::compute(8, 8); // u64/u64

        let mut addrs = Vec::with_capacity(100);
        for i in 0u64..100 {
            let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
            let addr = writer.write_record(&info, &i, &(i * 10)).expect("write");
            addrs.push(addr);
        }

        for (i, &addr) in addrs.iter().enumerate() {
            let rk: u64 = reader.read_key(addr, &layout).expect("read key");
            assert_eq!(rk, i as u64);

            let rv: u64 = reader.read_value(addr, &layout).expect("read value");
            assert_eq!(rv, (i as u64) * 10);
        }
    }

    // 7. Write 3 records with previous_address linking, traverse the chain.
    #[test]
    fn version_chain_traversal() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        // Pre-allocate a small block so that actual records start at valid
        // addresses (address 0 is LogicalAddress::ZERO which is !is_valid()).
        alloc.try_allocate(8).expect("dummy alloc");

        let key: u64 = 42;
        let layout = RecordLayout::compute(8, 8);

        // Record 1: no predecessor.
        let info1 = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let addr1 = writer.write_record(&info1, &key, &100u64).expect("write1");

        // Record 2: points back to record 1.
        let info2 = RecordInfo::new(addr1, 2, false, false, false);
        let addr2 = writer.write_record(&info2, &key, &200u64).expect("write2");

        // Record 3: points back to record 2.
        let info3 = RecordInfo::new(addr2, 3, false, false, false);
        let addr3 = writer.write_record(&info3, &key, &300u64).expect("write3");

        // Traverse chain from record 3.
        let chain: Vec<(LogicalAddress, u64)> = reader
            .version_chain(addr3, layout.total_size() as u32)
            .map(|(a, acc)| (a, acc.value::<u64>(&layout)))
            .collect();

        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], (addr3, 300));
        assert_eq!(chain[1], (addr2, 200));
        assert_eq!(chain[2], (addr1, 100));
    }

    // 8. Allocate until page boundary, verify None on overflow.
    #[test]
    fn record_at_page_boundary() {
        let alloc = make_allocator();
        let page_size = alloc.page_size();

        // Fill most of the first page via the raw allocator, leaving room
        // for exactly one 24-byte u64/u64 record.
        let layout = RecordLayout::compute(8, 8);
        let record_size = layout.total_size() as u32;
        assert_eq!(record_size, 24);

        let fill = page_size - record_size;
        alloc.try_allocate(fill).expect("fill page");

        // One record should still fit.
        let writer = LogRecordWriter::new(&alloc);
        let key: u64 = 1;
        let value: u64 = 2;
        let result = writer.allocate_record(&key, &value);
        assert!(result.is_some(), "should fit in remaining space");

        // The next allocation on the same page should fail (page is full,
        // tail has wrapped to the next page or there is zero space left).
        // We intentionally ignore the result — the real boundary test is below.
        let _ = writer.allocate_record(&key, &value);

        // Fresh allocator: leave only 16 bytes on the page, try a 24-byte record.
        let alloc2 = make_allocator();
        let fill2 = alloc2.page_size() - 16;
        alloc2.try_allocate(fill2).expect("fill page2");
        let writer2 = LogRecordWriter::new(&alloc2);
        assert!(
            writer2.allocate_record(&1u64, &2u64).is_none(),
            "24-byte record should not fit in 16-byte remainder"
        );
    }

    // 9. key_matches returns true for correct key, false for incorrect.
    #[test]
    fn key_matches_correct_and_incorrect() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let key: u64 = 42;
        let value: u64 = 999;
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let layout = RecordLayout::for_kv(&key, &value);

        let addr = writer.write_record(&info, &key, &value).expect("write");

        assert!(reader.key_matches(addr, &42u64, &layout));
        assert!(!reader.key_matches(addr, &99u64, &layout));
        assert!(!reader.key_matches(addr, &0u64, &layout));
    }

    // 10. Test the one-step write_record convenience method.
    #[test]
    fn write_record_convenience() {
        let alloc = make_allocator();
        let writer = LogRecordWriter::new(&alloc);
        let reader = LogRecordReader::new(&alloc);

        let key: u64 = 123;
        let value: u64 = 456;
        let prev = LogicalAddress::new(Page(5), Offset(128));
        let info = RecordInfo::new(prev, 7, false, true, false);

        let addr = writer.write_record(&info, &key, &value).expect("write");
        let layout = RecordLayout::for_kv(&key, &value);

        // Verify all fields.
        let ri = reader.read_record_info(addr).expect("info");
        assert_eq!(ri.previous_address(), prev);
        assert_eq!(ri.checkpoint_version(), 7);
        assert!(ri.is_tombstone());
        assert!(!ri.is_invalid());
        assert!(!ri.is_final());

        let rk: u64 = reader.read_key(addr, &layout).expect("key");
        assert_eq!(rk, 123);

        let rv: u64 = reader.read_value(addr, &layout).expect("value");
        assert_eq!(rv, 456);
    }
}
