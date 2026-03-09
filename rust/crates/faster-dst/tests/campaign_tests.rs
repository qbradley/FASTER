//! Campaign engine integration tests.
//!
//! Validates the seed-sweep campaign system, standard scenario templates,
//! and reporting output.

use faster_core::store::{FasterKv, FasterSession, SimpleFunctions};

use faster_dst::campaign::SeedCampaign;
use faster_dst::invariant::Invariant;
use faster_dst::report::CampaignReport;
use faster_dst::scenario::ScenarioTemplate;
use faster_dst::workload::CrudWorkload;

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Invariant that always fails — for testing failure reporting.
struct AlwaysFails;

impl Invariant for AlwaysFails {
    fn check(
        &self,
        _store: &FasterKv<SimpleFunctions<u64, u64>>,
        _session: &mut FasterSession<SimpleFunctions<u64, u64>>,
    ) -> Result<(), String> {
        Err("intentionally failing invariant".to_string())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. Basic campaign — all pass
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn campaign_single_scenario_all_pass() {
    let template = ScenarioTemplate::builder("basic_recovery")
        .workload(|seed| CrudWorkload::new(seed, 20))
        .check_committed_recoverable()
        .build();

    let mut campaign = SeedCampaign::new();
    campaign
        .add_scenario(template)
        .seed_range(0..10)
        .thread_count(2);
    let report = campaign.run();

    assert_eq!(report.total, 10);
    assert!(report.all_passed(), "failures: {:?}", report.failures);
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Failure reporting
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn campaign_reports_failing_seeds() {
    let template = ScenarioTemplate::builder("always_fail")
        .workload(|seed| CrudWorkload::new(seed, 10))
        .invariant(|_wl| Box::new(AlwaysFails))
        .build();

    let mut campaign = SeedCampaign::new();
    campaign
        .add_scenario(template)
        .seed_range(0..5)
        .thread_count(1);
    let report = campaign.run();

    assert_eq!(report.total, 5);
    assert_eq!(report.failed, 5);
    assert_eq!(report.passed, 0);
    assert_eq!(report.failures.len(), 5);

    for f in &report.failures {
        assert_eq!(f.scenario_name, "always_fail");
        assert!(f.error_message.contains("intentionally failing"));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Parallel execution
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn campaign_parallel_execution() {
    let t1 = ScenarioTemplate::builder("scenario_a")
        .workload(|seed| CrudWorkload::new(seed, 15))
        .check_committed_recoverable()
        .build();

    let t2 = ScenarioTemplate::builder("scenario_b")
        .workload(|seed| CrudWorkload::new(seed.wrapping_add(1000), 25))
        .check_committed_recoverable()
        .build();

    let mut campaign = SeedCampaign::new();
    campaign
        .add_scenario(t1)
        .add_scenario(t2)
        .seed_range(0..50)
        .thread_count(4);
    let report = campaign.run();

    assert_eq!(report.total, 100); // 50 seeds × 2 scenarios
    assert!(report.all_passed(), "failures: {:?}", report.failures);
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Standard template: checkpoint crash
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn campaign_checkpoint_crash_template() {
    let template = faster_dst::scenarios::checkpoint_crash::template();

    let mut campaign = SeedCampaign::new();
    campaign
        .add_scenario(template)
        .seed_range(0..30)
        .thread_count(4);
    let report = campaign.run();

    assert_eq!(report.total, 30);
    assert!(
        report.all_passed(),
        "checkpoint crash failures: {}",
        report.display_human()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Standard template: torn write
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn campaign_torn_write_template() {
    let template = faster_dst::scenarios::torn_write::template();

    let mut campaign = SeedCampaign::new();
    campaign
        .add_scenario(template)
        .seed_range(0..30)
        .thread_count(4);
    let report = campaign.run();

    assert_eq!(report.total, 30);
    assert!(
        report.all_passed(),
        "torn write failures: {}",
        report.display_human()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. JSON report format
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn campaign_report_json_format() {
    let template = ScenarioTemplate::builder("json_test")
        .workload(|seed| CrudWorkload::new(seed, 10))
        .invariant(|_wl| Box::new(AlwaysFails))
        .build();

    let mut campaign = SeedCampaign::new();
    campaign
        .add_scenario(template)
        .seed_range(0..3)
        .thread_count(1);
    let report = campaign.run();
    let json = report.display_json();

    // Verify structural fields
    assert!(json.contains("\"total\": 3"), "json: {json}");
    assert!(json.contains("\"failed\": 3"), "json: {json}");
    assert!(json.contains("\"passed\": 0"), "json: {json}");
    assert!(json.contains("\"elapsed_ms\":"), "json: {json}");
    assert!(json.contains("\"failures\":"), "json: {json}");
    assert!(json.contains("\"scenario\": \"json_test\""), "json: {json}");
    assert!(json.contains("\"seed\":"), "json: {json}");
    assert!(json.contains("\"invariant\":"), "json: {json}");
    assert!(json.contains("\"error\":"), "json: {json}");
}

// ═══════════════════════════════════════════════════════════════════════════
// 7. Reproduction command
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn campaign_reproduction_command() {
    let f = faster_dst::report::ScenarioFailure {
        scenario_name: "checkpoint_crash".to_string(),
        seed: 42,
        invariant_violated: "recovery".to_string(),
        error_message: "key 5 missing".to_string(),
    };
    let cmd = CampaignReport::reproduction_command(&f);
    assert!(cmd.contains("cargo test"));
    assert!(cmd.contains("-p faster-dst"));
    assert!(cmd.contains("--test campaign_tests"));
    assert!(cmd.contains("checkpoint_crash"));
    assert!(cmd.contains("--seed=42"));
}

// ═══════════════════════════════════════════════════════════════════════════
// 8. Full campaign benchmark (CI Tier 3)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore] // CI Tier 3 — full campaign
fn campaign_1000_seeds_under_60_seconds() {
    let template = ScenarioTemplate::builder("perf_test")
        .workload(|seed| CrudWorkload::new(seed, 50))
        .check_committed_recoverable()
        .build();

    let mut campaign = SeedCampaign::new();
    campaign.add_scenario(template).seed_range(0..1000);
    let report = campaign.run();

    assert!(report.all_passed(), "failures: {}", report.display_human());
    assert!(
        report.elapsed.as_secs() < 60,
        "campaign took {:.1}s, expected <60s",
        report.elapsed.as_secs_f64()
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// 9. Mega campaign (CI Tier 3)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore] // CI Tier 3 — full campaign
fn campaign_10000_seeds_5_templates() {
    let mut campaign = SeedCampaign::new();
    campaign
        .add_scenario(faster_dst::scenarios::crud_stress::template())
        .add_scenario(faster_dst::scenarios::checkpoint_crash::template())
        .add_scenario(faster_dst::scenarios::compaction_crash::template())
        .add_scenario(faster_dst::scenarios::recovery_stress::template())
        .add_scenario(faster_dst::scenarios::torn_write::template())
        .seed_range(0..10_000);

    let report = campaign.run();
    assert!(
        report.all_passed(),
        "{} failures in {} total:\n{}",
        report.failed,
        report.total,
        report.display_human()
    );
}
