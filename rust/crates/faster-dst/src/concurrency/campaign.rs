//! Concurrency campaign runner.
//!
//! Runs every combination of (scenario, seed) through [`DstRunner`],
//! collects outcomes, and produces a [`CampaignReport`] with reproduction
//! commands for any failures.

use std::time::{Duration, Instant};

use super::runner::{DstRunner, RunOutcome};
use super::scenarios::ScenarioFactory;

/// A single failure captured during a campaign run.
#[derive(Debug)]
pub struct CampaignFailure {
    /// Human-readable scenario name (e.g. `"pipeline_deadlock"`).
    pub scenario_name: String,
    /// Seed that triggered the failure.
    pub seed: u64,
    /// Full outcome from the runner.
    pub outcome: RunOutcome,
}

/// Aggregated results from a full concurrency campaign sweep.
#[derive(Debug)]
pub struct ConcurrencyCampaignReport {
    /// Total `(scenario, seed)` pairs executed.
    pub total_runs: usize,
    /// Number that produced `RunOutcome::Pass`.
    pub passed: usize,
    /// Number that produced a non-pass outcome.
    pub failed: usize,
    /// Details of each failure.
    pub failures: Vec<CampaignFailure>,
    /// Wall-clock duration of the entire campaign.
    pub wall_duration: Duration,
}

impl ConcurrencyCampaignReport {
    /// `true` when every run passed.
    pub fn all_passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Run every (scenario, seed) pair sequentially and collect results.
///
/// Phase 1: sequential execution.  A future Phase 2 will add thread-pool
/// parallelism.
pub fn run_concurrency_campaign(
    scenarios: &[(&str, ScenarioFactory)],
    seeds: &[u64],
) -> ConcurrencyCampaignReport {
    let wall_start = Instant::now();
    let mut passed = 0usize;
    let mut failures = Vec::new();
    let total_runs = scenarios.len() * seeds.len();

    for &(name, factory) in scenarios {
        for &seed in seeds {
            let (config, assertions) = factory(seed);
            let runner = DstRunner::new(config, assertions);
            let outcome = runner.run();

            match &outcome {
                RunOutcome::Pass { .. } => {
                    passed += 1;
                }
                _ => {
                    eprintln!(
                        "FAIL: {name} seed=0x{seed:016X}  \
                         Reproduce: cargo test -p faster-dst --test dst_concurrency -- {name} --seed 0x{seed:016X}"
                    );
                    failures.push(CampaignFailure {
                        scenario_name: name.to_string(),
                        seed,
                        outcome,
                    });
                }
            }
        }
    }

    let wall_duration = wall_start.elapsed();
    let failed = failures.len();

    ConcurrencyCampaignReport {
        total_runs,
        passed,
        failed,
        failures,
        wall_duration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concurrency::scenarios::single_writer_baseline_config;

    /// Campaign with a single easy scenario × 1 seed must pass.
    #[test]
    fn campaign_single_scenario_single_seed() {
        let scenarios: &[(&str, ScenarioFactory)] =
            &[("single_writer_baseline", single_writer_baseline_config)];
        let seeds: &[u64] = &[42];

        let report = run_concurrency_campaign(scenarios, seeds);

        assert_eq!(report.total_runs, 1);
        assert_eq!(report.passed, 1);
        assert!(report.all_passed());
    }

    /// Empty campaign produces a clean report.
    #[test]
    fn campaign_empty() {
        let scenarios: &[(&str, ScenarioFactory)] = &[];
        let seeds: &[u64] = &[1, 2, 3];

        let report = run_concurrency_campaign(scenarios, seeds);

        assert_eq!(report.total_runs, 0);
        assert!(report.all_passed());
    }
}
