//! Campaign-level smoke test: all scenarios × canary seeds.
//!
//! Always `#[ignore]` — this runs the full matrix and is intended for
//! pre-release or nightly CI, not per-PR.

use faster_dst::concurrency::campaign::run_concurrency_campaign;
use faster_dst::concurrency::scenarios::ALL_SCENARIOS;
use faster_dst::concurrency::seeds::canary;

#[test]
#[ignore = "tier-2: DST campaign smoke test — runs all scenarios × canary seeds"]
fn campaign_smoke_test() {
    let report = run_concurrency_campaign(ALL_SCENARIOS, canary::CANARY_SEEDS);

    println!(
        "Campaign: {}/{} passed in {:?}",
        report.passed, report.total_runs, report.wall_duration
    );
    for failure in &report.failures {
        println!(
            "  FAIL: {} seed=0x{:016X}",
            failure.scenario_name, failure.seed
        );
    }

    assert!(
        report.all_passed(),
        "{} failures in campaign out of {} runs",
        report.failed,
        report.total_runs
    );
}
