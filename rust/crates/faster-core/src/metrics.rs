//! Atomic operational counters for observability.
//!
//! [`Metrics`] collects advisory counters for key operations (hash lookups,
//! allocations, epoch bumps, etc.). All increments use [`Relaxed`] ordering
//! — they are cheap, contention-tolerant, and not used for synchronization.
//!
//! Call [`Metrics::snapshot()`] to capture a consistent-ish point-in-time
//! view as plain `u64` values suitable for logging or export.

use crate::sync::{AtomicU64, Ordering};
use std::fmt;

/// Atomic operational counters.
///
/// Designed to live in a shared `Arc<Metrics>` and be incremented from
/// hot paths with [`Ordering::Relaxed`]. Individual counter reads may be
/// slightly stale; use [`snapshot()`](Metrics::snapshot) for a coherent
/// (though still advisory) view.
pub struct Metrics {
    /// Number of `find_or_create` calls on the hash index.
    pub hash_lookups: AtomicU64,
    /// Number of `find_or_create` calls that created a new entry.
    pub hash_inserts: AtomicU64,
    /// Number of overflow bucket allocations.
    pub overflow_allocations: AtomicU64,
    /// Number of overflow bucket frees.
    pub overflow_frees: AtomicU64,
    /// Number of epoch bumps.
    pub epoch_bumps: AtomicU64,
    /// Number of epoch drain passes that executed callbacks.
    pub epoch_drains: AtomicU64,
    /// Number of allocator `allocate` calls.
    pub allocator_allocs: AtomicU64,
    /// Number of allocator `free` calls.
    pub allocator_frees: AtomicU64,
    /// Number of pending I/O operations currently in-flight.
    pub pending_io_inflight: AtomicU64,
    /// Number of checkpoints completed.
    pub checkpoint_count: AtomicU64,
    /// Total number of CRUD operations (reads + upserts + rmws + deletes).
    pub total_operations: AtomicU64,
    /// Number of page flushes issued.
    pub flush_count: AtomicU64,
}

impl Metrics {
    /// Creates a new `Metrics` instance with all counters at zero.
    pub fn new() -> Self {
        Self {
            hash_lookups: AtomicU64::new(0),
            hash_inserts: AtomicU64::new(0),
            overflow_allocations: AtomicU64::new(0),
            overflow_frees: AtomicU64::new(0),
            epoch_bumps: AtomicU64::new(0),
            epoch_drains: AtomicU64::new(0),
            allocator_allocs: AtomicU64::new(0),
            allocator_frees: AtomicU64::new(0),
            pending_io_inflight: AtomicU64::new(0),
            checkpoint_count: AtomicU64::new(0),
            total_operations: AtomicU64::new(0),
            flush_count: AtomicU64::new(0),
        }
    }

    /// Captures a point-in-time snapshot of all counters as plain `u64` values.
    ///
    /// Each load uses [`Ordering::Relaxed`] — the snapshot is advisory and
    /// individual fields may reflect slightly different moments in time.
    ///
    /// # ARM safety (H2 audit)
    ///
    /// `Relaxed` loads are safe on ARM (aarch64) because these counters are
    /// purely advisory — they are never used for synchronization or ordering
    /// of other memory accesses. Individual atomicity of each 64-bit load is
    /// guaranteed by the hardware. Cross-field inconsistency is acceptable
    /// and documented above.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            hash_lookups: self.hash_lookups.load(Ordering::Relaxed),
            hash_inserts: self.hash_inserts.load(Ordering::Relaxed),
            overflow_allocations: self.overflow_allocations.load(Ordering::Relaxed),
            overflow_frees: self.overflow_frees.load(Ordering::Relaxed),
            epoch_bumps: self.epoch_bumps.load(Ordering::Relaxed),
            epoch_drains: self.epoch_drains.load(Ordering::Relaxed),
            allocator_allocs: self.allocator_allocs.load(Ordering::Relaxed),
            allocator_frees: self.allocator_frees.load(Ordering::Relaxed),
            pending_io_inflight: self.pending_io_inflight.load(Ordering::Relaxed),
            checkpoint_count: self.checkpoint_count.load(Ordering::Relaxed),
            total_operations: self.total_operations.load(Ordering::Relaxed),
            flush_count: self.flush_count.load(Ordering::Relaxed),
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metrics")
            .field("hash_lookups", &self.hash_lookups.load(Ordering::Relaxed))
            .field("hash_inserts", &self.hash_inserts.load(Ordering::Relaxed))
            .field(
                "overflow_allocations",
                &self.overflow_allocations.load(Ordering::Relaxed),
            )
            .field(
                "overflow_frees",
                &self.overflow_frees.load(Ordering::Relaxed),
            )
            .field("epoch_bumps", &self.epoch_bumps.load(Ordering::Relaxed))
            .field("epoch_drains", &self.epoch_drains.load(Ordering::Relaxed))
            .field(
                "allocator_allocs",
                &self.allocator_allocs.load(Ordering::Relaxed),
            )
            .field(
                "allocator_frees",
                &self.allocator_frees.load(Ordering::Relaxed),
            )
            .field(
                "pending_io_inflight",
                &self.pending_io_inflight.load(Ordering::Relaxed),
            )
            .field(
                "checkpoint_count",
                &self.checkpoint_count.load(Ordering::Relaxed),
            )
            .field(
                "total_operations",
                &self.total_operations.load(Ordering::Relaxed),
            )
            .field("flush_count", &self.flush_count.load(Ordering::Relaxed))
            .finish()
    }
}

/// A point-in-time snapshot of [`Metrics`] counters as plain `u64` values.
///
/// Suitable for logging, serialization, or display. Obtained via
/// [`Metrics::snapshot()`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Number of `find_or_create` calls on the hash index.
    pub hash_lookups: u64,
    /// Number of `find_or_create` calls that created a new entry.
    pub hash_inserts: u64,
    /// Number of overflow bucket allocations.
    pub overflow_allocations: u64,
    /// Number of overflow bucket frees.
    pub overflow_frees: u64,
    /// Number of epoch bumps.
    pub epoch_bumps: u64,
    /// Number of epoch drain passes that executed callbacks.
    pub epoch_drains: u64,
    /// Number of allocator `allocate` calls.
    pub allocator_allocs: u64,
    /// Number of allocator `free` calls.
    pub allocator_frees: u64,
    /// Number of pending I/O operations currently in-flight.
    pub pending_io_inflight: u64,
    /// Number of checkpoints completed.
    pub checkpoint_count: u64,
    /// Total number of CRUD operations.
    pub total_operations: u64,
    /// Number of page flushes issued.
    pub flush_count: u64,
}

impl fmt::Display for MetricsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "hash(lookups={}, inserts={}) overflow(allocs={}, frees={}) \
             epoch(bumps={}, drains={}) allocator(allocs={}, frees={}) \
             io(inflight={}, flushes={}) checkpoint={} ops={}",
            self.hash_lookups,
            self.hash_inserts,
            self.overflow_allocations,
            self.overflow_frees,
            self.epoch_bumps,
            self.epoch_drains,
            self.allocator_allocs,
            self.allocator_frees,
            self.pending_io_inflight,
            self.flush_count,
            self.checkpoint_count,
            self.total_operations,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn new_starts_at_zero() {
        let m = Metrics::new();
        let snap = m.snapshot();
        assert_eq!(snap.hash_lookups, 0);
        assert_eq!(snap.hash_inserts, 0);
        assert_eq!(snap.overflow_allocations, 0);
        assert_eq!(snap.overflow_frees, 0);
        assert_eq!(snap.epoch_bumps, 0);
        assert_eq!(snap.epoch_drains, 0);
        assert_eq!(snap.allocator_allocs, 0);
        assert_eq!(snap.allocator_frees, 0);
        assert_eq!(snap.pending_io_inflight, 0);
        assert_eq!(snap.checkpoint_count, 0);
        assert_eq!(snap.total_operations, 0);
        assert_eq!(snap.flush_count, 0);
    }

    #[test]
    fn counters_increment() {
        let m = Metrics::new();
        m.hash_lookups.fetch_add(10, Ordering::Relaxed);
        m.hash_inserts.fetch_add(3, Ordering::Relaxed);
        m.overflow_allocations.fetch_add(2, Ordering::Relaxed);
        m.overflow_frees.fetch_add(1, Ordering::Relaxed);
        m.epoch_bumps.fetch_add(5, Ordering::Relaxed);
        m.epoch_drains.fetch_add(4, Ordering::Relaxed);
        m.allocator_allocs.fetch_add(7, Ordering::Relaxed);
        m.allocator_frees.fetch_add(6, Ordering::Relaxed);
        m.pending_io_inflight.fetch_add(8, Ordering::Relaxed);
        m.checkpoint_count.fetch_add(2, Ordering::Relaxed);
        m.total_operations.fetch_add(100, Ordering::Relaxed);
        m.flush_count.fetch_add(9, Ordering::Relaxed);

        let snap = m.snapshot();
        assert_eq!(snap.hash_lookups, 10);
        assert_eq!(snap.hash_inserts, 3);
        assert_eq!(snap.overflow_allocations, 2);
        assert_eq!(snap.overflow_frees, 1);
        assert_eq!(snap.epoch_bumps, 5);
        assert_eq!(snap.epoch_drains, 4);
        assert_eq!(snap.allocator_allocs, 7);
        assert_eq!(snap.allocator_frees, 6);
        assert_eq!(snap.pending_io_inflight, 8);
        assert_eq!(snap.checkpoint_count, 2);
        assert_eq!(snap.total_operations, 100);
        assert_eq!(snap.flush_count, 9);
    }

    #[test]
    fn concurrent_increments() {
        let m = Arc::new(Metrics::new());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let m = Arc::clone(&m);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        m.hash_lookups.fetch_add(1, Ordering::Relaxed);
                        m.allocator_allocs.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        let snap = m.snapshot();
        assert_eq!(snap.hash_lookups, 4000);
        assert_eq!(snap.allocator_allocs, 4000);
    }

    #[test]
    fn snapshot_display() {
        let m = Metrics::new();
        m.hash_lookups.fetch_add(42, Ordering::Relaxed);
        let snap = m.snapshot();
        let s = format!("{snap}");
        assert!(s.contains("lookups=42"));
    }

    #[test]
    fn snapshot_debug() {
        let m = Metrics::new();
        let snap = m.snapshot();
        let s = format!("{snap:?}");
        assert!(s.contains("MetricsSnapshot"));
    }

    #[test]
    fn metrics_debug() {
        let m = Metrics::new();
        m.epoch_bumps.fetch_add(7, Ordering::Relaxed);
        let s = format!("{m:?}");
        assert!(s.contains("epoch_bumps: 7"));
    }

    #[test]
    fn default_is_zero() {
        let m = Metrics::default();
        assert_eq!(m.snapshot().hash_lookups, 0);
    }
}
