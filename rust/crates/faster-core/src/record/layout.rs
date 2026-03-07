//! Record layout computation and memory-level record operations.
//!
//! Provides utilities for computing field offsets within a record and
//! reading/writing complete records to byte buffers.
//!
//! # Record layout
//!
//! ```text
//! ┌────────────────────┬───────────┬──────────────┬───────────┬────────────────┬──────────┐
//! │  RecordInfo (8B)   │  padding  │  Key (K B)   │  padding  │  Value (V B)   │  padding │
//! └────────────────────┴───────────┴──────────────┴───────────┴────────────────┴──────────┘
//! │<── key_offset ────>│           │<──── value_offset ──────>│                │
//! │<──────────────────────── total_size (multiple of 8) ──────────────────────>│
//! ```
//!
//! All internal alignment is 8 bytes (matching [`RecordInfo`] natural alignment).
//! This matches the architecture v1 cap of 8-byte alignment and ensures good
//! performance for all primitive types.

use crate::record::record_info::RecordInfo;
use crate::record::traits::{Key, LENGTH_PREFIX_SIZE, Value};

/// Size of the [`RecordInfo`] header in bytes (always 8).
pub const RECORD_HEADER_SIZE: usize = core::mem::size_of::<RecordInfo>();

/// Minimum record alignment in bytes.
///
/// All records are aligned to 8 bytes, matching the natural alignment of
/// `RecordInfo` (`u64`). This ensures atomic access to the header and
/// avoids unaligned memory access penalties.
pub const RECORD_ALIGNMENT: usize = core::mem::align_of::<u64>();

// ── pad_alignment ────────────────────────────────────────────────────

/// Rounds `size` up to the next multiple of `alignment`.
///
/// `alignment` must be a power of two. This matches the C++ `pad_alignment`
/// function in `auto_ptr.h`.
///
/// # Examples
///
/// ```
/// use faster_core::record::pad_alignment;
///
/// assert_eq!(pad_alignment(8, 8), 8);
/// assert_eq!(pad_alignment(9, 8), 16);
/// assert_eq!(pad_alignment(0, 8), 0);
/// assert_eq!(pad_alignment(7, 4), 8);
/// assert_eq!(pad_alignment(1, 1), 1);
/// ```
#[inline(always)]
pub const fn pad_alignment(size: usize, alignment: usize) -> usize {
    debug_assert!(
        alignment > 0 && (alignment & (alignment - 1)) == 0,
        "alignment must be a power of two"
    );
    let mask = alignment - 1;
    (size + mask) & !mask
}

// ── RecordLayout ─────────────────────────────────────────────────────

/// Pre-computed offsets and total size for a record with given key and
/// value sizes.
///
/// Use this to calculate where the key and value sit within a record
/// buffer, and how large the total record is (including all padding).
///
/// # Layout
///
/// ```text
/// Offset 0:     [ RecordInfo  (8 bytes)           ]
/// Offset 8:     [ Key data    (key_size bytes)     ]
/// Offset K:     [ padding to 8-byte alignment      ]
/// Offset V0:    [ Value data  (value_size bytes)   ]
/// Offset V1:    [ padding to 8-byte alignment      ]
///               ← total_size (multiple of 8)
/// ```
///
/// # Examples
///
/// ```
/// use faster_core::record::RecordLayout;
///
/// // Fixed-size: u64 key + u64 value
/// let layout = RecordLayout::compute(8, 8);
/// assert_eq!(layout.key_offset(), 8);    // right after header
/// assert_eq!(layout.value_offset(), 16); // right after key
/// assert_eq!(layout.total_size(), 24);   // 8 + 8 + 8
///
/// // Smaller key with padding: u32 key + u64 value
/// let layout = RecordLayout::compute(4, 8);
/// assert_eq!(layout.key_offset(), 8);
/// assert_eq!(layout.value_offset(), 16); // padded from 12 to 16
/// assert_eq!(layout.total_size(), 24);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLayout {
    key_offset: usize,
    value_offset: usize,
    total_size: usize,
}

impl RecordLayout {
    /// Computes the record layout for a key of `key_size` bytes and a value
    /// of `value_size` bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::record::RecordLayout;
    ///
    /// let layout = RecordLayout::compute(4, 12);
    /// assert_eq!(layout.key_offset(), 8);
    /// assert_eq!(layout.value_offset(), 16);  // pad_alignment(12, 8) = 16
    /// assert_eq!(layout.total_size(), 32);    // pad_alignment(28, 8) = 32
    /// ```
    #[inline]
    pub const fn compute(key_size: usize, value_size: usize) -> Self {
        // Key starts right after the 8-byte header (already 8-byte aligned)
        let key_offset = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT);
        // Value starts at the next 8-byte boundary after the key
        let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);
        // Total size is rounded up to 8-byte boundary
        let total_size = pad_alignment(value_offset + value_size, RECORD_ALIGNMENT);
        Self {
            key_offset,
            value_offset,
            total_size,
        }
    }

    /// Computes the layout for a specific key and value instance.
    #[inline]
    pub fn for_kv<K: Key, V: Value>(key: &K, value: &V) -> Self {
        Self::compute(key.serialized_size(), value.serialized_size())
    }

    /// Byte offset of the key data from the start of the record.
    #[inline(always)]
    pub const fn key_offset(&self) -> usize {
        self.key_offset
    }

    /// Byte offset of the value data from the start of the record.
    #[inline(always)]
    pub const fn value_offset(&self) -> usize {
        self.value_offset
    }

    /// Total record size in bytes (including all padding, multiple of 8).
    #[inline(always)]
    pub const fn total_size(&self) -> usize {
        self.total_size
    }
}

// ── Record read/write helpers ────────────────────────────────────────

/// Writes a complete record into `buf`.
///
/// The record info header is written at offset 0, the key at
/// `layout.key_offset()`, and the value at `layout.value_offset()`.
///
/// # Panics
///
/// Debug-panics if `buf` is smaller than `layout.total_size()`.
///
/// # Examples
///
/// ```
/// use faster_core::address::LogicalAddress;
/// use faster_core::record::{RecordInfo, RecordLayout, Key, Value, write_record, read_record_info, read_key, read_value};
///
/// let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
/// let key: u64 = 42;
/// let value: u64 = 999;
/// let layout = RecordLayout::for_kv(&key, &value);
///
/// let mut buf = vec![0u8; layout.total_size()];
/// write_record(&mut buf, &info, &key, &value, &layout);
///
/// assert_eq!(read_record_info(&buf).checkpoint_version(), 1);
/// assert_eq!(read_key::<u64>(&buf, &layout), 42);
/// assert_eq!(read_value::<u64>(&buf, &layout), 999);
/// ```
pub fn write_record<K: Key, V: Value>(
    buf: &mut [u8],
    info: &RecordInfo,
    key: &K,
    value: &V,
    layout: &RecordLayout,
) {
    debug_assert!(
        buf.len() >= layout.total_size(),
        "buffer too small: need {}, got {}",
        layout.total_size(),
        buf.len(),
    );

    // Write RecordInfo header as 8 little-endian bytes
    buf[..RECORD_HEADER_SIZE].copy_from_slice(&info.raw().to_le_bytes());

    // Write key at computed offset
    let key_end = layout.key_offset() + key.serialized_size();
    key.serialize(&mut buf[layout.key_offset()..key_end]);

    // Write value at computed offset
    let value_end = layout.value_offset() + value.serialized_size();
    value.serialize(&mut buf[layout.value_offset()..value_end]);
}

/// Reads the [`RecordInfo`] header from the start of a record buffer.
///
/// # Panics
///
/// Panics if `buf` is shorter than 8 bytes.
#[inline]
pub fn read_record_info(buf: &[u8]) -> RecordInfo {
    let raw = u64::from_le_bytes(
        buf[..RECORD_HEADER_SIZE]
            .try_into()
            .expect("buffer too short for RecordInfo"),
    );
    RecordInfo::from_raw(raw)
}

/// Reads a key from a record buffer at the offset specified by `layout`.
///
/// # Panics
///
/// Panics if the buffer is too short for the key deserialization.
#[inline]
pub fn read_key<K: Key>(buf: &[u8], layout: &RecordLayout) -> K {
    K::deserialize(&buf[layout.key_offset()..])
}

/// Reads a value from a record buffer at the offset specified by `layout`.
///
/// # Panics
///
/// Panics if the buffer is too short for the value deserialization.
#[inline]
pub fn read_value<V: Value>(buf: &[u8], layout: &RecordLayout) -> V {
    V::deserialize(&buf[layout.value_offset()..])
}

/// Computes the total size of a record for the given key and value.
///
/// Convenience function equivalent to
/// `RecordLayout::for_kv(key, value).total_size()`.
#[inline]
pub fn record_size<K: Key, V: Value>(key: &K, value: &V) -> usize {
    RecordLayout::for_kv(key, value).total_size()
}

// ── RecordSizeError ─────────────────────────────────────────────────

/// Errors from computing record size from raw page bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordSizeError {
    /// Not enough page space remaining to read the minimum record header
    /// and length prefix.
    InsufficientPageSpace,
    /// Record data at the given offset appears corrupted — the computed
    /// size exceeds the remaining page space.
    CorruptedRecord {
        /// Byte offset of the corrupted record within the page.
        offset: usize,
    },
}

impl core::fmt::Display for RecordSizeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecordSizeError::InsufficientPageSpace => {
                write!(f, "insufficient page space for record header + prefix")
            }
            RecordSizeError::CorruptedRecord { offset } => {
                write!(f, "corrupted record at page offset {offset}")
            }
        }
    }
}

impl std::error::Error for RecordSizeError {}

// ── record_size_from_bytes ──────────────────────────────────────────

/// Computes the total record size from raw page bytes without
/// deserializing key or value data.
///
/// This is the core building block for variable-length record scanning
/// during compaction. It reads the key and value length prefixes (for
/// variable-length types) or uses compile-time sizes (for fixed-size
/// types) to determine the full record footprint including all padding.
///
/// # Arguments
///
/// * `page_bytes` — Byte slice covering the full page.
/// * `offset` — Byte offset of the record within `page_bytes`.
/// * `page_size` — Total page size in bytes.
///
/// # Errors
///
/// Returns [`RecordSizeError::InsufficientPageSpace`] if fewer than 12
/// bytes remain (the minimum to read header + one length prefix).
///
/// Returns [`RecordSizeError::CorruptedRecord`] if the computed field
/// offsets or total size exceed the remaining page space, indicating a
/// corrupted length prefix.
pub fn record_size_from_bytes<K: Key, V: Value>(
    page_bytes: &[u8],
    offset: usize,
    page_size: usize,
) -> Result<usize, RecordSizeError> {
    let remaining = page_size.saturating_sub(offset);

    // Minimum readable: 8-byte header + 4-byte length prefix = 12.
    // For fixed-size keys this is conservative (they ignore the buffer),
    // but keeps the check type-agnostic.
    const MIN_READABLE: usize = RECORD_HEADER_SIZE + LENGTH_PREFIX_SIZE; // 12
    if remaining < MIN_READABLE {
        return Err(RecordSizeError::InsufficientPageSpace);
    }

    let key_offset = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT); // = 8
    let key_buf = &page_bytes[offset + key_offset..];
    let key_size = K::serialized_size_from_bytes(key_buf);
    let value_offset = pad_alignment(key_offset + key_size, RECORD_ALIGNMENT);

    // Validate that the key didn't push us past the page boundary.
    if value_offset > remaining {
        return Err(RecordSizeError::CorruptedRecord { offset });
    }

    let value_buf = &page_bytes[offset + value_offset..];
    let value_size = V::serialized_size_from_bytes(value_buf);
    let total = pad_alignment(value_offset + value_size, RECORD_ALIGNMENT);

    // Validate that the complete record fits on the page.
    if total > remaining {
        return Err(RecordSizeError::CorruptedRecord { offset });
    }

    Ok(total)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};

    // ── pad_alignment ───────────────────────────────────────────────

    #[test]
    fn pad_alignment_already_aligned() {
        assert_eq!(pad_alignment(8, 8), 8);
        assert_eq!(pad_alignment(16, 8), 16);
        assert_eq!(pad_alignment(0, 8), 0);
        assert_eq!(pad_alignment(4, 4), 4);
    }

    #[test]
    fn pad_alignment_needs_rounding() {
        assert_eq!(pad_alignment(1, 8), 8);
        assert_eq!(pad_alignment(7, 8), 8);
        assert_eq!(pad_alignment(9, 8), 16);
        assert_eq!(pad_alignment(15, 8), 16);
        assert_eq!(pad_alignment(1, 4), 4);
        assert_eq!(pad_alignment(3, 4), 4);
        assert_eq!(pad_alignment(5, 4), 8);
    }

    #[test]
    fn pad_alignment_one() {
        assert_eq!(pad_alignment(0, 1), 0);
        assert_eq!(pad_alignment(1, 1), 1);
        assert_eq!(pad_alignment(100, 1), 100);
    }

    // ── RecordLayout computation ────────────────────────────────────

    #[test]
    fn layout_u64_u64() {
        let layout = RecordLayout::compute(8, 8);
        assert_eq!(layout.key_offset(), 8);
        assert_eq!(layout.value_offset(), 16);
        assert_eq!(layout.total_size(), 24);
    }

    #[test]
    fn layout_u32_u64() {
        let layout = RecordLayout::compute(4, 8);
        assert_eq!(layout.key_offset(), 8);
        assert_eq!(layout.value_offset(), 16); // 8 + 4 = 12, padded to 16
        assert_eq!(layout.total_size(), 24);
    }

    #[test]
    fn layout_u32_u32() {
        let layout = RecordLayout::compute(4, 4);
        assert_eq!(layout.key_offset(), 8);
        assert_eq!(layout.value_offset(), 16); // 8 + 4 = 12, padded to 16
        assert_eq!(layout.total_size(), 24); // 16 + 4 = 20, padded to 24
    }

    #[test]
    fn layout_variable_length() {
        // Vec<u8> with 10 bytes: length prefix (4) + data (10) = 14 bytes
        let layout = RecordLayout::compute(14, 14);
        assert_eq!(layout.key_offset(), 8);
        assert_eq!(layout.value_offset(), 24); // 8 + 14 = 22, padded to 24
        assert_eq!(layout.total_size(), 40); // 24 + 14 = 38, padded to 40
    }

    #[test]
    fn layout_empty_value() {
        // Zero-length value (edge case)
        let layout = RecordLayout::compute(8, 0);
        assert_eq!(layout.key_offset(), 8);
        assert_eq!(layout.value_offset(), 16);
        assert_eq!(layout.total_size(), 16);
    }

    #[test]
    fn layout_for_kv() {
        let key: u64 = 42;
        let value: u64 = 999;
        let layout = RecordLayout::for_kv(&key, &value);
        assert_eq!(layout, RecordLayout::compute(8, 8));
    }

    #[test]
    fn layout_total_size_always_multiple_of_8() {
        for key_size in 0..64 {
            for value_size in 0..64 {
                let layout = RecordLayout::compute(key_size, value_size);
                assert_eq!(
                    layout.total_size() % 8,
                    0,
                    "total_size {} not multiple of 8 for key_size={}, value_size={}",
                    layout.total_size(),
                    key_size,
                    value_size,
                );
            }
        }
    }

    // ── Record write/read round-trips ───────────────────────────────

    #[test]
    fn write_read_u64_u64() {
        let info = RecordInfo::new(
            LogicalAddress::new(Page(10), Offset(256)),
            42,
            false,
            false,
            false,
        );
        let key: u64 = 0xDEAD_BEEF;
        let value: u64 = 0xCAFE_BABE;
        let layout = RecordLayout::for_kv(&key, &value);

        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        let read_info = read_record_info(&buf);
        assert_eq!(read_info, info);
        assert_eq!(
            read_info.previous_address(),
            LogicalAddress::new(Page(10), Offset(256))
        );
        assert_eq!(read_info.checkpoint_version(), 42);

        let read_k: u64 = read_key(&buf, &layout);
        assert_eq!(read_k, key);

        let read_v: u64 = read_value(&buf, &layout);
        assert_eq!(read_v, value);
    }

    #[test]
    fn write_read_u32_u64() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let key: u32 = 42;
        let value: u64 = 999_999_999;
        let layout = RecordLayout::for_kv(&key, &value);

        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        assert_eq!(read_key::<u32>(&buf, &layout), 42);
        assert_eq!(read_value::<u64>(&buf, &layout), 999_999_999);
    }

    #[test]
    fn write_read_vec_u8_key_and_value() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let key: Vec<u8> = vec![1, 2, 3, 4, 5];
        let value: Vec<u8> = vec![10, 20, 30];
        let layout = RecordLayout::for_kv(&key, &value);

        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        assert_eq!(read_key::<Vec<u8>>(&buf, &layout), key);
        assert_eq!(read_value::<Vec<u8>>(&buf, &layout), value);
    }

    #[test]
    fn write_read_string_key_and_value() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 100, true, false, false);
        let key = String::from("hello");
        let value = String::from("world");
        let layout = RecordLayout::for_kv(&key, &value);

        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        let read_info = read_record_info(&buf);
        assert!(read_info.is_invalid());
        assert_eq!(read_info.checkpoint_version(), 100);
        assert_eq!(read_key::<String>(&buf, &layout), "hello");
        assert_eq!(read_value::<String>(&buf, &layout), "world");
    }

    #[test]
    fn write_read_u32_key_large_value() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let key: u32 = 123;
        let value: Vec<u8> = vec![0xAB; 128];
        let layout = RecordLayout::for_kv(&key, &value);

        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        assert_eq!(read_key::<u32>(&buf, &layout), 123);
        assert_eq!(read_value::<Vec<u8>>(&buf, &layout), value);
    }

    #[test]
    fn write_read_with_tombstone_and_version_chain() {
        let prev = LogicalAddress::new(Page(42), Offset(1024));
        let info = RecordInfo::new(prev, 7, false, true, false);
        let key: u64 = 0;
        let value: u64 = 0;
        let layout = RecordLayout::for_kv(&key, &value);

        let mut buf = vec![0u8; layout.total_size()];
        write_record(&mut buf, &info, &key, &value, &layout);

        let read_info = read_record_info(&buf);
        assert!(read_info.is_tombstone());
        assert!(!read_info.is_invalid());
        assert_eq!(read_info.previous_address(), prev);
        assert_eq!(read_info.checkpoint_version(), 7);
    }

    // ── record_size convenience function ────────────────────────────

    #[test]
    fn record_size_matches_layout() {
        let key: u64 = 42;
        let value: u64 = 999;
        assert_eq!(
            record_size(&key, &value),
            RecordLayout::for_kv(&key, &value).total_size()
        );
    }

    #[test]
    fn record_size_variable_length() {
        let key = String::from("test");
        let value: Vec<u8> = vec![1, 2, 3];
        let size = record_size(&key, &value);
        assert_eq!(size % 8, 0); // always 8-byte aligned
    }

    // ── Property tests ──────────────────────────────────────────────

    // ── record_size_from_bytes tests ────────────────────────────────

    /// Helper: write a record into a page-sized buffer at the given offset.
    fn write_record_at<K: Key, V: Value>(
        page: &mut [u8],
        offset: usize,
        key: &K,
        value: &V,
    ) -> usize {
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let layout = RecordLayout::for_kv(key, value);
        write_record(&mut page[offset..], &info, key, value, &layout);
        layout.total_size()
    }

    #[test]
    fn record_size_from_bytes_u64_u64() {
        let page_size = 256;
        let mut page = vec![0u8; page_size];
        let key: u64 = 42;
        let value: u64 = 999;
        write_record_at(&mut page, 0, &key, &value);

        let result = record_size_from_bytes::<u64, u64>(&page, 0, page_size);
        assert_eq!(result.unwrap(), RecordLayout::compute(8, 8).total_size());
    }

    #[test]
    fn record_size_from_bytes_u32_u64() {
        let page_size = 256;
        let mut page = vec![0u8; page_size];
        let key: u32 = 42;
        let value: u64 = 999;
        write_record_at(&mut page, 0, &key, &value);

        let result = record_size_from_bytes::<u32, u64>(&page, 0, page_size);
        assert_eq!(result.unwrap(), RecordLayout::compute(4, 8).total_size());
    }

    #[test]
    fn record_size_from_bytes_vec_u8() {
        let page_size = 512;
        let mut page = vec![0u8; page_size];
        let key: Vec<u8> = vec![1, 2, 3, 4, 5]; // 4+5 = 9 bytes
        let value: Vec<u8> = vec![10, 20, 30]; // 4+3 = 7 bytes
        let written = write_record_at(&mut page, 0, &key, &value);

        let result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size).unwrap();
        assert_eq!(result, written);
        assert_eq!(result, RecordLayout::for_kv(&key, &value).total_size());
    }

    #[test]
    fn record_size_from_bytes_string() {
        let page_size = 512;
        let mut page = vec![0u8; page_size];
        let key = String::from("hello");
        let value = String::from("world");
        let written = write_record_at(&mut page, 0, &key, &value);

        let result = record_size_from_bytes::<String, String>(&page, 0, page_size);
        assert_eq!(result.unwrap(), written);
    }

    #[test]
    fn record_size_from_bytes_zero_length_value() {
        let page_size = 256;
        let mut page = vec![0u8; page_size];
        let key: Vec<u8> = vec![1, 2, 3];
        let value: Vec<u8> = vec![];
        let written = write_record_at(&mut page, 0, &key, &value);

        let result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size);
        assert_eq!(result.unwrap(), written);
    }

    #[test]
    fn record_size_from_bytes_at_offset() {
        let page_size = 512;
        let mut page = vec![0u8; page_size];
        let key: u64 = 42;
        let value: u64 = 999;
        let offset = 64;
        write_record_at(&mut page, offset, &key, &value);

        let result = record_size_from_bytes::<u64, u64>(&page, offset, page_size);
        assert_eq!(result.unwrap(), RecordLayout::compute(8, 8).total_size());
    }

    #[test]
    fn record_size_from_bytes_multiple_records() {
        let page_size = 512;
        let mut page = vec![0u8; page_size];
        let key1: Vec<u8> = vec![1, 2, 3]; // 4+3=7 bytes key
        let val1: Vec<u8> = vec![10; 20]; // 4+20=24 bytes value
        let size1 = write_record_at(&mut page, 0, &key1, &val1);

        let key2: Vec<u8> = vec![4, 5]; // 4+2=6 bytes key
        let val2: Vec<u8> = vec![20; 5]; // 4+5=9 bytes value
        let size2 = write_record_at(&mut page, size1, &key2, &val2);

        assert_eq!(
            record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size).unwrap(),
            size1
        );
        assert_eq!(
            record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, size1, page_size).unwrap(),
            size2
        );
    }

    #[test]
    fn record_size_from_bytes_insufficient_space() {
        let page_size = 10; // Less than MIN_READABLE (12)
        let page = vec![0u8; page_size];
        let result = record_size_from_bytes::<u64, u64>(&page, 0, page_size);
        assert_eq!(result, Err(RecordSizeError::InsufficientPageSpace));
    }

    #[test]
    fn record_size_from_bytes_insufficient_at_offset() {
        let page_size = 64;
        let page = vec![0u8; page_size];
        // Only 4 bytes remaining — not enough
        let result = record_size_from_bytes::<u64, u64>(&page, 60, page_size);
        assert_eq!(result, Err(RecordSizeError::InsufficientPageSpace));
    }

    #[test]
    fn record_size_from_bytes_corrupted_key_prefix() {
        let page_size = 64;
        let mut page = vec![0u8; page_size];
        // Write a corrupted key length prefix: u32::MAX at key_offset (8)
        let corrupted_len = u32::MAX;
        page[8..12].copy_from_slice(&corrupted_len.to_le_bytes());

        let result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size);
        assert_eq!(result, Err(RecordSizeError::CorruptedRecord { offset: 0 }));
    }

    #[test]
    fn record_size_from_bytes_corrupted_value_prefix() {
        let page_size = 128;
        let mut page = vec![0u8; page_size];
        // Write a valid key (0 bytes data = just the 4-byte prefix)
        let key: Vec<u8> = vec![];
        let value: Vec<u8> = vec![];
        write_record_at(&mut page, 0, &key, &value);

        // Now corrupt the value length prefix.
        // key_offset=8, key_size=4 (prefix only), value_offset=pad(8+4,8)=16
        let corrupted_len = u32::MAX;
        page[16..20].copy_from_slice(&corrupted_len.to_le_bytes());

        let result = record_size_from_bytes::<Vec<u8>, Vec<u8>>(&page, 0, page_size);
        assert_eq!(result, Err(RecordSizeError::CorruptedRecord { offset: 0 }));
    }

    #[test]
    fn record_size_from_bytes_page_boundary_tight_fit() {
        // Record exactly fills the remaining page space.
        let key: u64 = 42;
        let value: u64 = 999;
        let expected_size = RecordLayout::compute(8, 8).total_size(); // 24
        let page_size = expected_size;
        let mut page = vec![0u8; page_size];
        write_record_at(&mut page, 0, &key, &value);

        let result = record_size_from_bytes::<u64, u64>(&page, 0, page_size);
        assert_eq!(result.unwrap(), expected_size);
    }

    #[test]
    fn record_size_from_bytes_page_boundary_exact_min_readable() {
        // Exactly 12 bytes remaining — the minimum.
        let page_size = 64;
        let page = vec![0u8; page_size];
        let offset = page_size - 12;
        // For u64 key (8 bytes), key area starts at offset+8, we need 8 bytes
        // for key but only have 4 bytes remaining after key_offset. The total
        // record (24 bytes) exceeds remaining (12), so this should be corrupted.
        let result = record_size_from_bytes::<u64, u64>(&page, offset, page_size);
        // u64 key_size=8, value_offset=pad(8+8,8)=16, 16>12 → CorruptedRecord
        assert_eq!(result, Err(RecordSizeError::CorruptedRecord { offset }));
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        prop_compose! {
            fn arb_logical_address()(raw in 0u64..=(1u64 << 48) - 1) -> LogicalAddress {
                LogicalAddress::from_raw(raw)
            }
        }

        proptest! {
            #[test]
            fn u64_record_round_trip(
                addr in arb_logical_address(),
                version in 0u16..=crate::record::record_info::MAX_VERSION,
                invalid in any::<bool>(),
                tombstone in any::<bool>(),
                final_bit in any::<bool>(),
                key in any::<u64>(),
                value in any::<u64>(),
            ) {
                let info = RecordInfo::new(addr, version, invalid, tombstone, final_bit);
                let layout = RecordLayout::for_kv(&key, &value);
                let mut buf = vec![0u8; layout.total_size()];
                write_record(&mut buf, &info, &key, &value, &layout);

                let ri = read_record_info(&buf);
                prop_assert_eq!(ri.previous_address(), addr);
                prop_assert_eq!(ri.checkpoint_version(), version);
                prop_assert_eq!(ri.is_invalid(), invalid);
                prop_assert_eq!(ri.is_tombstone(), tombstone);
                prop_assert_eq!(ri.is_final(), final_bit);

                let rk: u64 = read_key(&buf, &layout);
                prop_assert_eq!(rk, key);

                let rv: u64 = read_value(&buf, &layout);
                prop_assert_eq!(rv, value);
            }

            #[test]
            fn vec_u8_record_round_trip(
                key in proptest::collection::vec(any::<u8>(), 0..256),
                value in proptest::collection::vec(any::<u8>(), 0..256),
            ) {
                let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
                let layout = RecordLayout::for_kv(&key, &value);
                let mut buf = vec![0u8; layout.total_size()];
                write_record(&mut buf, &info, &key, &value, &layout);

                let rk: Vec<u8> = read_key(&buf, &layout);
                prop_assert_eq!(rk, key);

                let rv: Vec<u8> = read_value(&buf, &layout);
                prop_assert_eq!(rv, value);
            }
        }
    }
}
