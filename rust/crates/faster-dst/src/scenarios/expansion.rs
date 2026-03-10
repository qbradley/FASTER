//! Parameterized scenario expansion engine.
//!
//! Generates 1000+ distinct [`ScenarioTemplate`]s by systematically combining
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

/// Graduated fault configurations: (write_error_rate, partial_write_rate, record_count).
const GRADUATED_CONFIGS: &[(f64, f64, usize)] = &[
    (0.01, 0.01, 50),
    (0.02, 0.05, 50),
    (0.05, 0.02, 75),
    (0.05, 0.10, 50),
    (0.10, 0.05, 100),
];

/// Boundary-condition record counts near powers of two.
const BOUNDARY_CKPT_SIZES: &[usize] = &[1, 2, 7, 15, 31, 63, 127, 255];
const BOUNDARY_COMPACT_SIZES: &[usize] = &[7, 15, 31, 63, 127, 255];
const BOUNDARY_RECOVERY_SIZES: &[usize] = &[7, 15, 31, 63, 127, 255];

/// Generate all expanded scenario templates.
///
/// Returns a `Vec` of 1000+ distinct `ScenarioTemplate` instances,
/// each with a unique name. These are designed for the campaign engine's
/// seed-sweep model.
pub fn all_expanded_scenarios() -> Vec<ScenarioTemplate> {
    let mut scenarios = Vec::with_capacity(1100);

    // ═════════════════════════════════════════════════════════════════════
    // SECTION A: Per-crash-point × record-size matrices (126)
    // ═════════════════════════════════════════════════════════════════════

    // ── 1. Checkpoint crash variants (6 × 7 = 42) ───────────────────
    let ckpt_sizes: &[usize] = &[5, 10, 25, 50, 100, 200, 500];
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

    // ── 2. Compaction crash variants (6 × 7 = 42) ───────────────────
    let compact_sizes: &[usize] = &[10, 25, 50, 100, 200, 500, 1000];
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

    // ── 3. Recovery crash variants (6 × 7 = 42) ─────────────────────
    let recovery_sizes: &[usize] = &[5, 10, 25, 50, 100, 200, 500];
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

    // ═════════════════════════════════════════════════════════════════════
    // SECTION B: Cross-subsystem crash combinations (138)
    // ═════════════════════════════════════════════════════════════════════

    // ── 4. Concurrent checkpoint + compaction, full cross (6 × 6 = 36) ─
    for ckpt in 0..6 {
        for compact in 0..6 {
            scenarios.push(super::concurrent_crash::template(ckpt, compact));
        }
    }

    // ── 5. Dual checkpoint + recovery, full cross (6 × 6 = 36) ─────
    for ckpt in 0..6 {
        for rec in 0..6 {
            scenarios.push(super::dual_subsystem_crash::template_ckpt_recovery(
                ckpt, rec,
            ));
        }
    }

    // ── 6. Dual compaction + recovery, full cross (6 × 6 = 36) ─────
    for compact in 0..6 {
        for rec in 0..6 {
            scenarios.push(super::dual_subsystem_crash::template_compact_recovery(
                compact, rec,
            ));
        }
    }

    // ── 7. Triple subsystem crash (30) ──────────────────────────────
    // Selected representative combos from the 216 possible (6³).
    for base in 0..6 {
        for offset in &[0usize, 1, 2, 3, 5] {
            let compact = (base + offset) % 6;
            let recovery = (base + offset * 2) % 6;
            scenarios.push(super::triple_crash::template(base, compact, recovery));
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION C: Overwrite scenarios (54)
    // ═════════════════════════════════════════════════════════════════════

    // ── 8. Overwrite + checkpoint crash (6 × 4 = 24) ────────────────
    for &point in &CrashPoint::ALL_CHECKPOINT {
        for &count in &[20usize, 50, 100, 200] {
            scenarios.push(super::overwrite_recovery::template(point, count));
        }
    }

    // ── 9. Overwrite + compaction crash (6 × 3 = 18) ────────────────
    for &point in &CrashPoint::ALL_COMPACTION {
        for &count in &[30usize, 75, 150] {
            scenarios.push(super::overwrite_compaction_crash::template(point, count));
        }
    }

    // ── 10. Overwrite + recovery crash (6 × 2 = 12) ─────────────────
    for &point in &CrashPoint::ALL_RECOVERY {
        for &count in &[50usize, 100] {
            scenarios.push(super::overwrite_recovery::template(point, count));
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION D: Scale and recovery stress (23)
    // ═════════════════════════════════════════════════════════════════════

    // ── 11. Large record recovery (8) ────────────────────────────────
    for &count in &[100usize, 250, 500, 750, 1000, 1500, 2000, 3000] {
        scenarios.push(super::large_record_recovery::template(count));
    }

    // ── 12. Compaction-scale recovery (5) ────────────────────────────
    // Distinct counts from category 11 to avoid name collisions.
    for &count in &[150usize, 300, 450, 650, 1250] {
        scenarios.push(super::large_record_recovery::template(count));
    }

    // ── 13. Sequential recovery stress (10) ──────────────────────────
    // No crash, varied record counts for pure recovery integrity.
    for &count in &[5usize, 10, 20, 50, 75, 100, 200, 500, 1000, 2000] {
        let name = format!("sequential_stress_{count}");
        scenarios.push(
            ScenarioTemplate::builder(name)
                .workload(move |seed| crate::workload::CrudWorkload::new(seed, count))
                .check_committed_recoverable()
                .build(),
        );
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION E: High-density hash table (30)
    // ═════════════════════════════════════════════════════════════════════

    // ── 14. High-density + checkpoint crash (6 × 3 = 18) ────────────
    for &point in &CrashPoint::ALL_CHECKPOINT {
        for &density in &[500usize, 750, 1000] {
            scenarios.push(super::high_density_crash::template(point, density));
        }
    }

    // ── 15. High-density + compaction crash (6 × 2 = 12) ────────────
    for &point in &CrashPoint::ALL_COMPACTION {
        for &density in &[500usize, 1000] {
            scenarios.push(super::high_density_crash::template(point, density));
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION F: Torn write scenarios (49)
    // ═════════════════════════════════════════════════════════════════════

    // ── 16. Torn write no crash (7) ──────────────────────────────────
    for &rate in &[0.01, 0.02, 0.05, 0.10, 0.15, 0.20, 0.25] {
        scenarios.push(super::torn_write_varied::template_no_crash(rate));
    }

    // ── 17. Torn write + checkpoint crash (4 × 6 = 24) ──────────────
    for &rate in &[0.05, 0.10, 0.15, 0.25] {
        for &point in &CrashPoint::ALL_CHECKPOINT {
            scenarios.push(super::torn_write_varied::template(rate, point));
        }
    }

    // ── 18. Torn write + compaction crash (3 × 6 = 18) ──────────────
    for &rate in &[0.05, 0.10, 0.25] {
        for &point in &CrashPoint::ALL_COMPACTION {
            scenarios.push(super::torn_write_varied::template(rate, point));
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION G: Write error injection (42)
    // ═════════════════════════════════════════════════════════════════════

    // ── 19. Write error recovery (no crash) (6) ──────────────────────
    for &(records, limit) in &[
        (30usize, 50u64),
        (50, 100),
        (75, 150),
        (100, 200),
        (150, 300),
        (200, 500),
    ] {
        scenarios.push(super::write_error_recovery::template(records, limit));
    }

    // ── 20. Write error + checkpoint crash (3 × 6 = 18) ─────────────
    for &(records, limit) in &[(50usize, 200u64), (100, 400), (200, 800)] {
        for &point in &CrashPoint::ALL_CHECKPOINT {
            scenarios.push(super::write_error_crash::template(point, records, limit));
        }
    }

    // ── 21. Write error + recovery crash (3 × 6 = 18) ───────────────
    for &(records, limit) in &[(50usize, 200u64), (100, 400), (200, 800)] {
        for &point in &CrashPoint::ALL_RECOVERY {
            scenarios.push(super::write_error_crash::template(point, records, limit));
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION H: Delayed crash triggers (72)
    // ═════════════════════════════════════════════════════════════════════

    // ── 22. Delayed recovery crash (3 × 6 × 2 = 36) ─────────────────
    for &visit in &[2u64, 3, 4] {
        for &point in &CrashPoint::ALL_RECOVERY {
            for &size in &[25usize, 100] {
                scenarios.push(super::mixed_crash_timing::template(point, visit, size));
            }
        }
    }

    // ── 23. Delayed checkpoint crash (3 × 6 × 2 = 36) ───────────────
    for &visit in &[2u64, 3, 4] {
        for &point in &CrashPoint::ALL_CHECKPOINT {
            for &size in &[25usize, 100] {
                scenarios.push(super::mixed_crash_timing::template(point, visit, size));
            }
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION I: Sparse key distribution (50)
    // ═════════════════════════════════════════════════════════════════════

    // ── 24. Sparse key + checkpoint crash (6 × 4 = 24) ──────────────
    for &point in &CrashPoint::ALL_CHECKPOINT {
        for &count in &[10usize, 50, 100, 300] {
            scenarios.push(super::sparse_key_crash::template(point, count));
        }
    }

    // ── 25. Sparse key + compaction crash (6 × 2 = 12) ──────────────
    for &point in &CrashPoint::ALL_COMPACTION {
        for &count in &[50usize, 200] {
            scenarios.push(super::sparse_key_crash::template(point, count));
        }
    }

    // ── 26. Sparse key + recovery crash (6 × 2 = 12) ────────────────
    for &point in &CrashPoint::ALL_RECOVERY {
        for &count in &[50usize, 200] {
            scenarios.push(super::sparse_key_crash::template(point, count));
        }
    }

    // ── 27. Sparse key no-crash recovery (2) ─────────────────────────
    for &count in &[100usize, 500] {
        scenarios.push(super::sparse_key_crash::template_no_crash(count));
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION J: Graduated multi-fault injection (95)
    // ═════════════════════════════════════════════════════════════════════

    // ── 28. Graduated fault + checkpoint crash (5 × 6 = 30) ─────────
    for &(we, pw, rc) in GRADUATED_CONFIGS {
        for &point in &CrashPoint::ALL_CHECKPOINT {
            scenarios.push(super::graduated_fault_crash::template(we, pw, point, rc));
        }
    }

    // ── 29. Graduated fault + compaction crash (5 × 6 = 30) ─────────
    for &(we, pw, rc) in GRADUATED_CONFIGS {
        for &point in &CrashPoint::ALL_COMPACTION {
            scenarios.push(super::graduated_fault_crash::template(we, pw, point, rc));
        }
    }

    // ── 30. Graduated fault + recovery crash (5 × 6 = 30) ───────────
    for &(we, pw, rc) in GRADUATED_CONFIGS {
        for &point in &CrashPoint::ALL_RECOVERY {
            scenarios.push(super::graduated_fault_crash::template(we, pw, point, rc));
        }
    }

    // ── 31. Graduated fault no crash (5) ─────────────────────────────
    for &(we, pw, rc) in GRADUATED_CONFIGS {
        scenarios.push(super::graduated_fault_crash::template_no_crash(we, pw, rc));
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION K: Boundary record counts (120)
    // ═════════════════════════════════════════════════════════════════════

    // ── 32. Boundary + checkpoint (8 × 6 = 48) ──────────────────────
    for &size in BOUNDARY_CKPT_SIZES {
        for (phase_idx, &point) in CrashPoint::ALL_CHECKPOINT.iter().enumerate() {
            scenarios.push(super::boundary_record_crash::template(point, size));
            let _ = phase_idx; // used only for unique naming in template
        }
    }

    // ── 33. Boundary + compaction (6 × 6 = 36) ──────────────────────
    for &size in BOUNDARY_COMPACT_SIZES {
        for &point in &CrashPoint::ALL_COMPACTION {
            scenarios.push(super::boundary_record_crash::template(point, size));
        }
    }

    // ── 34. Boundary + recovery (6 × 6 = 36) ────────────────────────
    for &size in BOUNDARY_RECOVERY_SIZES {
        for &point in &CrashPoint::ALL_RECOVERY {
            scenarios.push(super::boundary_record_crash::template(point, size));
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION L: Overwrite + torn write combinations (36)
    // ═════════════════════════════════════════════════════════════════════

    // ── 35. Overwrite + torn write + checkpoint crash (3 × 6 × 2 = 36)
    for &rate in &[0.05, 0.10, 0.20] {
        for &point in &CrashPoint::ALL_CHECKPOINT {
            for &count in &[30usize, 100] {
                scenarios.push(super::overwrite_torn_write::template(rate, point, count));
            }
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION M: Torn write + non-checkpoint crashes (18)
    // ═════════════════════════════════════════════════════════════════════

    // ── 36. Torn write + recovery crash (3 × 6 = 18) ────────────────
    for &rate in &[0.05, 0.10, 0.25] {
        for &point in &CrashPoint::ALL_RECOVERY {
            scenarios.push(super::torn_write_varied::template(rate, point));
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION N: Additional cross-subsystem combinations (78)
    // ═════════════════════════════════════════════════════════════════════

    // ── 37. Write error + compaction crash (3 × 6 = 18) ─────────────
    for &(records, limit) in &[(50usize, 200u64), (100, 400), (200, 800)] {
        for &point in &CrashPoint::ALL_COMPACTION {
            scenarios.push(super::write_error_crash::template(point, records, limit));
        }
    }

    // ── 38. High-density + recovery crash (6 × 2 = 12) ──────────────
    for &point in &CrashPoint::ALL_RECOVERY {
        for &density in &[500usize, 1000] {
            scenarios.push(super::high_density_crash::template(point, density));
        }
    }

    // ── 39. Delayed compaction crash (3 × 6 = 18) ───────────────────
    for &visit in &[2u64, 3, 4] {
        for &point in &CrashPoint::ALL_COMPACTION {
            scenarios.push(super::mixed_crash_timing::template(point, visit, 75));
        }
    }

    // ── 40. Additional triple subsystem combos (~50) ──────────────────
    // Systematically fill in unique (ckpt, compact, recovery) triples not
    // covered by category 7. We track what cat 7 already generated.
    {
        let mut seen = std::collections::HashSet::new();
        // Record cat 7 combos.
        for base in 0..6usize {
            for &offset in &[0usize, 1, 2, 3, 5] {
                let c = (base + offset) % 6;
                let r = (base + offset * 2) % 6;
                seen.insert((base, c, r));
            }
        }
        // Add new combos up to 50 additional.
        let mut added = 0usize;
        'outer: for ckpt in 0..6usize {
            for compact in 0..6usize {
                for recovery in 0..6usize {
                    if added >= 50 {
                        break 'outer;
                    }
                    if seen.insert((ckpt, compact, recovery)) {
                        scenarios.push(super::triple_crash::template(ckpt, compact, recovery));
                        added += 1;
                    }
                }
            }
        }
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION O: Sparse key + fault combinations (20)
    // ═════════════════════════════════════════════════════════════════════

    // ── 41. Sparse key + additional compaction sizes (6 × 2 = 12) ───
    for &point in &CrashPoint::ALL_COMPACTION {
        for &count in &[100usize, 500] {
            scenarios.push(super::sparse_key_crash::template(point, count));
        }
    }

    // ── 42. Sparse key no-crash additional sizes (6) ─────────────────
    for &count in &[10usize, 25, 50, 200, 300, 1000] {
        scenarios.push(super::sparse_key_crash::template_no_crash(count));
    }

    // ── 43. Sparse key + recovery additional sizes (6 × 1 = 6) ──────
    for &point in &CrashPoint::ALL_RECOVERY {
        scenarios.push(super::sparse_key_crash::template(point, 300));
    }

    // ── 43b. Additional sequential stress sizes (10) ─────────────────
    // Fills in gap sizes not covered by category 13.
    for &count in &[3usize, 7, 15, 30, 40, 125, 250, 400, 750, 1500] {
        let name = format!("sequential_stress_{count}");
        scenarios.push(
            ScenarioTemplate::builder(name)
                .workload(move |seed| crate::workload::CrudWorkload::new(seed, count))
                .check_committed_recoverable()
                .build(),
        );
    }

    // ═════════════════════════════════════════════════════════════════════
    // SECTION P: Overwrite + torn write + non-checkpoint (18)
    // ═════════════════════════════════════════════════════════════════════

    // ── 44. Overwrite + torn write + compaction crash (3 × 6 = 18) ──
    for &rate in &[0.05, 0.10, 0.20] {
        for &point in &CrashPoint::ALL_COMPACTION {
            scenarios.push(super::overwrite_torn_write::template(rate, point, 50));
        }
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
    fn scenario_count_at_least_1000() {
        let scenarios = all_expanded_scenarios();
        assert!(
            scenarios.len() >= 1000,
            "expected ≥1000 scenarios, got {}",
            scenarios.len()
        );
    }

    #[test]
    fn all_scenario_names_unique() {
        let scenarios = all_expanded_scenarios();
        let mut seen = HashSet::with_capacity(scenarios.len());
        let mut dupes = Vec::new();
        for s in &scenarios {
            if !seen.insert(s.name.as_str()) {
                dupes.push(s.name.as_str());
            }
        }
        assert!(
            dupes.is_empty(),
            "{} duplicate names out of {} total. First dupes: {:?}",
            dupes.len(),
            scenarios.len(),
            &dupes[..dupes.len().min(10)],
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
