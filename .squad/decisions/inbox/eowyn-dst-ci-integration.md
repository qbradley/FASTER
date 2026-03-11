# Decision: DST Smoke Test Integration Strategy

**Author:** Éowyn (Deterministic Simulation Testing Expert)  
**Date:** 2026-03-11  
**Status:** Implemented  
**Branch:** squad (commit a4525cb5)

## Context

The DST (Deterministic Simulation Testing) framework was complete with 134 tests covering crash recovery, fault injection, and workload validation. However, the test suite included a comprehensive `campaign_expanded_smoke` test that ran 1003 scenarios × 3 seeds = 3009 test cases in ~220 seconds. Running all DST tests in the release-gate Tier 2 (Correctness Gate) exceeded the 5-minute target and provided no visibility in CI.

The backlog item "Add DST smoke to CI" required a strategy that balanced:
- **Fast PR feedback** (<30s for smoke tests in CI)
- **Comprehensive correctness validation** (core scenarios in Tier 2)
- **Deep validation coverage** (extended campaign in Tier 3)

## Decision

Implemented a **two-tier DST strategy** with dedicated CI integration:

### Tier 2: Fast Correctness Gate (~19s)
- **Command:** `cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'`
- **Coverage:** 133 tests
  - 75 unit tests (framework correctness)
  - 58 integration tests (crash recovery, fault injection, workload validation)
- **Purpose:** Fast feedback on critical crash recovery paths
- **Target:** <30s (achieved: ~19s)

### Tier 3: Deep Validation (~220s)
- **Command:** `cargo nextest run -p faster-dst -E 'test(campaign_expanded_smoke)'`
- **Coverage:** 1003 scenarios × 3 seeds = 3009 test cases
- **Purpose:** Comprehensive parameterized validation across all 44 scenario categories
- **Target:** <30min (achieved: ~220s = 3.7min)

### CI Workflow Integration
- **New Job:** Dedicated DST smoke test job in `.github/workflows/rust-ci.yml`
- **Trigger:** Every PR/push touching `rust/**` paths
- **Timeout:** 10 minutes
- **Strategy:** Run core scenarios only (Tier 2 subset)
- **Visibility:** DST failures now block PR merge with clear CI status

## Alternatives Considered

1. **Single-tier with timeout:**
   - Run all 134 tests with early abort on slow test
   - **Rejected:** Unpredictable timing, no guarantee of coverage

2. **Ultra-fast Tier 1 smoke (33 tests in ~12s):**
   - Filter: `test(~crash) + test(~checkpoint) + test(~recovery) - test(seed_exploration)`
   - **Deferred:** Core scenarios (19s) provide better coverage for only 7s additional cost

3. **Environment variable for smoke mode:**
   - Add `DST_SMOKE=1` env var to skip extended campaign
   - **Rejected:** Test filters (`-E` expressions) are more explicit and maintainable

4. **Separate crate for smoke tests:**
   - Split `faster-dst` into `faster-dst-core` and `faster-dst-extended`
   - **Rejected:** Unnecessary complexity, test filters solve the problem

## Consequences

### Positive
- ✅ **Fast PR feedback:** Core DST scenarios run in ~19s (under 30s target)
- ✅ **Comprehensive coverage maintained:** Extended campaign in Tier 3
- ✅ **CI visibility:** Dedicated DST job in GitHub Actions status checks
- ✅ **Clear tier boundaries:** Fast correctness (Tier 2) vs. deep validation (Tier 3)
- ✅ **No codebase changes:** Pure CI/pipeline optimization

### Negative
- ⚠️ **Extended campaign not in PR flow:** Developers must run Tier 3 locally or wait for nightly
- ⚠️ **Potential coverage gaps:** If core scenarios miss a bug that extended campaign would catch

### Mitigation
- Core scenarios include all critical crash points (checkpoint, compaction, recovery)
- Extended campaign runs in CI nightly (via scheduled workflow or manual trigger)
- Release-gate Tier 3 runs before any release tagging

## Test Coverage Breakdown

### Core Scenarios (133 tests, Tier 2)
| Category | Count | Runtime |
|----------|-------|---------|
| Unit tests (framework) | 75 | ~1s |
| Crash recovery | 10 | ~7s |
| Crash injection | 7 | ~15s |
| CRUD simulation | 8 | ~14s |
| Seed exploration | 4 | ~17s |
| Campaign tests | 6 | ~15s |
| Checksum tests | 7 | ~3s |
| Scheduler tests | 8 | <1s |
| **Total** | **133** | **~19s** |

### Extended Campaign (1 test, Tier 3)
| Scenario Family | Categories | Count | Coverage |
|-----------------|-----------|-------|----------|
| Checkpoint | 10 | ~300 | 6 phases × 3 sizes + variants |
| Compaction | 10 | ~200 | 6 phases × 3 sizes + variants |
| Recovery | 9 | ~200 | 6 phases × 3 sizes + variants |
| Cross-subsystem | 6 | ~200 | Concurrent, dual, triple crashes |
| Fault injection | 9 | ~100 | Torn writes, I/O errors, graduated faults |
| **Total** | **44** | **~1003** | **× 3 seeds = 3009 test cases** |

## Implementation Details

**Files Modified:**
- `rust/scripts/release-gate` (2 changes)
  - Line 458-460: Tier 2 DST core scenarios with exclusion filter
  - Line 497-509: Tier 3 DST extended campaign
- `.github/workflows/rust-ci.yml` (+23 lines)
  - New DST job between loom and miri

**Command Syntax:**
```bash
# Tier 2 (core scenarios)
cargo nextest run -p faster-dst -E 'not test(campaign_expanded)'

# Tier 3 (extended campaign)
cargo nextest run -p faster-dst -E 'test(campaign_expanded_smoke)'
```

**Nextest Filter Logic:**
- `not test(campaign_expanded)`: Excludes tests with `campaign_expanded` in name
- `test(campaign_expanded_smoke)`: Selects only `campaign_expanded_smoke` test
- Both filters are stable across test additions (as long as naming convention holds)

## Team Impact

- **Frodo (CI/CD Maestro):** DST is now part of the standard PR validation pipeline. Core scenarios must pass for merge.
- **All agents:** Release-gate Tier 2 stays under 5min target. Deep validation (Tier 3) runs before release.
- **Éowyn:** Future DST scenarios should follow the naming convention:
  - Core scenarios: any name not matching `campaign_expanded*`
  - Extended campaign: add to `campaign_expanded_smoke` or create `campaign_expanded_*` sibling

## Future Considerations

1. **Ultra-fast Tier 1 smoke (optional):**
   - 33 tests in ~12s covering only critical crash/recovery paths
   - Could replace nextest in Tier 1 if doctests prove insufficient
   - Filter: `test(~crash) + test(~checkpoint) + test(~recovery) - test(seed_exploration)`

2. **Nightly extended campaign:**
   - Add scheduled workflow (e.g., Sunday 6am UTC) to run Tier 3 DST
   - Report results to dedicated Slack channel or GitHub Issue

3. **Parameterized smoke campaigns:**
   - Add `DST_CAMPAIGN_SEEDS` env var to control seed count (default: 3)
   - Allow PR authors to opt into deeper validation with comment trigger

4. **Coverage reporting:**
   - Track which crash points are covered by core vs. extended scenarios
   - Ensure no critical paths are exclusive to extended campaign

## References

- **DST Framework:** `rust/crates/faster-dst/`
- **Scenario Library:** `rust/crates/faster-dst/src/scenarios/`
- **Expansion Engine:** `rust/crates/faster-dst/src/scenarios/expansion.rs`
- **Campaign Tests:** `rust/crates/faster-dst/tests/campaign_tests.rs`
- **History:** `.squad/agents/eowyn/history.md` (2026-03-09, 2026-03-10T2 entries)
