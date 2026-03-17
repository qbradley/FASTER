//! Concurrency scenario configurations for DST v2.0.
//!
//! Each function returns `(ScenarioConfig, ScenarioAssertions)` tuned to
//! exercise a specific concurrency bug class or serve as a correctness
//! baseline.  The `seed` parameter ensures deterministic replay.

use super::assertions::ScenarioAssertions;
use super::config::{KeyDist, ScenarioConfig};
use crate::SimIoConfig;

/// Factory function type for scenario configurations.
pub type ScenarioFactory = fn(u64) -> (ScenarioConfig, ScenarioAssertions);

/// All built-in concurrency scenarios, ordered from lightest to heaviest.
///
/// Each entry is `(name, factory_fn)`.  The campaign runner iterates this
/// list × a seed set to produce the full test matrix.
pub static ALL_SCENARIOS: &[(&str, ScenarioFactory)] = &[
    ("single_writer_baseline", single_writer_baseline_config),
    ("multi_writer_saturation", multi_writer_saturation_config),
    ("pipeline_deadlock", pipeline_deadlock_config),
    (
        "lossy_truncate_starvation",
        lossy_truncate_starvation_config,
    ),
];

/// Correctness baseline: single writer, no contention.
///
/// Validates that the runner + SimDeviceV2 path works end-to-end without
/// any concurrency pressure.  Should always pass.
///
/// With small-pages (64 KB): 4 pages = 256 KB buffer.
/// 10K ops with Uniform(50K) ≈ 9K unique keys × 24 bytes = 216 KB (fits).
pub fn single_writer_baseline_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 1,
        ops_per_writer: 10_000,
        buffer_pool_pages: 4,
        key_dist: KeyDist::Uniform { range: 50_000 },
        lossy: false,
        io: SimIoConfig {
            queue_depth: 16,
            seed,
            ..SimIoConfig::default()
        },
        maintenance_interval: 200,
        seed,
    };

    let assertions = ScenarioAssertions {
        all_writers_complete: true,
        ..ScenarioAssertions::default()
    };

    (config, assertions)
}

/// High contention baseline: many writers competing for limited buffer.
///
/// 16 writers fight over 8 buffer pool pages with moderate queue depth.
/// Large key range (10M) ensures heavy allocation pressure with minimal
/// in-place update hits.
///
/// With small-pages (64 KB): 8 pages = 512 KB buffer.
/// 16 × 20K ops × 24 bytes = 7.68 MB — forces ~15× buffer worth of flushes.
pub fn multi_writer_saturation_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 16,
        ops_per_writer: 20_000,
        buffer_pool_pages: 8,
        key_dist: KeyDist::Uniform { range: 500_000 },
        lossy: false,
        io: SimIoConfig {
            queue_depth: 8,
            seed,
            ..SimIoConfig::default()
        },
        maintenance_interval: 100,
        seed,
    };

    let assertions = ScenarioAssertions {
        all_writers_complete: true,
        no_deadlock: true,
        ..ScenarioAssertions::default()
    };

    (config, assertions)
}

/// Bug 1 reproduction: Pipeline deadlock under high I/O back-pressure.
///
/// 8 writers, tiny buffer pool (4 pages), tight queue_depth (4), high
/// latency.  Large key range (10M) ensures virtually no in-place updates —
/// every op allocates a new record, creating continuous flush/eviction
/// pressure that exercises the pipeline deadlock bug path.
///
/// With small-pages (64 KB): 4 pages = 256 KB buffer.
/// 8 × 50K ops × 24 bytes = 9.6 MB — forces ~37× buffer worth of flushes.
pub fn pipeline_deadlock_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 8,
        ops_per_writer: 50_000,
        buffer_pool_pages: 4,
        key_dist: KeyDist::Uniform { range: 500_000 },
        lossy: false,
        io: SimIoConfig {
            queue_depth: 4,
            base_latency_ns: 10_000_000,  // 10 ms — high latency
            latency_jitter_ns: 5_000_000, // 5 ms jitter
            seed,
            ..SimIoConfig::default()
        },
        maintenance_interval: 20,
        seed,
    };

    let assertions = ScenarioAssertions {
        all_writers_complete: true,
        no_deadlock: true,
        max_stall_ns: Some(10_000_000_000), // 10 s simulated time
        // With healthy pipeline, all 400K ops succeed.
        // Broken pipeline: < 5% succeed (mostly aborted).
        min_total_ops: Some(200_000),
        ..ScenarioAssertions::default()
    };

    (config, assertions)
}

/// Bug 2 reproduction: Lossy truncate starvation under write lock.
///
/// Lossy mode, moderate writers, O(n) truncate charging via virtual time.
/// Large key range (10M) ensures virtually no in-place updates — every
/// op allocates, creating continuous buffer pressure.
///
/// With small-pages (64 KB): 4 pages = 256 KB buffer.
/// 4 × 50K ops × 24 bytes = 4.8 MB — forces ~19× buffer worth of flushes.
pub fn lossy_truncate_starvation_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 4,
        ops_per_writer: 50_000,
        buffer_pool_pages: 4,
        key_dist: KeyDist::Uniform { range: 500_000 },
        lossy: true,
        io: SimIoConfig {
            queue_depth: 8,
            truncate_ns_per_byte: 1,
            seed,
            ..SimIoConfig::default()
        },
        maintenance_interval: 50,
        seed,
    };

    let assertions = ScenarioAssertions {
        all_writers_complete: true,
        no_deadlock: true,
        max_stall_ns: Some(15_000_000_000), // 15 s simulated time
        // With healthy pipeline, all 200K ops succeed.
        // Broken pipeline: < 5% succeed (mostly aborted).
        min_total_ops: Some(100_000),
        ..ScenarioAssertions::default()
    };

    (config, assertions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_scenarios_produce_valid_configs() {
        for &(name, factory) in ALL_SCENARIOS {
            let (config, assertions) = factory(42);
            assert!(config.num_writers > 0, "{name}: num_writers must be > 0");
            assert!(
                config.ops_per_writer > 0,
                "{name}: ops_per_writer must be > 0"
            );
            assert!(
                config.buffer_pool_pages > 0,
                "{name}: buffer_pool_pages must be > 0"
            );
            assert!(
                assertions.all_writers_complete,
                "{name}: all_writers_complete should be true"
            );
        }
    }

    #[test]
    fn scenario_count() {
        assert_eq!(ALL_SCENARIOS.len(), 4, "expected 4 built-in scenarios");
    }

    #[test]
    fn seed_propagates_to_io_config() {
        let seed = 0xDEAD_BEEF;
        for &(name, factory) in ALL_SCENARIOS {
            let (config, _) = factory(seed);
            assert_eq!(config.seed, seed, "{name}: master seed mismatch");
            assert_eq!(config.io.seed, seed, "{name}: io seed mismatch");
        }
    }
}
