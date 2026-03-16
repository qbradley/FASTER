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
    ("lossy_truncate_starvation", lossy_truncate_starvation_config),
];

/// Correctness baseline: single writer, no contention.
///
/// Validates that the runner + SimDeviceV2 path works end-to-end without
/// any concurrency pressure.  Should always pass.
pub fn single_writer_baseline_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 1,
        ops_per_writer: 5_000,
        buffer_pool_pages: 16,
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
/// 16 writers fight over 32 buffer pool pages with moderate queue depth.
/// Validates the system doesn't deadlock under heavy write contention.
pub fn multi_writer_saturation_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 16,
        ops_per_writer: 2_000,
        buffer_pool_pages: 32,
        key_dist: KeyDist::Uniform { range: 100_000 },
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
/// 8 writers, small buffer pool (16 pages), tight queue_depth (4), high
/// latency.  The pipeline fills up, QueueFull cascades, flush can't make
/// progress.  This is the scenario that catches the multi-writer flush
/// pipeline deadlock under resource exhaustion.
pub fn pipeline_deadlock_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 8,
        ops_per_writer: 5_000,
        buffer_pool_pages: 16,
        key_dist: KeyDist::Uniform { range: 50_000 },
        lossy: false,
        io: SimIoConfig {
            queue_depth: 4,
            base_latency_ns: 10_000_000,     // 10 ms — high latency
            latency_jitter_ns: 5_000_000,    // 5 ms jitter
            seed,
            ..SimIoConfig::default()
        },
        maintenance_interval: 50,
        seed,
    };

    let assertions = ScenarioAssertions {
        all_writers_complete: true,
        no_deadlock: true,
        max_stall_ns: Some(10_000_000_000), // 10 s simulated time
        ..ScenarioAssertions::default()
    };

    (config, assertions)
}

/// Bug 2 reproduction: Lossy truncate starvation under write lock.
///
/// Lossy mode, moderate writers, O(n) truncate charging via virtual time.
/// The truncate cost (1 ns/byte) slows the simulated clock, starving
/// writers that are waiting for buffer space to be reclaimed.
pub fn lossy_truncate_starvation_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
    let config = ScenarioConfig {
        num_writers: 4,
        ops_per_writer: 10_000,
        buffer_pool_pages: 8,
        key_dist: KeyDist::Uniform { range: 100_000 },
        lossy: true,
        io: SimIoConfig {
            queue_depth: 8,
            truncate_ns_per_byte: 1,
            seed,
            ..SimIoConfig::default()
        },
        maintenance_interval: 100,
        seed,
    };

    let assertions = ScenarioAssertions {
        all_writers_complete: true,
        no_deadlock: true,
        max_stall_ns: Some(15_000_000_000), // 15 s simulated time
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
            assert!(config.ops_per_writer > 0, "{name}: ops_per_writer must be > 0");
            assert!(config.buffer_pool_pages > 0, "{name}: buffer_pool_pages must be > 0");
            assert!(assertions.all_writers_complete, "{name}: all_writers_complete should be true");
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
