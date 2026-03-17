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

use crate::address::{LogicalAddress, OFFSET_BITS};
use crate::record::{
    AtomicRecordInfo, KEY_OFFSET, Key, RECORD_HEADER_SIZE, RecordInfo, RecordLayout, Value,
    read_record_info as layout_read_record_info, write_record as layout_write_record,
};

use super::log_allocator::HybridLogAllocator;

// ── Helper ──────────────────────────────────────────────────────────

/// Compute a safe `record_size` for reading a record at `addr`.
///
/// Records never span page boundaries, so the remaining space within the
/// current page is a safe upper bound on the record's actual size.  This
/// is critical for variable-length records (e.g. `Vec<u8>`) where a
/// caller might only know a minimum header+key size at lookup time.
///
/// Returns the number of bytes remaining on the page from `addr`'s offset.
#[inline]
fn safe_record_size(addr: LogicalAddress) -> u32 {
    let page_size = 1u32 << OFFSET_BITS;
    let offset = addr.offset().0;
    page_size.saturating_sub(offset)
}

// ── RecordAccessor ──────────────────────────────────────────────────

/// Provides typed, zero-copy access to a record in the hybrid log.
///
/// This is a lightweight handle that holds a raw pointer to a record's
/// memory location in a page frame. The caller is responsible for ensuring
/// the record remains valid (i.e., the page frame is in memory — either
/// by holding a [`PinnedPage`] guard or because the record is in the
/// mutable region which cannot be evicted).
///
/// # Not `Send` / `Sync`
///
/// `RecordAccessor` holds a raw pointer to page memory that could be
/// evicted; it is intentionally `!Send` and `!Sync` (automatic because
/// `*const u8` is `!Send + !Sync`).
///
/// [`PinnedPage`]: super::page::PinnedPage
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
    ///   page frame must be pinned via [`PinnedPage`], or the record must
    ///   be in the mutable region which is not subject to eviction).
    /// - `ptr` must be 8-byte aligned (records always start at 8-byte
    ///   boundaries).
    ///
    /// [`PinnedPage`]: super::page::PinnedPage
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

    /// Creates a read-only accessor to a record at `addr` using the allocator
    /// for logical-to-physical translation.
    ///
    /// Returns `None` if the address is not currently in memory.
    ///
    /// **Note:** This does not pin the page — the returned accessor's raw
    /// pointer could become invalid if the page is evicted. For read paths
    /// that may race with eviction (compaction scanner, version chain walks
    /// in the read-only region), prefer [`from_log_pinned`] which holds a
    /// [`PinnedPage`] guard.
    ///
    /// For mutable-region accesses (upsert, RMW, delete), the page cannot
    /// be evicted because it is above `head_address`, so pinning is not
    /// required.
    ///
    /// [`from_log_pinned`]: RecordAccessor::from_log_pinned
    /// [`PinnedPage`]: super::page::PinnedPage
    #[inline]
    pub(crate) fn from_log(
        allocator: &HybridLogAllocator,
        addr: LogicalAddress,
        record_size: u32,
    ) -> Option<Self> {
        let ptr = allocator.get_physical_address(addr)?;
        // SAFETY: `HybridLogAllocator::get_physical_address` returns a valid
        // pointer within an allocated page frame. Record offsets are always
        // multiples of 8 (RECORD_ALIGNMENT), ensuring 8-byte alignment.
        // The caller is responsible for ensuring the page is not evicted
        // during the accessor's lifetime (e.g., the record is in the mutable
        // region, or the caller holds a PinnedPage guard at a higher scope).
        Some(unsafe { Self::new(ptr as *const u8, record_size) })
    }

    /// Creates a read-only accessor to a record at `addr` with pin
    /// protection against eviction.
    ///
    /// Returns `(PinnedPage, RecordAccessor)` — the caller must keep the
    /// `PinnedPage` alive for as long as the `RecordAccessor` is used.
    /// The pin prevents the page from being evicted while the accessor
    /// is active.
    ///
    /// Returns `None` if the address is below `head_address` (evicted) or
    /// the page frame is not allocated.
    #[inline]
    pub(crate) fn from_log_pinned<'a>(
        allocator: &'a HybridLogAllocator,
        addr: LogicalAddress,
        record_size: u32,
    ) -> Option<(super::page::PinnedPage<'a>, Self)> {
        let pinned = allocator.pin_page(addr)?;
        let offset = addr.offset().0 as usize;
        // SAFETY: The frame is pinned, preventing eviction. The offset is
        // derived from addr.offset() which is bounded by page_size.
        // Records start at 8-byte aligned offsets (RECORD_ALIGNMENT).
        let ptr = unsafe { pinned.frame().as_ptr().add(offset) };
        // SAFETY: ptr is within the pinned frame (see above), aligned to 8
        // bytes (record alignment invariant), and valid for record_size bytes.
        let accessor = unsafe { Self::new(ptr, record_size) };
        Some((pinned, accessor))
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
        &self.as_slice()[layout.key_offset()..layout.value_offset()]
    }

    /// Returns the raw value bytes (zero-copy).
    #[inline]
    pub fn value_ref(&self, layout: &RecordLayout) -> &[u8] {
        &self.as_slice()[layout.value_offset()..self.record_size as usize]
    }

    /// Returns a mutable raw pointer to the start of the value data.
    #[inline]
    pub fn value_mut_ptr(&mut self, layout: &RecordLayout) -> *mut u8 {
        self.as_mut_slice()[layout.value_offset()..].as_mut_ptr()
    }

    /// Returns the full record as a read-only byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `record_size` bytes (constructor invariant).
        // The borrow lifetime is tied to `&self`.
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
    pub fn write_record_info(&mut self, info: &RecordInfo) {
        self.as_mut_slice()[..RECORD_HEADER_SIZE].copy_from_slice(&info.raw().to_le_bytes());
    }

    /// Serializes and writes a key at the position specified by `layout`.
    #[inline]
    pub fn write_key<K: Key>(&mut self, key: &K, layout: &RecordLayout) {
        let buf = self.as_mut_slice();
        let key_end = layout.key_offset() + key.serialized_size();
        key.serialize(&mut buf[layout.key_offset()..key_end]);
    }

    /// Serializes and writes a value at the position specified by `layout`.
    #[inline]
    pub fn write_value<V: Value>(&mut self, value: &V, layout: &RecordLayout) {
        let buf = self.as_mut_slice();
        let value_end = layout.value_offset() + value.serialized_size();
        value.serialize(&mut buf[layout.value_offset()..value_end]);
    }

    /// Writes a complete record (header + key + value) using the existing
    /// [`write_record`](crate::record::write_record) helper.
    #[inline]
    pub fn write_full_record<K: Key, V: Value>(
        &mut self,
        info: &RecordInfo,
        key: &K,
        value: &V,
        layout: &RecordLayout,
    ) {
        layout_write_record(self.as_mut_slice(), info, key, value, layout);
    }

    /// Zeros the entire record (for clearing or reinitializing).
    ///
    /// Safe because `&mut self` guarantees exclusive access — no aliasing
    /// violation.
    #[inline]
    pub fn zero(&mut self) {
        self.as_mut_slice().fill(0);
    }

    /// Returns the full record as a mutable byte slice.
    ///
    /// The `&mut self` receiver guarantees exclusive access, preventing
    /// overlapping `&mut [u8]` references (Stacked Borrows compliant).
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `record_size` writable bytes (constructor
        // invariant). `&mut self` guarantees no aliased references exist.
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

    /// Allocate space for a record of the given byte size.
    ///
    /// Unlike [`allocate_record`](Self::allocate_record), this does not
    /// require typed key/value references — the caller provides the exact
    /// size in bytes. This is used by the compaction copier, which copies
    /// raw record bytes without deserializing.
    ///
    /// Returns `None` if the allocator is sealed or if the record would
    /// cross a page boundary.
    pub fn allocate_raw(&self, size: u32) -> Option<(LogicalAddress, MutableRecordAccessor)> {
        let addr = self.allocator.try_allocate(size)?;
        let ptr = self.allocator.get_physical_address(addr)?;

        // SAFETY: Same guarantees as `allocate_record` — freshly allocated,
        // 8-byte-aligned space within a valid page frame with exclusive access.
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
        let (addr, mut accessor) = self.allocate_record(key, value)?;
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
        RecordAccessor::from_log(self.allocator, addr, record_size)
    }

    /// Read the [`RecordInfo`] header at the given address.
    ///
    /// Pins the page during the read to prevent eviction. This is safer
    /// than [`get_record`] for read-only region accesses that may race
    /// with the page evictor.
    ///
    /// Returns `None` if the address is not in memory.
    pub fn read_record_info(&self, addr: LogicalAddress) -> Option<RecordInfo> {
        let (_pin, accessor) =
            RecordAccessor::from_log_pinned(self.allocator, addr, RECORD_HEADER_SIZE as u32)?;
        Some(accessor.record_info())
    }

    /// Read a key at the given address with the given layout.
    ///
    /// Uses page-bounded record sizing so that variable-length records
    /// (where `layout.total_size()` may underestimate the true size)
    /// are read correctly.
    ///
    /// Returns `None` if the address is not in memory.
    pub fn read_key<K: Key>(&self, addr: LogicalAddress, layout: &RecordLayout) -> Option<K> {
        let record_size = safe_record_size(addr);
        let (_pin, accessor) =
            RecordAccessor::from_log_pinned(self.allocator, addr, record_size)?;
        Some(accessor.key(layout))
    }

    /// Read a value at the given address with the given layout.
    ///
    /// Uses page-bounded record sizing so that variable-length records
    /// (where `layout.total_size()` may underestimate the true size)
    /// are read correctly. Pins the page to prevent eviction during read.
    ///
    /// Returns `None` if the address is not in memory.
    pub fn read_value<V: Value>(&self, addr: LogicalAddress, layout: &RecordLayout) -> Option<V> {
        let record_size = safe_record_size(addr);
        let (_pin, accessor) =
            RecordAccessor::from_log_pinned(self.allocator, addr, record_size)?;
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
    ///
    /// Uses page-bounded record sizing: since records never span page
    /// boundaries, the remaining space in the current page is always a
    /// safe upper bound.  This is essential for variable-length value
    /// types where `layout.total_size()` (computed from
    /// `std::mem::size_of::<V>()`) may be smaller than the true record.
    pub fn read_header_and_match_key<K: Key>(
        &self,
        addr: LogicalAddress,
        key: &K,
        layout: &RecordLayout,
    ) -> Option<(RecordInfo, bool)> {
        let record_size = safe_record_size(addr);
        let (_pin, accessor) =
            RecordAccessor::from_log_pinned(self.allocator, addr, record_size)?;
        let ri = accessor.record_info();
        let stored_key: K = accessor.key(layout);
        Some((ri, stored_key == *key))
    }

    /// Like [`read_header_and_match_key`](Self::read_header_and_match_key)
    /// but does not require a pre-computed [`RecordLayout`].
    ///
    /// Uses page-bounded access and [`Key::eq_from_bytes`] for zero-copy
    /// key comparison. This is correct for hash chains containing records
    /// of different sizes (variable-length keys/values).
    ///
    /// The key offset is always 8 bytes from the record start — an
    /// invariant of the record layout (`pad_alignment(RECORD_HEADER_SIZE,
    /// RECORD_ALIGNMENT) == 8`).
    pub fn read_header_and_match_key_varlen<K: Key>(
        &self,
        addr: LogicalAddress,
        key: &K,
    ) -> Option<(RecordInfo, bool)> {
        let record_size = safe_record_size(addr);
        let (_pin, accessor) =
            RecordAccessor::from_log_pinned(self.allocator, addr, record_size)?;
        let ri = accessor.record_info();
        let key_matches = key.eq_from_bytes(&accessor.as_slice()[KEY_OFFSET..]);
        Some((ri, key_matches))
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
        let (addr, mut accessor) = writer.allocate_record(&key, &original).expect("alloc");
        accessor.write_full_record(&info, &key, &original, &layout);

        // Verify original value.
        let rv: u64 = reader.read_value(addr, &layout).expect("read original");
        assert_eq!(rv, 100);

        // Update value in place via a new mutable accessor.
        let ptr = alloc.get_physical_address(addr).expect("phys addr");
        // SAFETY: ptr is valid, 8-byte aligned, and we control access in this test.
        let mut mut_acc = unsafe { MutableRecordAccessor::new(ptr, layout.total_size() as u32) };
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
