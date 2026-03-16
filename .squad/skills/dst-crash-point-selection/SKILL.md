# Skill: DST Crash Point Selection

## When to Use

When designing crash recovery test scenarios. Choosing where to inject crashes determines what recovery paths are tested. Strategic crash point selection maximizes bug-finding effectiveness while avoiding redundant coverage.

## Pattern

### The 18 Critical Crash Points

**Checkpoint Subsystem (6 points):**
1. `BeforeFlush` — Before any data written to disk
2. `DuringFlush` — Partial data persisted (torn checkpoint)
3. `AfterFlush` — Data complete, metadata not yet written
4. `BeforeMetadata` — About to write checkpoint token
5. `DuringMetadata` — Partial metadata write
6. `AfterMetadata` — Checkpoint fully committed

**Compaction Subsystem (6 points):**
1. `BeforeCompact` — Before compaction starts
2. `DuringCompact` — Partial compaction (some pages merged)
3. `AfterCompact` — Compaction complete, not yet committed
4. `BeforeMerge` — About to merge compacted data
5. `DuringMerge` — Partial merge
6. `AfterMerge` — Merge complete

**Recovery Subsystem (6 points):**
1. `BeforeValidate` — Before validation starts
2. `DuringValidate` — Partial validation
3. `AfterValidate` — Validation complete
4. `BeforeLoad` — About to load data
5. `DuringLoad` — Partial data loaded
6. `AfterLoad` — Recovery complete

### Selection Strategy by Test Goal

**Goal: Fast smoke test (coverage over depth)**
- Pick 1 point per subsystem: `DuringFlush`, `DuringCompact`, `DuringLoad`
- These catch most state machine bugs
- ~3 scenarios, runs in seconds

**Goal: Comprehensive subsystem test**
- All 6 points in target subsystem × 3 record sizes [10, 50, 200]
- Example: Checkpoint testing = 18 scenarios
- Catches phase-specific bugs and scale-dependent issues

**Goal: Cross-subsystem interaction testing**
- Concurrent crashes: `CheckpointDuringFlush` + `CompactionDuringCompact`
- Sequential crashes: `CheckpointAfterMetadata` → recovery → `RecoveryDuringLoad`
- Tests state machine coordination between subsystems

**Goal: Boundary condition testing**
- Pick 1 crash point, vary record counts: [1, 2, 7, 15, 31, 63, 127, 255]
- Catches off-by-one, empty-state, and power-of-two boundary bugs
- Best point: `DuringFlush` (exercises checkpointing logic)

### Crash Point vs Crash Trigger

**Crash Point**: *Where* in the code the crash can occur
```rust
CrashPoint::CheckpointDuringFlush
```

**Crash Trigger**: *When* the crash actually fires
```rust
CrashTrigger::Always         // Always crash at this point
CrashTrigger::OnVisit(2)     // Crash on 2nd visit to this point
CrashTrigger::Probability(0.5) // 50% chance per visit
```

**Default: Use `Always` for determinism**
- Scenarios should be reproducible (same seed = same crash)
- `OnVisit(N)` for delayed crashes (test multi-checkpoint scenarios)
- `Probability` for fuzzing (non-deterministic, avoid in regression tests)

### Naming Convention

Encode crash point in scenario name:
```rust
// Good: Name shows crash point
"checkpoint_crash_during_flush_r50"

// Bad: Generic name (can't tell what it tests)
"test_checkpoint_50"
```

### Coverage Matrix Pattern

For comprehensive testing, build a coverage matrix:

```rust
// In expansion.rs
for &crash_point in &CrashPoint::ALL_CHECKPOINT {
    for &size in &[10, 50, 200] {
        scenarios.push(checkpoint_crash::template(crash_point, size));
    }
}
```

This generates 6 crash points × 3 sizes = 18 scenarios automatically.

### What Each Crash Point Tests

| Crash Point | Tests | Example Bug It Would Catch |
|-------------|-------|---------------------------|
| BeforeFlush | Setup/teardown logic | Crash during state machine transition |
| DuringFlush | Partial write handling | Missing checksum validation |
| AfterFlush | Commit protocol | Metadata commit before data flush |
| BeforeMetadata | Transaction boundary | Using partial checkpoint |
| DuringMetadata | Torn metadata handling | Missing metadata validation |
| AfterMetadata | Post-commit cleanup | Resource leak after commit |

### Crash Point Selection Anti-Patterns

❌ **Testing every single line** — Crashes in `Before*` and `After*` often behave identically; pick representative points

❌ **Ignoring cross-subsystem** — Concurrent checkpoint+compaction reveals different bugs than isolated testing

❌ **Only testing `DuringFlush`** — Other phases have unique bugs (metadata corruption, transaction boundaries)

❌ **Non-deterministic triggers** — Use `Always` for regression tests; save `Probability` for fuzzing

### Crash Point Evolution

**Start small (5 scenarios):**
- `CheckpointDuringFlush`
- `CompactionDuringCompact`
- `RecoveryDuringLoad`
- `CheckpointAfterMetadata`
- `CompactionBeforeCompact`

**Expand systematically (108 scenarios):**
- All 6 × 3 for each subsystem (18 each)
- Cross-subsystem concurrent (36)
- Fault injection variants (36)

**Deep validation (1003 scenarios):**
- Add boundary conditions (8 sizes × 18 crash points)
- Triple crashes (checkpoint + compaction + recovery)
- Graduated faults (multi-fault × crash points)

## Confidence: High

## Learned From

**Iteration**: 2026-03-09 DST Expanded to 108 Scenarios (Wave 2)

**Context**: Started with 5 ad-hoc crash points, expanded to systematic coverage of all 18 points across 3 subsystems.

**Key Decision**: Use fixed crash points (not `seed % 6` selection) for better per-point coverage. Each crash point tested independently across multiple seeds.

**Iteration**: 2026-03-10 Extended to 1003 Scenarios (Wave 4)

**Key Insight**: Systematic crash point selection (all 18 points × multiple sizes × fault configs) found zero new bugs, confirming robustness. Coverage completeness matters more than depth at any single point.

**Bug Found**: Initial `overwrite_recovery::template()` didn't include crash point label in name, causing collisions. Fix: Always include ALL parameters in scenario name.

## Key Files

- `rust/crates/faster-dst/src/crash_point.rs` — All 18 crash point definitions
- `rust/crates/faster-dst/src/crash_schedule.rs` — Crash trigger logic
- `rust/crates/faster-dst/src/scenarios/expansion.rs` — Systematic crash point coverage matrix
- `rust/crates/faster-dst/src/scenarios/checkpoint_crash.rs` — Example checkpoint crash testing
