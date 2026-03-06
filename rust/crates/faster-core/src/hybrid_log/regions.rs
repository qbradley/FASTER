//! Address region classification and boundary snapshot utilities.
//!
//! The hybrid log is divided into regions based on five monotonically advancing
//! address boundaries. This module provides:
//!
//! - [`AddressRegion`] — an enum classifying where an address falls
//! - [`AddressInfo`] — a point-in-time snapshot of all five boundaries
//! - Page-level utility functions for address arithmetic
//!
//! # Region layout
//!
//! ```text
//! begin          head           read_only     safe_ro       tail
//!   │              │               │             │            │
//!   │  ON DISK     │  READ-ONLY    │  FUZZY       │  MUTABLE   │
//!   │  (evicted)   │  (in memory,  │  (flushing)  │ (writable) │
//!   │              │   immutable)  │              │            │
//! ```

use core::fmt;

use crate::address::{LogicalAddress, Offset, Page};

// ---------------------------------------------------------------------------
// AddressRegion
// ---------------------------------------------------------------------------

/// Which region of the hybrid log an address falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressRegion {
    /// Address is below begin_address — data has been truncated.
    Truncated,
    /// Address is on disk only (between begin and head).
    OnDisk,
    /// Address is in the read-only in-memory region (between head and read_only).
    ReadOnly,
    /// Address is in the fuzzy region — may be flushing (between read_only and safe_read_only).
    FuzzyRegion,
    /// Address is in the mutable region (between safe_read_only and tail).
    Mutable,
    /// Address is beyond the tail — not yet allocated.
    Invalid,
}

impl fmt::Display for AddressRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddressRegion::Truncated => write!(f, "Truncated"),
            AddressRegion::OnDisk => write!(f, "OnDisk"),
            AddressRegion::ReadOnly => write!(f, "ReadOnly"),
            AddressRegion::FuzzyRegion => write!(f, "FuzzyRegion"),
            AddressRegion::Mutable => write!(f, "Mutable"),
            AddressRegion::Invalid => write!(f, "Invalid"),
        }
    }
}

// ---------------------------------------------------------------------------
// AddressInfo
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of the hybrid log's address boundaries.
///
/// Taking a snapshot avoids TOCTOU races when making decisions based on
/// multiple boundaries (e.g., "is this address mutable AND is there room
/// for another page?").
#[derive(Debug, Clone, Copy)]
pub struct AddressInfo {
    /// Start of valid data on disk.
    pub begin_address: LogicalAddress,
    /// Start of in-memory region (pages below are evicted).
    pub head_address: LogicalAddress,
    /// Start of read-only region (below: read-only, above: mutable).
    pub read_only_address: LogicalAddress,
    /// All threads have observed the read-only transition up to here.
    pub safe_read_only_address: LogicalAddress,
    /// Next allocation point (bump pointer).
    pub tail_address: LogicalAddress,
}

impl AddressInfo {
    /// Classify which region an address falls in.
    ///
    /// Boundary addresses belong to the **higher** region, with two exceptions:
    /// - `addr == begin_address` → [`AddressRegion::OnDisk`]
    /// - `addr == tail_address` → [`AddressRegion::Invalid`]
    #[inline]
    pub fn classify(&self, addr: LogicalAddress) -> AddressRegion {
        if addr >= self.tail_address {
            AddressRegion::Invalid
        } else if addr >= self.safe_read_only_address {
            AddressRegion::Mutable
        } else if addr >= self.read_only_address {
            AddressRegion::FuzzyRegion
        } else if addr >= self.head_address {
            AddressRegion::ReadOnly
        } else if addr >= self.begin_address {
            AddressRegion::OnDisk
        } else {
            AddressRegion::Truncated
        }
    }

    /// Number of pages in the mutable region.
    #[inline]
    pub fn mutable_pages(&self) -> u32 {
        self.tail_address
            .page()
            .0
            .saturating_sub(self.safe_read_only_address.page().0)
    }

    /// Number of pages in the read-only in-memory region.
    #[inline]
    pub fn read_only_pages(&self) -> u32 {
        self.read_only_address
            .page()
            .0
            .saturating_sub(self.head_address.page().0)
    }

    /// Number of pages currently in memory (head to tail).
    #[inline]
    pub fn in_memory_pages(&self) -> u32 {
        self.tail_address
            .page()
            .0
            .saturating_sub(self.head_address.page().0)
    }

    /// Total logical pages allocated (begin to tail).
    #[inline]
    pub fn total_pages(&self) -> u32 {
        self.tail_address
            .page()
            .0
            .saturating_sub(self.begin_address.page().0)
    }

    /// Whether the mutable region has grown past the configured limit.
    #[inline]
    pub fn needs_read_only_shift(&self, mutable_fraction_pages: u32) -> bool {
        self.mutable_pages() > mutable_fraction_pages
    }

    /// Whether pages need to be evicted (in-memory exceeds buffer capacity).
    #[inline]
    pub fn needs_eviction(&self, buffer_size_pages: u32) -> bool {
        self.in_memory_pages() > buffer_size_pages
    }

    /// Whether pages in the read-only region need flushing.
    ///
    /// Returns `true` when there is a fuzzy region with unflushed pages
    /// (read_only < safe_read_only) OR when read-only pages exist that
    /// could be evicted after flush (head < read_only).
    #[inline]
    pub fn needs_flush(&self) -> bool {
        self.read_only_address < self.safe_read_only_address
            || self.head_address < self.read_only_address
    }
}

// ---------------------------------------------------------------------------
// Page utility functions
// ---------------------------------------------------------------------------

/// Get the page number for a logical address.
#[inline]
pub fn page_of(addr: LogicalAddress) -> Page {
    addr.page()
}

/// Get the first address on a given page.
#[inline]
pub fn page_start(page: Page) -> LogicalAddress {
    LogicalAddress::new(page, Offset(0))
}

/// Get the last valid offset on a page.
#[inline]
pub fn page_end_offset(page_size: u32) -> Offset {
    Offset(page_size - 1)
}

/// Calculate the number of pages between two addresses (inclusive of partial pages).
///
/// If `from` and `to` are on the same page, returns 1. If `to <= from`, returns 0.
#[inline]
pub fn pages_between(from: LogicalAddress, to: LogicalAddress) -> u32 {
    if to <= from {
        return 0;
    }
    let from_page = from.page().0;
    let to_page = to.page().0;
    if from_page == to_page {
        1
    } else {
        // Include partial pages: from_page to to_page.
        // If `to` has a non-zero offset, we need to_page - from_page + 1,
        // otherwise to_page - from_page is sufficient (to sits at page start).
        let base = to_page - from_page;
        if to.offset().0 > 0 { base + 1 } else { base }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, OFFSET_BITS, Offset, Page};

    /// Helper: create an AddressInfo with the given page-based boundaries.
    /// Each boundary is placed at the start of the given page number.
    fn info_from_pages(
        begin: u32,
        head: u32,
        read_only: u32,
        safe_ro: u32,
        tail: u32,
    ) -> AddressInfo {
        AddressInfo {
            begin_address: LogicalAddress::new(Page(begin), Offset(0)),
            head_address: LogicalAddress::new(Page(head), Offset(0)),
            read_only_address: LogicalAddress::new(Page(read_only), Offset(0)),
            safe_read_only_address: LogicalAddress::new(Page(safe_ro), Offset(0)),
            tail_address: LogicalAddress::new(Page(tail), Offset(0)),
        }
    }

    // 1. classify_truncated — address below begin
    #[test]
    fn classify_truncated() {
        let info = info_from_pages(5, 10, 15, 18, 20);
        let addr = LogicalAddress::new(Page(3), Offset(100));
        assert_eq!(info.classify(addr), AddressRegion::Truncated);
    }

    // 2. classify_on_disk — address between begin and head
    #[test]
    fn classify_on_disk() {
        let info = info_from_pages(5, 10, 15, 18, 20);
        let addr = LogicalAddress::new(Page(7), Offset(512));
        assert_eq!(info.classify(addr), AddressRegion::OnDisk);
    }

    // 3. classify_read_only — address between head and read_only
    #[test]
    fn classify_read_only() {
        let info = info_from_pages(5, 10, 15, 18, 20);
        let addr = LogicalAddress::new(Page(12), Offset(0));
        assert_eq!(info.classify(addr), AddressRegion::ReadOnly);
    }

    // 4. classify_fuzzy — address between read_only and safe_read_only
    #[test]
    fn classify_fuzzy() {
        let info = info_from_pages(5, 10, 15, 18, 20);
        let addr = LogicalAddress::new(Page(16), Offset(64));
        assert_eq!(info.classify(addr), AddressRegion::FuzzyRegion);
    }

    // 5. classify_mutable — address between safe_read_only and tail
    #[test]
    fn classify_mutable() {
        let info = info_from_pages(5, 10, 15, 18, 20);
        let addr = LogicalAddress::new(Page(19), Offset(128));
        assert_eq!(info.classify(addr), AddressRegion::Mutable);
    }

    // 6. classify_invalid — address beyond tail
    #[test]
    fn classify_invalid() {
        let info = info_from_pages(5, 10, 15, 18, 20);
        let addr = LogicalAddress::new(Page(25), Offset(0));
        assert_eq!(info.classify(addr), AddressRegion::Invalid);
    }

    // 7. classify_boundary_cases — address exactly AT each boundary
    #[test]
    fn classify_boundary_cases() {
        let info = info_from_pages(5, 10, 15, 18, 20);

        // At begin → OnDisk (not Truncated)
        let at_begin = LogicalAddress::new(Page(5), Offset(0));
        assert_eq!(info.classify(at_begin), AddressRegion::OnDisk);

        // At head → ReadOnly (not OnDisk)
        let at_head = LogicalAddress::new(Page(10), Offset(0));
        assert_eq!(info.classify(at_head), AddressRegion::ReadOnly);

        // At read_only → FuzzyRegion (not ReadOnly)
        let at_ro = LogicalAddress::new(Page(15), Offset(0));
        assert_eq!(info.classify(at_ro), AddressRegion::FuzzyRegion);

        // At safe_read_only → Mutable (not FuzzyRegion)
        let at_sro = LogicalAddress::new(Page(18), Offset(0));
        assert_eq!(info.classify(at_sro), AddressRegion::Mutable);

        // At tail → Invalid (not Mutable)
        let at_tail = LogicalAddress::new(Page(20), Offset(0));
        assert_eq!(info.classify(at_tail), AddressRegion::Invalid);
    }

    // 8. mutable_pages_count — verify page counting
    #[test]
    fn mutable_pages_count() {
        let info = info_from_pages(0, 2, 5, 8, 12);
        assert_eq!(info.mutable_pages(), 4); // 12 - 8
        assert_eq!(info.read_only_pages(), 3); // 5 - 2
        assert_eq!(info.in_memory_pages(), 10); // 12 - 2
        assert_eq!(info.total_pages(), 12); // 12 - 0
    }

    // 9. needs_read_only_shift — test threshold detection
    #[test]
    fn needs_read_only_shift() {
        // mutable pages = 12 - 8 = 4
        let info = info_from_pages(0, 2, 5, 8, 12);
        assert!(info.needs_read_only_shift(3)); // 4 > 3
        assert!(!info.needs_read_only_shift(4)); // 4 > 4 is false
        assert!(!info.needs_read_only_shift(5)); // 4 > 5 is false
    }

    // 10. needs_eviction — test buffer overflow detection
    #[test]
    fn needs_eviction() {
        // in-memory pages = 12 - 2 = 10
        let info = info_from_pages(0, 2, 5, 8, 12);
        assert!(info.needs_eviction(9)); // 10 > 9
        assert!(!info.needs_eviction(10)); // 10 > 10 is false
        assert!(!info.needs_eviction(16)); // 10 > 16 is false
    }

    // 11. pages_between_same_page — addresses on same page = 1
    #[test]
    fn pages_between_same_page() {
        let a = LogicalAddress::new(Page(5), Offset(100));
        let b = LogicalAddress::new(Page(5), Offset(500));
        assert_eq!(pages_between(a, b), 1);
    }

    // 12. pages_between_different_pages — cross-page counting
    #[test]
    fn pages_between_different_pages() {
        // From page 3 offset 100 to page 7 offset 200: pages 3,4,5,6,7 = 5
        let a = LogicalAddress::new(Page(3), Offset(100));
        let b = LogicalAddress::new(Page(7), Offset(200));
        assert_eq!(pages_between(a, b), 5);

        // From page 3 to page 7 offset 0: pages 3,4,5,6 = 4
        let c = LogicalAddress::new(Page(7), Offset(0));
        assert_eq!(pages_between(a, c), 4);

        // to <= from: 0
        assert_eq!(pages_between(b, a), 0);
    }

    // 13. snapshot_from_allocator — create allocator, allocate, take snapshot
    #[test]
    fn snapshot_from_allocator() {
        use super::super::log_allocator::HybridLogAllocator;

        let alloc = HybridLogAllocator::new(4, 0.5, 512);

        // Allocate some data
        alloc.try_allocate(64).expect("alloc 1");
        alloc.try_allocate(64).expect("alloc 2");
        alloc.try_allocate(64).expect("alloc 3");

        let snap = alloc.snapshot();

        // Begin, head, read_only, safe_read_only should all be at page 0 offset 0
        let start = LogicalAddress::new(Page(0), Offset(0));
        assert_eq!(snap.begin_address, start);
        assert_eq!(snap.head_address, start);
        assert_eq!(snap.read_only_address, start);
        assert_eq!(snap.safe_read_only_address, start);

        // Tail should be at offset 192
        assert_eq!(snap.tail_address, LogicalAddress::new(Page(0), Offset(192)));

        // Everything allocated should classify as Mutable
        let addr = LogicalAddress::new(Page(0), Offset(64));
        assert_eq!(snap.classify(addr), AddressRegion::Mutable);

        // Beyond tail is Invalid
        let beyond = LogicalAddress::new(Page(0), Offset(200));
        assert_eq!(snap.classify(beyond), AddressRegion::Invalid);
    }

    // Additional: needs_flush tests
    #[test]
    fn needs_flush_with_fuzzy_region() {
        // read_only < safe_read_only → fuzzy region exists
        let info = info_from_pages(0, 2, 5, 8, 12);
        assert!(info.needs_flush());
    }

    #[test]
    fn needs_flush_with_read_only_pages() {
        // head < read_only, but read_only == safe_read_only
        let info = info_from_pages(0, 2, 5, 5, 12);
        assert!(info.needs_flush());
    }

    #[test]
    fn needs_flush_nothing_to_flush() {
        // head == read_only == safe_read_only → no flush needed
        let info = info_from_pages(0, 5, 5, 5, 12);
        assert!(!info.needs_flush());
    }

    // page utility function tests
    #[test]
    fn page_of_returns_page() {
        let addr = LogicalAddress::new(Page(42), Offset(1024));
        assert_eq!(page_of(addr), Page(42));
    }

    #[test]
    fn page_start_returns_first_address() {
        let addr = page_start(Page(7));
        assert_eq!(addr.page(), Page(7));
        assert_eq!(addr.offset(), Offset(0));
    }

    #[test]
    fn page_end_offset_returns_last_valid() {
        let page_size = 1u32 << OFFSET_BITS;
        let end = page_end_offset(page_size);
        assert_eq!(end, Offset(page_size - 1));
    }

    #[test]
    fn display_address_region() {
        assert_eq!(format!("{}", AddressRegion::Truncated), "Truncated");
        assert_eq!(format!("{}", AddressRegion::OnDisk), "OnDisk");
        assert_eq!(format!("{}", AddressRegion::ReadOnly), "ReadOnly");
        assert_eq!(format!("{}", AddressRegion::FuzzyRegion), "FuzzyRegion");
        assert_eq!(format!("{}", AddressRegion::Mutable), "Mutable");
        assert_eq!(format!("{}", AddressRegion::Invalid), "Invalid");
    }

    // Edge: all boundaries at the same address
    #[test]
    fn all_boundaries_same() {
        let info = info_from_pages(0, 0, 0, 0, 0);
        // Everything at page 0 offset 0 is at the tail → Invalid
        let addr = LogicalAddress::new(Page(0), Offset(0));
        assert_eq!(info.classify(addr), AddressRegion::Invalid);
        assert_eq!(info.mutable_pages(), 0);
        assert_eq!(info.in_memory_pages(), 0);
    }

    #[test]
    fn pages_between_equal_addresses() {
        let a = LogicalAddress::new(Page(5), Offset(100));
        assert_eq!(pages_between(a, a), 0);
    }
}
