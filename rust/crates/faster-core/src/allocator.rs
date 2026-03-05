//! Fixed-size page allocator (`MallocFixedPageSize`).
//!
//! A lock-free, page-granularity memory allocator for fixed-size items.
//! Used by the hash index for overflow bucket allocation and by the hybrid
//! log for in-memory page management.
//!
//! # Design
//!
//! Two-level structure modeled after C++ `malloc_fixed_page_size.h`:
//!
//! - **Level 1 — Page directory:** A growable array of page pointers. Each slot
//!   is an [`AtomicPtr`] that is either null (page not yet allocated) or points
//!   to a heap-allocated page. Growth doubles the directory and is protected by
//!   a [`Mutex`] (extremely rare operation).
//!
//! - **Level 2 — Pages:** Each page is a cache-line-aligned heap allocation
//!   holding [`ITEMS_PER_PAGE`] items of type `T`. Pages are allocated on
//!   demand via lock-free CAS and are never moved or freed until the allocator
//!   is dropped.
//!
//! # Allocation strategy
//!
//! 1. **Free list (reuse path):** A lock-free Treiber stack with 16-bit ABA
//!    tags. Freed items store the next-pointer in their first 8 bytes.
//! 2. **Bump pointer (fresh path):** An atomic `fetch_add` on a monotonic
//!    counter. Page boundaries trigger on-demand page allocation.
//!
//! # Address encoding
//!
//! Items are addressed via [`LogicalAddress`] where:
//! - `page()` → index into the page directory
//! - `offset()` → item index within that page
//!
//! The allocator uses [`ITEMS_PER_PAGE_BITS`] = 20 (matching C++), so offsets
//! range from 0 to 1,048,575. This fits within [`LogicalAddress`]'s 25-bit
//! offset field.
//!
//! # Thread safety
//!
//! The allocator is `Send + Sync`. All operations are lock-free on the common
//! path. Only directory growth (very rare) acquires a mutex.
//!
//! # Examples
//!
//! ```
//! use faster_core::allocator::MallocFixedPageSize;
//!
//! // Allocator for 16-byte items (minimum 8 bytes required)
//! #[repr(C)]
//! struct Item { a: u64, b: u64 }
//!
//! let alloc: MallocFixedPageSize<Item> = MallocFixedPageSize::new();
//! let addr = alloc.allocate();
//! assert!(addr.is_valid());
//!
//! // Access the allocated item
//! let item: &Item = alloc.get(addr);
//! assert_eq!(item.a, 0); // zero-initialized from fresh page
//! ```

use std::alloc::{self, Layout};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::address::{LogicalAddress, Offset, Page, MAX_OFFSET, MAX_PAGE};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of bits for the item index within a page.
/// Each page holds 2^20 = 1,048,576 items, matching C++ `FixedPageAddress::kOffsetBits`.
pub const ITEMS_PER_PAGE_BITS: u32 = 20;

/// Number of items per allocator page.
pub const ITEMS_PER_PAGE: usize = 1 << ITEMS_PER_PAGE_BITS;

/// Bit mask for extracting the item index from a flat counter value.
const ITEMS_PER_PAGE_MASK: u64 = (ITEMS_PER_PAGE as u64) - 1;

/// Initial number of page slots in the directory.
const INITIAL_DIR_CAPACITY: usize = 16;

/// Minimum alignment for page allocations (cache-line size).
const PAGE_ALIGNMENT: usize = 64;

/// Sentinel value for an empty free list.
const FREE_LIST_EMPTY: u64 = 0;

/// Number of upper bits used for the ABA tag in free list entries.
const TAG_BITS: u32 = 16;

/// Bit position where the tag starts (bits 48..63).
const TAG_SHIFT: u32 = 64 - TAG_BITS;

/// Mask for extracting the address portion (lower 48 bits).
const ADDR_MASK: u64 = (1u64 << TAG_SHIFT) - 1;

/// Increment applied to the tag on each push/pop operation.
const TAG_INCREMENT: u64 = 1u64 << TAG_SHIFT;

// Compile-time invariants.
const _: () = assert!(
    ITEMS_PER_PAGE_BITS <= crate::address::OFFSET_BITS,
    "ITEMS_PER_PAGE must fit in LogicalAddress offset field"
);
const _: () = assert!(
    ITEMS_PER_PAGE.is_power_of_two(),
    "ITEMS_PER_PAGE must be a power of two"
);

// ---------------------------------------------------------------------------
// PageDir — the page directory
// ---------------------------------------------------------------------------

/// A directory of page pointers.
///
/// Each slot is an [`AtomicPtr<T>`] that starts null and is lazily filled
/// when items on that page are first allocated.
struct PageDir<T> {
    slots: Box<[AtomicPtr<T>]>,
}

impl<T> PageDir<T> {
    /// Create a new directory with `capacity` null slots.
    fn new(capacity: usize) -> Self {
        let slots: Vec<AtomicPtr<T>> = (0..capacity)
            .map(|_| AtomicPtr::new(ptr::null_mut()))
            .collect();
        PageDir {
            slots: slots.into_boxed_slice(),
        }
    }

    /// Create a new directory that copies existing page pointers from `old`.
    fn grow_from(old: &PageDir<T>, new_capacity: usize) -> Self {
        debug_assert!(new_capacity > old.capacity());
        let mut slots: Vec<AtomicPtr<T>> = Vec::with_capacity(new_capacity);
        for i in 0..old.capacity() {
            let page = old.slots[i].load(Ordering::Acquire);
            slots.push(AtomicPtr::new(page));
        }
        for _ in old.capacity()..new_capacity {
            slots.push(AtomicPtr::new(ptr::null_mut()));
        }
        PageDir {
            slots: slots.into_boxed_slice(),
        }
    }

    #[inline]
    fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Load the page pointer at `idx`. Returns null if the page hasn't been
    /// allocated yet.
    #[inline]
    fn get_page(&self, idx: usize) -> *mut T {
        debug_assert!(idx < self.capacity());
        self.slots[idx].load(Ordering::Acquire)
    }

    /// Load the page pointer at `idx`, allocating a new page if necessary.
    ///
    /// Uses CAS to ensure only one thread allocates the page. Losing threads
    /// discard their allocation and use the winner's page.
    fn get_or_add_page(&self, idx: usize) -> *mut T {
        debug_assert!(idx < self.capacity());
        let page = self.slots[idx].load(Ordering::Acquire);
        if !page.is_null() {
            return page;
        }
        self.add_page(idx)
    }

    /// Allocate and install a new page at `idx`. If another thread wins the
    /// race, the losing allocation is freed immediately.
    fn add_page(&self, idx: usize) -> *mut T {
        let new_page = Self::alloc_page();
        match self.slots[idx].compare_exchange(
            ptr::null_mut(),
            new_page,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => new_page,
            Err(winner) => {
                // SAFETY: `new_page` was just allocated by `alloc_page` and no
                // reference to it has been shared; safe to deallocate.
                unsafe { Self::dealloc_page(new_page) };
                winner
            }
        }
    }

    /// Heap-allocate one page (zeroed, cache-line-aligned).
    fn alloc_page() -> *mut T {
        let size = ITEMS_PER_PAGE
            .checked_mul(std::mem::size_of::<T>())
            .expect("page size overflow");
        // Zero-size types don't need real allocation.
        if size == 0 {
            return NonNull::dangling().as_ptr();
        }
        let align = PAGE_ALIGNMENT.max(std::mem::align_of::<T>());
        let layout =
            Layout::from_size_align(size, align).expect("invalid page layout");
        // SAFETY: `layout` has non-zero size (checked above) and valid alignment.
        let ptr = unsafe { alloc::alloc_zeroed(layout) } as *mut T;
        if ptr.is_null() {
            alloc::handle_alloc_error(layout);
        }
        ptr
    }

    /// Deallocate a page previously obtained from [`alloc_page`].
    ///
    /// # Safety
    ///
    /// `page` must have been allocated by [`alloc_page`] and must not be in
    /// use by any other thread.
    unsafe fn dealloc_page(page: *mut T) {
        let size = ITEMS_PER_PAGE * std::mem::size_of::<T>();
        if size == 0 {
            return;
        }
        let align = PAGE_ALIGNMENT.max(std::mem::align_of::<T>());
        let layout =
            Layout::from_size_align(size, align).expect("invalid page layout");
        // SAFETY: Caller guarantees `page` was allocated with this layout.
        unsafe { alloc::dealloc(page as *mut u8, layout) };
    }
}

// ---------------------------------------------------------------------------
// MallocFixedPageSize
// ---------------------------------------------------------------------------

/// A fixed-size page allocator for items of type `T`.
///
/// Thread-safe, lock-free allocator modeled after C++ `MallocFixedPageSize`.
/// Manages a two-level hierarchy: a growable directory of page pointers, where
/// each page is a heap-allocated array of [`ITEMS_PER_PAGE`] items.
///
/// # Addressing
///
/// Items are addressed via [`LogicalAddress`]:
/// - `page()` → index into the page directory (which allocated page)
/// - `offset()` → item index within that page
///
/// # Memory lifecycle
///
/// - Pages are allocated on demand and never freed until the allocator is dropped.
/// - Items returned by [`allocate`](Self::allocate) come from zero-initialized
///   pages (fresh) or from the free list (may contain stale data — caller must
///   reinitialize).
/// - The allocator must outlive all references obtained via [`get`](Self::get)
///   or [`get_mut`](Self::get_mut).
///
/// # Panics
///
/// - [`new`](Self::new) panics if `size_of::<T>() < 8` (required for free
///   list linkage).
/// - [`get`](Self::get) / [`get_mut`](Self::get_mut) panic (in debug builds)
///   if the address refers to an unallocated page.
pub struct MallocFixedPageSize<T> {
    /// Pointer to the current page directory. Replaced atomically on growth;
    /// old directories are pushed to `retired_dirs`.
    dir: AtomicPtr<PageDir<T>>,

    /// Monotonic flat-item counter. The next `fetch_add(1)` value encodes
    /// `page_idx = flat >> ITEMS_PER_PAGE_BITS`, `item_idx = flat & mask`.
    /// Initialized to 2 so that addresses 0 ([`LogicalAddress::ZERO`]) and 1
    /// ([`LogicalAddress::INVALID`]) are never returned.
    count: AtomicU64,

    /// Free list head — a Treiber stack with 16-bit ABA tag.
    ///
    /// Encoding: `[63:48 tag | 47:0 LogicalAddress raw]`. `0` = empty.
    ///
    /// Each freed item stores the raw u64 of the previous head in its first
    /// 8 bytes, forming a singly-linked list.
    free_list: AtomicU64,

    /// Mutex protecting directory growth (doubles the directory).
    /// Held only during the rare grow operation; never on the allocation
    /// hot-path.
    grow_lock: Mutex<()>,

    /// Old page directories that were replaced during growth. Freed on drop.
    /// Separate from the grow_lock to avoid holding two locks.
    retired_dirs: Mutex<Vec<NonNull<PageDir<T>>>>,

    _phantom: PhantomData<T>,
}

// SAFETY: All shared state is accessed through atomics or Mutex. Pages are
// heap-allocated and never moved. The raw pointers in `dir` and `retired_dirs`
// point to owned allocations that are only freed when the allocator is dropped
// (which requires `&mut self`, guaranteeing exclusive access).
unsafe impl<T: Send> Send for MallocFixedPageSize<T> {}
// SAFETY: Same rationale as Send. Concurrent access to the allocator is safe
// because: (1) the bump counter uses atomic fetch_add, (2) the free list uses
// lock-free CAS, (3) page installation uses atomic CAS, and (4) directory
// growth is mutex-protected. No data races are possible.
unsafe impl<T: Send> Sync for MallocFixedPageSize<T> {}

impl<T> MallocFixedPageSize<T> {
    /// Creates a new allocator.
    ///
    /// Pre-allocates the first page so that the initial allocations are fast.
    ///
    /// # Panics
    ///
    /// Panics if `size_of::<T>() < 8` or `align_of::<T>() < 8`. The free list
    /// embeds a 8-byte next pointer inside freed items, so `T` must be at
    /// least 8 bytes with at least 8-byte alignment.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::allocator::MallocFixedPageSize;
    ///
    /// let alloc: MallocFixedPageSize<[u64; 8]> = MallocFixedPageSize::new();
    /// let addr = alloc.allocate();
    /// assert!(addr.is_valid());
    /// ```
    pub fn new() -> Self {
        assert!(
            std::mem::size_of::<T>() >= std::mem::size_of::<u64>(),
            "MallocFixedPageSize<T> requires size_of::<T>() >= 8 \
             (for free list linkage); got size_of::<T>() = {}",
            std::mem::size_of::<T>(),
        );
        assert!(
            std::mem::align_of::<T>() >= std::mem::align_of::<u64>(),
            "MallocFixedPageSize<T> requires align_of::<T>() >= 8 \
             (free list writes u64 into freed items); got align_of::<T>() = {}",
            std::mem::align_of::<T>(),
        );

        let dir = Box::into_raw(Box::new(PageDir::<T>::new(INITIAL_DIR_CAPACITY)));

        // Pre-allocate page 0 (avoids a CAS on the first allocation).
        // SAFETY: `dir` was just created and is valid.
        unsafe { (*dir).get_or_add_page(0) };

        MallocFixedPageSize {
            dir: AtomicPtr::new(dir),
            // Start at flat index 2 ⇒ first returned address is
            // LogicalAddress::MIN_VALID (page=0, offset=2).
            count: AtomicU64::new(2),
            free_list: AtomicU64::new(FREE_LIST_EMPTY),
            grow_lock: Mutex::new(()),
            retired_dirs: Mutex::new(Vec::new()),
            _phantom: PhantomData,
        }
    }

    /// Allocates one item, returning its [`LogicalAddress`].
    ///
    /// The returned address is always valid (`addr.is_valid() == true`).
    ///
    /// **Fast path:** pops from the free list (lock-free CAS).
    /// **Slow path:** bumps the monotonic counter and lazily allocates a new
    /// page if necessary.
    ///
    /// Items from fresh pages are zero-initialized. Items reused from the free
    /// list may contain stale data — the caller should reinitialize.
    ///
    /// # Panics
    ///
    /// Panics on address-space exhaustion (> 8 million pages × 1 million items).
    pub fn allocate(&self) -> LogicalAddress {
        // Fast path: try the free list first.
        if let Some(addr) = self.pop_free_list() {
            return addr;
        }
        // Slow path: bump allocation.
        self.bump_allocate()
    }

    /// Returns a shared reference to the item at `addr`.
    ///
    /// # Safety contract (caller must uphold)
    ///
    /// - `addr` was previously returned by [`allocate`](Self::allocate) on
    ///   this allocator and has not been freed.
    /// - No thread holds a `&mut T` to the same item (obtained via
    ///   [`get_mut`](Self::get_mut)). Concurrent shared reads (`&T`) are fine
    ///   when `T` uses interior mutability (e.g. [`AtomicU64`], [`AtomicPtr`]).
    ///
    /// These invariants are enforced at a higher level by the epoch system and
    /// session ownership model. This method is safe because the intended usage
    /// (hash bucket lookup) always goes through atomic fields inside `T`.
    ///
    /// # Panics (debug)
    ///
    /// Debug-asserts that the page is allocated.
    #[inline]
    pub fn get(&self, addr: LogicalAddress) -> &T {
        let (page_ptr, item_idx) = self.resolve(addr);
        // SAFETY: `resolve` guarantees `page_ptr` is a valid, aligned, non-null
        // pointer to an allocation of ITEMS_PER_PAGE items, and `item_idx` is
        // in bounds. The caller upholds the aliasing contract (no concurrent
        // `&mut T`). The lifetime of the returned reference is tied to `&self`,
        // and the page is never freed while the allocator is alive.
        unsafe { &*page_ptr.add(item_idx) }
    }

    /// Returns an exclusive reference to the item at `addr`.
    ///
    /// # Safety
    ///
    /// In addition to the [`get`](Self::get) contract:
    /// - The caller must have **exclusive logical ownership** of this item.
    ///   No other thread may concurrently read or write the item.
    /// - This is typically ensured by session ownership (the item was allocated
    ///   by this session and hasn't been published) or by epoch protection
    ///   (all other threads have exited the epoch that references this item).
    ///
    /// # Panics (debug)
    ///
    /// Debug-asserts that the page is allocated.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self, addr: LogicalAddress) -> &mut T {
        let (page_ptr, item_idx) = self.resolve(addr);
        // SAFETY: Same as `get`, plus the caller guarantees exclusive access.
        // No other `&T` or `&mut T` exists for this item.
        unsafe { &mut *page_ptr.add(item_idx) }
    }

    /// Returns the item at `addr` as a raw pointer.
    ///
    /// This is a lower-level alternative to [`get`] / [`get_mut`] for callers
    /// who need a pointer without creating a reference.
    #[inline]
    pub fn get_ptr(&self, addr: LogicalAddress) -> *mut T {
        let (page_ptr, item_idx) = self.resolve(addr);
        // SAFETY: pointer arithmetic within a valid page allocation.
        unsafe { page_ptr.add(item_idx) }
    }

    /// Frees an item, returning it to the lock-free free list for reuse.
    ///
    /// The item's first 8 bytes will be overwritten with the free-list linkage.
    ///
    /// # Safety contract (caller must uphold)
    ///
    /// - `addr` was returned by [`allocate`](Self::allocate) and has not
    ///   already been freed.
    /// - No thread will access the item after this call (enforced by epoch
    ///   protection at a higher level).
    ///
    /// In the future, epoch-gated deallocation (`free_at_epoch`) will defer
    /// the actual free-list push until all threads have passed a safe epoch.
    pub fn free(&self, addr: LogicalAddress) {
        self.push_free_list(addr);
    }

    /// Returns the number of item slots that have been bump-allocated
    /// (excluding the 2 reserved slots at flat indices 0 and 1).
    ///
    /// This counts slots that were ever allocated, including those
    /// subsequently freed. It does **not** subtract free-list entries.
    #[inline]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed).saturating_sub(2)
    }

    // -----------------------------------------------------------------------
    // Internal: address resolution
    // -----------------------------------------------------------------------

    /// Resolve a [`LogicalAddress`] to a `(page_ptr, item_index)` pair.
    #[inline]
    fn resolve(&self, addr: LogicalAddress) -> (*mut T, usize) {
        let page_idx = addr.page().0 as usize;
        let item_idx = addr.offset().0 as usize;
        debug_assert!(
            item_idx < ITEMS_PER_PAGE,
            "item index {item_idx} exceeds ITEMS_PER_PAGE {ITEMS_PER_PAGE}",
        );

        // SAFETY: `self.dir` is always a valid pointer to a `PageDir` that was
        // created by `new()` or `expand_directory()`. We load with Acquire to
        // synchronize with the Release store in `expand_directory`.
        let dir = unsafe { &*self.dir.load(Ordering::Acquire) };
        debug_assert!(
            page_idx < dir.capacity(),
            "page index {} exceeds directory capacity {}",
            page_idx,
            dir.capacity(),
        );

        let page_ptr = dir.get_page(page_idx);
        debug_assert!(!page_ptr.is_null(), "page {page_idx} not allocated");

        (page_ptr, item_idx)
    }

    // -----------------------------------------------------------------------
    // Internal: bump allocator
    // -----------------------------------------------------------------------

    /// Allocate via the bump counter. Lazily creates pages and grows the
    /// directory as needed.
    fn bump_allocate(&self) -> LogicalAddress {
        let flat = self.count.fetch_add(1, Ordering::Relaxed);

        let page_idx = (flat >> ITEMS_PER_PAGE_BITS) as u32;
        let item_idx = (flat & ITEMS_PER_PAGE_MASK) as u32;

        assert!(
            page_idx <= MAX_PAGE,
            "allocator exhausted: page index {page_idx} exceeds MAX_PAGE {MAX_PAGE}",
        );
        assert!(
            item_idx <= MAX_OFFSET,
            "item index {item_idx} exceeds MAX_OFFSET {MAX_OFFSET}",
        );

        // SAFETY: `self.dir` is valid (invariant).
        let dir = unsafe { &*self.dir.load(Ordering::Acquire) };

        // Grow directory if needed.
        let dir = if (page_idx as usize) >= dir.capacity() {
            self.expand_directory(page_idx as usize + 1)
        } else {
            dir
        };

        // Ensure the page exists.
        dir.get_or_add_page(page_idx as usize);

        // Eagerly allocate the *next* page when we hit offset 0 on a new page,
        // to reduce contention (matches C++ behavior).
        if item_idx == 0 && (page_idx as usize + 1) < dir.capacity() {
            dir.get_or_add_page(page_idx as usize + 1);
        }

        LogicalAddress::new(Page(page_idx), Offset(item_idx))
    }

    /// Double the directory capacity until it can hold `min_pages` entries.
    /// Returns a reference to the (possibly new) current directory.
    fn expand_directory(&self, min_pages: usize) -> &PageDir<T> {
        let _guard = self.grow_lock.lock().unwrap_or_else(|e| e.into_inner());

        // Re-check under lock — another thread may have already grown.
        // SAFETY: `self.dir` is valid.
        let current = unsafe { &*self.dir.load(Ordering::Acquire) };
        if current.capacity() >= min_pages {
            return current;
        }

        // Compute new capacity (next power of two ≥ min_pages).
        let new_cap = min_pages.next_power_of_two();
        let new_dir = Box::into_raw(Box::new(PageDir::grow_from(current, new_cap)));

        // Swap in the new directory.
        let old_dir = self.dir.swap(new_dir, Ordering::AcqRel);

        // Retire the old directory. It will be freed on Drop.
        // SAFETY: `old_dir` is non-null (was the valid current directory).
        let old_nn = unsafe { NonNull::new_unchecked(old_dir) };
        self.retired_dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(old_nn);

        // SAFETY: `new_dir` was just created and is valid.
        unsafe { &*new_dir }
    }

    // -----------------------------------------------------------------------
    // Internal: lock-free free list (Treiber stack with ABA tags)
    // -----------------------------------------------------------------------

    /// Push `addr` onto the free list.
    fn push_free_list(&self, addr: LogicalAddress) {
        let item_ptr = self.get_ptr(addr);
        let addr_raw = addr.raw();

        loop {
            let old_head = self.free_list.load(Ordering::Acquire);
            let old_addr_raw = old_head & ADDR_MASK;
            let old_tag = old_head & !ADDR_MASK;

            // Write the current head's address into the freed item's first
            // 8 bytes (the "next" pointer in the Treiber stack).
            // SAFETY: `item_ptr` points to a valid, allocated item of at least
            // 8 bytes. No other thread accesses this item (caller contract).
            unsafe {
                ptr::write(item_ptr as *mut u64, old_addr_raw);
            }

            // CAS the head: old → (addr, tag+1).
            let new_head = addr_raw | old_tag.wrapping_add(TAG_INCREMENT);
            match self.free_list.compare_exchange_weak(
                old_head,
                new_head,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(_) => continue, // Retry — another thread modified the head.
            }
        }
    }

    /// Pop an address from the free list, or `None` if empty.
    fn pop_free_list(&self) -> Option<LogicalAddress> {
        loop {
            let old_head = self.free_list.load(Ordering::Acquire);
            let head_addr_raw = old_head & ADDR_MASK;

            if head_addr_raw == 0 {
                return None; // Empty list.
            }

            let head_addr = LogicalAddress::from_raw(head_addr_raw);
            let head_ptr = self.get_ptr(head_addr);

            // Read the "next" pointer from the head item's first 8 bytes.
            // SAFETY: `head_ptr` points to a valid item in an allocated page.
            // The item is on the free list so no other thread is writing to it
            // (concurrent pops may read the same value, but only one CAS will
            // succeed). Reading a stale value is harmless — the CAS will fail.
            let next_raw = unsafe { ptr::read(head_ptr as *const u64) };

            let old_tag = old_head & !ADDR_MASK;
            let new_head = (next_raw & ADDR_MASK) | old_tag.wrapping_add(TAG_INCREMENT);

            match self.free_list.compare_exchange_weak(
                old_head,
                new_head,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(head_addr),
                Err(_) => continue, // Retry.
            }
        }
    }
}

impl<T> Default for MallocFixedPageSize<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for MallocFixedPageSize<T> {
    fn drop(&mut self) {
        // SAFETY: We have `&mut self`, so no concurrent access. All outstanding
        // references to items must have been dropped by the caller.

        // Free all pages in the current directory.
        let dir = *self.dir.get_mut();
        if !dir.is_null() {
            // SAFETY: `dir` is valid (invariant maintained throughout lifetime).
            let dir_ref = unsafe { &*dir };
            for i in 0..dir_ref.capacity() {
                let page = dir_ref.slots[i].load(Ordering::Relaxed);
                if !page.is_null() {
                    // SAFETY: Each non-null page was allocated by `PageDir::alloc_page`.
                    unsafe { PageDir::<T>::dealloc_page(page) };
                }
            }
            // SAFETY: `dir` was created by `Box::into_raw`.
            drop(unsafe { Box::from_raw(dir) });
        }

        // Free retired directories (they don't own the pages — only the
        // current directory does).
        let retired = self.retired_dirs.get_mut().unwrap_or_else(|e| e.into_inner());
        for old_dir in retired.drain(..) {
            // SAFETY: Each retired dir was created by `Box::into_raw` in
            // `expand_directory` and is still valid.
            drop(unsafe { Box::from_raw(old_dir.as_ptr()) });
        }
    }
}

impl<T> std::fmt::Debug for MallocFixedPageSize<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.count.load(Ordering::Relaxed);
        let page_idx = count >> ITEMS_PER_PAGE_BITS;
        f.debug_struct("MallocFixedPageSize")
            .field("type", &std::any::type_name::<T>())
            .field("item_size", &std::mem::size_of::<T>())
            .field("items_allocated", &self.count())
            .field("pages_touched", &(page_idx + 1))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    /// A test item that is exactly 64 bytes (cache-line sized).
    #[repr(C, align(64))]
    #[derive(Clone)]
    struct TestItem {
        value: u64,
        _pad: [u8; 56],
    }

    impl Default for TestItem {
        fn default() -> Self {
            TestItem {
                value: 0,
                _pad: [0u8; 56],
            }
        }
    }

    const _: () = assert!(std::mem::size_of::<TestItem>() == 64);

    /// Minimal 8-byte item — the smallest allowed size.
    #[repr(C)]
    #[derive(Clone, Default)]
    struct SmallItem {
        value: u64,
    }

    // -- Construction --

    #[test]
    fn new_allocator_is_valid() {
        let alloc: MallocFixedPageSize<TestItem> = MallocFixedPageSize::new();
        assert_eq!(alloc.count(), 0);
    }

    #[test]
    fn debug_format() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        let _ = alloc.allocate();
        let dbg = format!("{alloc:?}");
        assert!(dbg.contains("MallocFixedPageSize"));
        assert!(dbg.contains("items_allocated"));
    }

    // -- Single-threaded allocation --

    #[test]
    fn allocate_returns_valid_addresses() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        for _ in 0..100 {
            let addr = alloc.allocate();
            assert!(addr.is_valid(), "allocated address must be valid");
        }
    }

    #[test]
    fn all_addresses_unique() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        let mut addrs: Vec<u64> = (0..10_000).map(|_| alloc.allocate().raw()).collect();
        addrs.sort_unstable();
        addrs.dedup();
        assert_eq!(addrs.len(), 10_000, "all 10K addresses must be unique");
    }

    #[test]
    fn get_returns_zeroed_item() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        let addr = alloc.allocate();
        let item = alloc.get(addr);
        assert_eq!(item.value, 0, "fresh allocation from zeroed page should be 0");
    }

    #[test]
    fn get_mut_allows_write() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        let addr = alloc.allocate();
        // SAFETY: Single-threaded test — exclusive access guaranteed.
        unsafe { alloc.get_mut(addr).value = 42 };
        assert_eq!(alloc.get(addr).value, 42);
    }

    #[test]
    fn count_tracks_allocations() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        assert_eq!(alloc.count(), 0);
        for i in 1..=50 {
            let _ = alloc.allocate();
            assert_eq!(alloc.count(), i);
        }
    }

    #[test]
    fn address_encoding_roundtrip() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        for _ in 0..1000 {
            let addr = alloc.allocate();
            let page = addr.page().0 as usize;
            let offset = addr.offset().0 as usize;
            assert!(offset < ITEMS_PER_PAGE);
            // Reconstruct and verify
            let reconstructed = LogicalAddress::new(Page(page as u32), Offset(offset as u32));
            assert_eq!(addr, reconstructed);
        }
    }

    // -- Free list --

    #[test]
    fn free_and_reallocate_reuses_address() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();

        // Allocate an item, write to it, then free it.
        let addr = alloc.allocate();
        // SAFETY: single-threaded test.
        unsafe { alloc.get_mut(addr).value = 99 };
        alloc.free(addr);

        // Next allocation should reuse the freed address.
        let reused = alloc.allocate();
        assert_eq!(reused, addr, "free list should reuse the freed address");
    }

    #[test]
    fn free_list_lifo_order() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        let a = alloc.allocate();
        let b = alloc.allocate();
        let c = alloc.allocate();

        // Free in order a, b, c → reallocation should return c, b, a (LIFO).
        alloc.free(a);
        alloc.free(b);
        alloc.free(c);

        assert_eq!(alloc.allocate(), c);
        assert_eq!(alloc.allocate(), b);
        assert_eq!(alloc.allocate(), a);
    }

    #[test]
    fn free_list_no_new_pages() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();

        // Allocate 1000 items.
        let addrs: Vec<LogicalAddress> = (0..1000).map(|_| alloc.allocate()).collect();
        let count_after_alloc = alloc.count();

        // Free all 1000.
        for &addr in &addrs {
            alloc.free(addr);
        }

        // Reallocate 1000 — count should not increase (all from free list).
        for _ in 0..1000 {
            let _ = alloc.allocate();
        }
        assert_eq!(
            alloc.count(),
            count_after_alloc,
            "reallocation from free list should not bump the counter"
        );
    }

    // -- Page boundary crossing --

    #[test]
    fn cross_page_boundary() {
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        // Allocate enough to cross into page 1.
        // Counter starts at 2, so we need ITEMS_PER_PAGE - 2 more to fill page 0.
        let to_fill_page_0 = ITEMS_PER_PAGE - 2;
        for _ in 0..to_fill_page_0 {
            let addr = alloc.allocate();
            assert_eq!(addr.page(), Page(0));
        }
        // Next allocation should be on page 1.
        let first_on_page1 = alloc.allocate();
        assert_eq!(first_on_page1.page(), Page(1));
        assert_eq!(first_on_page1.offset(), Offset(0));
        // And it should be readable.
        let item = alloc.get(first_on_page1);
        assert_eq!(item.value, 0);
    }

    // -- Directory growth --

    #[test]
    fn directory_expansion() {
        // Force directory growth by allocating across many pages.
        // INITIAL_DIR_CAPACITY = 16, so we need > 16 pages.
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        let target_pages = INITIAL_DIR_CAPACITY + 4;
        let items_needed = target_pages * ITEMS_PER_PAGE;

        // Use bump counter directly to skip to the right flat index.
        // We can do this by just allocating items_needed items.
        // For efficiency, jump the counter instead of allocating one-by-one.
        alloc
            .count
            .store((items_needed as u64) + 2, Ordering::Relaxed);

        // Now allocate one item — this should trigger directory growth.
        let addr = alloc.allocate();
        assert_eq!(addr.page().0 as usize, target_pages);
        assert!(alloc.get(addr).value == 0);
    }

    // -- Alignment --

    #[test]
    fn page_alignment() {
        let alloc: MallocFixedPageSize<TestItem> = MallocFixedPageSize::new();
        let addr = alloc.allocate();
        let ptr = alloc.get_ptr(addr);
        assert!(
            (ptr as usize) % std::mem::align_of::<TestItem>() == 0,
            "item pointer must be aligned to align_of::<T>()"
        );
        assert!(
            (ptr as usize) % PAGE_ALIGNMENT == 0 || std::mem::align_of::<TestItem>() < PAGE_ALIGNMENT,
            "first item on page should be cache-line aligned"
        );
    }

    #[test]
    fn every_allocation_aligned() {
        let alloc: MallocFixedPageSize<TestItem> = MallocFixedPageSize::new();
        for _ in 0..200 {
            let addr = alloc.allocate();
            let ptr = alloc.get_ptr(addr);
            assert_eq!(
                (ptr as usize) % std::mem::align_of::<TestItem>(),
                0,
                "every item must be properly aligned"
            );
        }
    }

    // -- Drop / no leak --

    #[test]
    fn drop_frees_all_pages() {
        // This test primarily checks that Drop doesn't panic/crash.
        // Miri would catch actual leaks.
        let alloc: MallocFixedPageSize<SmallItem> = MallocFixedPageSize::new();
        for _ in 0..5000 {
            let _ = alloc.allocate();
        }
        // Force directory growth.
        let big_flat = (INITIAL_DIR_CAPACITY as u64 + 2) * (ITEMS_PER_PAGE as u64) + 2;
        alloc.count.store(big_flat, Ordering::Relaxed);
        let _ = alloc.allocate();
        drop(alloc); // should not panic or leak
    }

    // -- Concurrent stress test --

    #[test]
    fn concurrent_allocate_stress() {
        let alloc = Arc::new(MallocFixedPageSize::<SmallItem>::new());
        let num_threads = 8;
        let allocs_per_thread = 10_000;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    let mut addrs = Vec::with_capacity(allocs_per_thread);
                    for _ in 0..allocs_per_thread {
                        addrs.push(alloc.allocate().raw());
                    }
                    addrs
                })
            })
            .collect();

        let mut all_addrs: Vec<u64> = Vec::with_capacity(num_threads * allocs_per_thread);
        for h in handles {
            all_addrs.extend(h.join().unwrap());
        }

        // All addresses must be unique.
        all_addrs.sort_unstable();
        all_addrs.dedup();
        assert_eq!(
            all_addrs.len(),
            num_threads * allocs_per_thread,
            "concurrent allocations must produce unique addresses"
        );
    }

    #[test]
    fn concurrent_allocate_free_stress() {
        let alloc = Arc::new(MallocFixedPageSize::<SmallItem>::new());
        let num_threads = 8;
        let ops_per_thread = 5_000;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    let mut live: Vec<LogicalAddress> = Vec::new();
                    for i in 0..ops_per_thread {
                        if i % 3 == 0 && !live.is_empty() {
                            // Free the most recently allocated item.
                            let addr = live.pop().unwrap();
                            alloc.free(addr);
                        } else {
                            let addr = alloc.allocate();
                            assert!(addr.is_valid());
                            live.push(addr);
                        }
                    }
                    live
                })
            })
            .collect();

        // Collect all live addresses and verify readability.
        for h in handles {
            let live = h.join().unwrap();
            for addr in live {
                let _ = alloc.get(addr);
            }
        }
    }

    #[test]
    fn concurrent_free_list_contention() {
        // Hammer the free list from multiple threads to stress the Treiber stack.
        let alloc = Arc::new(MallocFixedPageSize::<SmallItem>::new());

        // Pre-allocate items.
        let n = 4000;
        let addrs: Vec<LogicalAddress> = (0..n).map(|_| alloc.allocate()).collect();

        // Free all items.
        for &addr in &addrs {
            alloc.free(addr);
        }

        // Now have multiple threads race to allocate from the free list.
        let num_threads = 8;
        let per_thread = n / num_threads;
        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    let mut got = Vec::with_capacity(per_thread);
                    for _ in 0..per_thread {
                        got.push(alloc.allocate().raw());
                    }
                    got
                })
            })
            .collect();

        let mut all: Vec<u64> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "all reused addresses must be unique");
    }

    // -- Atomic item test (mirrors real HashBucket usage) --

    #[test]
    fn atomic_items_concurrent_access() {
        /// Mimics a simplified hash bucket with atomic interior.
        #[repr(C, align(64))]
        struct AtomicItem {
            counter: AtomicU64,
            _pad: [u8; 56],
        }

        let alloc = Arc::new(MallocFixedPageSize::<AtomicItem>::new());
        let addr = alloc.allocate();

        let num_threads = 8;
        let increments = 10_000;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    for _ in 0..increments {
                        let item = alloc.get(addr);
                        item.counter.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let final_val = alloc.get(addr).counter.load(Ordering::Relaxed);
        assert_eq!(
            final_val,
            (num_threads * increments) as u64,
            "all atomic increments must be visible"
        );
    }
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// 8-byte test item for property tests.
    #[repr(C)]
    #[derive(Clone, Default)]
    struct Item8(u64);

    proptest! {
        #[test]
        fn allocate_n_all_unique(n in 1usize..5000) {
            let alloc = MallocFixedPageSize::<Item8>::new();
            let mut addrs: Vec<u64> = (0..n).map(|_| alloc.allocate().raw()).collect();
            addrs.sort_unstable();
            addrs.dedup();
            prop_assert_eq!(addrs.len(), n);
        }

        #[test]
        fn allocate_n_all_valid(n in 1usize..5000) {
            let alloc = MallocFixedPageSize::<Item8>::new();
            for _ in 0..n {
                let addr = alloc.allocate();
                prop_assert!(addr.is_valid());
                prop_assert!(addr.offset().0 < ITEMS_PER_PAGE as u32);
            }
        }

        #[test]
        fn free_and_realloc_no_leak(n in 1usize..2000) {
            let alloc = MallocFixedPageSize::<Item8>::new();
            let addrs: Vec<_> = (0..n).map(|_| alloc.allocate()).collect();
            let count_before = alloc.count();
            for &a in &addrs {
                alloc.free(a);
            }
            for _ in 0..n {
                let _ = alloc.allocate();
            }
            // Count should not have increased — all came from free list.
            prop_assert_eq!(alloc.count(), count_before);
        }

        #[test]
        fn get_after_allocate_doesnt_panic(n in 1usize..3000) {
            let alloc = MallocFixedPageSize::<Item8>::new();
            let addrs: Vec<_> = (0..n).map(|_| alloc.allocate()).collect();
            for &addr in &addrs {
                let _ = alloc.get(addr);
            }
        }
    }
}
