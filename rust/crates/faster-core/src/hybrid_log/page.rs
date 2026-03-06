//! Page frame allocation, lifecycle management, and page table.
//!
//! A [`PageFrame`] is a sector-aligned slab of memory backing one hybrid log
//! page (default 32 MB = 2^25 bytes). The [`PageTable`] is a power-of-two
//! circular buffer that maps logical [`Page`] numbers to physical frames via
//! atomic pointers, enabling lock-free concurrent access.
//!
//! # Page lifecycle
//!
//! ```text
//! Free ──► Open ──► Sealed ──► Flushing ──► Flushed ──► Evicted ──► Free (recycle)
//! ```
//!
//! Transitions are performed atomically via [`AtomicPageState::try_transition`].

use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, Ordering};
use std::alloc::Layout;

use crate::address::Page;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default sector alignment for page frame allocations (512 bytes).
pub const DEFAULT_SECTOR_SIZE: usize = 512;

/// Default page size (2^25 = 32 MB), matching `address::OFFSET_BITS`.
pub const DEFAULT_PAGE_SIZE: usize = 1 << crate::address::OFFSET_BITS;

// ---------------------------------------------------------------------------
// PageState — lifecycle state machine
// ---------------------------------------------------------------------------

/// Lifecycle states for a hybrid log page.
///
/// Each page frame transitions through these states exactly once per
/// generation, with the exception of the `Free → Open` recycling path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageState {
    /// Page frame is not allocated or has been evicted and recycled.
    Free = 0,
    /// Page is open for writes (the current tail page).
    Open = 1,
    /// Page has been sealed — no more writes allowed.
    Sealed = 2,
    /// Page is currently being flushed to disk.
    Flushing = 3,
    /// Page has been successfully flushed to disk.
    Flushed = 4,
    /// Page has been evicted from memory (data is on disk only).
    Evicted = 5,
}

impl PageState {
    /// Convert a raw `u8` into a `PageState`, returning `None` for invalid
    /// discriminants.
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Free),
            1 => Some(Self::Open),
            2 => Some(Self::Sealed),
            3 => Some(Self::Flushing),
            4 => Some(Self::Flushed),
            5 => Some(Self::Evicted),
            _ => None,
        }
    }
}

impl fmt::Display for PageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

// ---------------------------------------------------------------------------
// AtomicPageState — lock-free state wrapper
// ---------------------------------------------------------------------------

/// Atomic wrapper around [`PageState`] for lock-free state transitions.
///
/// Uses `AtomicU8` internally. State transitions are performed via CAS
/// through [`try_transition`](Self::try_transition).
pub struct AtomicPageState {
    inner: AtomicU8,
}

impl AtomicPageState {
    /// Create a new `AtomicPageState` with the given initial state.
    pub fn new(state: PageState) -> Self {
        Self {
            inner: AtomicU8::new(state as u8),
        }
    }

    /// Load the current state.
    pub fn load(&self, order: Ordering) -> PageState {
        let v = self.inner.load(order);
        // All values stored through our API are valid discriminants.
        PageState::from_u8(v).expect("AtomicPageState: corrupted discriminant")
    }

    /// Store a state unconditionally.
    pub fn store(&self, state: PageState, order: Ordering) {
        self.inner.store(state as u8, order);
    }

    /// Attempt an atomic CAS transition from `expected` to `desired`.
    ///
    /// Returns `true` if the transition succeeded, `false` if the current
    /// state did not match `expected`.
    pub fn try_transition(&self, expected: PageState, desired: PageState) -> bool {
        self.inner
            .compare_exchange(
                expected as u8,
                desired as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

impl fmt::Debug for AtomicPageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AtomicPageState({:?})", self.load(Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// PageFrame — physical memory for one page
// ---------------------------------------------------------------------------

/// A sector-aligned slab of zeroed memory backing one hybrid log page.
///
/// Page frames are allocated via [`std::alloc::alloc_zeroed`] with alignment
/// equal to the sector size (default 512 bytes). The frame owns its memory
/// exclusively and frees it on drop.
///
/// # Safety
///
/// The `Send` and `Sync` impls are safe because:
/// - The frame exclusively owns the allocation (no aliased pointers).
/// - Mutable access is gated by the caller holding appropriate lifecycle
///   guarantees (e.g. the page is in `Open` state and the caller is the
///   sole writer to a given offset range).
pub struct PageFrame {
    /// Pointer to the allocated memory (page_size bytes, sector-aligned).
    data: NonNull<u8>,
    /// Size of this page frame in bytes.
    size: usize,
    /// Memory layout used for deallocation.
    layout: Layout,
    /// Current state of this page frame.
    state: AtomicPageState,
    /// How many bytes of this page have been flushed to disk.
    /// Uses `AtomicU32` with `fetch_max` for concurrent flush progress tracking.
    flushed_until: AtomicU32,
}

// SAFETY: `PageFrame` exclusively owns its heap allocation. No aliased
// mutable access is possible, so sending across threads is safe.
unsafe impl Send for PageFrame {}
// SAFETY: See `Send` impl above — exclusive ownership. Concurrent readers
// access disjoint offset ranges and state transitions are atomic.
unsafe impl Sync for PageFrame {}

impl PageFrame {
    /// Allocate a new page frame with `page_size` bytes, aligned to
    /// `sector_size`.
    ///
    /// The memory is zeroed on allocation (records check zeroed `RecordInfo`
    /// as "empty slot").
    ///
    /// # Why `alloc_zeroed` instead of `Vec` / `Box<[u8]>`
    ///
    /// Page frames require sector-aligned allocations (typically 512 bytes)
    /// for direct I/O. The standard allocator only guarantees alignment up
    /// to `std::mem::align_of::<T>()` (typically 1 for `u8`, 8 for `u64`).
    /// `Vec::with_capacity` and `Box<[u8]>` cannot satisfy sector alignment,
    /// so we use `std::alloc::alloc_zeroed` with a custom `Layout`.
    ///
    /// # Panics
    ///
    /// Panics if `page_size` is 0 or if the system allocator fails.
    pub fn new(page_size: usize, sector_size: usize) -> Self {
        assert!(page_size > 0, "page_size must be > 0");
        assert!(
            sector_size.is_power_of_two(),
            "sector_size must be a power of two"
        );

        let layout =
            Layout::from_size_align(page_size, sector_size).expect("invalid layout for page frame");

        // SAFETY: `layout` has non-zero size (asserted above) and valid
        // alignment (power of two, checked by Layout::from_size_align).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let data = NonNull::new(ptr).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));

        Self {
            data,
            size: page_size,
            layout,
            state: AtomicPageState::new(PageState::Free),
            flushed_until: AtomicU32::new(0),
        }
    }

    /// Returns the page frame size in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns a raw const pointer to the page data.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Returns a raw mutable pointer to the page data.
    #[inline]
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.data.as_ptr()
    }

    /// Returns the page data as an immutable byte slice.
    ///
    /// # Safety
    ///
    /// The caller must ensure no mutable references to overlapping regions
    /// exist. In practice this is upheld by the page lifecycle: only the
    /// `Open` state permits writes, and writers target disjoint offset ranges.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `data` points to `size` bytes of valid, allocated memory
        // that remains live for the lifetime of `self`.
        unsafe { core::slice::from_raw_parts(self.data.as_ptr(), self.size) }
    }

    /// Returns the page data as a mutable byte slice.
    ///
    /// Exclusive access is guaranteed by the `&mut self` borrow — the caller
    /// must hold a unique reference to the page frame.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `data` points to `size` bytes of valid, allocated memory.
        // `&mut self` guarantees no aliased references exist.
        unsafe { core::slice::from_raw_parts_mut(self.data.as_ptr(), self.size) }
    }

    /// Zero the entire page frame (for reuse after eviction).
    ///
    /// # Safety
    ///
    /// The caller must guarantee exclusive access — no concurrent readers
    /// or writers. This is ensured by the frame being in `Evicted` or
    /// `Free` state (not mapped by any thread).
    pub unsafe fn zero(&self) {
        // SAFETY: Caller guarantees exclusive access. `data` points to
        // `size` bytes of valid memory.
        unsafe {
            core::ptr::write_bytes(self.data.as_ptr(), 0, self.size);
        }
    }

    /// Returns a reference to the atomic page state.
    #[inline]
    pub fn state(&self) -> &AtomicPageState {
        &self.state
    }

    /// Returns a reference to the flushed-until counter.
    #[inline]
    pub fn flushed_until(&self) -> &AtomicU32 {
        &self.flushed_until
    }

    /// Reset flushed_until to 0 (for frame recycling).
    pub fn reset_flush_progress(&self) {
        self.flushed_until.store(0, Ordering::Release);
    }
}

impl Drop for PageFrame {
    fn drop(&mut self) {
        // SAFETY: `data` was allocated with `self.layout` via
        // `std::alloc::alloc_zeroed` and has not been freed yet.
        unsafe {
            std::alloc::dealloc(self.data.as_ptr(), self.layout);
        }
    }
}

impl fmt::Debug for PageFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageFrame")
            .field("data", &self.data)
            .field("size", &self.size)
            .field("state", &self.state)
            .field("flushed_until", &self.flushed_until.load(Ordering::Relaxed))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// PageTable — circular buffer of page frames
// ---------------------------------------------------------------------------

/// Circular buffer mapping logical [`Page`] numbers to physical [`PageFrame`]s.
///
/// The table uses atomic pointers for lock-free concurrent access. The buffer
/// size must be a power of two so that modular indexing reduces to a bitmask.
///
/// When a frame slot is empty (null pointer) and [`get_or_allocate_frame`] is
/// called, a new frame is CAS-allocated. When a slot holds an evicted frame,
/// it is recycled (zeroed and transitioned back to `Open`) instead of
/// allocating fresh memory.
///
/// [`get_or_allocate_frame`]: PageTable::get_or_allocate_frame
pub struct PageTable {
    /// Array of page frame pointers. Index = `page.0 as usize & buffer_mask`.
    frames: Box<[AtomicPtr<PageFrame>]>,
    /// Number of frames in the circular buffer (power of 2).
    buffer_size: usize,
    /// Bit mask for modular indexing: `buffer_size - 1`.
    buffer_mask: usize,
    /// Page size in bytes.
    page_size: usize,
    /// Sector alignment for I/O.
    sector_size: usize,
}

// SAFETY: All shared state in `PageTable` is behind atomics (`AtomicPtr`).
// `PageFrame` is `Send + Sync`, so sharing `PageTable` across threads is safe.
unsafe impl Send for PageTable {}
// SAFETY: See `Send` impl above.
unsafe impl Sync for PageTable {}

impl PageTable {
    /// Create a new page table with the given buffer size.
    ///
    /// # Panics
    ///
    /// Panics if `buffer_size` is 0 or not a power of two.
    pub fn new(buffer_size: usize, page_size: usize, sector_size: usize) -> Self {
        assert!(buffer_size > 0, "buffer_size must be > 0");
        assert!(
            buffer_size.is_power_of_two(),
            "buffer_size must be a power of two"
        );
        assert!(page_size > 0, "page_size must be > 0");
        assert!(
            sector_size.is_power_of_two(),
            "sector_size must be a power of two"
        );

        let mut frames = Vec::with_capacity(buffer_size);
        for _ in 0..buffer_size {
            frames.push(AtomicPtr::new(core::ptr::null_mut()));
        }

        Self {
            frames: frames.into_boxed_slice(),
            buffer_size,
            buffer_mask: buffer_size - 1,
            page_size,
            sector_size,
        }
    }

    /// Compute the circular buffer index for a logical page number.
    #[inline]
    pub fn frame_index(&self, page: Page) -> usize {
        page.0 as usize & self.buffer_mask
    }

    // -------------------------------------------------------------------
    // Private helper: centralized pointer-to-reference conversion
    // -------------------------------------------------------------------

    /// Convert a non-null `*mut PageFrame` to a shared reference.
    ///
    /// # Safety
    ///
    /// - `ptr` must be a valid, non-null pointer to a `PageFrame` that was
    ///   created by `Box::into_raw` and stored in this table.
    /// - The frame must not have been freed (via `Box::from_raw`) and must
    ///   remain live for the lifetime of `self`.
    #[inline]
    unsafe fn frame_ref(&self, ptr: *mut PageFrame) -> &PageFrame {
        debug_assert!(!ptr.is_null(), "frame_ref called with null pointer");
        // SAFETY: Caller guarantees ptr is valid, non-null, and live.
        unsafe { &*ptr }
    }

    // -------------------------------------------------------------------
    // Public API
    // -------------------------------------------------------------------

    /// Load the frame pointer for `page`, returning a reference if allocated.
    ///
    /// Returns `None` if the slot is null (no frame allocated for this page).
    ///
    /// # TODO (SF-9)
    ///
    /// The circular buffer index (`page % buffer_size`) means different pages
    /// can alias to the same slot. This method does not verify that the frame
    /// actually belongs to the requested page. Add a debug-mode page-number
    /// field to `PageFrame` and assert it matches here.
    pub fn get_frame(&self, page: Page) -> Option<&PageFrame> {
        let idx = self.frame_index(page);
        let ptr = self.frames[idx].load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: Non-null pointers in the table are always valid,
            // heap-allocated `PageFrame`s created by `get_or_allocate_frame`.
            // The frame lives until explicitly evicted or the table is dropped.
            Some(unsafe { self.frame_ref(ptr) })
        }
    }

    /// Get the frame for `page`, allocating (or recycling) one if the slot is
    /// empty or evicted.
    ///
    /// If multiple threads race to allocate the same slot, exactly one CAS
    /// succeeds and the loser's allocation is freed. Both threads receive a
    /// valid reference to the winning frame.
    ///
    /// If the slot contains an evicted frame, it is recycled: zeroed and
    /// transitioned to `Open`.
    pub fn get_or_allocate_frame(&self, page: Page) -> &PageFrame {
        let idx = self.frame_index(page);

        // Fast path: frame already exists.
        let existing = self.frames[idx].load(Ordering::Acquire);
        if !existing.is_null() {
            // SAFETY: Non-null pointers in the table are valid, heap-allocated
            // `PageFrame`s (invariant maintained by get_or_allocate_frame).
            let frame = unsafe { self.frame_ref(existing) };
            let state = frame.state().load(Ordering::Acquire);
            if state == PageState::Evicted || state == PageState::Free {
                // SF-8: Use CAS to prevent concurrent recycling races.
                // Only one thread wins the transition; losers skip zeroing
                // and return the already-recycled frame.
                if frame.state().try_transition(state, PageState::Open) {
                    // SAFETY: We won the CAS — no concurrent recycler will
                    // also zero this frame. Exclusive write access is
                    // established by winning the state transition.
                    unsafe { frame.zero() };
                    frame.reset_flush_progress();
                }
            }
            return frame;
        }

        // Slow path: allocate a new frame.
        let new_frame = Box::new(PageFrame::new(self.page_size, self.sector_size));
        new_frame.state().store(PageState::Open, Ordering::Release);
        let raw = Box::into_raw(new_frame);

        // CAS null → new frame pointer.
        match self.frames[idx].compare_exchange(
            core::ptr::null_mut(),
            raw,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // We won the race.
                // SAFETY: `raw` was just created via Box::into_raw and stored
                // in the table. It is a valid, live PageFrame pointer.
                unsafe { self.frame_ref(raw) }
            }
            Err(winner) => {
                // Another thread won — free our allocation and use theirs.
                // SAFETY: `raw` was created via `Box::into_raw` and no one
                // else holds a reference to it (the CAS failed). `winner`
                // is a valid pointer stored by the winning thread via a
                // successful CAS on the same slot.
                unsafe {
                    let _ = Box::from_raw(raw);
                    self.frame_ref(winner)
                }
            }
        }
    }

    /// Attempt to evict the frame at the given page slot.
    ///
    /// Transitions the frame to `Evicted` state and nulls out the slot
    /// pointer, freeing the frame memory.
    ///
    /// Returns `Some(())` if eviction succeeded, `None` if the slot was
    /// already empty.
    pub fn try_evict_frame(&self, page: Page) -> Option<()> {
        let idx = self.frame_index(page);

        let ptr = self.frames[idx].load(Ordering::Acquire);
        if ptr.is_null() {
            return None;
        }

        // SAFETY: Non-null pointers in the table are valid `PageFrame`s
        // (invariant maintained by get_or_allocate_frame).
        let frame = unsafe { self.frame_ref(ptr) };
        frame.state().store(PageState::Evicted, Ordering::Release);

        // Null out the slot. Use CAS to avoid ABA if another thread already
        // swapped in a different frame.
        match self.frames[idx].compare_exchange(
            ptr,
            core::ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: We successfully removed `ptr` from the table.
                // No other thread can access it now. Reconstruct Box to free.
                let _ = unsafe { Box::from_raw(ptr) };
                Some(())
            }
            Err(_) => {
                // Slot was changed by another thread; leave it alone.
                None
            }
        }
    }

    /// Returns the page size in bytes.
    #[inline]
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns the number of frame slots in the circular buffer.
    #[inline]
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Returns the sector alignment size.
    #[inline]
    pub fn sector_size(&self) -> usize {
        self.sector_size
    }
}

impl Drop for PageTable {
    fn drop(&mut self) {
        for slot in self.frames.iter_mut() {
            let ptr = *slot.get_mut();
            if !ptr.is_null() {
                // SAFETY: Non-null pointers in the table are valid
                // `PageFrame`s allocated via `Box::into_raw`. We have
                // exclusive access via `&mut self` in `drop`.
                let _ = unsafe { Box::from_raw(ptr) };
            }
        }
    }
}

impl fmt::Debug for PageTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let allocated = self
            .frames
            .iter()
            .filter(|s| !s.load(Ordering::Relaxed).is_null())
            .count();
        f.debug_struct("PageTable")
            .field("buffer_size", &self.buffer_size)
            .field("page_size", &self.page_size)
            .field("sector_size", &self.sector_size)
            .field("allocated_frames", &allocated)
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Page;

    // -- PageFrame basics ---------------------------------------------------

    #[test]
    fn page_frame_allocate_and_access() {
        let page_size = 4096;
        let mut frame = PageFrame::new(page_size, 512);
        assert_eq!(frame.size(), page_size);

        // Write a pattern.
        let slice = frame.as_mut_slice();
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }

        // Read back.
        let slice = frame.as_slice();
        for (i, &byte) in slice.iter().enumerate() {
            assert_eq!(byte, (i % 256) as u8, "mismatch at offset {i}");
        }
    }

    #[test]
    fn page_frame_zero_clears_content() {
        let page_size = 4096;
        let mut frame = PageFrame::new(page_size, 512);

        // Write non-zero data.
        let slice = frame.as_mut_slice();
        slice.fill(0xAB);

        // Zero the frame.
        // SAFETY: Exclusive access, no concurrent readers.
        unsafe { frame.zero() };

        let slice = frame.as_slice();
        assert!(
            slice.iter().all(|&b| b == 0),
            "page frame should be all zeros after zero()"
        );
    }

    #[test]
    fn page_frame_state_transitions() {
        let frame = PageFrame::new(4096, 512);
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Free);

        // Free → Open
        assert!(
            frame
                .state()
                .try_transition(PageState::Free, PageState::Open)
        );
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Open);

        // Open → Sealed
        assert!(
            frame
                .state()
                .try_transition(PageState::Open, PageState::Sealed)
        );
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Sealed);

        // Sealed → Flushing
        assert!(
            frame
                .state()
                .try_transition(PageState::Sealed, PageState::Flushing)
        );
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Flushing);

        // Flushing → Flushed
        assert!(
            frame
                .state()
                .try_transition(PageState::Flushing, PageState::Flushed)
        );
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Flushed);

        // Flushed → Evicted
        assert!(
            frame
                .state()
                .try_transition(PageState::Flushed, PageState::Evicted)
        );
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Evicted);
    }

    #[test]
    fn page_frame_invalid_transition_fails() {
        let frame = PageFrame::new(4096, 512);

        // Move to Sealed.
        frame.state().store(PageState::Sealed, Ordering::Relaxed);

        // Trying to transition *from Open* should fail because the current
        // state is Sealed (CAS expected value mismatch).
        assert!(
            !frame
                .state()
                .try_transition(PageState::Open, PageState::Free),
            "transition from Open should fail when state is Sealed"
        );

        // Trying to transition from Free should also fail.
        assert!(
            !frame
                .state()
                .try_transition(PageState::Free, PageState::Open),
            "transition from Free should fail when state is Sealed"
        );

        // State should still be Sealed — nothing changed.
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Sealed);
    }

    // -- PageTable basics ---------------------------------------------------

    #[test]
    fn page_table_create_and_index() {
        let table = PageTable::new(16, 4096, 512);
        assert_eq!(table.buffer_size(), 16);
        assert_eq!(table.page_size(), 4096);
        assert_eq!(table.sector_size(), 512);

        // Verify wrapping: page 0 and page 16 map to the same index.
        assert_eq!(table.frame_index(Page(0)), 0);
        assert_eq!(table.frame_index(Page(5)), 5);
        assert_eq!(table.frame_index(Page(15)), 15);
        assert_eq!(table.frame_index(Page(16)), 0); // wraps
        assert_eq!(table.frame_index(Page(17)), 1);
        assert_eq!(table.frame_index(Page(31)), 15);
        assert_eq!(table.frame_index(Page(32)), 0);
    }

    #[test]
    fn page_table_allocate_frame() {
        let table = PageTable::new(4, 4096, 512);

        // Initially no frame.
        assert!(table.get_frame(Page(0)).is_none());

        // Allocate.
        let frame = table.get_or_allocate_frame(Page(0));
        assert_eq!(frame.size(), 4096);
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Open);

        // Second call returns the same frame.
        let frame2 = table.get_or_allocate_frame(Page(0));
        assert_eq!(
            frame as *const PageFrame, frame2 as *const PageFrame,
            "should return the same frame"
        );

        // get_frame also returns it.
        assert!(table.get_frame(Page(0)).is_some());
    }

    #[test]
    fn page_table_concurrent_allocate() {
        use std::sync::Arc;

        let table = Arc::new(PageTable::new(4, 4096, 512));
        let num_threads = 8;
        let mut handles = Vec::new();

        for _ in 0..num_threads {
            let t = Arc::clone(&table);
            handles.push(std::thread::spawn(move || {
                let frame = t.get_or_allocate_frame(Page(0));
                frame as *const PageFrame as usize
            }));
        }

        let ptrs: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All threads must see the same frame pointer.
        let first = ptrs[0];
        for (i, &p) in ptrs.iter().enumerate().skip(1) {
            assert_eq!(p, first, "thread {i} got a different frame pointer");
        }

        // The frame should be valid.
        let frame = table.get_frame(Page(0)).expect("frame should exist");
        assert_eq!(frame.size(), 4096);
    }

    #[test]
    fn page_table_frame_reuse() {
        let table = PageTable::new(4, 4096, 512);

        // Allocate and write data.
        let frame = table.get_or_allocate_frame(Page(0));
        assert_eq!(frame.state().load(Ordering::Relaxed), PageState::Open);

        // Simulate lifecycle: Open → Sealed → Flushing → Flushed → Evicted
        frame
            .state()
            .try_transition(PageState::Open, PageState::Sealed);
        frame
            .state()
            .try_transition(PageState::Sealed, PageState::Flushing);
        frame
            .state()
            .try_transition(PageState::Flushing, PageState::Flushed);
        frame
            .state()
            .try_transition(PageState::Flushed, PageState::Evicted);

        // Evict: free the memory.
        assert!(table.try_evict_frame(Page(0)).is_some());
        assert!(table.get_frame(Page(0)).is_none());

        // Allocate again — new frame at the same slot.
        let frame2 = table.get_or_allocate_frame(Page(0));
        assert_eq!(frame2.state().load(Ordering::Relaxed), PageState::Open);
        // Data should be zeroed.
        assert!(
            frame2.as_slice().iter().all(|&b| b == 0),
            "recycled frame should be zeroed"
        );
    }

    #[test]
    fn page_frame_alignment_check() {
        // Test a few common sector sizes.
        for &sector_size in &[512, 4096] {
            let frame = PageFrame::new(8192, sector_size);
            let addr = frame.as_ptr() as usize;
            assert_eq!(
                addr % sector_size,
                0,
                "frame data should be aligned to sector_size={sector_size}"
            );
        }
    }

    #[test]
    fn page_table_evict_empty_slot_returns_none() {
        let table = PageTable::new(4, 4096, 512);
        assert!(table.try_evict_frame(Page(0)).is_none());
    }

    #[test]
    fn page_frame_flushed_until_tracking() {
        let frame = PageFrame::new(4096, 512);
        assert_eq!(frame.flushed_until().load(Ordering::Relaxed), 0);

        frame.flushed_until().fetch_max(512, Ordering::Release);
        assert_eq!(frame.flushed_until().load(Ordering::Acquire), 512);

        frame.flushed_until().fetch_max(1024, Ordering::Release);
        assert_eq!(frame.flushed_until().load(Ordering::Acquire), 1024);

        // fetch_max with a smaller value is a no-op.
        frame.flushed_until().fetch_max(256, Ordering::Release);
        assert_eq!(frame.flushed_until().load(Ordering::Acquire), 1024);

        frame.reset_flush_progress();
        assert_eq!(frame.flushed_until().load(Ordering::Relaxed), 0);
    }

    #[test]
    fn page_state_display() {
        assert_eq!(format!("{}", PageState::Free), "Free");
        assert_eq!(format!("{}", PageState::Open), "Open");
        assert_eq!(format!("{}", PageState::Evicted), "Evicted");
    }

    #[test]
    fn atomic_page_state_debug() {
        let s = AtomicPageState::new(PageState::Flushing);
        let dbg = format!("{s:?}");
        assert!(dbg.contains("Flushing"));
    }
}
