# Skill: DST Recovery Verification Pattern

## When to Use

When writing DST scenarios that test crash recovery. Every scenario that injects a crash must verify recovery correctness — the system must recover to a consistent state that matches what was committed before the crash.

## Pattern

### The Recovery Correctness Invariant

**Rule**: After crash recovery, all committed operations must be visible, and no uncommitted operations may be visible.

### Verification Levels

**1. Check Committed Recoverable (Standard)**
```rust
ScenarioTemplate::builder(name)
    .workload(|seed| my_workload(seed))
    .crash_schedule(|seed| my_crash_schedule(seed))
    .check_committed_recoverable() // <-- Standard check
    .build()
```

**What it does:**
- Tracks which operations completed before crash
- After recovery, verifies all committed keys are readable
- Verifies values match what was written
- Fails if any committed key is missing or has wrong value

**2. Check No Data Loss (Stricter)**
```rust
.check_no_data_loss()
```
- Same as committed_recoverable, but also checks record count
- Fails if record count after recovery < record count before crash
- Use when testing checkpoint/compaction that should preserve all data

**3. Custom Verification**
```rust
.custom_verification(|runner| {
    // Access runner.store after recovery
    // Run custom validation logic
    Ok(())
})
```

### Workload Tracking Pattern

The DST framework automatically tracks committed operations:

```rust
// During scenario execution
workload.insert(key, value); // Tracked internally
// If crash happens here, insert is "committed" if fsync completed

// After recovery
// Framework checks: store.get(key) == Some(value)
```

### Recovery State Validation Checklist

- [ ] **Data integrity**: All committed keys recoverable with correct values
- [ ] **No phantom data**: Uncommitted operations did not persist
- [ ] **Metadata consistency**: Version numbers, checksums, extent maps valid
- [ ] **Log consistency**: No orphaned pages, all segments accounted for
- [ ] **Index consistency**: Hash table matches recovered records

### Crash Point Coverage Strategy

Test recovery after crash at each critical phase:

**Checkpoint subsystem:**
- BeforeFlush: No data written, recovery sees pre-checkpoint state
- DuringFlush: Partial data persisted, recovery validates checksums
- AfterFlush: Data complete, metadata incomplete, recovery ignores partial checkpoint
- BeforeMetadata: Data complete, metadata being written, recovery validates token
- DuringMetadata: Metadata partial, recovery sees previous checkpoint
- AfterMetadata: Checkpoint complete, recovery uses new checkpoint

**For each crash point, verify:**
1. Recovery completes without panic
2. Store is readable after recovery
3. Committed data matches expectations
4. No corruption detected (checksums pass)

### Debugging Recovery Failures

When recovery verification fails:

1. **Check crash timing**
   ```rust
   // Was crash before or after commit point?
   println!("Crash point: {:?}", crash_point);
   println!("Operations before crash: {}", committed_count);
   ```

2. **Inspect persisted state**
   ```bash
   # Temp dir preserved on failure
   ls -lh target/faster-dst-<seed>/
   hexdump -C target/faster-dst-<seed>/log.0 | head
   ```

3. **Check recovery logs**
   ```bash
   RUST_LOG=trace cargo test scenario_name -- --nocapture
   # Look for: "Recovery: Found checkpoint", "Validating checksums"
   ```

4. **Verify checksum behavior**
   - Torn writes should fail checksum, not silently load garbage
   - Missing files should be handled gracefully
   - Version mismatches should be detected

### Common Recovery Bugs

| Symptom | Root Cause | Fix |
|---------|------------|-----|
| Committed data missing | Crash before fsync | Adjust crash point timing |
| Uncommitted data visible | Missing transaction boundary | Add commit marker |
| Checksum failure | Torn write not detected | Validate page checksums |
| Wrong value recovered | Overwrite not persisted | Check flush ordering |
| Panic during recovery | Missing null/error check | Handle corrupted metadata |

### Recovery Performance Bounds

- Small workload (10 records): ~1-5ms recovery
- Medium workload (100 records): ~5-20ms recovery
- Large workload (1000 records): ~20-100ms recovery
- Recovery time grows with log size and checkpoint size

Scenarios with >2000 records may slow down test suite — use sparingly.

### Integration with Fault Injection

Recovery verification works with fault injection:

```rust
ScenarioTemplate::builder(name)
    .workload(|seed| workload(seed))
    .fault_config(FaultConfig::builder()
        .partial_write_rate(0.10) // Torn writes
        .build())
    .crash_schedule(|_| {
        let mut sched = CrashSchedule::new();
        sched.add(CrashPoint::CheckpointDuringFlush, CrashTrigger::Always);
        sched
    })
    .check_committed_recoverable() // Verification handles torn writes
    .build()
```

Framework expectations:
- Torn writes fail checksum validation
- Recovery ignores partial checkpoint
- Falls back to previous good checkpoint
- All committed data still recoverable

## Confidence: High

## Learned From

**Iteration**: 2026-03-09 DST Expanded to 108 Scenarios (Wave 2)

**Context**: All 108 scenarios use `.check_committed_recoverable()`. No recovery failures found — framework handles all crash points correctly.

**Key Insight**: The "committed before crash" concept is the key invariant. Framework tracks this automatically, so scenario authors just declare `.check_committed_recoverable()` and focus on workload design.

**Iteration**: 2026-03-10 Extended to 1003 Scenarios (Wave 4)

**What was tested**: 1003 scenarios × 3 seeds = 3009 recovery cycles. Zero recovery failures. Recovery is robust across:
- All 18 crash points (6 checkpoint + 6 compaction + 6 recovery)
- Fault injection (torn writes, write errors, graduated faults)
- Cross-subsystem crashes (concurrent, dual, triple)
- Boundary conditions (1, 2, 7, 15, 31, 63, 127, 255, 500, 1000, 2000 records)

## Key Files

- `rust/crates/faster-dst/src/scenario.rs` — `check_committed_recoverable()` builder method
- `rust/crates/faster-dst/src/runner.rs` — Recovery verification logic
- `rust/crates/faster-core/src/recovery/log_recovery.rs` — Actual recovery implementation
- `rust/crates/faster-dst/tests/crash_recovery.rs` — Recovery integration tests
