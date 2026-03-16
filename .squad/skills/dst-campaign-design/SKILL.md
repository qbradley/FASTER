# DST Campaign Design Skill

## Pattern: Parameterized Scenario Expansion

### Problem
Need to test 1000+ crash/recovery scenarios without writing 1000 test functions.

### Solution
1. **Scenario templates** — Small modules exposing `template(params) -> ScenarioTemplate`
2. **Expansion engine** — Single function `all_expanded_scenarios()` that generates all
   combinations by calling template factories with varied parameters
3. **Campaign runner** — Seeds each template across many seeds for coverage

### Key Design Rules
- Every template name must encode ALL parameters (crash point label, record count, rates)
- Use `HashSet` deduplication for cross-products that may overlap
- Keep record counts reasonable (≤3000) to bound I/O time
- Boundary-condition sizes (1, 2, 7, 15, 31, 63, 127, 255) catch off-by-one bugs
- Cross-subsystem combos (concurrent, dual, triple) reveal state machine interaction bugs

### Template Module Pattern
```rust
// src/scenarios/my_scenario.rs
pub fn template(crash_point: CrashPoint, record_count: usize) -> ScenarioTemplate {
    let name = format!("my_scenario_{record_count}_{}", crash_point.label());
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
```

### Expansion Pattern
```rust
// In expansion.rs — systematic cross-product
for &point in &CrashPoint::ALL_CHECKPOINT {
    for &size in &[10, 50, 100, 500] {
        scenarios.push(super::my_scenario::template(point, size));
    }
}
```

### Performance Budget
- ~3000 scenario instances (1000 templates × 3 seeds) in ~5 minutes
- ~0.1s per scenario instance (I/O-bound: temp dir + file write + recovery)
- Large record counts (>1000) take 2-3× longer per scenario

### Common Pitfalls

❌ **Missing parameters in name** — `format!("scenario_{}", point.label())` → name collision when called with different record counts. **Always** include all parameters in name.

❌ **Seed-dependent crash points** — Using `seed % 6` to select crash point reduces per-point coverage. Use fixed crash points, vary seeds instead.

❌ **Unbounded record counts** — Scenarios with >3000 records can dominate test suite runtime. Keep boundary tests sparse.

❌ **Manual deduplication** — Cross-products (concurrent, triple crashes) can create duplicates. Use `HashSet` to deduplicate before returning.

## Confidence: High

## Learned From

**Iteration**: 2026-03-09 DST Expanded to 108 Scenarios (Wave 2)

**Key decision**: Fixed crash points (not seed-dependent) for systematic per-point coverage. Campaign runner handles seed sweeps.

**Iteration**: 2026-03-10 Extended to 1003 Scenarios (Wave 4)

**Bug found**: `overwrite_recovery::template()` missing `count` in name → name collisions. Fixed by encoding all parameters.

**Scale achieved**: 1003 scenarios × 3 seeds = 3009 test cases in ~5 min. Zero production bugs found (confirms robustness). Expansion engine uses HashSet deduplication for triple crashes.

### Files
- `rust/crates/faster-dst/src/scenarios/expansion.rs` — Main expansion engine (generates 1003 scenarios)
- `rust/crates/faster-dst/src/scenarios/*.rs` — 14 template modules (checkpoint, compaction, recovery, fault injection)
- `rust/crates/faster-dst/tests/campaign_tests.rs` — Campaign test functions (smoke, full, category coverage)
