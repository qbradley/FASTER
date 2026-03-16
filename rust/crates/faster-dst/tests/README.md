# DST Concurrency Test Suite

Deterministic Simulation Testing (DST) for FASTER's concurrency subsystems.
These tests exercise real bug classes — pipeline deadlocks, lossy eviction
starvation — under controlled, reproducible scheduling.

## Quick Start

```bash
# From rust/ directory:

# Run tier-1 concurrency scenarios (canary seeds, ~1s)
cargo nextest run -p faster-dst --test dst_concurrency

# Run tier-2 concurrency scenarios (ignored by default, ~2s)
cargo nextest run -p faster-dst --test dst_concurrency -- --ignored

# Run the full campaign: all scenarios × canary seeds (~5s)
cargo nextest run -p faster-dst --test dst_campaign -- --ignored

# Run everything in faster-dst (including extended campaign, ~3-4 min)
cargo nextest run -p faster-dst
```

## Test Structure

### Concurrency Scenarios (`dst_concurrency.rs`)

| Test | Tier | Description |
|------|------|-------------|
| `dst_single_writer_baseline` | 1 | Single writer, no contention — validates runner + SimDeviceV2 end-to-end |
| `dst_multi_writer_saturation` | 1 | 16 writers competing for 32 buffer pages — deadlock detection |
| `dst_bug1_pipeline_deadlock` | 2 (`#[ignore]`) | Exercises the multi-writer pipeline deadlock bug class |
| `dst_bug2_lossy_truncate_starvation` | 2 (`#[ignore]`) | Exercises the lossy truncation starvation bug class |

### Campaign Tests (`dst_campaign.rs`)

| Test | Tier | Description |
|------|------|-------------|
| `campaign_smoke_test` | 2 (`#[ignore]`) | All scenarios × canary seeds — full matrix sweep |

### Other Test Files

| File | Description |
|------|-------------|
| `campaign_tests.rs` | Extended campaign with 1003 scenarios × 3 seeds (3009 cases) |
| `checksum_tests.rs` | Crash recovery checksum validation |
| `crash_injection.rs` | Fault injection at checkpoint/compaction crash points |
| `crash_recovery.rs` | Recovery correctness after simulated crashes |
| `crud_simulation.rs` | Deterministic CRUD operations under simulation |
| `scheduler_tests.rs` | Cooperative scheduler unit tests |
| `seed_exploration.rs` | Property-based seed space exploration |

## Seed Mechanism

Every scenario is parameterized by a `u64` seed that controls:
- **SimDeviceV2 I/O timing:** Completion latencies and queue pressure patterns
- **Worker thread scheduling:** Deterministic interleaving order
- **Key distribution:** Which keys are touched and in what order

### Seed Categories

1. **Canary seeds** (`concurrency/seeds.rs::canary::CANARY_SEEDS`):
   5 hand-picked seeds that exercise diverse code paths. Used in PR gates.
   ```
   0x0000_0000_0000_0001  — minimal, near-zero
   0xDEAD_BEEF_CAFE_BABE  — high bits set
   0x0123_4567_89AB_CDEF  — sequential bit pattern
   0xFFFF_FFFF_FFFF_FFFF  — all ones, edge case for masking
   0x5555_5555_5555_5555  — alternating bits, stresses XOR paths
   ```

2. **Exploration seeds** (`seeds::exploration_seeds(root, count)`):
   ChaCha20-derived seeds from a root. Same `(root, count)` → same output.
   Used in nightly extended campaigns for broad coverage.

3. **Regression seeds** (`seeds::regression::REGRESSION_SEEDS`):
   Seeds that caught real bugs. **Append-only, never remove.** Currently empty —
   will accumulate as bugs are found and fixed.

### Reproducing a Failure

When a test fails, it prints a reproduction command:

```
FAIL: pipeline_deadlock seed=0xDEADBEEFCAFEBABE assertion=...
Reproduce: cargo test -p faster-dst --test dst_concurrency -- pipeline_deadlock --seed 0xDEADBEEFCAFEBABE
```

To reproduce manually:
1. Copy the printed reproduction command
2. Run it from `rust/`
3. The same seed produces the same scheduling decisions → same failure

## Adding New Scenarios

1. **Define the config factory** in `src/concurrency/scenarios.rs`:
   ```rust
   pub fn my_new_scenario_config(seed: u64) -> (ScenarioConfig, ScenarioAssertions) {
       let config = ScenarioConfig {
           num_writers: 4,
           ops_per_writer: 1_000,
           // ... tune for the bug class you're targeting
           seed,
       };
       let assertions = ScenarioAssertions {
           all_writers_complete: true,
           ..ScenarioAssertions::default()
       };
       (config, assertions)
   }
   ```

2. **Register in `ALL_SCENARIOS`** (same file):
   ```rust
   pub static ALL_SCENARIOS: &[(&str, ScenarioFactory)] = &[
       // ... existing scenarios ...
       ("my_new_scenario", my_new_scenario_config),
   ];
   ```

3. **Add a test function** in `tests/dst_concurrency.rs`:
   ```rust
   #[test]
   fn dst_my_new_scenario() {
       let failures = run_scenario_on_canary_seeds("my_new_scenario", my_new_scenario_config);
       assert_eq!(failures, 0, "my_new_scenario: {failures} seed(s) failed");
   }
   ```
   Use `#[ignore]` for tier-2 (bug-exercise) scenarios that may legitimately fail.

4. **Run it:**
   ```bash
   cargo nextest run -p faster-dst --test dst_concurrency -- dst_my_new_scenario
   ```

## Adding Regression Seeds

When a bug is found via a specific seed:

1. **Add the seed** to `src/concurrency/seeds.rs`:
   ```rust
   pub mod regression {
       pub const REGRESSION_SEEDS: &[u64] = &[
           0xDEAD_BEEF_CAFE_BABE, // #42: pipeline deadlock under 16-writer saturation
       ];
   }
   ```

2. **Document the bug** in a comment next to the seed (issue number + short description).

3. **Never remove regression seeds** — they guard against regressions forever.

## Release Gate Tiers

The release gate (`scripts/release-gate`) runs 4 validation tiers:

| Tier | What | Time | When |
|------|------|------|------|
| **1** | Formatting, clippy, compile checks | ~30s | Every PR |
| **2** | All tests: faster-core + faster-dst smoke + loom | ~30s | Every PR |
| **3** | Extended DST campaign (1003+ scenarios) + Miri | ~5 min | Pre-release / nightly |
| **4** | Benchmarks + semver checks | ~2 min | Release candidates |

### CI Pipeline (GitHub Actions)

| Job | Trigger | What Runs |
|-----|---------|-----------|
| `ci` | push/PR | Compile + clippy + `cargo test --workspace` (includes tier-1 DST concurrency) |
| `loom` | push/PR | Loom concurrency tests (25 tests) |
| `dst` | push/PR | DST smoke tests — all non-campaign faster-dst tests (~178 tests) |
| `dst-extended` | weekly/manual | Extended campaign (3009 test cases) |
| `miri` | weekly/manual | Miri undefined behavior detection |

### PR Gate (~5 min)
Runs automatically on every push/PR touching `rust/**`:
```bash
cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'
```
This includes the tier-1 DST concurrency scenarios (single_writer_baseline,
multi_writer_saturation) plus all crash recovery, fault injection, and
scheduler tests.

### Nightly Extended (~15 min)
Runs on weekly schedule or manual trigger:
```bash
cargo nextest run -p faster-dst -E 'test(campaign_expanded)'
```
1003 scenarios × 3 seeds = 3009 test cases covering all 44 scenario categories.

## Test Counts (Current)

| Suite | Tests | Skipped | Time |
|-------|-------|---------|------|
| faster-core | 1722 | 40 | ~6s |
| faster-dst | 178 | 9 | ~204s |
| loom | 25 | 0 | ~8s |
| **Total** | **1925** | **49** | — |
