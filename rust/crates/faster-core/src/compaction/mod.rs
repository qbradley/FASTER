//! Online log compaction (garbage collection) for the FASTER hybrid log.
//!
//! Compaction identifies dead (superseded) and tombstoned records in the
//! hybrid log so their space can be reclaimed. The process has two phases:
//!
//! 1. **Scan** — Walk a contiguous address range, classify each record as
//!    live, dead, or tombstoned by consulting the hash index. Produces a
//!    [`CompactionPlan`].
//! 2. **Copy** (future K2–K6) — Copy live records to the log tail, update
//!    hash index entries, and advance the begin-address.
//!
//! # Key safety invariant
//!
//! A compaction scanner **must never classify a live record as dead**.
//! False positives (retaining dead records) waste space but are safe.
//! False negatives (dropping live records) lose data. When uncertain
//! (e.g., concurrent modifications, out-of-memory chain), the scanner
//! conservatively classifies records as live.

pub mod scanner;

use crate::address::LogicalAddress;

// ── LiveRecord ──────────────────────────────────────────────────────

/// A record classified as live by the compaction scanner.
///
/// The scanner determined that this record's key still resolves to this
/// address in the hash index — meaning no newer version supersedes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRecord {
    /// Logical address of the live record in the hybrid log.
    pub address: LogicalAddress,
    /// Total size of the record in bytes (header + key + value + padding).
    pub record_size: usize,
}

// ── CompactionPlan ──────────────────────────────────────────────────

/// Results of scanning a log region for compaction.
///
/// A `CompactionPlan` tells the copier (future work) which records must
/// be preserved and how much space can be reclaimed.
///
/// # Invariants
///
/// - `live_records.len()` + `dead_count` + `tombstone_count` equals the
///   total number of non-null records in the scanned range.
/// - `live_bytes` equals the sum of `record_size` across all `live_records`.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Addresses of live records that need to be copied.
    pub live_records: Vec<LiveRecord>,
    /// Number of dead records found (superseded by a newer version).
    pub dead_count: usize,
    /// Number of tombstoned records found.
    pub tombstone_count: usize,
    /// Total bytes in the scan range (including dead + tombstone records).
    pub total_bytes_scanned: usize,
    /// Total bytes of live data that must be preserved.
    pub live_bytes: usize,
}

impl CompactionPlan {
    /// Returns the total number of classified records.
    #[inline]
    pub fn total_records(&self) -> usize {
        self.live_records.len() + self.dead_count + self.tombstone_count
    }

    /// Returns the fraction of scanned bytes that are live (0.0..=1.0).
    ///
    /// Returns 0.0 if no bytes were scanned.
    #[inline]
    pub fn live_fraction(&self) -> f64 {
        if self.total_bytes_scanned == 0 {
            0.0
        } else {
            self.live_bytes as f64 / self.total_bytes_scanned as f64
        }
    }
}
