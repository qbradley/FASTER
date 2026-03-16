# Skill: Deterministic Seed Strategy for Simulation Testing

## When to Use

When writing deterministic simulation tests that need to be reproducible across runs while still providing coverage through variation. Every DST scenario uses a seed to control randomness deterministically.

## Pattern

### The Golden Rule

**Same seed = Same execution = Same result**

Every source of randomness in a scenario must be seeded:
- Workload generation (which keys to insert/update/delete)
- Crash timing (when using probability-based triggers)
- Fault injection (which writes get partial/failed)
- Thread scheduling (for concurrent scenarios)

### Seed Usage Pattern

```rust
pub fn template(crash_point: CrashPoint, record_count: usize) -> ScenarioTemplate {
    ScenarioTemplate::builder(name)
        .workload(move |seed| {
            // CRITICAL: Use seed parameter, don't generate new randomness
            CrudWorkload::builder()
                .seed(seed)  // <-- Pass seed through
                .record_count(record_count)
                .build()
        })
        .crash_schedule(move |seed| {
            // If crash timing is seed-dependent (rare)
            let trigger = if seed % 10 < 3 {
                CrashTrigger::OnVisit(2)
            } else {
                CrashTrigger::Always
            };
            
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, trigger);
            schedule
        })
        .build()
}
```

### Campaign Seed Strategy

**Smoke tests: 3 seeds**
```rust
let seeds = vec![42, 1337, 9999];
```
- Fast feedback (3× base scenario count)
- Covers basic variation
- Runtime: ~30s for 1000 scenarios

**Integration tests: 10 seeds**
```rust
let seeds = (0..10).collect();
```
- Moderate coverage (10× base scenario count)
- Catches most seed-dependent bugs
- Runtime: ~5min for 1000 scenarios

**Deep validation: 100 seeds**
```rust
let seeds = (0..100).collect();
```
- Exhaustive coverage (100× base scenario count)
- Nightly/pre-release only
- Runtime: ~50min for 1000 scenarios

### What Seeds Control

**In CRUD workloads:**
```rust
let mut rng = StdRng::seed_from_u64(seed);
let keys: Vec<u64> = (0..record_count)
    .map(|_| rng.gen_range(0..1_000_000))
    .collect();
```
- Which keys are inserted
- Order of operations
- Update vs delete ratio
- Key distribution (uniform, skewed, clustered)

**Determinism guarantee:**
- `StdRng::seed_from_u64(42)` always generates same sequence
- Same scenario + same seed = identical I/O pattern
- Failed test can be reproduced exactly with same seed

### Debugging with Seeds

**When a scenario fails:**
1. Note the seed from test output: `"Failed: scenario_name seed=1337"`
2. Run single scenario with that seed:
   ```rust
   #[test]
   fn reproduce_failure() {
       let scenario = my_scenario::template(crash_point, 100);
       let result = run_scenario(&scenario, 1337);
       assert!(result.is_ok());
   }
   ```
3. Add debug logging: `RUST_LOG=trace cargo test reproduce_failure`
4. Inspect temp dir: `target/faster-dst-1337/`

### Seed Selection Guidelines

**For regression tests:**
- Use fixed seeds: `vec![42, 1337, 9999]`
- Document why these seeds were chosen (found bugs, edge cases)
- Never randomize seeds in CI

**For fuzzing:**
- Generate seed from timestamp: `let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();`
- Log seed prominently: `println!("Fuzzing with seed: {}", seed);`
- On failure, convert seed to fixed regression test

**For stress testing:**
- Use sequential seeds: `(0..1000).collect()`
- Ensures coverage of seed space
- Easier to bisect failures (seed 573 fails → test seeds 570-576)

### Anti-Patterns

❌ **Don't generate randomness without seeding**
```rust
// BAD: Non-reproducible
let mut rng = rand::thread_rng();
let key = rng.gen();
```

❌ **Don't ignore the seed parameter**
```rust
// BAD: Seed parameter unused
.workload(move |_seed| {
    CrudWorkload::fixed_pattern() // Always same workload
})
```

❌ **Don't use randomness for crash point selection in regression tests**
```rust
// BAD: Non-deterministic crash timing
if rand::random::<f64>() < 0.5 {
    schedule.add(point, CrashTrigger::Always);
}
// GOOD: Use seed to decide
if seed % 2 == 0 {
    schedule.add(point, CrashTrigger::Always);
}
```

### Seed Coverage Strategy

**Scenario expansion approach:**
- 1003 scenario templates
- × 3 seeds (smoke) / 10 seeds (integration) / 100 seeds (deep)
- = 3,009 / 10,030 / 100,300 test cases

**Coverage achieved:**
- Different key distributions
- Different operation orders
- Different hash table states
- Different compaction triggers

**Bugs found:**
- Zero (as of Wave 4) — no seed-dependent bugs in recovery paths
- Confirms robustness across workload variations

### Performance Considerations

**Seed iteration overhead:**
- Scenario setup: ~5ms per seed (temp dir creation, device init)
- Workload execution: ~10-50ms per seed (I/O-bound)
- Recovery verification: ~1-5ms per seed

**Total runtime:**
- 1000 scenarios × 3 seeds × 50ms = ~150s = 2.5 min
- Parallelization (nextest): ~5× speedup on 8-core machine

**When to reduce seed count:**
- CI smoke tests: 3 seeds max (fast feedback)
- PR validation: 10 seeds (moderate coverage)
- Nightly/release: 100 seeds (exhaustive)

## Confidence: High

## Learned From

**Iteration**: 2026-03-09 DST Expanded to 108 Scenarios (Wave 2)

**Context**: Campaign runner sweeps scenarios across multiple seeds. Initial design used 3 seeds for smoke, discovered no seed-dependent bugs.

**Key Decision**: All randomness goes through `StdRng::seed_from_u64(seed)` for reproducibility. Test failures can be reproduced exactly by running with same seed.

**Iteration**: 2026-03-10 Extended to 1003 Scenarios (Wave 4)

**Testing**: 1003 scenarios × 3 seeds = 3009 test cases, all deterministic. Zero flaky failures across multiple runs. Determinism confirmed.

**Iteration**: 2026-03-11 DST Smoke Tests in CI

**Performance tuning**: 3 seeds for Tier 2 (fast, ~19s for 133 scenarios), 10 seeds for Tier 3 (deep, ~220s for 1003 scenarios). Seed count is the coverage knob.

## Key Files

- `rust/crates/faster-dst/src/runner.rs` — Campaign runner with seed iteration
- `rust/crates/faster-dst/src/workload.rs` — `StdRng::seed_from_u64()` usage
- `rust/crates/faster-dst/tests/campaign_tests.rs` — Seed count configuration per test tier
