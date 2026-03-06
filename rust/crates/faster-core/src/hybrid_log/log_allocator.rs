//! Hybrid log bump allocator.
//!
//! The [`HybridLogAllocator`] manages a contiguous logical address space using a
//! lock-free bump pointer for appends. It tracks five address boundaries that
//! partition the log into regions:
//!
//! ```text
//! ┌──────────┬──────────────────┬───────────────────┬──────────────────┐
//! │ On disk  │   Read-only      │   Safe read-only   │   Mutable        │
//! │          │   (flushed,      │   (flushing or     │   (in-memory,    │
//! │          │   evictable)     │   flushed)         │   writable)      │
//! ├──────────┼──────────────────┼───────────────────┼──────────────────┤
//! begin      head               read_only            safe_read_only     tail
//! ```
//!
//! **Invariant:** `begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail`
//!
//! The allocator does *not* handle flushing or eviction — those are separate
//! components.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::address::{AtomicLogicalAddress, LogicalAddress, MAX_PAGE, OFFSET_BITS, Offset, Page};

use super::page::{PageState, PageTable};

// ---------------------------------------------------------------------------
// HybridLogAllocator
// ---------------------------------------------------------------------------

/// Bump allocator for the FASTER hybrid log.
///
/// Allocation is lock-free: threads race on a CAS loop over `tail_address`.
/// Page frames are lazily allocated via the underlying [`PageTable`].
pub struct HybridLogAllocator {
    /// The page table backing this allocator.
    page_table: PageTable,

    /// Start of valid data on disk.
    begin_address: AtomicLogicalAddress,
    /// Start of in-memory region (pages below are evicted).
    head_address: AtomicLogicalAddress,
    /// Start of read-only region (below: read-only, above: mutable).
    read_only_address: AtomicLogicalAddress,
    /// All threads have observed the read-only transition up to here.
    safe_read_only_address: AtomicLogicalAddress,
    /// Next allocation point (bump pointer).
    tail_address: AtomicLogicalAddress,

    /// Bytes per page (2^OFFSET_BITS).
    page_size: u32,
    /// Number of mutable pages before the read-only boundary advances.
    mutable_fraction_pages: u32,
    /// Whether the allocator is sealed (no more allocations).
    sealed: AtomicBool,
}

impl HybridLogAllocator {
    /// Create a new allocator.
    ///
    /// - `buffer_size_pages`: number of in-memory page frames (must be a power
    ///   of 2).
    /// - `mutable_fraction`: fraction of the buffer that remains mutable
    ///   (`0.0..1.0`).
    /// - `sector_size`: alignment for page frame allocations.
    pub fn new(buffer_size_pages: usize, mutable_fraction: f64, sector_size: usize) -> Self {
        assert!(
            (0.0..=1.0).contains(&mutable_fraction),
            "mutable_fraction must be in 0.0..=1.0"
        );

        let page_size = 1u32 << OFFSET_BITS;
        let mutable_fraction_pages =
            ((buffer_size_pages as f64 * mutable_fraction).ceil() as u32).max(1);

        let start = LogicalAddress::new(Page(0), Offset(0));

        Self {
            page_table: PageTable::new(buffer_size_pages, page_size as usize, sector_size),
            begin_address: AtomicLogicalAddress::new(start),
            head_address: AtomicLogicalAddress::new(start),
            read_only_address: AtomicLogicalAddress::new(start),
            safe_read_only_address: AtomicLogicalAddress::new(start),
            tail_address: AtomicLogicalAddress::new(start),
            page_size,
            mutable_fraction_pages,
            sealed: AtomicBool::new(false),
        }
    }

    /// Try to allocate `size` bytes in the log.
    ///
    /// Returns the [`LogicalAddress`] where the record should be written.
    /// Returns `None` if the allocator is sealed or if the allocation would
    /// cross a page boundary (the caller must handle page overflow via
    /// [`advance_to_next_page`](Self::advance_to_next_page)).
    ///
    /// This is a lock-free CAS loop on `tail_address`.
    pub fn try_allocate(&self, size: u32) -> Option<LogicalAddress> {
        // C-9: Reject zero-size allocations — they would create overlapping
        // addresses where new_tail == current.
        debug_assert!(size > 0, "try_allocate: size must be > 0");

        if self.sealed.load(Ordering::Acquire) {
            return None;
        }

        loop {
            let current = self.tail_address.load(Ordering::Acquire);
            let current_offset = current.offset().0;
            let new_offset = current_offset + size;

            // Would the allocation cross the page boundary?
            if new_offset > self.page_size {
                return None;
            }

            // Compute the new tail address. If the allocation fills the page
            // exactly (new_offset == page_size), the offset would exceed
            // MAX_OFFSET, so we wrap to the start of the next page.
            let new_tail = if new_offset == self.page_size {
                // SF-7: Guard against page number overflow at MAX_PAGE.
                if current.page().0 >= MAX_PAGE {
                    return None;
                }
                LogicalAddress::new(Page(current.page().0 + 1), Offset(0))
            } else {
                LogicalAddress::new(current.page(), Offset(new_offset))
            };

            match self.tail_address.compare_exchange(
                current,
                new_tail,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Ensure the page frame exists.
                    self.page_table.get_or_allocate_frame(current.page());
                    return Some(current);
                }
                Err(_) => {
                    // Another thread won the race — retry.
                    continue;
                }
            }
        }
    }

    /// Get a raw pointer to the memory at the given logical address.
    ///
    /// The address must be in the in-memory region (between head and tail).
    /// Returns `None` if the page frame is not allocated.
    pub fn get_physical_address(&self, addr: LogicalAddress) -> Option<*mut u8> {
        let frame = self.page_table.get_frame(addr.page())?;
        let offset = addr.offset().0 as usize;

        // SAFETY: `offset` is within the page (bounded by page_size which
        // matches the frame allocation size). The returned pointer is within
        // the frame's allocation.
        Some(unsafe { frame.as_mut_ptr().add(offset) })
    }

    /// Advance the read-only boundary to the current tail address.
    ///
    /// Called when the mutable region grows too large. This seals the
    /// currently-mutable pages so they become read-only.
    pub fn shift_read_only_to_tail(&self) {
        let tail = self.tail_address.load(Ordering::Acquire);
        // Monotonically advance — never move backward.
        loop {
            let current = self.read_only_address.load(Ordering::Acquire);
            if current >= tail {
                break;
            }
            match self.read_only_address.compare_exchange(
                current,
                tail,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
    }

    /// Check if an address is in the mutable region (between safe_read_only
    /// and tail).
    #[inline]
    pub fn is_mutable(&self, addr: LogicalAddress) -> bool {
        let sro = self.safe_read_only_address.load(Ordering::Acquire);
        let tail = self.tail_address.load(Ordering::Acquire);
        addr >= sro && addr < tail
    }

    /// Check if an address is in the safe read-only region (between head and
    /// safe_read_only).
    #[inline]
    pub fn is_safe_read_only(&self, addr: LogicalAddress) -> bool {
        let head = self.head_address.load(Ordering::Acquire);
        let sro = self.safe_read_only_address.load(Ordering::Acquire);
        addr >= head && addr < sro
    }

    /// Check if an address is in memory (between head and tail).
    #[inline]
    pub fn is_in_memory(&self, addr: LogicalAddress) -> bool {
        let head = self.head_address.load(Ordering::Acquire);
        let tail = self.tail_address.load(Ordering::Acquire);
        addr >= head && addr < tail
    }

    /// Get the current tail address.
    #[inline]
    pub fn tail_address(&self) -> LogicalAddress {
        self.tail_address.load(Ordering::Acquire)
    }

    /// Get the current read-only address.
    #[inline]
    pub fn read_only_address(&self) -> LogicalAddress {
        self.read_only_address.load(Ordering::Acquire)
    }

    /// Get the current head address.
    #[inline]
    pub fn head_address(&self) -> LogicalAddress {
        self.head_address.load(Ordering::Acquire)
    }

    /// Get the current begin address.
    #[inline]
    pub fn begin_address(&self) -> LogicalAddress {
        self.begin_address.load(Ordering::Acquire)
    }

    /// Seal the allocator — no more allocations allowed.
    pub fn seal(&self) {
        self.sealed.store(true, Ordering::Release);
    }

    /// Get the page table reference.
    #[inline]
    pub fn page_table(&self) -> &PageTable {
        &self.page_table
    }

    /// Advance to the next page.
    ///
    /// Called when [`try_allocate`](Self::try_allocate) returns `None` due to a
    /// page boundary crossing. Seals the current page and opens the next one.
    ///
    /// Returns the first address on the new page, or `None` if sealed.
    pub fn advance_to_next_page(&self) -> Option<LogicalAddress> {
        if self.sealed.load(Ordering::Acquire) {
            return None;
        }

        loop {
            let current = self.tail_address.load(Ordering::Acquire);
            let current_page = current.page();
            // SF-7: Guard against page number overflow at MAX_PAGE.
            if current_page.0 >= MAX_PAGE {
                return None;
            }
            let next_page = Page(current_page.0 + 1);
            let new_tail = LogicalAddress::new(next_page, Offset(0));

            match self.tail_address.compare_exchange(
                current,
                new_tail,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Seal the old page if its frame exists.
                    if let Some(frame) = self.page_table.get_frame(current_page) {
                        // Best-effort seal: the page may already be in a
                        // later state if another component transitioned it.
                        let _ = frame
                            .state()
                            .try_transition(PageState::Open, PageState::Sealed);
                    }
                    // Ensure the new page frame is allocated and open.
                    self.page_table.get_or_allocate_frame(next_page);
                    return Some(new_tail);
                }
                Err(_) => {
                    // Another thread advanced — retry if we're still on the
                    // same page, otherwise someone else already did the work.
                    let updated = self.tail_address.load(Ordering::Acquire);
                    if updated.page() != current_page {
                        // Already advanced past this page — return the
                        // current tail (beginning of the new page).
                        return Some(updated);
                    }
                    // Still on the same page; retry.
                    continue;
                }
            }
        }
    }

    /// Returns the number of mutable-fraction pages.
    #[inline]
    pub fn mutable_fraction_pages(&self) -> u32 {
        self.mutable_fraction_pages
    }

    /// Returns the page size in bytes.
    #[inline]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Get the current safe read-only address.
    #[inline]
    pub fn safe_read_only_address(&self) -> LogicalAddress {
        self.safe_read_only_address.load(Ordering::Acquire)
    }

    /// Take a consistent snapshot of the current address boundaries.
    ///
    /// Read order: tail first, then safe_read_only, read_only, head, begin.
    /// This ensures tail ≥ head in the snapshot because boundaries advance
    /// monotonically — a stale read is always conservative.
    ///
    /// Acquire ordering suffices on x86-64 where loads cannot reorder with
    /// each other (TSO). On ARM/weakly-ordered architectures the monotonic
    /// advancement invariant still guarantees safety: a stale value is
    /// conservative (never overestimates progress).
    pub fn snapshot(&self) -> super::regions::AddressInfo {
        // C-2: Read tail first so that any concurrent advance of head
        // is captured as head <= tail in the returned snapshot.
        let tail_address = self.tail_address.load(Ordering::Acquire);
        let safe_read_only_address = self.safe_read_only_address.load(Ordering::Acquire);
        let read_only_address = self.read_only_address.load(Ordering::Acquire);
        let head_address = self.head_address.load(Ordering::Acquire);
        let begin_address = self.begin_address.load(Ordering::Acquire);
        super::regions::AddressInfo {
            begin_address,
            head_address,
            read_only_address,
            safe_read_only_address,
            tail_address,
        }
    }

    /// Try to advance `head_address` to a new value.
    ///
    /// Only succeeds if `new_head` > current head (monotonic advance).
    /// Returns the actual head after the attempt.
    pub fn try_advance_head(&self, new_head: LogicalAddress) -> LogicalAddress {
        loop {
            let current = self.head_address.load(Ordering::Acquire);
            if new_head.raw() <= current.raw() {
                return current;
            }
            match self.head_address.compare_exchange(
                current,
                new_head,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return new_head,
                Err(_) => continue,
            }
        }
    }

    /// Try to advance `begin_address` to a new value (for truncation).
    ///
    /// Only succeeds if `new_begin` > current begin (monotonic advance).
    /// Returns the actual begin after the attempt.
    pub fn try_advance_begin(&self, new_begin: LogicalAddress) -> LogicalAddress {
        loop {
            let current = self.begin_address.load(Ordering::Acquire);
            if new_begin.raw() <= current.raw() {
                return current;
            }
            match self.begin_address.compare_exchange(
                current,
                new_begin,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return new_begin,
                Err(_) => continue,
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR_SIZE: usize = 512;
    const BUFFER_PAGES: usize = 4;
    const MUTABLE_FRAC: f64 = 0.5;

    fn make_allocator() -> HybridLogAllocator {
        HybridLogAllocator::new(BUFFER_PAGES, MUTABLE_FRAC, SECTOR_SIZE)
    }

    // 1. Create allocator, verify all addresses are at same starting point
    #[test]
    fn allocator_new_valid_state() {
        let alloc = make_allocator();
        let start = LogicalAddress::new(Page(0), Offset(0));

        assert_eq!(alloc.begin_address(), start);
        assert_eq!(alloc.head_address(), start);
        assert_eq!(alloc.read_only_address(), start);
        assert_eq!(alloc.tail_address(), start);
        assert_eq!(alloc.page_size(), 1u32 << OFFSET_BITS);
        assert!(alloc.mutable_fraction_pages() > 0);
    }

    // 2. Allocate 3 records of 64 bytes, verify addresses are sequential
    #[test]
    fn allocate_returns_sequential_addresses() {
        let alloc = make_allocator();

        let a0 = alloc.try_allocate(64).expect("first allocation");
        let a1 = alloc.try_allocate(64).expect("second allocation");
        let a2 = alloc.try_allocate(64).expect("third allocation");

        assert_eq!(a0, LogicalAddress::new(Page(0), Offset(0)));
        assert_eq!(a1, LogicalAddress::new(Page(0), Offset(64)));
        assert_eq!(a2, LogicalAddress::new(Page(0), Offset(128)));

        // Tail should be at 192
        assert_eq!(
            alloc.tail_address(),
            LogicalAddress::new(Page(0), Offset(192))
        );
    }

    // 3. Fill most of a page, verify next alloc that would cross boundary returns None
    #[test]
    fn allocate_within_page_boundary() {
        let alloc = make_allocator();
        let page_size = alloc.page_size();

        // Allocate almost the entire page (leave 64 bytes)
        let big = page_size - 64;
        let a0 = alloc.try_allocate(big).expect("big allocation");
        assert_eq!(a0.offset(), Offset(0));

        // This should succeed (exactly fits remaining space)
        let a1 = alloc.try_allocate(64).expect("fitting allocation");
        assert_eq!(a1.offset(), Offset(big));

        // Page is now full (tail wrapped to next page) — an allocation
        // that would exceed the *next* page boundary should fail only if
        // it overflows. But a small allocation on the fresh page succeeds.
        // To test a true boundary crossing, partially fill a page and then
        // request more than remaining.
        let alloc2 = make_allocator();
        alloc2.try_allocate(page_size - 32).expect("almost fill");
        // 33 bytes won't fit in the remaining 32
        assert!(alloc2.try_allocate(33).is_none());
    }

    // 4. Trigger page advance, verify tail is on next page
    #[test]
    fn advance_to_next_page() {
        let alloc = make_allocator();
        let page_size = alloc.page_size();

        // Partially fill the page so tail stays on page 0
        alloc.try_allocate(page_size - 128).expect("partial fill");
        // This allocation won't fit — crosses boundary
        assert!(alloc.try_allocate(256).is_none());

        // Advance
        let new_addr = alloc.advance_to_next_page().expect("advance");
        assert_eq!(new_addr, LogicalAddress::new(Page(1), Offset(0)));
        assert_eq!(
            alloc.tail_address(),
            LogicalAddress::new(Page(1), Offset(0))
        );

        // Should be able to allocate on the new page
        let a = alloc.try_allocate(128).expect("alloc on new page");
        assert_eq!(a, LogicalAddress::new(Page(1), Offset(0)));
    }

    // 5. Allocate, get physical addr, write data, read back
    #[test]
    fn get_physical_address_valid() {
        let alloc = make_allocator();
        let addr = alloc.try_allocate(64).expect("allocate");
        let ptr = alloc.get_physical_address(addr).expect("physical addr");

        // SAFETY: We just allocated this region and have exclusive access
        // in this single-threaded test.
        unsafe {
            core::ptr::write(ptr, 0xAB);
            assert_eq!(core::ptr::read(ptr), 0xAB);
        }
    }

    // 6. Query address far beyond tail
    #[test]
    fn get_physical_address_out_of_memory_returns_none() {
        let alloc = make_allocator();
        // Page 100 was never allocated
        let far_addr = LogicalAddress::new(Page(100), Offset(0));
        assert!(alloc.get_physical_address(far_addr).is_none());
    }

    // 7. Verify is_mutable/is_safe_read_only/is_in_memory for various addresses
    #[test]
    fn region_checks() {
        let alloc = make_allocator();

        // Allocate some data
        let a0 = alloc.try_allocate(64).expect("alloc");
        let a1 = alloc.try_allocate(64).expect("alloc");
        let _a2 = alloc.try_allocate(64).expect("alloc");

        // Everything is mutable (read_only == safe_read_only == begin)
        assert!(alloc.is_mutable(a0));
        assert!(alloc.is_mutable(a1));
        assert!(alloc.is_in_memory(a0));
        assert!(alloc.is_in_memory(a1));
        assert!(!alloc.is_safe_read_only(a0));

        // Address at or beyond tail is NOT in memory
        let beyond = alloc.tail_address();
        assert!(!alloc.is_in_memory(beyond));
        assert!(!alloc.is_mutable(beyond));
    }

    // 8. Allocate some data, shift read-only, verify new region boundaries
    #[test]
    fn shift_read_only_to_tail() {
        let alloc = make_allocator();

        let a0 = alloc.try_allocate(64).expect("alloc");
        let _a1 = alloc.try_allocate(64).expect("alloc");

        let tail_before = alloc.tail_address();
        alloc.shift_read_only_to_tail();
        assert_eq!(alloc.read_only_address(), tail_before);

        // a0 is no longer mutable (read_only has advanced past it), but
        // safe_read_only hasn't moved yet so a0 is still considered mutable
        // from the safe_read_only perspective.
        assert!(alloc.is_mutable(a0));
        assert!(alloc.is_in_memory(a0));

        // Manually advance safe_read_only to match read_only for testing
        loop {
            let current = alloc.safe_read_only_address.load(Ordering::Acquire);
            let target = alloc.read_only_address.load(Ordering::Acquire);
            if current >= target {
                break;
            }
            let _ = alloc.safe_read_only_address.compare_exchange(
                current,
                target,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }

        // Now a0 should be safe_read_only, not mutable
        assert!(alloc.is_safe_read_only(a0));
        assert!(!alloc.is_mutable(a0));
        assert!(alloc.is_in_memory(a0));
    }

    // 9. Seal allocator, verify try_allocate returns None
    #[test]
    fn seal_prevents_allocation() {
        let alloc = make_allocator();
        alloc.try_allocate(64).expect("alloc before seal");

        alloc.seal();

        assert!(alloc.try_allocate(64).is_none());
        assert!(alloc.advance_to_next_page().is_none());
    }

    // 10. 4 threads each allocating 1000 records, verify no overlapping addresses
    #[test]
    fn concurrent_allocate() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let alloc = Arc::new(HybridLogAllocator::new(16, 0.5, 512));
        let record_size = 64u32;
        let records_per_thread = 1000;
        let num_threads = 4;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    let mut addrs = Vec::with_capacity(records_per_thread);
                    for _ in 0..records_per_thread {
                        loop {
                            match alloc.try_allocate(record_size) {
                                Some(addr) => {
                                    addrs.push(addr);
                                    break;
                                }
                                None => {
                                    // Page boundary — advance and retry
                                    alloc.advance_to_next_page();
                                }
                            }
                        }
                    }
                    addrs
                })
            })
            .collect();

        let mut all_addrs = HashSet::new();
        for h in handles {
            let addrs = h.join().expect("thread panicked");
            assert_eq!(addrs.len(), records_per_thread);
            for addr in addrs {
                assert!(all_addrs.insert(addr.raw()), "duplicate address: {addr:?}");
            }
        }

        assert_eq!(all_addrs.len(), num_threads * records_per_thread);
    }
}
