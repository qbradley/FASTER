//! Page eviction manager for the hybrid log.
//!
//! The [`PageEvictor`] reclaims in-memory pages after they have been flushed to
//! disk. Eviction frees physical memory by transitioning pages from `Flushed`
//! to `Evicted` and advancing the `head_address` boundary so the circular
//! buffer slot can be reused.
//!
//! # Invariants
//!
//! - Only `Flushed` pages can be evicted (`Flushed → Evicted` transition).
//! - `head_address` only advances when **all** pages below the new head are
//!   confirmed `Flushed` or `Evicted` (no gaps).
//! - Eviction is best-effort: pages that aren't `Flushed` yet are skipped.
//!
//! # Usage
//!
//! ```text
//! let evictor = PageEvictor::new(EvictionPolicy::default(), page_size);
//! if evictor.needs_eviction(&allocator.snapshot()) {
//!     evictor.evict_pages(&allocator);
//! }
//! ```

use crate::sync::Ordering;

use crate::address::{LogicalAddress, Offset, Page};
use crate::device::Device;

use super::log_allocator::HybridLogAllocator;
use super::page::{PageState, PageTable};
use super::regions::AddressInfo;

// ---------------------------------------------------------------------------
// EvictionPolicy
// ---------------------------------------------------------------------------

/// Policy for when and how aggressively to evict pages.
#[derive(Debug, Clone)]
pub struct EvictionPolicy {
    /// Maximum number of pages to keep in memory before triggering eviction.
    /// Should be ≤ buffer_size to prevent circular buffer overflow.
    pub max_in_memory_pages: u32,
    /// Number of pages to evict in a single batch (amortize eviction cost).
    pub eviction_batch_size: u32,
}

impl Default for EvictionPolicy {
    fn default() -> Self {
        Self {
            max_in_memory_pages: 256,
            eviction_batch_size: 4,
        }
    }
}

// ---------------------------------------------------------------------------
// PageEvictor
// ---------------------------------------------------------------------------

/// Manages page eviction from the hybrid log's in-memory buffer.
///
/// The evictor scans pages starting from the current `head_address`, evicts
/// those in `Flushed` state, and advances the head boundary so the circular
/// buffer can reuse the freed frame slots.
pub struct PageEvictor {
    /// Eviction policy.
    policy: EvictionPolicy,
    /// Page size in bytes (used for device offset computation).
    page_size: u32,
}

impl PageEvictor {
    /// Create a new `PageEvictor` with the given policy and page size.
    pub fn new(policy: EvictionPolicy, page_size: u32) -> Self {
        Self { policy, page_size }
    }

    /// Check if eviction is needed based on current address boundaries.
    #[inline]
    pub fn needs_eviction(&self, info: &AddressInfo) -> bool {
        info.in_memory_pages() > self.policy.max_in_memory_pages
    }

    /// Evict pages starting from `head_address`, advancing head forward.
    ///
    /// Only evicts pages that are in `Flushed` state. Stops after
    /// `eviction_batch_size` pages or when a non-flushed page is encountered.
    /// Returns the number of pages successfully evicted.
    pub fn evict_pages(&self, allocator: &HybridLogAllocator) -> u32 {
        let info = allocator.snapshot();
        let page_table = allocator.page_table();

        let head_page = info.head_address.page().0;

        let mut evicted = 0u32;

        for i in 0..self.policy.eviction_batch_size {
            let page_num = head_page + i;
            let page = Page(page_num);
            let page_start = LogicalAddress::new(page, Offset(0));

            // Don't evict past read_only boundary.
            if page_start.raw() >= info.read_only_address.raw() {
                break;
            }

            if self.try_evict_page(page, page_table) {
                evicted += 1;
            } else {
                break;
            }
        }

        // Advance head to cover all contiguously evicted pages.
        if evicted > 0 {
            self.advance_head(allocator);
        }

        evicted
    }

    /// Try to evict a single page. Returns `true` if evicted.
    ///
    /// The page must be in `Flushed` state. The transition is:
    /// `Flushed → Evicted` (via [`PageTable::try_evict_frame`]).
    fn try_evict_page(&self, page: Page, page_table: &PageTable) -> bool {
        match page_table.get_frame(page) {
            Some(frame) => {
                let state = frame.state().load(Ordering::Acquire);
                if state == PageState::Flushed {
                    // try_evict_frame transitions to Evicted and frees the frame.
                    page_table.try_evict_frame(page).is_some()
                } else {
                    false
                }
            }
            None => {
                // Frame already evicted/null — consider it done.
                true
            }
        }
    }

    /// Advance `head_address` to reflect evicted pages.
    ///
    /// Scans forward from the current head until finding a page that is
    /// neither `Flushed` nor `Evicted` (and whose frame is not null).
    /// The head only advances contiguously — no gaps.
    fn advance_head(&self, allocator: &HybridLogAllocator) -> LogicalAddress {
        let current_head = allocator.head_address();
        let read_only = allocator.read_only_address();
        let page_table = allocator.page_table();

        let mut new_head_page = current_head.page().0;

        loop {
            let page = Page(new_head_page);
            let page_start = LogicalAddress::new(page, Offset(0));

            // Don't advance past read_only.
            if page_start.raw() >= read_only.raw() {
                break;
            }

            match page_table.get_frame(page) {
                Some(frame) => {
                    let state = frame.state().load(Ordering::Acquire);
                    if state == PageState::Flushed || state == PageState::Evicted {
                        new_head_page += 1;
                    } else {
                        break;
                    }
                }
                None => {
                    // Frame already evicted (null pointer) — can advance past it.
                    new_head_page += 1;
                }
            }
        }

        let new_head = LogicalAddress::new(Page(new_head_page), Offset(0));
        allocator.try_advance_head(new_head)
    }

    /// Evict pages and optionally truncate the device.
    ///
    /// Truncation deletes device data that is no longer needed (below the new
    /// begin_address). This saves disk space but is not correctness-critical.
    pub fn evict_and_truncate(&self, allocator: &HybridLogAllocator, device: &dyn Device) -> u32 {
        let evicted = self.evict_pages(allocator);

        if evicted > 0 {
            let head = allocator.head_address();
            // Advance begin_address to match head (data below is evicted).
            let new_begin = allocator.try_advance_begin(head);
            // Truncate device up to the new begin offset.
            let truncate_offset = u64::from(new_begin.page().0) * u64::from(self.page_size);
            device.truncate_until(truncate_offset);
        }

        evicted
    }

    /// Get the eviction policy.
    #[inline]
    pub fn policy(&self) -> &EvictionPolicy {
        &self.policy
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};
    use crate::device::InMemoryDevice;
    use crate::hybrid_log::log_allocator::HybridLogAllocator;
    use crate::hybrid_log::page::PageState;
    use crate::hybrid_log::regions::AddressInfo;

    const SECTOR_SIZE: usize = 512;
    const BUFFER_PAGES: usize = 8;
    const MUTABLE_FRAC: f64 = 0.5;

    fn make_allocator() -> HybridLogAllocator {
        HybridLogAllocator::new(BUFFER_PAGES, MUTABLE_FRAC, SECTOR_SIZE)
    }

    /// Helper: create an AddressInfo snapshot with page-based boundaries.
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

    /// Helper: fill pages and advance them through the lifecycle so
    /// pages 0..num_pages exist in the page table. Returns the allocator.
    fn fill_pages(num_pages: u32) -> HybridLogAllocator {
        let alloc = make_allocator();
        let page_size = alloc.page_size();
        for _ in 0..num_pages {
            // Fill the page and advance to the next one.
            alloc.try_allocate(page_size - 64).expect("fill alloc");
            alloc.advance_to_next_page();
        }
        alloc
    }

    // 1. needs_eviction_when_full
    #[test]
    fn needs_eviction_when_full() {
        let policy = EvictionPolicy {
            max_in_memory_pages: 4,
            eviction_batch_size: 2,
        };
        let evictor = PageEvictor::new(policy, 1 << 25);

        // 6 in-memory pages > max 4
        let info = info_from_pages(0, 2, 5, 6, 8);
        assert!(evictor.needs_eviction(&info));
    }

    // 2. no_eviction_when_below_limit
    #[test]
    fn no_eviction_when_below_limit() {
        let policy = EvictionPolicy {
            max_in_memory_pages: 10,
            eviction_batch_size: 2,
        };
        let evictor = PageEvictor::new(policy, 1 << 25);

        // 6 in-memory pages ≤ max 10
        let info = info_from_pages(0, 2, 5, 6, 8);
        assert!(!evictor.needs_eviction(&info));
    }

    // 3. evict_flushed_page — set page to Flushed, evict, verify Evicted
    #[test]
    fn evict_flushed_page() {
        let alloc = fill_pages(4);
        let page_table = alloc.page_table();

        // Transition page 0: Open → Sealed → Flushing → Flushed
        let frame = page_table.get_frame(Page(0)).expect("page 0 exists");
        // Page 0 was sealed by advance_to_next_page, so it should be Sealed.
        let state = frame.state().load(Ordering::Acquire);
        if state == PageState::Open {
            frame
                .state()
                .try_transition(PageState::Open, PageState::Sealed);
        }
        frame
            .state()
            .try_transition(PageState::Sealed, PageState::Flushing);
        frame
            .state()
            .try_transition(PageState::Flushing, PageState::Flushed);

        // Advance read_only past page 0 so eviction is allowed.
        alloc.shift_read_only_to_tail();

        let policy = EvictionPolicy {
            max_in_memory_pages: 1,
            eviction_batch_size: 1,
        };
        let evictor = PageEvictor::new(policy, alloc.page_size());

        let evicted = evictor.evict_pages(&alloc);
        assert_eq!(evicted, 1);

        // Frame should be gone (evicted and freed).
        assert!(page_table.get_frame(Page(0)).is_none());
    }

    // 4. evict_skips_non_flushed — Sealed page should not be evicted
    #[test]
    fn evict_skips_non_flushed() {
        let alloc = fill_pages(4);
        let page_table = alloc.page_table();

        // Page 0 is Sealed (set by advance_to_next_page), NOT Flushed.
        let frame = page_table.get_frame(Page(0)).expect("page 0 exists");
        let state = frame.state().load(Ordering::Acquire);
        // Ensure it's Sealed.
        if state == PageState::Open {
            frame
                .state()
                .try_transition(PageState::Open, PageState::Sealed);
        }
        assert_eq!(
            page_table
                .get_frame(Page(0))
                .unwrap()
                .state()
                .load(Ordering::Acquire),
            PageState::Sealed
        );

        alloc.shift_read_only_to_tail();

        let policy = EvictionPolicy {
            max_in_memory_pages: 1,
            eviction_batch_size: 4,
        };
        let evictor = PageEvictor::new(policy, alloc.page_size());

        let evicted = evictor.evict_pages(&alloc);
        assert_eq!(evicted, 0);

        // Frame should still exist.
        assert!(page_table.get_frame(Page(0)).is_some());
    }

    // 5. evict_multiple_pages — flush and evict a batch
    #[test]
    fn evict_multiple_pages() {
        let alloc = fill_pages(6);
        let page_table = alloc.page_table();

        // Flush pages 0, 1, 2.
        for p in 0..3 {
            let frame = page_table.get_frame(Page(p)).expect("page exists");
            let state = frame.state().load(Ordering::Acquire);
            if state == PageState::Open {
                frame
                    .state()
                    .try_transition(PageState::Open, PageState::Sealed);
            }
            frame
                .state()
                .try_transition(PageState::Sealed, PageState::Flushing);
            frame
                .state()
                .try_transition(PageState::Flushing, PageState::Flushed);
        }

        alloc.shift_read_only_to_tail();

        let policy = EvictionPolicy {
            max_in_memory_pages: 1,
            eviction_batch_size: 4,
        };
        let evictor = PageEvictor::new(policy, alloc.page_size());

        let evicted = evictor.evict_pages(&alloc);
        assert_eq!(evicted, 3);

        // All 3 pages should be evicted.
        for p in 0..3 {
            assert!(page_table.get_frame(Page(p)).is_none());
        }
    }

    // 6. advance_head_stops_at_unflushed
    #[test]
    fn advance_head_stops_at_unflushed() {
        let alloc = fill_pages(6);
        let page_table = alloc.page_table();

        // Flush pages 0 and 1.
        for p in 0..2 {
            let frame = page_table.get_frame(Page(p)).expect("page exists");
            let state = frame.state().load(Ordering::Acquire);
            if state == PageState::Open {
                frame
                    .state()
                    .try_transition(PageState::Open, PageState::Sealed);
            }
            frame
                .state()
                .try_transition(PageState::Sealed, PageState::Flushing);
            frame
                .state()
                .try_transition(PageState::Flushing, PageState::Flushed);
        }

        // Page 2 stays Sealed (unflushed).
        let frame2 = page_table.get_frame(Page(2)).expect("page 2 exists");
        let state2 = frame2.state().load(Ordering::Acquire);
        if state2 == PageState::Open {
            frame2
                .state()
                .try_transition(PageState::Open, PageState::Sealed);
        }

        alloc.shift_read_only_to_tail();

        let policy = EvictionPolicy {
            max_in_memory_pages: 1,
            eviction_batch_size: 8,
        };
        let evictor = PageEvictor::new(policy, alloc.page_size());

        let evicted = evictor.evict_pages(&alloc);
        assert_eq!(evicted, 2);

        // Head should advance to page 2, not further.
        let head = alloc.head_address();
        assert_eq!(head.page(), Page(2));
    }

    // 7. evict_and_truncate_with_device
    #[test]
    fn evict_and_truncate_with_device() {
        let alloc = fill_pages(4);
        let page_table = alloc.page_table();

        // Write some data to the device so truncation has something to work with.
        let device = InMemoryDevice::new();
        let page_size = alloc.page_size();
        let data = vec![0xABu8; page_size as usize];
        device.write_sync(0, &data).expect("write page 0 to device");
        device
            .write_sync(page_size as u64, &data)
            .expect("write page 1 to device");

        // Flush pages 0 and 1.
        for p in 0..2 {
            let frame = page_table.get_frame(Page(p)).expect("page exists");
            let state = frame.state().load(Ordering::Acquire);
            if state == PageState::Open {
                frame
                    .state()
                    .try_transition(PageState::Open, PageState::Sealed);
            }
            frame
                .state()
                .try_transition(PageState::Sealed, PageState::Flushing);
            frame
                .state()
                .try_transition(PageState::Flushing, PageState::Flushed);
        }

        alloc.shift_read_only_to_tail();

        let policy = EvictionPolicy {
            max_in_memory_pages: 1,
            eviction_batch_size: 4,
        };
        let evictor = PageEvictor::new(policy, page_size);

        let evicted = evictor.evict_and_truncate(&alloc, &device);
        assert_eq!(evicted, 2);

        // begin_address should have advanced.
        let begin = alloc.begin_address();
        assert!(begin.page().0 >= 2);

        // The truncated region on the device should be zeroed.
        let mut buf = vec![0xFFu8; page_size as usize];
        device.read_sync(0, &mut buf).expect("read back");
        assert!(
            buf.iter().all(|&b| b == 0),
            "truncated region should be zeroed"
        );
    }

    // 8. try_advance_head_monotonic
    #[test]
    fn try_advance_head_monotonic() {
        let alloc = make_allocator();

        let addr5 = LogicalAddress::new(Page(5), Offset(0));
        let addr3 = LogicalAddress::new(Page(3), Offset(0));
        let addr8 = LogicalAddress::new(Page(8), Offset(0));

        // Advance to page 5.
        let result = alloc.try_advance_head(addr5);
        assert_eq!(result, addr5);
        assert_eq!(alloc.head_address(), addr5);

        // Try to go backward to page 3 — should not move.
        let result = alloc.try_advance_head(addr3);
        assert_eq!(result, addr5);
        assert_eq!(alloc.head_address(), addr5);

        // Advance forward to page 8.
        let result = alloc.try_advance_head(addr8);
        assert_eq!(result, addr8);
        assert_eq!(alloc.head_address(), addr8);
    }

    // Additional: try_advance_begin_monotonic
    #[test]
    fn try_advance_begin_monotonic() {
        let alloc = make_allocator();

        let addr2 = LogicalAddress::new(Page(2), Offset(0));
        let addr1 = LogicalAddress::new(Page(1), Offset(0));
        let addr4 = LogicalAddress::new(Page(4), Offset(0));

        let result = alloc.try_advance_begin(addr2);
        assert_eq!(result, addr2);
        assert_eq!(alloc.begin_address(), addr2);

        // Backward — stays at 2.
        let result = alloc.try_advance_begin(addr1);
        assert_eq!(result, addr2);

        // Forward.
        let result = alloc.try_advance_begin(addr4);
        assert_eq!(result, addr4);
        assert_eq!(alloc.begin_address(), addr4);
    }

    // Additional: default policy
    #[test]
    fn default_policy_values() {
        let policy = EvictionPolicy::default();
        assert_eq!(policy.max_in_memory_pages, 256);
        assert_eq!(policy.eviction_batch_size, 4);
    }

    // Additional: policy accessor
    #[test]
    fn evictor_policy_accessor() {
        let policy = EvictionPolicy {
            max_in_memory_pages: 16,
            eviction_batch_size: 2,
        };
        let evictor = PageEvictor::new(policy.clone(), 4096);
        assert_eq!(evictor.policy().max_in_memory_pages, 16);
        assert_eq!(evictor.policy().eviction_batch_size, 2);
    }
}
