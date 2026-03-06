//! Pre-registered I/O buffer pool for io_uring.
//!
//! This module provides [`BufferPool`], a pool of sector-aligned buffers that
//! can be registered with io_uring for fixed-buffer I/O. Registration avoids
//! per-I/O page pinning overhead, which is critical for FASTER's
//! high-throughput, direct-I/O workloads.
//!
//! # Buffer Lifecycle
//!
//! 1. **Create** a [`BufferPool`] with the desired count and buffer size.
//! 2. **Register** the pool with a [`Ring`](crate::Ring) via
//!    [`Ring::register_buffers`](crate::Ring::register_buffers).
//! 3. **Checkout** a [`RegisteredBuffer`] from the pool (lock-free).
//! 4. **Use** the buffer for I/O — the [`buf_index`](RegisteredBuffer::buf_index)
//!    identifies it for `ReadFixed` / `WriteFixed` operations.
//! 5. **Return** the buffer by dropping the [`RegisteredBuffer`] (lock-free).
//! 6. **Unregister** via [`Ring::unregister_buffers`](crate::Ring::unregister_buffers)
//!    when done.
//!
//! # Concurrency
//!
//! Checkout and return are lock-free (Treiber stack with ABA-safe tagged
//! pointers). Multiple threads can concurrently check out and return buffers.
//!
//! # Alignment
//!
//! All buffers are aligned to [`SECTOR_ALIGNMENT`] (4096 bytes) for
//! compatibility with `O_DIRECT` I/O.

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::{io, slice};

/// Sector alignment for direct I/O buffers (4 KiB).
///
/// All buffers allocated by [`BufferPool`] have their start address aligned to
/// this value, which is required for `O_DIRECT` I/O on most Linux filesystems.
pub const SECTOR_ALIGNMENT: usize = 4096;

/// Sentinel value indicating an empty stack or end-of-list.
const EMPTY: u32 = u32::MAX;

// ── Lock-free Treiber Stack ────────────────────────────────────────────────

/// An ABA-safe, lock-free stack of `u32` indices.
///
/// Uses a tagged pointer packed into an [`AtomicU64`] where the high 32 bits
/// are a monotonically increasing generation tag and the low 32 bits are the
/// index of the current top element (or [`EMPTY`]).
///
/// Per-node "next" pointers are stored in a parallel [`AtomicU32`] array,
/// avoiding heap allocation for list nodes.
struct TreiberStack {
    head: AtomicU64,
    next: Box<[AtomicU32]>,
}

impl TreiberStack {
    /// Create a stack pre-loaded with indices `0..count`.
    ///
    /// The linked list is: 0 → 1 → 2 → … → (count-1) → EMPTY.
    fn new(count: u32) -> Self {
        let next: Vec<AtomicU32> = (0..count)
            .map(|i| {
                if i + 1 < count {
                    AtomicU32::new(i + 1)
                } else {
                    AtomicU32::new(EMPTY)
                }
            })
            .collect();

        let top = if count > 0 { 0 } else { EMPTY };
        Self {
            head: AtomicU64::new(pack(0, top)),
            next: next.into_boxed_slice(),
        }
    }

    /// Pop the top index. Returns `None` if the stack is empty.
    fn pop(&self) -> Option<u32> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let (tag, top) = unpack(head);
            if top == EMPTY {
                return None;
            }
            // The Acquire on `head` synchronizes with the AcqRel CAS of the
            // push that made `top` the stack head, so this Relaxed load sees
            // the correct `next` value written during that push.
            let next_idx = self.next[top as usize].load(Ordering::Relaxed);
            let new_head = pack(tag.wrapping_add(1), next_idx);
            if self
                .head
                .compare_exchange_weak(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(top);
            }
        }
    }

    /// Push `index` onto the stack.
    fn push(&self, index: u32) {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let (tag, top) = unpack(head);
            // Will become visible to the popper via the Acquire on `head`
            // that synchronizes with the AcqRel CAS below.
            self.next[index as usize].store(top, Ordering::Relaxed);
            let new_head = pack(tag.wrapping_add(1), index);
            if self
                .head
                .compare_exchange_weak(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Returns `true` if the stack appears empty at this instant.
    fn is_empty(&self) -> bool {
        let (_, top) = unpack(self.head.load(Ordering::Acquire));
        top == EMPTY
    }
}

#[inline]
const fn pack(tag: u32, index: u32) -> u64 {
    ((tag as u64) << 32) | (index as u64)
}

#[inline]
const fn unpack(val: u64) -> (u32, u32) {
    ((val >> 32) as u32, val as u32)
}

// ── BufferPool ─────────────────────────────────────────────────────────────

/// A pool of pre-allocated, sector-aligned I/O buffers with lock-free
/// checkout/return semantics.
///
/// Buffers are allocated once at construction and can be registered with
/// io_uring for fixed-buffer I/O via
/// [`Ring::register_buffers`](crate::Ring::register_buffers).
///
/// # Capacity
///
/// Buffer count is limited to [`u16::MAX`] (65 535) because io_uring's
/// `buf_index` field is 16 bits.
///
/// # Drop Safety
///
/// If buffers have been registered with io_uring, the caller **must** call
/// [`Ring::unregister_buffers`](crate::Ring::unregister_buffers) (and ensure
/// all in-flight I/O using these buffers has completed) **before** dropping
/// the pool. Failure to do so results in the kernel holding dangling
/// references.
pub struct BufferPool {
    /// Raw pointers to each aligned buffer allocation.
    buffers: Vec<*mut u8>,
    /// Matching iovec array for `io_uring_register_buffers`.
    iovecs: Vec<libc::iovec>,
    /// Lock-free free-list of buffer indices.
    stack: TreiberStack,
    /// Size of each individual buffer in bytes.
    buf_size: usize,
    /// Total number of buffers in the pool.
    count: u32,
    /// Layout used for each allocation (needed for dealloc).
    layout: Layout,
}

// SAFETY: After construction, the `buffers` and `iovecs` vectors are never
// mutated. Individual buffers are accessed exclusively through
// `RegisteredBuffer`, which the Treiber stack guarantees is handed out to at
// most one owner at a time. This is equivalent to a collection of
// `Box<[u8]>` behind a lock-free allocator, which is naturally Send + Sync.
unsafe impl Send for BufferPool {}

// SAFETY: See Send justification. Concurrent `checkout`/`return_buffer`
// calls are synchronized by the lock-free Treiber stack. Shared references
// (`&self`) never provide mutable access to buffer contents directly.
unsafe impl Sync for BufferPool {}

impl BufferPool {
    /// Allocate a new buffer pool.
    ///
    /// All `count` buffers of `buf_size` bytes are allocated immediately, each
    /// aligned to [`SECTOR_ALIGNMENT`] and zero-initialized.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - `count` is 0 or exceeds [`u16::MAX`]
    /// - `buf_size` is 0
    /// - Memory allocation fails
    pub fn new(count: u32, buf_size: usize) -> io::Result<Self> {
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer count must be > 0",
            ));
        }
        if count > u16::MAX as u32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("buffer count {count} exceeds u16::MAX (io_uring buf_index limit)"),
            ));
        }
        if buf_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffer size must be > 0",
            ));
        }

        let layout = Layout::from_size_align(buf_size, SECTOR_ALIGNMENT)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let mut buffers = Vec::with_capacity(count as usize);
        let mut iovecs = Vec::with_capacity(count as usize);

        for i in 0..count {
            // SAFETY: `layout` has non-zero size (checked above) and
            // power-of-two alignment (SECTOR_ALIGNMENT = 4096).
            let ptr = unsafe { alloc::alloc_zeroed(layout) };
            if ptr.is_null() {
                // Clean up already-allocated buffers on OOM.
                for &prev in &buffers {
                    // SAFETY: Each `prev` was returned by a successful
                    // `alloc_zeroed` call with the same `layout`.
                    unsafe {
                        alloc::dealloc(prev, layout);
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    format!("failed to allocate aligned buffer {i}/{count}"),
                ));
            }
            iovecs.push(libc::iovec {
                iov_base: ptr.cast(),
                iov_len: buf_size,
            });
            buffers.push(ptr);
        }

        Ok(Self {
            buffers,
            iovecs,
            stack: TreiberStack::new(count),
            buf_size,
            count,
            layout,
        })
    }

    /// Check out a buffer from the pool.
    ///
    /// Returns `None` if all buffers are currently checked out. The returned
    /// [`RegisteredBuffer`] automatically returns the buffer to the pool when
    /// dropped.
    ///
    /// This operation is lock-free.
    pub fn checkout(&self) -> Option<RegisteredBuffer<'_>> {
        self.stack.pop().map(|index| RegisteredBuffer {
            pool: self,
            index: index as u16,
        })
    }

    /// Return a buffer to the free-list (called by [`RegisteredBuffer::drop`]).
    fn return_buffer(&self, index: u16) {
        self.stack.push(index as u32);
    }

    /// Size of each buffer in the pool, in bytes.
    pub fn buf_size(&self) -> usize {
        self.buf_size
    }

    /// Total number of buffers in the pool.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Returns `true` if the free-list is empty (all buffers checked out).
    pub fn is_exhausted(&self) -> bool {
        self.stack.is_empty()
    }

    /// The iovec array for io_uring buffer registration.
    pub(crate) fn iovecs(&self) -> &[libc::iovec] {
        &self.iovecs
    }
}

impl Drop for BufferPool {
    fn drop(&mut self) {
        for &ptr in &self.buffers {
            // SAFETY: Each pointer was returned by a successful
            // `alloc::alloc_zeroed` call with `self.layout` during
            // construction, and has not been deallocated since.
            unsafe {
                alloc::dealloc(ptr, self.layout);
            }
        }
    }
}

// ── RegisteredBuffer ───────────────────────────────────────────────────────

/// A checked-out buffer from a [`BufferPool`].
///
/// Provides `&[u8]` / `&mut [u8]` access to the buffer and carries the
/// [`buf_index`](Self::buf_index) needed for `ReadFixed` / `WriteFixed`
/// io_uring operations.
///
/// The buffer is automatically returned to the pool when this guard is
/// dropped.
///
/// # I/O Safety
///
/// If this buffer has been submitted for io_uring I/O, the caller **must not**
/// drop it until the corresponding completion has been reaped. Dropping a
/// buffer while the kernel references it is undefined behavior.
pub struct RegisteredBuffer<'pool> {
    pool: &'pool BufferPool,
    index: u16,
}

impl RegisteredBuffer<'_> {
    /// The io_uring buffer index for `ReadFixed` / `WriteFixed` operations.
    pub fn buf_index(&self) -> u16 {
        self.index
    }

    /// Size of this buffer in bytes.
    pub fn len(&self) -> usize {
        self.pool.buf_size
    }

    /// Returns `true` if the buffer has zero length.
    ///
    /// Always `false` because [`BufferPool::new`] rejects `buf_size == 0`.
    pub fn is_empty(&self) -> bool {
        self.pool.buf_size == 0
    }

    /// Raw const pointer to the buffer start (for building io_uring SQEs).
    pub fn as_ptr(&self) -> *const u8 {
        self.pool.buffers[self.index as usize]
    }

    /// Raw mutable pointer to the buffer start (for building io_uring SQEs).
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.pool.buffers[self.index as usize]
    }
}

impl Deref for RegisteredBuffer<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: The buffer pointer was allocated via `alloc_zeroed` with a
        // layout of `(buf_size, SECTOR_ALIGNMENT)`, so it is valid for reads
        // of `buf_size` bytes. The Treiber stack guarantees exclusive checkout,
        // so no other `RegisteredBuffer` aliases this memory. The shared
        // reference `&self` permits only immutable access.
        unsafe { slice::from_raw_parts(self.pool.buffers[self.index as usize], self.pool.buf_size) }
    }
}

impl DerefMut for RegisteredBuffer<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: Same as `Deref`. Additionally, `&mut self` guarantees no
        // other reference to this `RegisteredBuffer` exists, so creating a
        // mutable slice is sound.
        unsafe {
            slice::from_raw_parts_mut(self.pool.buffers[self.index as usize], self.pool.buf_size)
        }
    }
}

impl Drop for RegisteredBuffer<'_> {
    fn drop(&mut self) {
        self.pool.return_buffer(self.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    // ── Treiber Stack ──────────────────────────────────────────────────

    #[test]
    fn treiber_stack_pop_empty() {
        let stack = TreiberStack::new(0);
        assert!(stack.is_empty());
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn treiber_stack_push_pop_single() {
        let stack = TreiberStack::new(1);
        assert!(!stack.is_empty());
        assert_eq!(stack.pop(), Some(0));
        assert!(stack.is_empty());
        assert_eq!(stack.pop(), None);

        stack.push(0);
        assert!(!stack.is_empty());
        assert_eq!(stack.pop(), Some(0));
    }

    #[test]
    fn treiber_stack_pop_all_and_return() {
        let count = 8u32;
        let stack = TreiberStack::new(count);

        let mut popped = HashSet::new();
        for _ in 0..count {
            let idx = stack.pop().expect("should not be empty yet");
            assert!(popped.insert(idx), "duplicate index {idx}");
        }
        assert!(stack.is_empty());
        assert_eq!(popped.len(), count as usize);

        for idx in 0..count {
            stack.push(idx);
        }
        assert!(!stack.is_empty());

        let mut popped2 = HashSet::new();
        for _ in 0..count {
            let idx = stack.pop().expect("should not be empty yet");
            assert!(popped2.insert(idx), "duplicate index {idx}");
        }
        assert_eq!(popped2, popped);
    }

    #[test]
    fn treiber_stack_concurrent_stress() {
        let count = 64u32;
        let stack = Arc::new(TreiberStack::new(count));
        let num_threads = 8;
        let ops_per_thread = 1_000;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let stack = Arc::clone(&stack);
                thread::spawn(move || {
                    for _ in 0..ops_per_thread {
                        if let Some(idx) = stack.pop() {
                            std::hint::black_box(idx);
                            stack.push(idx);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // All indices should be back in the stack.
        let mut recovered = HashSet::new();
        while let Some(idx) = stack.pop() {
            assert!(recovered.insert(idx), "duplicate index {idx}");
        }
        assert_eq!(
            recovered.len(),
            count as usize,
            "all indices should be returned"
        );
    }

    // ── BufferPool (allocation + checkout/return) ──────────────────────

    #[test]
    fn buffer_pool_alignment_invariant() {
        let pool = BufferPool::new(16, 4096).expect("pool creation");
        for i in 0..pool.count() {
            let ptr = pool.buffers[i as usize];
            assert_eq!(
                ptr as usize % SECTOR_ALIGNMENT,
                0,
                "buffer {i} not aligned to {SECTOR_ALIGNMENT}"
            );
        }
    }

    #[test]
    fn buffer_pool_alignment_non_page_size() {
        // Even odd-sized buffers should have aligned start addresses.
        let pool = BufferPool::new(4, 5000).expect("pool creation");
        for i in 0..pool.count() {
            let ptr = pool.buffers[i as usize];
            assert_eq!(ptr as usize % SECTOR_ALIGNMENT, 0, "buffer {i} not aligned");
        }
    }

    #[test]
    fn buffer_pool_checkout_unique() {
        let pool = BufferPool::new(4, 4096).expect("pool creation");
        let b0 = pool.checkout().expect("checkout 0");
        let b1 = pool.checkout().expect("checkout 1");
        let b2 = pool.checkout().expect("checkout 2");
        let b3 = pool.checkout().expect("checkout 3");

        let indices: HashSet<u16> = [
            b0.buf_index(),
            b1.buf_index(),
            b2.buf_index(),
            b3.buf_index(),
        ]
        .into_iter()
        .collect();
        assert_eq!(indices.len(), 4, "all indices should be unique");

        let ptrs: HashSet<usize> = [b0.as_ptr(), b1.as_ptr(), b2.as_ptr(), b3.as_ptr()]
            .into_iter()
            .map(|p| p as usize)
            .collect();
        assert_eq!(ptrs.len(), 4, "all pointers should be unique");

        assert!(pool.checkout().is_none(), "pool should be exhausted");
        assert!(pool.is_exhausted());
    }

    #[test]
    fn buffer_pool_return_on_drop() {
        let pool = BufferPool::new(1, 4096).expect("pool creation");

        {
            let _buf = pool.checkout().expect("checkout");
            assert!(pool.checkout().is_none(), "only 1 buffer");
        }

        let _buf = pool.checkout().expect("should be available after return");
    }

    #[test]
    fn buffer_pool_read_write() {
        let pool = BufferPool::new(1, 4096).expect("pool creation");
        let mut buf = pool.checkout().expect("checkout");

        buf[0] = 0xAA;
        buf[4095] = 0xBB;

        assert_eq!(buf[0], 0xAA);
        assert_eq!(buf[4095], 0xBB);
        assert_eq!(buf.len(), 4096);
        assert!(!buf.is_empty());
    }

    #[test]
    fn buffer_pool_zero_initialized() {
        let pool = BufferPool::new(2, 4096).expect("pool creation");
        let buf = pool.checkout().expect("checkout");
        assert!(
            buf.iter().all(|&b| b == 0),
            "buffers should be zero-initialized"
        );
    }

    #[test]
    fn buffer_pool_invalid_params() {
        assert!(BufferPool::new(0, 4096).is_err(), "count=0 should fail");
        assert!(BufferPool::new(1, 0).is_err(), "buf_size=0 should fail");
        assert!(
            BufferPool::new(u16::MAX as u32 + 1, 4096).is_err(),
            "count > u16::MAX should fail"
        );
    }

    #[test]
    fn buffer_pool_iovecs_match_buffers() {
        let pool = BufferPool::new(4, 8192).expect("pool creation");
        let iovecs = pool.iovecs();

        assert_eq!(iovecs.len(), 4);
        for (i, iov) in iovecs.iter().enumerate() {
            assert_eq!(iov.iov_base as *const u8, pool.buffers[i] as *const u8);
            assert_eq!(iov.iov_len, 8192);
        }
    }

    #[test]
    fn buffer_pool_concurrent_checkout_return() {
        let pool = Arc::new(BufferPool::new(16, 4096).expect("pool creation"));
        let num_threads = 4;
        let ops_per_thread = 500;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let pool = Arc::clone(&pool);
                thread::spawn(move || {
                    for i in 0..ops_per_thread {
                        if let Some(mut buf) = pool.checkout() {
                            let tag = ((t as u16) << 8) | (i as u16 & 0xFF);
                            buf[0] = (tag >> 8) as u8;
                            buf[1] = (tag & 0xFF) as u8;
                            assert_eq!(buf[0], (tag >> 8) as u8);
                            assert_eq!(buf[1], (tag & 0xFF) as u8);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // All buffers should be back.
        let mut count = 0u32;
        while pool.checkout().is_some() {
            count += 1;
        }
        assert_eq!(count, 16, "all 16 buffers should be returned");
    }

    // ── Registration (requires io_uring kernel support) ────────────────

    #[test]
    fn register_unregister_buffers() {
        use crate::{Ring, UringConfig};

        let mut ring = Ring::new(UringConfig::default()).expect("create ring");
        let pool = BufferPool::new(4, 4096).expect("create pool");

        ring.register_buffers(&pool).expect("register buffers");
        ring.unregister_buffers().expect("unregister buffers");
    }

    #[test]
    fn register_many_buffers() {
        use crate::{Ring, UringConfig};

        let mut ring = Ring::new(UringConfig::default()).expect("create ring");
        let pool = BufferPool::new(256, 4096).expect("create pool");

        ring.register_buffers(&pool).expect("register buffers");
        ring.unregister_buffers().expect("unregister buffers");
    }

    #[test]
    fn double_register_fails() {
        use crate::{Ring, UringConfig};

        let mut ring = Ring::new(UringConfig::default()).expect("create ring");
        let pool = BufferPool::new(4, 4096).expect("create pool");

        ring.register_buffers(&pool).expect("first register");
        let result = ring.register_buffers(&pool);
        assert!(result.is_err(), "double register should fail");
        ring.unregister_buffers().expect("unregister");
    }

    #[test]
    fn checkout_after_registration() {
        use crate::{Ring, UringConfig};

        let mut ring = Ring::new(UringConfig::default()).expect("create ring");
        let pool = BufferPool::new(4, 4096).expect("create pool");

        ring.register_buffers(&pool).expect("register");

        let mut buf = pool.checkout().expect("checkout");
        buf[0] = 42;
        assert_eq!(buf[0], 42);
        assert!(buf.buf_index() < 4);
        drop(buf);

        ring.unregister_buffers().expect("unregister");
    }
}
