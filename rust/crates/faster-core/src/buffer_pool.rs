//! Sector-aligned buffer pool for I/O operations.
//!
//! Provides [`AlignedBuffer`], a RAII wrapper over a heap allocation whose
//! base address is aligned to a power-of-two boundary (typically the storage
//! device sector size — 512 or 4096 bytes). Buffers are reused through
//! [`BufferPool`], which maintains per-size-class free lists to avoid
//! repeated calls to the system allocator on the I/O path.
//!
//! # Size classes
//!
//! Buffers are bucketed into size classes that are powers of two starting at
//! `sector_size`:
//!
//! | class | size                 |
//! |-------|----------------------|
//! | 0     | `sector_size`        |
//! | 1     | `2 × sector_size`    |
//! | N     | `sector_size × 2^N`  |
//!
//! The maximum size class covers up to 64 MiB, which is sufficient for full
//! page flushes at 32 MiB page size.

use std::alloc::{Layout, alloc, dealloc};
use std::ptr::NonNull;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// AlignedBuffer
// ---------------------------------------------------------------------------

/// A RAII wrapper for a sector-aligned heap allocation.
///
/// The allocation is freed when the buffer is dropped. The buffer is `Send`
/// and `Sync` because it exclusively owns the allocation.
pub struct AlignedBuffer {
    ptr: NonNull<u8>,
    len: usize,
    layout: Layout,
}

// SAFETY: `AlignedBuffer` exclusively owns its heap allocation. No aliased
// mutable access is possible, so sending/sharing across threads is safe.
unsafe impl Send for AlignedBuffer {}
// SAFETY: See `Send` impl above — exclusive ownership with no interior
// mutability.
unsafe impl Sync for AlignedBuffer {}

impl AlignedBuffer {
    /// Allocate `size` bytes of memory aligned to `alignment`.
    ///
    /// # Panics
    ///
    /// Panics if `size` is zero, if `alignment` is not a power of two, or if
    /// the system allocator returns null.
    pub fn new(size: usize, alignment: usize) -> Self {
        assert!(size > 0, "buffer size must be > 0");
        assert!(
            alignment.is_power_of_two(),
            "alignment must be a power of two"
        );

        let layout =
            Layout::from_size_align(size, alignment).expect("invalid layout for size/alignment");

        // SAFETY: `layout` has non-zero size (checked above) and a valid
        // power-of-two alignment. The returned pointer is non-null or we
        // panic via `handle_alloc_error`.
        let raw = unsafe { alloc(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));

        Self {
            ptr,
            len: size,
            layout,
        }
    }

    /// Returns a raw const pointer to the buffer.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Returns a raw mutable pointer to the buffer.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Returns the length of the buffer in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the buffer has length zero.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the buffer contents as a byte slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` bytes and properly aligned. The
        // borrow lifetime is tied to `&self`, so no mutable aliasing can
        // occur while this slice is live.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Returns the buffer contents as a mutable byte slice.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `len` bytes and properly aligned. The
        // borrow lifetime is tied to `&mut self`, guaranteeing exclusive
        // access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // SAFETY: `ptr` was allocated with `self.layout` via `std::alloc::alloc`
        // and has not been deallocated yet (this is the only deallocation site).
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

// ---------------------------------------------------------------------------
// BufferPool
// ---------------------------------------------------------------------------

/// Maximum buffer size supported by the pool (64 MiB).
const MAX_BUFFER_SIZE: usize = 64 * 1024 * 1024;

/// Pool of reusable sector-aligned buffers, bucketed by size class.
///
/// Each size class has its own `Mutex<Vec<AlignedBuffer>>` free list.
/// Acquiring a buffer pops from the appropriate list (or allocates fresh),
/// and releasing pushes it back (or drops it if the list is full).
pub struct BufferPool {
    free_lists: Vec<Mutex<Vec<AlignedBuffer>>>,
    sector_size: usize,
    max_pool_size_per_class: usize,
}

/// Snapshot of buffer counts per size class, for metrics / diagnostics.
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// `(size_in_bytes, free_buffer_count)` for each size class.
    pub classes: Vec<(usize, usize)>,
}

impl BufferPool {
    /// Create a new buffer pool.
    ///
    /// `sector_size` must be a power of two (typically 512 or 4096).
    /// `max_pool_size_per_class` is the maximum number of buffers retained in
    /// each size class free list.
    ///
    /// # Panics
    ///
    /// Panics if `sector_size` is zero or not a power of two.
    pub fn new(sector_size: usize, max_pool_size_per_class: usize) -> Self {
        assert!(sector_size > 0, "sector_size must be > 0");
        assert!(
            sector_size.is_power_of_two(),
            "sector_size must be a power of two"
        );

        let num_classes = num_size_classes(sector_size);
        let free_lists = (0..num_classes).map(|_| Mutex::new(Vec::new())).collect();

        Self {
            free_lists,
            sector_size,
            max_pool_size_per_class,
        }
    }

    /// Acquire a buffer of at least `min_size` bytes, aligned to the pool's
    /// sector size.
    ///
    /// The returned buffer's length will be rounded up to the next size class
    /// (a power of two ≥ `sector_size`). Buffer contents are **not** zeroed;
    /// the caller is responsible for writing before reading.
    ///
    /// # Panics
    ///
    /// Panics if `min_size` is zero or exceeds the maximum supported size.
    pub fn acquire(&self, min_size: usize) -> AlignedBuffer {
        assert!(min_size > 0, "min_size must be > 0");

        let class = size_to_class(min_size, self.sector_size);
        let buf_size = class_to_size(class, self.sector_size);

        assert!(
            buf_size <= MAX_BUFFER_SIZE,
            "requested size {min_size} exceeds maximum {MAX_BUFFER_SIZE}"
        );

        // Try to pop a buffer from the free list.
        if class < self.free_lists.len() {
            let mut list = self.free_lists[class]
                .lock()
                .expect("buffer pool mutex poisoned");
            if let Some(buf) = list.pop() {
                return buf;
            }
        }

        // Free list empty — allocate a fresh buffer.
        AlignedBuffer::new(buf_size, self.sector_size)
    }

    /// Return a buffer to the pool for future reuse.
    ///
    /// If the free list for the buffer's size class is already at capacity,
    /// the buffer is dropped immediately.
    pub fn release(&self, buffer: AlignedBuffer) {
        let class = size_to_class(buffer.len(), self.sector_size);

        if class < self.free_lists.len() {
            let mut list = self.free_lists[class]
                .lock()
                .expect("buffer pool mutex poisoned");
            if list.len() < self.max_pool_size_per_class {
                list.push(buffer);
                return;
            }
        }
        // Excess or unknown class — buffer is dropped here.
        drop(buffer);
    }

    /// Returns a snapshot of the pool's per-class buffer counts.
    pub fn pool_stats(&self) -> PoolStats {
        let classes = self
            .free_lists
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let count = m.lock().expect("buffer pool mutex poisoned").len();
                (class_to_size(i, self.sector_size), count)
            })
            .collect();
        PoolStats { classes }
    }

    /// Returns the sector size this pool was configured with.
    #[inline]
    pub fn sector_size(&self) -> usize {
        self.sector_size
    }
}

// ---------------------------------------------------------------------------
// Size-class helpers
// ---------------------------------------------------------------------------

/// Returns the number of size classes for the given sector size.
///
/// Classes cover powers of two from `sector_size` up to (and including)
/// [`MAX_BUFFER_SIZE`].
fn num_size_classes(sector_size: usize) -> usize {
    // sector_size * 2^N = MAX_BUFFER_SIZE → N = log2(MAX_BUFFER_SIZE / sector_size)
    // We add 1 because class 0 corresponds to sector_size itself.
    (MAX_BUFFER_SIZE / sector_size).trailing_zeros() as usize + 1
}

/// Map an arbitrary byte size to the appropriate size class index.
///
/// The size is rounded up to the next power of two that is ≥ `sector_size`,
/// then converted to a class index where class 0 = `sector_size`.
pub fn size_to_class(size: usize, sector_size: usize) -> usize {
    let effective = size.max(sector_size);
    let rounded = effective.next_power_of_two();
    // rounded / sector_size is a power of two; its log2 is the class.
    (rounded / sector_size).trailing_zeros() as usize
}

/// Convert a size class index back to the buffer size in bytes.
#[inline]
fn class_to_size(class: usize, sector_size: usize) -> usize {
    sector_size << class
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buffer_basic() {
        let mut buf = AlignedBuffer::new(4096, 4096);
        assert_eq!(buf.len(), 4096);
        assert!(!buf.is_empty());

        // Verify alignment.
        assert_eq!(buf.as_ptr() as usize % 4096, 0);

        // Write and read back.
        let slice = buf.as_mut_slice();
        slice[0] = 0xAA;
        slice[4095] = 0xBB;
        assert_eq!(buf.as_slice()[0], 0xAA);
        assert_eq!(buf.as_slice()[4095], 0xBB);
    }

    #[test]
    fn aligned_buffer_various_sizes() {
        for &size in &[512, 4096, 32768, 1024 * 1024] {
            let buf = AlignedBuffer::new(size, 512);
            assert_eq!(buf.len(), size);
            assert_eq!(buf.as_ptr() as usize % 512, 0);
        }
    }

    #[test]
    fn aligned_buffer_alignment_check() {
        for &alignment in &[512, 4096, 8192] {
            let buf = AlignedBuffer::new(alignment, alignment);
            assert_eq!(
                buf.as_ptr() as usize % alignment,
                0,
                "pointer must be aligned to {alignment}"
            );
        }
    }

    #[test]
    fn pool_acquire_release_reuse() {
        let pool = BufferPool::new(4096, 8);

        // Acquire a buffer and record its pointer.
        let buf = pool.acquire(4096);
        let ptr = buf.as_ptr();
        pool.release(buf);

        // Stats should show 1 buffer in class 0.
        let stats = pool.pool_stats();
        assert_eq!(stats.classes[0].1, 1, "free list should have 1 buffer");

        // Acquire again — should reuse the same allocation.
        let buf2 = pool.acquire(4096);
        assert_eq!(buf2.as_ptr(), ptr, "should reuse the released buffer");

        // Free list should now be empty.
        let stats = pool.pool_stats();
        assert_eq!(
            stats.classes[0].1, 0,
            "free list should be empty after acquire"
        );
    }

    #[test]
    fn pool_size_class_routing() {
        let pool = BufferPool::new(512, 8);

        // sector_size=512 → class 0 = 512, class 1 = 1024, class 2 = 2048, class 3 = 4096
        let buf_512 = pool.acquire(512);
        assert_eq!(buf_512.len(), 512);

        let buf_1000 = pool.acquire(1000);
        assert_eq!(buf_1000.len(), 1024, "1000 rounds up to 1024 (class 1)");

        let buf_4096 = pool.acquire(4096);
        assert_eq!(buf_4096.len(), 4096, "4096 is exact class 3");

        let buf_5000 = pool.acquire(5000);
        assert_eq!(buf_5000.len(), 8192, "5000 rounds up to 8192 (class 4)");

        // Release all and verify they go to different classes.
        pool.release(buf_512);
        pool.release(buf_1000);
        pool.release(buf_4096);
        pool.release(buf_5000);

        let stats = pool.pool_stats();
        // class 0 = 512, class 1 = 1024, class 3 = 4096, class 4 = 8192
        assert_eq!(stats.classes[0].1, 1, "class 0 (512) should have 1");
        assert_eq!(stats.classes[1].1, 1, "class 1 (1024) should have 1");
        assert_eq!(stats.classes[3].1, 1, "class 3 (4096) should have 1");
        assert_eq!(stats.classes[4].1, 1, "class 4 (8192) should have 1");
    }

    #[test]
    fn pool_cap_enforcement() {
        let pool = BufferPool::new(4096, 2);

        // Acquire and release 4 buffers (cap = 2).
        let bufs: Vec<_> = (0..4).map(|_| pool.acquire(4096)).collect();
        for buf in bufs {
            pool.release(buf);
        }

        // Only 2 should be retained.
        let stats = pool.pool_stats();
        assert_eq!(
            stats.classes[0].1, 2,
            "pool should cap at max_pool_size_per_class=2"
        );
    }

    #[test]
    fn pool_concurrent_access() {
        use std::sync::Arc;

        let pool = Arc::new(BufferPool::new(4096, 16));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let pool = Arc::clone(&pool);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let mut buf = pool.acquire(4096);
                    // Write a pattern.
                    buf.as_mut_slice()[0] = 0xFF;
                    pool.release(buf);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        // Pool should still be consistent.
        let stats = pool.pool_stats();
        assert!(
            stats.classes[0].1 <= 16,
            "pool should not exceed max_pool_size_per_class"
        );
    }

    #[test]
    fn size_to_class_basic() {
        // sector_size = 512
        assert_eq!(size_to_class(512, 512), 0);
        assert_eq!(size_to_class(1024, 512), 1);
        assert_eq!(size_to_class(2048, 512), 2);
        assert_eq!(size_to_class(4096, 512), 3);
        // Round-up cases
        assert_eq!(size_to_class(513, 512), 1);
        assert_eq!(
            size_to_class(1, 512),
            0,
            "anything < sector_size maps to class 0"
        );
        assert_eq!(size_to_class(100, 4096), 0, "100 < 4096 maps to class 0");
    }

    #[test]
    fn num_size_classes_correctness() {
        // 512 → 64 MiB: 64*1024*1024 / 512 = 131072 = 2^17, so 17 + 1 = 18 classes.
        assert_eq!(num_size_classes(512), 18);
        // 4096 → 64 MiB: 64*1024*1024 / 4096 = 16384 = 2^14, so 14 + 1 = 15 classes.
        assert_eq!(num_size_classes(4096), 15);
    }

    #[test]
    #[should_panic(expected = "sector_size must be a power of two")]
    fn pool_rejects_non_power_of_two_sector() {
        BufferPool::new(1000, 4);
    }

    #[test]
    #[should_panic(expected = "buffer size must be > 0")]
    fn aligned_buffer_rejects_zero_size() {
        AlignedBuffer::new(0, 512);
    }
}
