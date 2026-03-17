//! Individual concurrency scenario tests.
//!
//! Each test runs a single scenario across the canary seed set.
//! Bug-class scenarios may legitimately fail — that IS the test working.

use faster_dst::concurrency::runner::{DstRunner, RunOutcome};
use faster_dst::concurrency::scenarios::{
    lossy_truncate_starvation_config, multi_writer_saturation_config, pipeline_deadlock_config,
    single_writer_baseline_config,
};
use faster_dst::concurrency::seeds::canary;

/// Helper: run a named scenario across all canary seeds, collecting results.
/// Returns the number of failures.
fn run_scenario_on_canary_seeds(
    name: &str,
    factory: fn(
        u64,
    ) -> (
        faster_dst::concurrency::config::ScenarioConfig,
        faster_dst::concurrency::assertions::ScenarioAssertions,
    ),
) -> usize {
    let mut failures = 0;
    for &seed in canary::CANARY_SEEDS {
        let (config, assertions) = factory(seed);
        let runner = DstRunner::new(config, assertions);
        let outcome = runner.run();
        match &outcome {
            RunOutcome::Pass { stats } => {
                println!(
                    "  PASS: {name} seed=0x{seed:016X} total_ops={} sim={:?} wall={:?}",
                    stats.total_ops, stats.sim_duration, stats.wall_duration,
                );
            }
            RunOutcome::Fail { assertion, stats } => {
                eprintln!(
                    "  FAIL: {name} seed=0x{seed:016X} assertion={assertion} total_ops={}",
                    stats.total_ops,
                );
                eprintln!(
                    "  Reproduce: cargo test -p faster-dst --test dst_concurrency -- {name} --seed 0x{seed:016X}"
                );
                failures += 1;
            }
            RunOutcome::Deadlock {
                blocked_info,
                stats,
            } => {
                eprintln!(
                    "  DEADLOCK: {name} seed=0x{seed:016X} info={blocked_info} total_ops={}",
                    stats.total_ops,
                );
                eprintln!(
                    "  Reproduce: cargo test -p faster-dst --test dst_concurrency -- {name} --seed 0x{seed:016X}"
                );
                failures += 1;
            }
            RunOutcome::Error { message } => {
                eprintln!("  ERROR: {name} seed=0x{seed:016X} message={message}");
                eprintln!(
                    "  Reproduce: cargo test -p faster-dst --test dst_concurrency -- {name} --seed 0x{seed:016X}"
                );
                failures += 1;
            }
        }
    }
    failures
}

#[test]
fn dst_single_writer_baseline() {
    let failures =
        run_scenario_on_canary_seeds("single_writer_baseline", single_writer_baseline_config);
    assert_eq!(
        failures, 0,
        "single_writer_baseline: {failures} seed(s) failed — see output above for reproduction commands"
    );
}

#[test]
fn dst_multi_writer_saturation() {
    let failures =
        run_scenario_on_canary_seeds("multi_writer_saturation", multi_writer_saturation_config);
    assert_eq!(
        failures, 0,
        "multi_writer_saturation: {failures} seed(s) failed — see output above for reproduction commands"
    );
}

#[test]
#[ignore = "tier-2: DST concurrency scenario — pipeline deadlock bug exercise"]
fn dst_bug1_pipeline_deadlock() {
    let failures = run_scenario_on_canary_seeds("pipeline_deadlock", pipeline_deadlock_config);
    if failures > 0 {
        eprintln!(
            "pipeline_deadlock: {failures}/{} seeds triggered the bug path (expected for unfixed bugs)",
            canary::CANARY_SEEDS.len()
        );
    }
    // NOTE: failure here means the scenario successfully exercised the bug.
    // Do NOT assert_eq!(failures, 0) — the point is to DETECT, not to pass.
    // However, we do assert to provide a clear signal:
    assert_eq!(
        failures, 0,
        "pipeline_deadlock: {failures} seed(s) failed — bug path was exercised, see output above"
    );
}

#[test]
#[ignore = "tier-2: DST concurrency scenario — lossy truncate starvation bug exercise"]
fn dst_bug2_lossy_truncate_starvation() {
    let failures = run_scenario_on_canary_seeds(
        "lossy_truncate_starvation",
        lossy_truncate_starvation_config,
    );
    if failures > 0 {
        eprintln!(
            "lossy_truncate_starvation: {failures}/{} seeds triggered the bug path (expected for unfixed bugs)",
            canary::CANARY_SEEDS.len()
        );
    }
    assert_eq!(
        failures, 0,
        "lossy_truncate_starvation: {failures} seed(s) failed — bug path was exercised, see output above"
    );
}
