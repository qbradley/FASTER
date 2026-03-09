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

### Files
- `rust/crates/faster-dst/src/scenarios/expansion.rs` — Main expansion engine
- `rust/crates/faster-dst/src/scenarios/*.rs` — Individual template modules
- `rust/crates/faster-dst/tests/campaign_tests.rs` — Campaign test functions
