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
use crate::record::traits::{Key, Value};

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
