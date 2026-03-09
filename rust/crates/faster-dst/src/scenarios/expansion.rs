//! Parameterized scenario expansion engine.
//!
//! Generates 100+ distinct [`ScenarioTemplate`]s by systematically combining
//! base templates with parameter variations (record counts, crash points,
//! fault configs, trigger timings). Each generated scenario has a unique
//! name for reproduction and reporting.
//!
//! # Design
//!
//! The expansion is fully deterministic — calling [`all_expanded_scenarios`]
//! always returns the same set of templates in the same order. Scenario
//! names encode all parameters for debugging:
//!
//! ```text
//! checkpoint_crash_p0_r10
//! │                 │  └── record count
//! │                 └───── crash-point phase index
//! └─────────────────────── template family
//! ```

use crate::fault::CrashPoint;
use crate::scenario::ScenarioTemplate;

/// Generate all expanded scenario templates.
///
/// Returns a `Vec` of at least 100 distinct `ScenarioTemplate` instances,
/// each with a unique name. These are designed for the campaign engine's
/// seed-sweep model.
pub fn all_expanded_scenarios() -> Vec<ScenarioTemplate> {
    let mut scenarios = Vec::with_capacity(128);

    // ── 1. Per-crash-point checkpoint variants (6 × 3 = 18) ──────────
    let ckpt_sizes: &[usize] = &[10, 50, 200];
    for (phase_idx, &point) in CrashPoint::ALL_CHECKPOINT.iter().enumerate() {
        for &size in ckpt_sizes {
            scenarios.push(per_crash_point_template(
                "checkpoint_crash",
                point,
                phase_idx,
                size,
            ));
        }
    }

    // ── 2. Per-crash-point compaction variants (6 × 3 = 18) ──────────
    let compact_sizes: &[usize] = &[50, 100, 500];
    for (phase_idx, &point) in CrashPoint::ALL_COMPACTION.iter().enumerate() {
        for &size in compact_sizes {
            scenarios.push(per_crash_point_template(
                "compaction_crash",
                point,
                phase_idx,
                size,
            ));
        }
    }

    // ── 3. Per-crash-point recovery variants (6 × 3 = 18) ───────────
    let recovery_sizes: &[usize] = &[10, 50, 200];
    for (phase_idx, &point) in CrashPoint::ALL_RECOVERY.iter().enumerate() {
        for &size in recovery_sizes {
            scenarios.push(per_crash_point_template(
                "recovery_crash",
                point,
                phase_idx,
                size,
            ));
        }
    }

    // ── 4. Concurrent checkpoint + compaction (6) ────────────────────
    for phase in 0..6 {
        scenarios.push(super::concurrent_crash::template(phase, phase));
    }

    // ── 5. Dual checkpoint + recovery (6) ────────────────────────────
    for phase in 0..6 {
        scenarios.push(super::dual_subsystem_crash::template_ckpt_recovery(
            phase, phase,
        ));
    }

    // ── 6. Dual compaction + recovery (6) ────────────────────────────
    for phase in 0..6 {
        scenarios.push(super::dual_subsystem_crash::template_compact_recovery(
            phase, phase,
        ));
    }

    // ── 7. Overwrite crash (6) ───────────────────────────────────────
    for &point in &CrashPoint::ALL_CHECKPOINT {
        scenarios.push(super::overwrite_recovery::template(point, 50));
    }

    // ── 8. Large record recovery (3) ─────────────────────────────────
    for &count in &[500usize, 1000, 2000] {
        scenarios.push(super::large_record_recovery::template(count));
    }

    // ── 9. High-density crash (6) ────────────────────────────────────
    for &point in &CrashPoint::ALL_CHECKPOINT {
        scenarios.push(super::high_density_crash::template(point, 500));
    }

    // ── 10. Torn write varied rates (3) ──────────────────────────────
    for &rate in &[0.05, 0.10, 0.25] {
        scenarios.push(super::torn_write_varied::template_no_crash(rate));
    }

    // ── 11. Torn write + crash (6) ───────────────────────────────────
    for &point in &CrashPoint::ALL_CHECKPOINT {
        scenarios.push(super::torn_write_varied::template(0.1, point));
    }

    // ── 12. Compaction-scale recovery (3) ────────────────────────────
    // Distinct record counts from category 8 to test different log sizes.
    for &count in &[150usize, 300, 750] {
        scenarios.push(super::large_record_recovery::template(count));
    }

    // ── 13. Write error recovery (3) ─────────────────────────────────
    for &(records, limit) in &[(50usize, 100u64), (100, 200), (200, 500)] {
        scenarios.push(super::write_error_recovery::template(records, limit));
    }

    // ── 14. Mixed crash timing — delayed triggers (6) ────────────────
    for (i, &point) in CrashPoint::ALL_RECOVERY.iter().enumerate() {
        let visit = (i as u64 % 3) + 2; // visits 2, 3, 4, 2, 3, 4
        scenarios.push(super::mixed_crash_timing::template(point, visit, 50));
    }

    scenarios
}

/// Helper: create a per-crash-point template with a fixed crash point and
/// record size.
fn per_crash_point_template(
    family: &str,
    crash_point: CrashPoint,
    phase_idx: usize,
    record_count: usize,
) -> ScenarioTemplate {
    use crate::crash::{CrashSchedule, CrashTrigger};
    use crate::workload::CrudWorkload;

    let name = format!("{family}_p{phase_idx}_r{record_count}");
    ScenarioTemplate::builder(name)
        .workload(move |seed| CrudWorkload::new(seed, record_count))
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn scenario_count_at_least_100() {
        let scenarios = all_expanded_scenarios();
        assert!(
            scenarios.len() >= 100,
            "expected ≥100 scenarios, got {}",
            scenarios.len()
        );
    }

    #[test]
    fn all_scenario_names_unique() {
        let scenarios = all_expanded_scenarios();
        let names: HashSet<&str> = scenarios.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names.len(),
            scenarios.len(),
            "duplicate scenario names detected ({} unique / {} total)",
            names.len(),
            scenarios.len()
        );
    }

    #[test]
    fn expansion_is_deterministic() {
        let a = all_expanded_scenarios();
        let b = all_expanded_scenarios();
        assert_eq!(a.len(), b.len());
        for (sa, sb) in a.iter().zip(b.iter()) {
            assert_eq!(sa.name, sb.name);
        }
    }
}
