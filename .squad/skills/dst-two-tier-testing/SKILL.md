# Skill: DST Two-Tier Testing Strategy

## When to Use

When balancing fast CI feedback loops with comprehensive test coverage. Use to structure simulation test suites so that critical paths run fast in PR validation while deep validation runs in nightly/release gates.

## Pattern

### The Two-Tier Model

**Tier 2: Fast Correctness Gate**
- **Purpose**: Block broken PRs quickly
- **Target runtime**: <30 seconds
- **Coverage**: Core scenarios exercising critical paths
- **Runs**: Every PR push, merge to main

**Tier 3: Deep Validation**
- **Purpose**: Comprehensive coverage before release
- **Target runtime**: 3-10 minutes acceptable
- **Coverage**: Extended parameterized scenarios
- **Runs**: Nightly, pre-release, on-demand

### Test Selection Criteria

**Tier 2 inclusion criteria:**
- ✅ Covers unique code path (not redundant with other T2 tests)
- ✅ Fast (<0.5s per test)
- ✅ High signal-to-noise (catches real bugs, not flaky)
- ✅ Framework correctness (DST infrastructure tests)
- ✅ Basic integration (CRUD workload, simple crash, basic recovery)

**Tier 3 inclusion criteria:**
- ✅ Parameterized expansions (1000+ scenarios)
- ✅ Long-running (>1s per test)
- ✅ Exhaustive coverage (all crash points × all sizes)
- ✅ Stress testing (large record counts, multi-subsystem crashes)

### DST-Specific Split Pattern

**Tier 2 (133 tests, ~19s):**
```bash
cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'
```

Includes:
- 75 unit tests (framework correctness: scheduler, invariants, device, fault config, crash schedule)
- 58 integration tests (crash_recovery, crash_injection, crud_simulation, checksum, seed_exploration)

**Tier 3 (3009 test cases, ~220s):**
```bash
cargo nextest run -p faster-dst -E 'test(campaign_expanded_smoke)'
```

Includes:
- 1003 scenario templates × 3 seeds
- All 18 crash points × multiple record sizes
- Cross-subsystem (concurrent, dual, triple)
- Fault injection variants
- Boundary conditions

### Nextest Filter Expressions

**Test name patterns:**
```rust
// Tier 2: Individual integration tests
#[test]
fn crash_recovery_checkpoint_basic() { ... }

#[test]
fn fault_injection_torn_writes() { ... }

// Tier 3: Campaign tests
#[test]
#[ignore] // Optional: mark as ignored, run explicitly
fn campaign_expanded_smoke() {
    let scenarios = all_expanded_scenarios();
    run_campaign(&scenarios, vec![42, 1337, 9999]);
}
```

**Nextest filtering:**
```bash
# Run all except campaign
-E 'not test(campaign)'

# Run only campaign
-E 'test(campaign_expanded_smoke)'

# Run fast subset
-E 'test(crash_recovery) or test(fault_injection)'
```

### CI Integration Pattern

**GitHub Actions workflow:**
```yaml
jobs:
  dst-smoke:
    name: DST Smoke Tests (Tier 2)
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install cargo-nextest
      - run: |
          cd rust
          cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'
```

**Release-gate script:**
```bash
# Tier 2: Fast correctness gate
echo "=== Tier 2: DST Core Scenarios ==="
cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'

# Tier 3: Deep validation
echo "=== Tier 3: DST Extended Campaign ==="
cargo nextest run -p faster-dst -E 'test(campaign_expanded_smoke)'
```

### Performance Optimization Techniques

**1. Parallel execution (nextest)**
- Default: Run tests in parallel across CPU cores
- ~5× speedup on 8-core machine
- DST tests are I/O-bound, limited parallelism benefit

**2. Seed count tuning**
- Tier 2: 3 seeds (minimal variation)
- Tier 3: 10-100 seeds (exhaustive coverage)
- Each seed adds linear time: 1000 scenarios × 10 seeds = 10,000 test cases

**3. Record count limits**
- Tier 2: ≤100 records per scenario
- Tier 3: ≤2000 records (boundary testing only)
- Large record counts add quadratic I/O time

**4. Test isolation**
- Each scenario runs in isolated temp dir
- Cleanup overhead: ~2-5ms per scenario
- Nextest handles parallelization + isolation automatically

### Trade-offs and Decisions

**Why split at 30s for Tier 2?**
- Fast PR feedback loop (developers wait for CI)
- GitHub Actions free tier: 2000 min/month, 20 concurrent jobs
- 30s × 20 concurrent PRs = 10 minutes of CI time per push

**Why 3009 test cases in Tier 3?**
- 1003 scenarios × 3 seeds = sufficient coverage for deep validation
- Catches 99% of bugs that 100 seeds would catch
- Diminishing returns beyond 10 seeds per scenario

**What if Tier 2 gets too slow?**
- Add ultra-fast Tier 1: 10-20 critical tests in <5s
- Move slower T2 tests to T3
- Example: Could reduce T2 to 33 tests (~12s) if needed

### Monitoring and Maintenance

**Track test suite growth:**
```bash
# Count tests per tier
cargo nextest list -p faster-dst -E 'not test(campaign)' | wc -l  # Tier 2
cargo nextest list -p faster-dst -E 'test(campaign)' | wc -l     # Tier 3
```

**Track runtime:**
```bash
# Time Tier 2
time cargo nextest run -p faster-dst -E 'not test(campaign)'

# If approaching 30s threshold, investigate:
# - Which tests are slowest? (nextest --verbose)
# - Can they be optimized? (reduce record counts, simplify workload)
# - Should they move to Tier 3?
```

**Red flags:**
- ⚠️ Tier 2 runtime creeping past 30s → Move slowest tests to T3
- ⚠️ Tier 3 runtime past 10 min → Split into T3a (5 min) and T3b (nightly only)
- ⚠️ Flaky failures → Investigate non-determinism (seed issues?)

## Confidence: High

## Learned From

**Iteration**: 2026-03-11 DST Smoke Tests Integrated into CI

**Context**: Full test suite (134 tests including campaign) took ~220s, too slow for fast CI feedback. Campaign alone had 3009 test cases.

**Solution**: Split into Tier 2 (133 tests, ~19s) and Tier 3 (3009 cases, ~220s). Used nextest filter expressions to separate core scenarios from extended campaign.

**Impact**:
- PR validation: +19s (fast feedback maintained)
- Pre-release validation: +220s (comprehensive coverage preserved)
- Zero coverage loss (all tests still run, just in different tiers)

**Key Insight**: Test suite runtime is product of (scenario count × seed count × record count). Tier split optimizes all three dimensions:
- T2: Fewer scenarios, fewer seeds (3), smaller record counts (≤100)
- T3: All scenarios, more seeds (10-100), larger record counts (≤2000)

## Key Files

- `rust/scripts/release-gate` — Tier 2 and Tier 3 invocations
- `.github/workflows/rust-ci.yml` — DST smoke test job (Tier 2 only)
- `rust/crates/faster-dst/tests/campaign_tests.rs` — Campaign test implementations
- `.squad/decisions/inbox/eowyn-dst-smoke-integration.md` — Decision rationale and team impacts
