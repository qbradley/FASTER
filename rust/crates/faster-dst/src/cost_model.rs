//! Virtual cost constants for deterministic O(n) operation modeling.
//!
//! In production, operations like `memset`, file truncation, and hash-index
//! invalidation have wall-clock costs proportional to their size.  In DST we
//! replace real work with a virtual time charge so that:
//!
//! 1. The simulated clock advances proportionally to work performed.
//! 2. Scheduling decisions that depend on elapsed time remain realistic.
//! 3. Cost regressions (e.g. accidentally zeroing an entire 32 MB page instead
//!    of a 4 KB region) become detectable at small data sizes.
//!
//! # Calibration rationale
//!
//! Constants are intentionally **inflated ~10×** relative to modern NVMe/DRAM
//! bandwidth so that even a 1 MB buffer produces a measurable 1 ms charge.
//! This lets small-scale DST runs (thousands of records, not millions) still
//! surface O(n) cost regressions that would otherwise hide in the noise.
//!
//! Real-world reference points (2024-era server hardware):
//!
//! | Operation               | Real cost          | Virtual cost        |
//! |-------------------------|--------------------|---------------------|
//! | DRAM memset             | ~0.1 ns/byte       | 1 ns/byte           |
//! | NVMe TRIM / fallocate   | ~0.1 ns/byte       | 1 ns/byte           |
//! | Hash bucket invalidation| ~10 ns/bucket      | 100 ns/bucket       |
//! | Epoch scan              | ~0.5 ns/entry      | 5 ns/entry          |

use std::time::Duration;

// ---------------------------------------------------------------------------
// Virtual cost constants
// ---------------------------------------------------------------------------

/// Virtual cost of zeroing memory (e.g. `memset` on a fresh page).
///
/// 10× inflated so that zeroing 1 MB charges 1 ms of virtual time, making
/// accidental whole-page clears visible even in small-scale DST runs.
pub const MEMZERO_NS_PER_BYTE: u64 = 1;

/// Virtual cost of file truncation / `fallocate` punch-hole.
///
/// Matches [`MEMZERO_NS_PER_BYTE`] because the dominant cost in both cases is
/// proportional to the byte range touched by the kernel.
pub const TRUNCATE_NS_PER_BYTE: u64 = 1;

/// Virtual cost of invalidating a single hash-index bucket.
///
/// Hash-index invalidation touches one cache line per bucket.  The 100 ns
/// virtual cost reflects a ~10× inflation over typical L3-miss latency to
/// ensure bucket-count regressions are caught at small table sizes.
pub const HASH_INVALIDATE_NS_PER_BUCKET: u64 = 100;

/// Virtual cost of scanning one entry during epoch advancement.
///
/// Epoch scans walk a linked list of protected contexts.  5 ns/entry is ~10×
/// the real per-pointer cost, making linear-scan regressions detectable when
/// only a handful of sessions are active.
pub const EPOCH_SCAN_NS_PER_ENTRY: u64 = 5;

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Compute the virtual time cost for a byte-proportional operation.
///
/// Returns `Duration::from_nanos(bytes * ns_per_byte)`.
///
/// # Examples
///
/// ```
/// use faster_dst::cost_model::bytes_cost;
/// use std::time::Duration;
///
/// // 1 MB at 1 ns/byte → 1 ms
/// assert_eq!(bytes_cost(1_000_000, 1), Duration::from_millis(1));
///
/// // 0 bytes → zero cost
/// assert_eq!(bytes_cost(0, 1), Duration::ZERO);
/// ```
pub fn bytes_cost(bytes: u64, ns_per_byte: u64) -> Duration {
    Duration::from_nanos(bytes.saturating_mul(ns_per_byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_megabyte_at_one_ns_per_byte_is_one_millisecond() {
        assert_eq!(bytes_cost(1_000_000, 1), Duration::from_millis(1));
    }

    #[test]
    fn zero_bytes_is_zero_cost() {
        assert_eq!(bytes_cost(0, MEMZERO_NS_PER_BYTE), Duration::ZERO);
    }

    #[test]
    fn zero_rate_is_zero_cost() {
        assert_eq!(bytes_cost(1_000_000, 0), Duration::ZERO);
    }

    #[test]
    fn truncate_cost_matches_memzero() {
        let size = 4096;
        assert_eq!(
            bytes_cost(size, TRUNCATE_NS_PER_BYTE),
            bytes_cost(size, MEMZERO_NS_PER_BYTE),
        );
    }

    #[test]
    fn hash_invalidate_cost_scales_linearly() {
        let buckets = 1024;
        let cost = Duration::from_nanos(buckets * HASH_INVALIDATE_NS_PER_BUCKET);
        // 1024 buckets × 100 ns = 102,400 ns
        assert_eq!(cost, Duration::from_nanos(102_400));
    }

    #[test]
    fn epoch_scan_cost_scales_linearly() {
        let entries = 200;
        let cost = Duration::from_nanos(entries * EPOCH_SCAN_NS_PER_ENTRY);
        // 200 entries × 5 ns = 1 µs
        assert_eq!(cost, Duration::from_micros(1));
    }

    #[test]
    fn saturating_mul_prevents_overflow() {
        // u64::MAX × 2 should saturate, not panic
        let cost = bytes_cost(u64::MAX, 2);
        assert_eq!(cost, Duration::from_nanos(u64::MAX));
    }

    #[test]
    fn constants_have_expected_values() {
        assert_eq!(MEMZERO_NS_PER_BYTE, 1);
        assert_eq!(TRUNCATE_NS_PER_BYTE, 1);
        assert_eq!(HASH_INVALIDATE_NS_PER_BUCKET, 100);
        assert_eq!(EPOCH_SCAN_NS_PER_ENTRY, 5);
    }
}
