//! Compaction policy engine.
//!
//! Decides **when** compaction should be triggered. The [`CompactionPolicy`]
//! trait provides a single predicate, [`should_compact`], that the
//! compaction orchestrator evaluates periodically to determine whether to
//! start a compaction cycle.
//!
//! Three built-in policies cover common scenarios:
//!
//! - [`SpaceAmplificationPolicy`] — triggers when the ratio of total log
//!   size to live data size exceeds a configurable threshold (default 2×).
//! - [`TombstonePercentPolicy`] — triggers when the percentage of
//!   tombstoned records exceeds a configurable threshold (default 25%).
//! - [`ManualPolicy`] — never triggers automatically; compaction happens
//!   only via an explicit `compact()` call.
//!
//! Policies are composable via [`AnyPolicy`] and [`AllPolicy`] combinators.
//!
//! # Examples
//!
//! ```
//! use faster_core::compaction::policy::{
//!     CompactionPolicy, CompactionStats, SpaceAmplificationPolicy,
//!     TombstonePercentPolicy, ManualPolicy,
//! };
//!
//! // Trigger when space amplification exceeds 3×.
//! let policy = SpaceAmplificationPolicy::new(3.0);
//!
//! let stats = CompactionStats {
//!     total_log_bytes: 300,
//!     live_data_bytes: 90,
//!     tombstone_count: 5,
//!     total_record_count: 100,
//! };
//!
//! assert!(policy.should_compact(&stats)); // 300/90 = 3.33 > 3.0
//! ```

// ── Statistics snapshot ─────────────────────────────────────────────

/// A point-in-time snapshot of log statistics used by compaction policies.
///
/// These values are typically gathered by scanning a region of the log
/// (see [`CompactionScanner`](super::scanner::CompactionScanner)) or
/// from the allocator's address boundaries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionStats {
    /// Total bytes in the candidate compaction region.
    pub total_log_bytes: u64,
    /// Bytes occupied by live (non-dead, non-tombstoned) records.
    pub live_data_bytes: u64,
    /// Number of tombstoned records in the region.
    pub tombstone_count: u64,
    /// Total number of records in the region (live + dead + tombstoned).
    pub total_record_count: u64,
}

impl CompactionStats {
    /// Returns the space amplification ratio: `total / live`.
    ///
    /// Returns `f64::INFINITY` if `live_data_bytes` is zero (all data is
    /// dead or tombstoned).
    #[inline]
    pub fn space_amplification(&self) -> f64 {
        if self.live_data_bytes == 0 {
            f64::INFINITY
        } else {
            self.total_log_bytes as f64 / self.live_data_bytes as f64
        }
    }

    /// Returns the tombstone percentage (0.0..=100.0).
    ///
    /// Returns 0.0 if there are no records.
    #[inline]
    pub fn tombstone_percent(&self) -> f64 {
        if self.total_record_count == 0 {
            0.0
        } else {
            self.tombstone_count as f64 / self.total_record_count as f64 * 100.0
        }
    }
}

// ── CompactionPolicy trait ──────────────────────────────────────────

/// Decides whether compaction should be triggered.
///
/// Implementations examine a [`CompactionStats`] snapshot and return
/// `true` to recommend compaction.
pub trait CompactionPolicy: Send + Sync {
    /// Returns `true` if compaction should be triggered given the current
    /// log statistics.
    fn should_compact(&self, stats: &CompactionStats) -> bool;

    /// Human-readable name for logging and diagnostics.
    fn name(&self) -> &str;
}

// ── SpaceAmplificationPolicy ────────────────────────────────────────

/// Triggers compaction when the space amplification ratio exceeds a
/// threshold.
///
/// Space amplification is `total_log_bytes / live_data_bytes`. A ratio
/// of 2.0 means the log is twice as large as the live data — half the
/// space is wasted on dead and tombstoned records.
///
/// # Default
///
/// Threshold = 2.0 (trigger when log is ≥ 2× the live data).
///
/// # Examples
///
/// ```
/// use faster_core::compaction::policy::{CompactionPolicy, CompactionStats, SpaceAmplificationPolicy};
///
/// let policy = SpaceAmplificationPolicy::default();
///
/// let stats = CompactionStats {
///     total_log_bytes: 200,
///     live_data_bytes: 90,
///     tombstone_count: 0,
///     total_record_count: 20,
/// };
/// assert!(policy.should_compact(&stats)); // 200/90 ≈ 2.22 > 2.0
///
/// let healthy = CompactionStats {
///     total_log_bytes: 100,
///     live_data_bytes: 80,
///     tombstone_count: 0,
///     total_record_count: 10,
/// };
/// assert!(!policy.should_compact(&healthy)); // 100/80 = 1.25 < 2.0
/// ```
#[derive(Debug, Clone)]
pub struct SpaceAmplificationPolicy {
    threshold: f64,
}

impl SpaceAmplificationPolicy {
    /// Creates a policy with the given space amplification threshold.
    ///
    /// # Panics
    ///
    /// Panics if `threshold` is less than 1.0 (amplification below 1.0
    /// is impossible — live data cannot exceed total log size).
    pub fn new(threshold: f64) -> Self {
        assert!(
            threshold >= 1.0,
            "space amplification threshold must be >= 1.0, got {threshold}"
        );
        Self { threshold }
    }
}

impl Default for SpaceAmplificationPolicy {
    fn default() -> Self {
        Self { threshold: 2.0 }
    }
}

impl CompactionPolicy for SpaceAmplificationPolicy {
    #[inline]
    fn should_compact(&self, stats: &CompactionStats) -> bool {
        stats.space_amplification() > self.threshold
    }

    fn name(&self) -> &str {
        "SpaceAmplification"
    }
}

// ── TombstonePercentPolicy ──────────────────────────────────────────

/// Triggers compaction when the percentage of tombstoned records exceeds
/// a threshold.
///
/// # Default
///
/// Threshold = 25.0 (trigger when > 25% of records are tombstones).
///
/// # Examples
///
/// ```
/// use faster_core::compaction::policy::{CompactionPolicy, CompactionStats, TombstonePercentPolicy};
///
/// let policy = TombstonePercentPolicy::default();
///
/// let stats = CompactionStats {
///     total_log_bytes: 1000,
///     live_data_bytes: 500,
///     tombstone_count: 30,
///     total_record_count: 100,
/// };
/// assert!(policy.should_compact(&stats)); // 30% > 25%
///
/// let ok = CompactionStats {
///     total_log_bytes: 1000,
///     live_data_bytes: 500,
///     tombstone_count: 10,
///     total_record_count: 100,
/// };
/// assert!(!policy.should_compact(&ok)); // 10% < 25%
/// ```
#[derive(Debug, Clone)]
pub struct TombstonePercentPolicy {
    threshold_percent: f64,
}

impl TombstonePercentPolicy {
    /// Creates a policy with the given tombstone percentage threshold.
    ///
    /// # Panics
    ///
    /// Panics if `threshold_percent` is not in `(0.0, 100.0]`.
    pub fn new(threshold_percent: f64) -> Self {
        assert!(
            threshold_percent > 0.0 && threshold_percent <= 100.0,
            "tombstone threshold must be in (0.0, 100.0], got {threshold_percent}"
        );
        Self { threshold_percent }
    }
}

impl Default for TombstonePercentPolicy {
    fn default() -> Self {
        Self {
            threshold_percent: 25.0,
        }
    }
}

impl CompactionPolicy for TombstonePercentPolicy {
    #[inline]
    fn should_compact(&self, stats: &CompactionStats) -> bool {
        stats.tombstone_percent() > self.threshold_percent
    }

    fn name(&self) -> &str {
        "TombstonePercent"
    }
}

// ── ManualPolicy ────────────────────────────────────────────────────

/// Never triggers automatic compaction.
///
/// Use this when compaction should only happen via an explicit
/// `compact()` call.
///
/// # Examples
///
/// ```
/// use faster_core::compaction::policy::{CompactionPolicy, CompactionStats, ManualPolicy};
///
/// let policy = ManualPolicy;
///
/// // No stats will trigger compaction.
/// let stats = CompactionStats {
///     total_log_bytes: u64::MAX,
///     live_data_bytes: 0,
///     tombstone_count: u64::MAX,
///     total_record_count: u64::MAX,
/// };
/// assert!(!policy.should_compact(&stats));
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ManualPolicy;

impl CompactionPolicy for ManualPolicy {
    #[inline]
    fn should_compact(&self, _stats: &CompactionStats) -> bool {
        false
    }

    fn name(&self) -> &str {
        "Manual"
    }
}

// ── Combinators ─────────────────────────────────────────────────────

/// Triggers when **any** of the inner policies recommends compaction.
///
/// # Examples
///
/// ```
/// use faster_core::compaction::policy::*;
///
/// let policy = AnyPolicy::new(vec![
///     Box::new(SpaceAmplificationPolicy::new(3.0)),
///     Box::new(TombstonePercentPolicy::new(50.0)),
/// ]);
///
/// let high_amp = CompactionStats {
///     total_log_bytes: 400,
///     live_data_bytes: 100,
///     tombstone_count: 0,
///     total_record_count: 100,
/// };
/// assert!(policy.should_compact(&high_amp)); // space amp triggers
/// ```
pub struct AnyPolicy {
    policies: Vec<Box<dyn CompactionPolicy>>,
}

impl AnyPolicy {
    /// Creates a composite policy that triggers when any sub-policy fires.
    pub fn new(policies: Vec<Box<dyn CompactionPolicy>>) -> Self {
        Self { policies }
    }
}

impl CompactionPolicy for AnyPolicy {
    fn should_compact(&self, stats: &CompactionStats) -> bool {
        self.policies.iter().any(|p| p.should_compact(stats))
    }

    fn name(&self) -> &str {
        "Any"
    }
}

/// Triggers only when **all** inner policies recommend compaction.
pub struct AllPolicy {
    policies: Vec<Box<dyn CompactionPolicy>>,
}

impl AllPolicy {
    /// Creates a composite policy that triggers only when all sub-policies fire.
    pub fn new(policies: Vec<Box<dyn CompactionPolicy>>) -> Self {
        Self { policies }
    }
}

impl CompactionPolicy for AllPolicy {
    fn should_compact(&self, stats: &CompactionStats) -> bool {
        !self.policies.is_empty() && self.policies.iter().all(|p| p.should_compact(stats))
    }

    fn name(&self) -> &str {
        "All"
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stats(total: u64, live: u64, tombstones: u64, records: u64) -> CompactionStats {
        CompactionStats {
            total_log_bytes: total,
            live_data_bytes: live,
            tombstone_count: tombstones,
            total_record_count: records,
        }
    }

    // ── CompactionStats ─────────────────────────────────────────────

    #[test]
    fn stats_space_amplification() {
        let stats = make_stats(200, 100, 0, 20);
        assert!((stats.space_amplification() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_space_amplification_all_dead() {
        let stats = make_stats(200, 0, 0, 20);
        assert!(stats.space_amplification().is_infinite());
    }

    #[test]
    fn stats_tombstone_percent() {
        let stats = make_stats(1000, 500, 25, 100);
        assert!((stats.tombstone_percent() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_tombstone_percent_no_records() {
        let stats = make_stats(0, 0, 0, 0);
        assert!((stats.tombstone_percent() - 0.0).abs() < f64::EPSILON);
    }

    // ── SpaceAmplificationPolicy ────────────────────────────────────

    #[test]
    fn space_amp_triggers_above_threshold() {
        let policy = SpaceAmplificationPolicy::new(2.0);
        assert!(policy.should_compact(&make_stats(201, 100, 0, 20)));
    }

    #[test]
    fn space_amp_does_not_trigger_at_threshold() {
        let policy = SpaceAmplificationPolicy::new(2.0);
        assert!(!policy.should_compact(&make_stats(200, 100, 0, 20)));
    }

    #[test]
    fn space_amp_does_not_trigger_below_threshold() {
        let policy = SpaceAmplificationPolicy::new(2.0);
        assert!(!policy.should_compact(&make_stats(150, 100, 0, 20)));
    }

    #[test]
    fn space_amp_triggers_all_dead() {
        let policy = SpaceAmplificationPolicy::new(2.0);
        assert!(policy.should_compact(&make_stats(100, 0, 0, 10)));
    }

    #[test]
    fn space_amp_default_threshold() {
        let policy = SpaceAmplificationPolicy::default();
        assert_eq!(policy.threshold, 2.0);
    }

    #[test]
    fn space_amp_custom_threshold() {
        let policy = SpaceAmplificationPolicy::new(5.0);
        assert!(!policy.should_compact(&make_stats(400, 100, 0, 40)));
        assert!(policy.should_compact(&make_stats(501, 100, 0, 50)));
    }

    #[test]
    #[should_panic(expected = "must be >= 1.0")]
    fn space_amp_rejects_below_one() {
        SpaceAmplificationPolicy::new(0.5);
    }

    // ── TombstonePercentPolicy ──────────────────────────────────────

    #[test]
    fn tombstone_triggers_above_threshold() {
        let policy = TombstonePercentPolicy::new(25.0);
        assert!(policy.should_compact(&make_stats(1000, 500, 26, 100)));
    }

    #[test]
    fn tombstone_does_not_trigger_at_threshold() {
        let policy = TombstonePercentPolicy::new(25.0);
        assert!(!policy.should_compact(&make_stats(1000, 500, 25, 100)));
    }

    #[test]
    fn tombstone_does_not_trigger_below_threshold() {
        let policy = TombstonePercentPolicy::new(25.0);
        assert!(!policy.should_compact(&make_stats(1000, 500, 10, 100)));
    }

    #[test]
    fn tombstone_does_not_trigger_empty() {
        let policy = TombstonePercentPolicy::new(25.0);
        assert!(!policy.should_compact(&make_stats(0, 0, 0, 0)));
    }

    #[test]
    fn tombstone_default_threshold() {
        let policy = TombstonePercentPolicy::default();
        assert_eq!(policy.threshold_percent, 25.0);
    }

    #[test]
    #[should_panic(expected = "must be in (0.0, 100.0]")]
    fn tombstone_rejects_zero() {
        TombstonePercentPolicy::new(0.0);
    }

    #[test]
    #[should_panic(expected = "must be in (0.0, 100.0]")]
    fn tombstone_rejects_above_100() {
        TombstonePercentPolicy::new(100.1);
    }

    // ── ManualPolicy ────────────────────────────────────────────────

    #[test]
    fn manual_never_triggers() {
        let policy = ManualPolicy;
        assert!(!policy.should_compact(&make_stats(u64::MAX, 0, u64::MAX, u64::MAX)));
    }

    #[test]
    fn manual_name() {
        assert_eq!(ManualPolicy.name(), "Manual");
    }

    // ── AnyPolicy combinator ────────────────────────────────────────

    #[test]
    fn any_triggers_when_one_fires() {
        let policy = AnyPolicy::new(vec![
            Box::new(SpaceAmplificationPolicy::new(10.0)), // won't fire
            Box::new(TombstonePercentPolicy::new(10.0)),   // will fire
        ]);
        assert!(policy.should_compact(&make_stats(100, 90, 15, 100)));
    }

    #[test]
    fn any_does_not_trigger_when_none_fire() {
        let policy = AnyPolicy::new(vec![
            Box::new(SpaceAmplificationPolicy::new(10.0)),
            Box::new(TombstonePercentPolicy::new(90.0)),
        ]);
        assert!(!policy.should_compact(&make_stats(100, 90, 5, 100)));
    }

    #[test]
    fn any_empty_does_not_trigger() {
        let policy = AnyPolicy::new(vec![]);
        assert!(!policy.should_compact(&make_stats(100, 0, 100, 100)));
    }

    // ── AllPolicy combinator ────────────────────────────────────────

    #[test]
    fn all_triggers_when_both_fire() {
        let policy = AllPolicy::new(vec![
            Box::new(SpaceAmplificationPolicy::new(2.0)),
            Box::new(TombstonePercentPolicy::new(10.0)),
        ]);
        // amp = 300/100 = 3.0 > 2.0, tombstone = 15% > 10%
        assert!(policy.should_compact(&make_stats(300, 100, 15, 100)));
    }

    #[test]
    fn all_does_not_trigger_when_one_does_not_fire() {
        let policy = AllPolicy::new(vec![
            Box::new(SpaceAmplificationPolicy::new(2.0)),
            Box::new(TombstonePercentPolicy::new(50.0)),
        ]);
        // amp triggers (3.0 > 2.0) but tombstone doesn't (15% < 50%)
        assert!(!policy.should_compact(&make_stats(300, 100, 15, 100)));
    }

    #[test]
    fn all_empty_does_not_trigger() {
        let policy = AllPolicy::new(vec![]);
        assert!(!policy.should_compact(&make_stats(100, 0, 100, 100)));
    }

    // ── Policy names ────────────────────────────────────────────────

    #[test]
    fn policy_names() {
        assert_eq!(
            SpaceAmplificationPolicy::default().name(),
            "SpaceAmplification"
        );
        assert_eq!(TombstonePercentPolicy::default().name(), "TombstonePercent");
        assert_eq!(ManualPolicy.name(), "Manual");
        assert_eq!(AnyPolicy::new(vec![]).name(), "Any");
        assert_eq!(AllPolicy::new(vec![]).name(), "All");
    }
}
