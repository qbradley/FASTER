//! Overflow bucket pool for the FASTER hash index.
//!
//! When all 7 data slots in a [`HashBucket`] are occupied, FASTER chains to
//! overflow buckets allocated from this pool. The pool wraps
//! [`MallocFixedPageSize<HashBucket>`] and provides:
//!
//! - **Allocation:** Fresh, zero-initialized buckets via the bump allocator or
//!   free-list reuse.
//! - **Resolution:** Convert a [`LogicalAddress`] to a `&HashBucket` reference.
//! - **Deallocation:** Return buckets to the free list for reuse.
//!
//! # Design rationale
//!
//! Overflow buckets cannot come from the system allocator (`malloc`) because:
//! 1. **Performance:** Overflow allocation is on the insert hot path. The pool
//!    uses lock-free bump allocation (fast) or free-list pop (faster).
//! 2. **Determinism:** Bounded, predictable memory usage.
//! 3. **Addressability:** Buckets are addressed via [`LogicalAddress`], which
//!    `MallocFixedPageSize` natively supports.
//!
//! # Concurrency
//!
//! The pool is `Send + Sync`. All operations are lock-free on the common path.
//! Multiple threads may concurrently allocate, resolve, and free overflow
//! buckets without contention (beyond the underlying atomic operations in the
//! allocator).
//!
//! # C++ correspondence
//!
//! | Rust                   | C++ (`malloc_fixed_page_size.h`)           |
//! |------------------------|--------------------------------------------|
//! | `OverflowBucketPool`   | `MallocFixedPageSize<HashBucket>`          |
//! | `allocate()`           | `Allocate()`                               |
//! | `get(addr)`            | `Get(addr)`                                |
//! | `free(addr)`           | `Free(addr)`                               |

use core::sync::atomic::Ordering;

use crate::address::LogicalAddress;
use crate::allocator::MallocFixedPageSize;
use crate::hash_bucket::HashBucket;

/// A pool of overflow [`HashBucket`]s for the hash index.
///
/// Wraps [`MallocFixedPageSize<HashBucket>`] to provide typed, zero-initialized
/// overflow bucket allocation. Overflow buckets are chained from the primary
/// hash bucket's overflow pointer when all 7 data slots are full.
///
/// # Examples
///
/// ```
/// use faster_core::overflow::OverflowBucketPool;
/// use faster_core::hash_bucket::HashBucket;
/// use core::sync::atomic::Ordering;
///
/// let pool = OverflowBucketPool::new();
/// let addr = pool.allocate();
/// assert!(addr.is_valid());
///
/// // The allocated bucket is zero-initialized (all entries empty).
/// let bucket: &HashBucket = pool.get(addr);
/// assert!(bucket.entry(0).load(Ordering::Relaxed).is_empty());
///
/// // Return to pool when no longer needed.
/// pool.free(addr);
/// ```
pub struct OverflowBucketPool {
    allocator: MallocFixedPageSize<HashBucket>,
}

impl OverflowBucketPool {
    /// Creates a new overflow bucket pool.
    ///
    /// Pre-allocates the first page of buckets (1,048,576 × 64 bytes = 64 MB).
    pub fn new() -> Self {
        Self {
            allocator: MallocFixedPageSize::new(),
        }
    }

    /// Allocates a fresh, zero-initialized overflow bucket.
    ///
    /// Returns a [`LogicalAddress`] that can be stored in a bucket's overflow
    /// pointer and later resolved back to a `&HashBucket` via [`get`](Self::get).
    ///
    /// **Fast path:** Reuses a previously freed bucket from the free list.
    /// When reused, the bucket is explicitly re-zeroed to ensure all entries
    /// are empty and the overflow pointer is null.
    ///
    /// **Slow path:** Bump-allocates from a fresh, zeroed page.
    ///
    /// # Panics
    ///
    /// Panics on pool exhaustion (extremely unlikely — requires > 8M pages).
    pub fn allocate(&self) -> LogicalAddress {
        let addr = self.allocator.allocate();

        // Re-zero the bucket. Fresh bump-allocated pages are already zeroed,
        // but free-list reuse may return a bucket with stale entries.
        // The cost of redundant zeroing is negligible compared to the
        // allocation itself and the subsequent cache miss for the bucket.
        let bucket = self.allocator.get(addr);
        for i in 0..crate::hash_bucket::BUCKET_NUM_ENTRIES {
            bucket
                .entry(i)
                .store(crate::hash_bucket::HashBucketEntry::EMPTY, Ordering::Relaxed);
        }
        bucket
            .overflow_address()
            .store(LogicalAddress::ZERO, Ordering::Relaxed);

        // Ensure all zeroed entries are visible before this bucket is installed
        // into an overflow chain (via a Release store of the overflow pointer).
        // Without this fence, a concurrent reader on ARM/POWER could observe
        // stale entry data from the bucket's previous lifetime.
        core::sync::atomic::fence(Ordering::Release);

        addr
    }

    /// Resolves a [`LogicalAddress`] to a shared reference to the overflow bucket.
    ///
    /// # Panics (debug)
    ///
    /// Debug-panics if `addr` refers to an unallocated page.
    #[inline]
    pub fn get(&self, addr: LogicalAddress) -> &HashBucket {
        self.allocator.get(addr)
    }

    /// Returns a bucket to the pool for reuse.
    ///
    /// The bucket will be re-zeroed on the next [`allocate`](Self::allocate) call.
    ///
    /// # Safety contract (caller must uphold)
    ///
    /// - `addr` was returned by [`allocate`](Self::allocate) on this pool.
    /// - No thread will access the bucket after this call.
    /// - The bucket is not referenced by any overflow chain.
    pub fn free(&self, addr: LogicalAddress) {
        self.allocator.free(addr);
    }

    /// Returns the total number of overflow buckets ever allocated.
    ///
    /// This includes buckets that were subsequently freed (returned to the
    /// free list). It is a diagnostic metric, not a precise live-count.
    #[inline]
    pub fn allocated_count(&self) -> u64 {
        self.allocator.count()
    }
}

impl Default for OverflowBucketPool {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for OverflowBucketPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "OverflowBucketPool {{ allocated: {} }}",
            self.allocator.count()
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_bucket::{HashBucketEntry, BUCKET_NUM_ENTRIES};
    use core::sync::atomic::Ordering;

    #[test]
    fn allocate_returns_valid_address() {
        let pool = OverflowBucketPool::new();
        let addr = pool.allocate();
        assert!(addr.is_valid());
    }

    #[test]
    fn allocated_bucket_is_zeroed() {
        let pool = OverflowBucketPool::new();
        let addr = pool.allocate();
        let bucket = pool.get(addr);

        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(
                bucket.entry(i).load(Ordering::Relaxed).is_empty(),
                "Entry {i} should be empty in a freshly allocated overflow bucket"
            );
        }
        assert_eq!(
            bucket.overflow_address().load(Ordering::Relaxed),
            LogicalAddress::ZERO,
        );
    }

    #[test]
    fn allocated_bucket_is_cache_line_aligned() {
        let pool = OverflowBucketPool::new();
        let addr = pool.allocate();
        let bucket = pool.get(addr);
        let ptr = bucket as *const HashBucket as usize;
        assert_eq!(ptr % 64, 0, "Overflow bucket at {ptr:#x} is not 64-byte aligned");
    }

    #[test]
    fn free_and_reallocate_is_zeroed() {
        let pool = OverflowBucketPool::new();
        let addr = pool.allocate();

        // Dirty the bucket.
        let bucket = pool.get(addr);
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::from_raw(42), false);
        bucket.entry(0).store(entry, Ordering::Relaxed);
        bucket.entry(3).store(entry, Ordering::Relaxed);
        bucket
            .overflow_address()
            .store(LogicalAddress::from_raw(99), Ordering::Relaxed);

        // Free and reallocate (should get the same address from free list).
        pool.free(addr);
        let addr2 = pool.allocate();

        // The re-allocated bucket must be clean regardless of address.
        let bucket2 = pool.get(addr2);
        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(
                bucket2.entry(i).load(Ordering::Relaxed).is_empty(),
                "Entry {i} should be empty after free+reallocate"
            );
        }
        assert_eq!(
            bucket2.overflow_address().load(Ordering::Relaxed),
            LogicalAddress::ZERO,
        );
    }

    #[test]
    fn multiple_allocations_are_distinct() {
        let pool = OverflowBucketPool::new();
        let a1 = pool.allocate();
        let a2 = pool.allocate();
        let a3 = pool.allocate();
        assert_ne!(a1, a2);
        assert_ne!(a2, a3);
        assert_ne!(a1, a3);
    }

    #[test]
    fn many_allocations() {
        let pool = OverflowBucketPool::new();
        let mut addrs = Vec::new();
        for _ in 0..1000 {
            let addr = pool.allocate();
            assert!(addr.is_valid());
            addrs.push(addr);
        }
        // All distinct.
        addrs.sort_by_key(|a| a.raw());
        addrs.dedup_by_key(|a| a.raw());
        assert_eq!(addrs.len(), 1000);
    }

    #[test]
    fn concurrent_allocate_and_free() {
        use std::sync::Arc;
        use std::thread;

        let pool = Arc::new(OverflowBucketPool::new());
        let num_threads = 8;
        let ops_per_thread = 500;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let pool = Arc::clone(&pool);
                thread::spawn(move || {
                    let mut addrs = Vec::with_capacity(ops_per_thread);
                    for _ in 0..ops_per_thread {
                        addrs.push(pool.allocate());
                    }
                    // Verify all are valid and zeroed.
                    for &addr in &addrs {
                        let bucket = pool.get(addr);
                        for i in 0..BUCKET_NUM_ENTRIES {
                            assert!(bucket.entry(i).load(Ordering::Relaxed).is_empty());
                        }
                    }
                    // Free half.
                    for &addr in addrs.iter().take(ops_per_thread / 2) {
                        pool.free(addr);
                    }
                    addrs
                })
            })
            .collect();

        let all_addrs: Vec<LogicalAddress> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(all_addrs.len(), num_threads * ops_per_thread);
    }
}
