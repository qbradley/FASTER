//! Forward-scanning iterator over the hybrid log.
//!
//! [`LogScanIterator`] walks the log in address order from a start address to an
//! end address, yielding [`ScanRecord`] entries for each record encountered.
//! Records that don't match the configured [`ScanOptions`] (e.g. tombstones,
//! invalid records) are transparently skipped.
//!
//! # Limitations
//!
//! - **In-memory only (current implementation):** Only pages between the
//!   allocator's head and tail are accessible. Records on disk (below
//!   head_address) are skipped. A future disk-scan extension will allow reading
//!   evicted pages via the storage device.
//! - **Fixed-size records only within a single scan:** The caller supplies
//!   `key_size` and `value_size` at construction time. Variable-length record
//!   scanning requires the caller to know the serialized sizes up front.
//!
//! # Examples
//!
//! ```ignore
//! use faster_core::hybrid_log::scan::{LogScanIterator, ScanOptions};
//!
//! let options = ScanOptions::default();
//! let iter = LogScanIterator::new(&allocator, start, end, 8, 8, options);
//! for record in iter {
//!     println!("addr={}, tombstone={}", record.address, record.record_info.is_tombstone());
//! }
//! ```

use crate::address::{LogicalAddress, OFFSET_BITS, Offset, Page};
use crate::record::{RECORD_HEADER_SIZE, RecordInfo, RecordLayout};

use super::log_allocator::HybridLogAllocator;

// ── ScanOptions ─────────────────────────────────────────────────────

/// Configuration for a log scan.
///
/// Controls which records are yielded by [`LogScanIterator`]. By default,
/// only valid, non-tombstone records are returned.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// If `true`, tombstone records (deleted keys) are included in the scan.
    /// Default: `false`.
    pub include_tombstones: bool,

    /// If `true`, invalidated records (superseded by a newer version) are
    /// included in the scan. Default: `false`.
    pub include_invalid: bool,

    /// If `true`, only scan the in-memory region (head → tail) regardless
    /// of the requested start address. Records below `head_address` are
    /// skipped entirely. Default: `false`.
    ///
    /// When `false`, records below `head_address` are *also* skipped in the
    /// current implementation (disk reads are not yet supported), but the
    /// iterator will advance through them rather than clamping the start
    /// address up front.
    pub in_memory_only: bool,
}

impl Default for ScanOptions {
    /// Returns the default scan options:
    /// - `include_tombstones: false`
    /// - `include_invalid: false`
    /// - `in_memory_only: false`
    fn default() -> Self {
        Self {
            include_tombstones: false,
            include_invalid: false,
            in_memory_only: false,
        }
    }
}

// ── ScanRecord ──────────────────────────────────────────────────────

/// A record yielded by the [`LogScanIterator`].
///
/// Contains the logical address, the [`RecordInfo`] header, and the
/// serialized sizes of the key and value. Use [`key_bytes`](Self::key_bytes)
/// and [`value_bytes`](Self::value_bytes) to access the raw data.
#[derive(Debug, Clone)]
pub struct ScanRecord {
    /// Logical address of this record in the hybrid log.
    pub address: LogicalAddress,

    /// The 8-byte record header (flags, previous address, version).
    pub record_info: RecordInfo,

    /// Serialized key size in bytes (as provided to the scan).
    pub key_size: u32,

    /// Serialized value size in bytes (as provided to the scan).
    pub value_size: u32,

    /// Copy of the raw key bytes.
    key_data: Vec<u8>,

    /// Copy of the raw value bytes.
    value_data: Vec<u8>,
}

impl ScanRecord {
    /// Returns the raw serialized key bytes.
    #[inline]
    pub fn key_bytes(&self) -> &[u8] {
        &self.key_data
    }

    /// Returns the raw serialized value bytes.
    #[inline]
    pub fn value_bytes(&self) -> &[u8] {
        &self.value_data
    }
}

// ── LogScanIterator ─────────────────────────────────────────────────

/// Forward-scanning iterator over hybrid log records.
///
/// Walks from `start` to `end` in address order, yielding one
/// [`ScanRecord`] per matching record. Handles page boundaries
/// transparently.
///
/// # Page boundary handling
///
/// Records never span pages. When the remaining space on the current page
/// is smaller than a single record, the iterator advances to offset 0 of
/// the next page.
///
/// # In-memory constraint
///
/// Only pages that are currently in memory (between the allocator's head
/// and tail addresses) can be scanned. Records on evicted pages are
/// silently skipped. This limitation will be lifted when disk-backed
/// scanning is implemented.
pub struct LogScanIterator<'a> {
    allocator: &'a HybridLogAllocator,
    /// Current scan position.
    current: LogicalAddress,
    /// Exclusive upper bound of the scan.
    end: LogicalAddress,
    /// Pre-computed record layout.
    layout: RecordLayout,
    /// Scan filter options.
    options: ScanOptions,
    /// Page size in bytes (cached from allocator).
    page_size: u32,
}

impl<'a> LogScanIterator<'a> {
    /// Creates a new log scan iterator.
    ///
    /// Scans records from `start` (inclusive) to `end` (exclusive) in the
    /// hybrid log managed by `allocator`. Each record is assumed to have a
    /// key of `key_size` bytes and a value of `value_size` bytes.
    ///
    /// If `options.in_memory_only` is `true`, the effective start is clamped
    /// to `max(start, head_address)`.
    ///
    /// # Arguments
    ///
    /// * `allocator` — The hybrid log allocator backing the pages.
    /// * `start` — First address to scan (inclusive).
    /// * `end` — Last address to scan (exclusive). Typically `tail_address`.
    /// * `key_size` — Serialized key size in bytes.
    /// * `value_size` — Serialized value size in bytes.
    /// * `options` — Filtering options.
    pub fn new(
        allocator: &'a HybridLogAllocator,
        start: LogicalAddress,
        end: LogicalAddress,
        key_size: usize,
        value_size: usize,
        options: ScanOptions,
    ) -> Self {
        let layout = RecordLayout::compute(key_size, value_size);
        let page_size = 1u32 << OFFSET_BITS;

        let effective_start = if options.in_memory_only {
            let head = allocator.head_address();
            if start < head { head } else { start }
        } else {
            start
        };

        Self {
            allocator,
            current: effective_start,
            end,
            layout,
            options,
            page_size,
        }
    }

    /// Advance `current` past any page-boundary gap.
    ///
    /// If the remaining bytes on the current page are fewer than one record,
    /// jump to offset 0 of the next page.
    fn align_to_record_boundary(&mut self) {
        let offset = self.current.offset().0;
        let record_size = self.layout.total_size() as u32;

        // If there isn't room for another record on this page, skip to the
        // next page.
        if offset > 0 && (self.page_size - offset) < record_size {
            let next_page = self.current.page().0 + 1;
            self.current = LogicalAddress::new(Page(next_page), Offset(0));
        }
    }

    /// Returns `true` if the record described by `info` should be yielded
    /// given the current [`ScanOptions`].
    fn should_yield(&self, info: &RecordInfo) -> bool {
        // A null RecordInfo (all zeros) means the slot was never written.
        if info.is_null() {
            return false;
        }
        if info.is_invalid() && !self.options.include_invalid {
            return false;
        }
        if info.is_tombstone() && !self.options.include_tombstones {
            return false;
        }
        true
    }

    /// Try to read the record at `current` from in-memory pages.
    ///
    /// Returns `None` if the page is not in memory (evicted / on disk).
    /// Uses [`pin_page`] to prevent eviction during the read — the pin is
    /// held while record data is copied into the returned [`ScanRecord`].
    ///
    /// [`pin_page`]: super::log_allocator::HybridLogAllocator::pin_page
    fn try_read_record(&self, addr: LogicalAddress) -> Option<ScanRecord> {
        let pinned = self.allocator.pin_page(addr)?;
        let offset = addr.offset().0 as usize;
        let record_size = self.layout.total_size();

        let slice = pinned.get_slice(offset, record_size)?;

        let raw_header = u64::from_le_bytes(
            slice[..RECORD_HEADER_SIZE]
                .try_into()
                .expect("slice is at least RECORD_HEADER_SIZE bytes"),
        );
        let info = RecordInfo::from_raw(raw_header);

        let key_start = self.layout.key_offset();
        let value_start = self.layout.value_offset();
        let key_size = value_start - key_start;
        let value_size = record_size - value_start;

        Some(ScanRecord {
            address: addr,
            record_info: info,
            key_size: key_size as u32,
            value_size: value_size as u32,
            key_data: slice[key_start..key_start + key_size].to_vec(),
            value_data: slice[value_start..value_start + value_size].to_vec(),
        })
        // PinnedPage dropped here — data was copied into ScanRecord.
    }
}

impl Iterator for LogScanIterator<'_> {
    type Item = ScanRecord;

    fn next(&mut self) -> Option<Self::Item> {
        let record_size = self.layout.total_size() as u32;

        loop {
            // Fix up page boundary before checking termination.
            self.align_to_record_boundary();

            // Have we reached the end of the scan range?
            if self.current >= self.end {
                return None;
            }

            // Snapshot the address we're about to read.
            let addr = self.current;

            // Advance past this record for the next iteration regardless of
            // whether we yield it.
            let new_offset = addr.offset().0 + record_size;
            if new_offset >= self.page_size {
                // Record ends exactly at (or would exceed) page boundary →
                // next record is on the next page.
                self.current = LogicalAddress::new(Page(addr.page().0 + 1), Offset(0));
            } else {
                self.current = LogicalAddress::new(addr.page(), Offset(new_offset));
            }

            // Try to read the record from in-memory pages.
            let record = match self.try_read_record(addr) {
                Some(r) => r,
                None => {
                    // Page is not in memory — skip. If in_memory_only mode
                    // was set, the start was already clamped. Otherwise we
                    // just continue scanning forward hoping the next page is
                    // in memory.
                    continue;
                }
            };

            // Apply filter options.
            if !self.should_yield(&record.record_info) {
                continue;
            }

            return Some(record);
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::LogicalAddress;
    use crate::hybrid_log::log_allocator::HybridLogAllocator;
    use crate::hybrid_log::record_ops::LogRecordWriter;
    use crate::record::{RecordInfo, RecordLayout};

    const SECTOR_SIZE: usize = 512;
    const BUFFER_PAGES: usize = 4;
    const MUTABLE_FRAC: f64 = 0.5;

    fn make_allocator() -> HybridLogAllocator {
        HybridLogAllocator::new(BUFFER_PAGES, MUTABLE_FRAC, SECTOR_SIZE)
    }

    /// Helper: write a u64→u64 record and return its address.
    fn write_record(
        alloc: &HybridLogAllocator,
        key: u64,
        value: u64,
        tombstone: bool,
        invalid: bool,
    ) -> LogicalAddress {
        let writer = LogRecordWriter::new(alloc);
        // Use version 1 so the record is not confused with unwritten (zeroed)
        // page memory. A zeroed RecordInfo (version=0, prev=ZERO, no flags)
        // has is_null() == true and would be skipped by the scanner.
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, invalid, tombstone, false);
        writer
            .write_record(&info, &key, &value)
            .expect("write should succeed")
    }

    // ── 1. Empty log (no records written) ───────────────────────────

    #[test]
    fn scan_empty_log() {
        let alloc = make_allocator();
        let start = alloc.begin_address();
        let end = alloc.tail_address();

        let iter = LogScanIterator::new(&alloc, start, end, 8, 8, ScanOptions::default());
        let records: Vec<_> = iter.collect();
        assert!(records.is_empty(), "empty log should yield no records");
    }

    // ── 2. Single record ────────────────────────────────────────────

    #[test]
    fn scan_single_record() {
        let alloc = make_allocator();
        let addr = write_record(&alloc, 42, 999, false, false);

        let start = alloc.begin_address();
        let end = alloc.tail_address();

        let records: Vec<_> =
            LogScanIterator::new(&alloc, start, end, 8, 8, ScanOptions::default()).collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].address, addr);
        assert!(!records[0].record_info.is_tombstone());
        assert!(!records[0].record_info.is_invalid());
    }

    // ── 3. Multiple records ─────────────────────────────────────────

    #[test]
    fn scan_multiple_records() {
        let alloc = make_allocator();
        let mut addrs = Vec::new();
        for i in 0u64..10 {
            addrs.push(write_record(&alloc, i, i * 100, false, false));
        }

        let start = alloc.begin_address();
        let end = alloc.tail_address();

        let records: Vec<_> =
            LogScanIterator::new(&alloc, start, end, 8, 8, ScanOptions::default()).collect();
        assert_eq!(records.len(), 10);
        for (i, rec) in records.iter().enumerate() {
            assert_eq!(rec.address, addrs[i]);
        }
    }

    // ── 4. Forward-only ordering ────────────────────────────────────

    #[test]
    fn scan_yields_forward_order() {
        let alloc = make_allocator();
        for i in 0u64..20 {
            write_record(&alloc, i, i, false, false);
        }

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            ScanOptions::default(),
        )
        .collect();

        for pair in records.windows(2) {
            assert!(
                pair[0].address < pair[1].address,
                "addresses must be strictly increasing"
            );
        }
    }

    // ── 5. Tombstone filtering ──────────────────────────────────────

    #[test]
    fn scan_excludes_tombstones_by_default() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 100, false, false);
        write_record(&alloc, 2, 200, true, false); // tombstone
        write_record(&alloc, 3, 300, false, false);

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            ScanOptions::default(),
        )
        .collect();

        assert_eq!(records.len(), 2, "tombstones should be excluded");
        assert!(!records[0].record_info.is_tombstone());
        assert!(!records[1].record_info.is_tombstone());
    }

    #[test]
    fn scan_includes_tombstones_when_configured() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 100, false, false);
        write_record(&alloc, 2, 200, true, false); // tombstone
        write_record(&alloc, 3, 300, false, false);

        let opts = ScanOptions {
            include_tombstones: true,
            ..ScanOptions::default()
        };

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            opts,
        )
        .collect();

        assert_eq!(records.len(), 3, "tombstones should be included");
        assert!(records[1].record_info.is_tombstone());
    }

    // ── 6. Invalid record filtering ─────────────────────────────────

    #[test]
    fn scan_excludes_invalid_by_default() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 100, false, false);
        write_record(&alloc, 2, 200, false, true); // invalid
        write_record(&alloc, 3, 300, false, false);

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            ScanOptions::default(),
        )
        .collect();

        assert_eq!(records.len(), 2, "invalid records should be excluded");
    }

    #[test]
    fn scan_includes_invalid_when_configured() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 100, false, false);
        write_record(&alloc, 2, 200, false, true); // invalid
        write_record(&alloc, 3, 300, false, false);

        let opts = ScanOptions {
            include_invalid: true,
            ..ScanOptions::default()
        };

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            opts,
        )
        .collect();

        assert_eq!(records.len(), 3, "invalid records should be included");
        assert!(records[1].record_info.is_invalid());
    }

    // ── 7. Combined options: include both tombstones and invalid ─────

    #[test]
    fn scan_all_options_enabled() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 100, false, false); // normal
        write_record(&alloc, 2, 200, true, false); // tombstone
        write_record(&alloc, 3, 300, false, true); // invalid
        write_record(&alloc, 4, 400, true, true); // both

        let opts = ScanOptions {
            include_tombstones: true,
            include_invalid: true,
            ..ScanOptions::default()
        };

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            opts,
        )
        .collect();

        assert_eq!(records.len(), 4, "all records should be included");
    }

    // ── 8. Page boundary crossing ───────────────────────────────────

    #[test]
    #[ignore = "tier-2: fills entire 32MB page (~1.4M records) to test boundary"]
    fn scan_across_page_boundary() {
        let alloc = make_allocator();
        let layout = RecordLayout::compute(8, 8);
        let record_size = layout.total_size() as u32;
        let page_size = 1u32 << OFFSET_BITS;

        // Fill the first page until there's not enough room for another
        // record, then advance to the next page.
        let records_per_page = page_size / record_size;
        let mut addrs = Vec::new();

        for i in 0u64..(records_per_page as u64) {
            match write_record_fallible(&alloc, i, i * 10) {
                Some(addr) => addrs.push(addr),
                None => {
                    // Page boundary hit — advance and retry.
                    alloc.advance_to_next_page();
                    addrs.push(write_record(&alloc, i, i * 10, false, false));
                }
            }
        }

        // Write a few more on the next page.
        for i in records_per_page as u64..(records_per_page as u64 + 5) {
            match write_record_fallible(&alloc, i, i * 10) {
                Some(addr) => addrs.push(addr),
                None => {
                    alloc.advance_to_next_page();
                    addrs.push(write_record(&alloc, i, i * 10, false, false));
                }
            }
        }

        let all_records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            ScanOptions::default(),
        )
        .collect();

        assert_eq!(all_records.len(), addrs.len());

        // Verify ordering across pages.
        for pair in all_records.windows(2) {
            assert!(pair[0].address < pair[1].address);
        }

        // Verify at least two different pages were scanned.
        let pages: std::collections::HashSet<u32> =
            all_records.iter().map(|r| r.address.page().0).collect();
        assert!(
            pages.len() >= 2,
            "expected records on at least 2 pages, got {}",
            pages.len()
        );
    }

    /// Helper that returns `None` on allocation failure instead of panicking.
    fn write_record_fallible(
        alloc: &HybridLogAllocator,
        key: u64,
        value: u64,
    ) -> Option<LogicalAddress> {
        let writer = LogRecordWriter::new(alloc);
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        writer.write_record(&info, &key, &value).ok()
    }

    // ── 9. In-memory-only mode clamps start ─────────────────────────

    #[test]
    fn scan_in_memory_only_clamps_start() {
        let alloc = make_allocator();
        // Write some records; head stays at 0 so all are in memory.
        for i in 0u64..5 {
            write_record(&alloc, i, i, false, false);
        }

        // With in_memory_only, start below head should clamp to head.
        let opts = ScanOptions {
            in_memory_only: true,
            ..ScanOptions::default()
        };

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            LogicalAddress::ZERO,
            alloc.tail_address(),
            8,
            8,
            opts,
        )
        .collect();

        // Head is at 0, so all 5 records should be visible.
        assert_eq!(records.len(), 5);
    }

    // ── 10. Partial range scan ──────────────────────────────────────

    #[test]
    fn scan_partial_range() {
        let alloc = make_allocator();
        let mut addrs = Vec::new();
        for i in 0u64..10 {
            addrs.push(write_record(&alloc, i, i, false, false));
        }

        // Scan only from addr[3] to addr[7] (exclusive).
        let records: Vec<_> =
            LogScanIterator::new(&alloc, addrs[3], addrs[7], 8, 8, ScanOptions::default())
                .collect();

        assert_eq!(records.len(), 4); // addrs[3], [4], [5], [6]
        assert_eq!(records[0].address, addrs[3]);
        assert_eq!(records[3].address, addrs[6]);
    }

    // ── 11. Key/value bytes correctness ─────────────────────────────

    #[test]
    fn scan_record_key_value_bytes() {
        let alloc = make_allocator();
        let key: u64 = 0xDEAD_BEEF;
        let value: u64 = 0xCAFE_BABE;
        write_record(&alloc, key, value, false, false);

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            ScanOptions::default(),
        )
        .collect();

        assert_eq!(records.len(), 1);

        // key_bytes should contain the LE representation of 0xDEAD_BEEF.
        let expected_key = 0xDEAD_BEEFu64.to_le_bytes();
        assert_eq!(records[0].key_bytes(), &expected_key);

        // value_bytes should contain the LE representation of 0xCAFE_BABE.
        let expected_value = 0xCAFE_BABEu64.to_le_bytes();
        assert_eq!(records[0].value_bytes(), &expected_value);
    }

    // ── 12. ScanOptions default values ──────────────────────────────

    #[test]
    fn scan_options_default() {
        let opts = ScanOptions::default();
        assert!(!opts.include_tombstones);
        assert!(!opts.include_invalid);
        assert!(!opts.in_memory_only);
    }

    // ── 13. start == end yields nothing ─────────────────────────────

    #[test]
    fn scan_start_equals_end() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 1, false, false);

        let addr = alloc.begin_address();
        let records: Vec<_> =
            LogScanIterator::new(&alloc, addr, addr, 8, 8, ScanOptions::default()).collect();
        assert!(records.is_empty());
    }

    // ── 14. start > end yields nothing ──────────────────────────────

    #[test]
    fn scan_start_past_end() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 1, false, false);

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.tail_address(),
            alloc.begin_address(),
            8,
            8,
            ScanOptions::default(),
        )
        .collect();
        assert!(records.is_empty());
    }

    // ── 15. Mixed tombstone + invalid — default filters both ────────

    #[test]
    fn scan_default_filters_tombstone_and_invalid() {
        let alloc = make_allocator();
        write_record(&alloc, 1, 1, false, false); // normal
        write_record(&alloc, 2, 2, true, false); // tombstone
        write_record(&alloc, 3, 3, false, true); // invalid
        write_record(&alloc, 4, 4, false, false); // normal

        let records: Vec<_> = LogScanIterator::new(
            &alloc,
            alloc.begin_address(),
            alloc.tail_address(),
            8,
            8,
            ScanOptions::default(),
        )
        .collect();

        assert_eq!(records.len(), 2);
    }
}
