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
use crate::device::Device;
use crate::record::RECORD_HEADER_SIZE;
use crate::recovery::log_recovery::LogRecoveryResult;
use crate::sync::Arc;

use super::page::{PageState, PageTable};
use super::record_ops::MutableRecordAccessor;

// ---------------------------------------------------------------------------
// HybridLogAllocator
// ---------------------------------------------------------------------------

/// Bump allocator for the FASTER hybrid log.
///
/// Allocation is lock-free: threads race on a CAS loop over `tail_address`.
/// Page frames are lazily allocated via the underlying [`PageTable`].
pub struct HybridLogAllocator {
    /// The page table backing this allocator.
    ///
    /// Wrapped in `Arc` so that flush callbacks can hold a strong reference,
    /// guaranteeing the table outlives any in-flight I/O without relying on
    /// struct field drop order.
    page_table: Arc<PageTable>,

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

    /// How far the log has been flushed to disk (monotonically advancing).
    flushed_until_address: AtomicLogicalAddress,

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
            page_table: Arc::new(PageTable::new(
                buffer_size_pages,
                page_size as usize,
                sector_size,
            )),
            begin_address: AtomicLogicalAddress::new(start),
            head_address: AtomicLogicalAddress::new(start),
            read_only_address: AtomicLogicalAddress::new(start),
            safe_read_only_address: AtomicLogicalAddress::new(start),
            tail_address: AtomicLogicalAddress::new(start),
            flushed_until_address: AtomicLogicalAddress::new(start),
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
                // SF-10: Prevent tail from lapping head by buffer_size.
                let next_page = current.page().0 + 1;
                let head_page = self.head_address().page().0;
                if (next_page.wrapping_sub(head_page) as usize) >= self.page_table.buffer_size() {
                    return None;
                }
                LogicalAddress::new(Page(next_page), Offset(0))
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

                    // If this allocation filled the page exactly, the tail
                    // moved to the next page. Seal the old page (Open →
                    // Sealed) and ensure the new page frame is ready.
                    if new_offset == self.page_size {
                        if let Some(frame) = self.page_table.get_frame(current.page()) {
                            let _ = frame
                                .state()
                                .try_transition(PageState::Open, PageState::Sealed);
                        }
                        self.page_table
                            .get_or_allocate_frame(Page(current.page().0 + 1));
                    }

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
    /// Returns `None` if the page is below `head_address` (evicted) or if
    /// the page frame is not allocated.
    ///
    /// The head-address check prevents a circular-buffer ABA hazard: after
    /// eviction, the frame slot (`page % buffer_size`) may be recycled for
    /// a higher-numbered page. Without this guard, callers would silently
    /// read data from the *wrong* page.
    ///
    /// **Note:** This returns a raw pointer with no eviction protection.
    /// For read paths that may race with eviction, prefer [`pin_page`]
    /// which returns a [`PinnedPage`] RAII guard that prevents eviction
    /// while held.
    ///
    /// [`pin_page`]: HybridLogAllocator::pin_page
    /// [`PinnedPage`]: super::page::PinnedPage
    pub fn get_physical_address(&self, addr: LogicalAddress) -> Option<*mut u8> {
        // Bounds check: reject addresses below head — their frame slots
        // may have been recycled for newer pages (ABA in circular buffer).
        let head = self.head_address.load(Ordering::Acquire);
        if addr.raw() < head.raw() {
            return None;
        }

        let frame = self.page_table.get_frame(addr.page())?;
        let offset = addr.offset().0 as usize;

        // Bounds check: offset must be within the page frame.
        let page_size = frame.size();
        if offset >= page_size {
            return None;
        }

        // SAFETY: `offset` is within bounds (checked above). The returned
        // pointer is within the frame's allocation.
        Some(unsafe { frame.as_mut_ptr().add(offset) })
    }

    /// Pin the page containing the given logical address, preventing eviction.
    ///
    /// Returns a [`PinnedPage`] RAII guard that holds an incremented pin
    /// count on the page. While any pin is held, [`PageTable::try_evict_frame`]
    /// will skip this page, guaranteeing the frame memory remains valid.
    ///
    /// Returns `None` if:
    /// - The address is below `head_address` (page already evicted).
    /// - The page frame is not allocated or is in `Free`/`Evicted` state.
    ///
    /// [`PinnedPage`]: super::page::PinnedPage
    /// [`PageTable::try_evict_frame`]: super::page::PageTable::try_evict_frame
    pub fn pin_page(&self, addr: LogicalAddress) -> Option<super::page::PinnedPage<'_>> {
        let head = self.head_address.load(Ordering::Acquire);
        if addr.raw() < head.raw() {
            return None;
        }
        self.page_table.pin_page(addr.page())
    }

    /// Returns a bounds-checked [`MutableRecordAccessor`] for a record in
    /// the mutable region at `addr`.
    ///
    /// The accessor's lifetime is tied to this allocator reference,
    /// preventing the raw pointer from outliving the backing page frame.
    ///
    /// # Bounds checking
    ///
    /// - The address must be above `head_address` (not evicted).
    /// - `record_size` must not exceed the remaining page space.
    /// - `record_size` must be at least `RECORD_HEADER_SIZE`.
    ///
    /// Returns `None` if:
    /// - The address is below head (page already evicted or recycled).
    /// - `record_size` exceeds the remaining space in the page.
    /// - `record_size` is smaller than `RECORD_HEADER_SIZE`.
    pub fn mutable_record_at(
        &self,
        addr: LogicalAddress,
        record_size: u32,
    ) -> Option<MutableRecordAccessor<'_>> {
        let ptr = self.get_physical_address(addr)?;

        // Bounds check: record must fit within the page.
        let page_size = 1u32 << OFFSET_BITS;
        let offset = addr.offset().0;
        let remaining = page_size.saturating_sub(offset);

        // Reject records that don't fit or are too small for a header.
        if record_size > remaining || (record_size as usize) < RECORD_HEADER_SIZE {
            debug_assert!(
                record_size <= remaining,
                "mutable_record_at: record_size {} exceeds page remainder {} at offset {}",
                record_size,
                remaining,
                offset,
            );
            debug_assert!(
                record_size as usize >= RECORD_HEADER_SIZE,
                "mutable_record_at: record_size {} smaller than header ({})",
                record_size,
                RECORD_HEADER_SIZE,
            );
            return None;
        }

        // SAFETY:
        // - `ptr` is from `get_physical_address`: valid, 8-byte aligned,
        //   within a live page frame, above head_address.
        // - `record_size <= remaining`: cannot write past page boundary.
        // - Accessor lifetime tied to `&self`: cannot outlive allocator.
        Some(unsafe { MutableRecordAccessor::new(ptr, record_size) })
    }

    /// Advance the read-only boundary to the current tail address.
    ///
    /// Called when the mutable region grows too large. This seals the
    /// currently-mutable pages so they become read-only.
    ///
    /// Also advances `safe_read_only_address` to match. In a future
    /// multi-threaded implementation this should happen via an epoch
    /// callback (so all threads have observed the new boundary before
    /// it becomes "safe"). For now the immediate advance is correct for
    /// single-threaded callers.
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

        // Advance safe_read_only to match. See doc comment above.
        loop {
            let current = self.safe_read_only_address.load(Ordering::Acquire);
            if current >= tail {
                break;
            }
            match self.safe_read_only_address.compare_exchange(
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

    /// Get a cloneable `Arc` handle to the page table.
    ///
    /// Used by flush callbacks that need to keep the page table alive
    /// independently of the allocator's lifetime.
    #[inline]
    pub fn page_table_arc(&self) -> &Arc<PageTable> {
        &self.page_table
    }

    /// Advance to the next page.
    ///
    /// Called when [`try_allocate`](Self::try_allocate) returns `None` due to a
    /// page boundary crossing. Seals the current page and opens the next one.
    ///
    /// Returns the first address on the new page, or `None` if sealed or if
    /// the circular page buffer is full (tail would lap head).
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

            // SF-10: Prevent the tail from lapping the head in the
            // circular page buffer.  If the number of in-memory pages
            // would exceed buffer_size, the caller must run maintenance
            // (flush + evict) before retrying.
            let head_page = self.head_address.load(Ordering::Acquire).page().0;
            let buffer_size = self.page_table.buffer_size() as u32;
            if next_page.0.saturating_sub(head_page) >= buffer_size {
                return None;
            }

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

    /// Get the current flushed-until address.
    ///
    /// All log data below this address has been durably written to disk.
    /// Advances monotonically as page flushes complete.
    #[inline]
    pub fn flushed_until_address(&self) -> LogicalAddress {
        self.flushed_until_address.load(Ordering::Acquire)
    }

    /// Try to advance `flushed_until_address` to a new value.
    ///
    /// Only succeeds if `new_flushed` > current flushed-until (monotonic
    /// advance). Returns the actual flushed-until address after the attempt.
    pub fn try_advance_flushed_until(&self, new_flushed: LogicalAddress) -> LogicalAddress {
        loop {
            let current = self.flushed_until_address.load(Ordering::Acquire);
            if new_flushed.raw() <= current.raw() {
                return current;
            }
            match self.flushed_until_address.compare_exchange(
                current,
                new_flushed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return new_flushed,
                Err(_) => continue,
            }
        }
    }

    /// Restore all address boundaries from a [`LogRecoveryResult`].
    ///
    /// Must be called **before** any concurrent access (i.e. before sessions
    /// are created). Sets all six atomic address boundaries to match the
    /// recovered state and clears the sealed flag.
    pub fn restore_from_recovery(&self, result: &LogRecoveryResult) {
        self.begin_address
            .store(result.begin_address, Ordering::Release);
        self.head_address
            .store(result.head_address, Ordering::Release);
        self.read_only_address
            .store(result.read_only_address, Ordering::Release);
        self.safe_read_only_address
            .store(result.read_only_address, Ordering::Release);
        self.tail_address
            .store(result.tail_address, Ordering::Release);
        self.flushed_until_address
            .store(result.flushed_until, Ordering::Release);
        self.sealed.store(false, Ordering::Release);
    }

    /// Load pages from a storage device into in-memory page frames.
    ///
    /// Reads pages between `head` (inclusive) and `tail` (inclusive page)
    /// from the device and marks them as [`PageState::Closed`]. This is
    /// used during recovery so that reads work immediately without going
    /// through the pending I/O path.
    ///
    /// Returns the number of pages loaded.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if any `device.read_sync()` call fails.
    pub fn load_pages_from_device(
        &self,
        device: &dyn Device,
        head: LogicalAddress,
        tail: LogicalAddress,
    ) -> Result<u32, std::io::Error> {
        let page_size = self.page_size as u64;
        let first_page = head.page().0;
        let last_page = tail.page().0;
        let mut pages_loaded: u32 = 0;

        for p in first_page..=last_page {
            let frame = self.page_table.get_or_allocate_frame(Page(p));

            // SAFETY: We are the only accessor during recovery (no concurrent
            // sessions). The frame was just allocated or recycled by
            // get_or_allocate_frame, so we have exclusive logical access.
            let buf = unsafe { core::slice::from_raw_parts_mut(frame.as_mut_ptr(), frame.size()) };
            let device_offset = p as u64 * page_size;
            device.read_sync(device_offset, buf)?;

            // Mark recovered pages as Flushed (they are on disk and
            // read-only — not open for new writes).
            frame.state().store(PageState::Flushed, Ordering::Release);
            pages_loaded += 1;
        }

        Ok(pages_loaded)
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

        // After shift_read_only_to_tail, both read_only and safe_read_only
        // have advanced past a0 (safe_read_only is now advanced immediately
        // rather than waiting for an epoch callback).
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
