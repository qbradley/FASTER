# Skill: DST Fault Injection Design

## When to Use

When designing deterministic simulation scenarios that test storage system behavior under I/O failures, partial writes, and resource exhaustion. Use to systematically inject faults at critical points in checkpoint/recovery/compaction paths.

## Pattern

### Fault Types and Selection

**1. Write Errors (I/O failure simulation)**
```rust
FaultConfig::builder()
    .write_limit(500) // Fail after 500 successful writes
    .build()
```
- Simulates disk full, write permission errors, device failures
- Use limits: 100 (early), 200 (mid), 500 (late) to test different failure windows
- Best for: Long-running operations (compaction, large checkpoints)

**2. Torn Writes (Partial persistence)**
```rust
FaultConfig::builder()
    .partial_write_rate(0.10) // 10% of writes are partial
    .build()
```
- Simulates power failure during write (only first N bytes persist)
- Rates: 5% (rare), 10% (moderate), 25% (aggressive)
- Best for: Testing recovery checksum validation, page boundary handling

**3. Graduated Faults (Multi-fault scenarios)**
```rust
FaultConfig::builder()
    .write_limit(200)
    .partial_write_rate(0.10)
    .build()
```
- Combines write errors + torn writes
- Tests recovery under multiple simultaneous failure modes
- Use for deep validation, not smoke tests

### Fault Injection Points

**Critical subsystems:**
- **Checkpoint**: 6 crash points (BeforeFlush, DuringFlush, AfterFlush, BeforeMetadata, DuringMetadata, AfterMetadata)
- **Compaction**: 6 crash points (BeforeCompact, DuringCompact, AfterCompact, BeforeMerge, DuringMerge, AfterMerge)
- **Recovery**: 6 crash points (BeforeValidate, DuringValidate, AfterValidate, BeforeLoad, DuringLoad, AfterLoad)

**Injection strategy:**
- Single-point: Test each crash point independently
- Concurrent: Two subsystems crash at different points (checkpoint + compaction)
- Sequential: Crash, recover, crash again during recovery
- Triple: Three subsystem crash points (rare, deep validation only)

### Scenario Template Pattern

```rust
pub fn template(
    crash_point: CrashPoint,
    record_count: usize,
    fault_config: FaultConfig,
) -> ScenarioTemplate {
    // CRITICAL: Name must encode ALL parameters for uniqueness
    let name = format!(
        "my_scenario_{}_r{}_w{}_p{}",
        crash_point.label(),
        record_count,
        fault_config.write_limit.unwrap_or(0),
        (fault_config.partial_write_rate * 100.0) as u32
    );
    
    ScenarioTemplate::builder(name)
        .workload(move |seed| {
            CrudWorkload::builder()
                .seed(seed)
                .record_count(record_count)
                .build()
        })
        .fault_config(fault_config)
        .crash_schedule(move |_seed| {
            let mut schedule = CrashSchedule::new();
            schedule.add(crash_point, CrashTrigger::Always);
            schedule
        })
        .check_committed_recoverable()
        .build()
}
```

### Fault Configuration Guidelines

1. **Smoke tests**: No faults or single fault type (write error OR torn write)
2. **Integration tests**: Single fault + crash
3. **Deep validation**: Graduated faults + multiple crash points
4. **Performance**: Fault injection adds ~10-20% overhead per scenario

### What Each Fault Tests

| Fault Type | Tests | Catches |
|------------|-------|---------|
| Write error | Resource exhaustion handling | Missing error checks, unwrap() on write |
| Torn write (no crash) | Checksum validation works | Missing checksums, stale validation |
| Torn write + crash | Recovery handles partial pages | Trusting unchecked data, missing magic numbers |
| Graduated fault | Multiple failure modes interact safely | State machine bugs under compound failures |
| No fault + crash | Clean crash recovery baseline | Crash point bugs without I/O complications |

### Debugging Failed Scenarios

When a scenario fails:
1. Check the seed — scenarios are deterministic, same seed = same failure
2. Run with `RUST_LOG=trace` to see checkpoint/recovery state machine
3. Isolate: Remove faults, test crash point alone
4. Inspect temp dir: `target/faster-dst-<seed>/log.*` files show what persisted
5. Check checksums: Torn writes should be detected, not silently accepted

## Confidence: High

## Learned From

**Iteration**: 2026-03-09 DST Expanded to 108 Scenarios (Wave 2)

**Context**: Introduced torn write scenarios, write error scenarios, and graduated fault scenarios. Initial templates did not include fault parameters in names, causing collisions.

**Key Bugs Found**: 
- Scenario name collision when `overwrite_recovery::template()` omitted `record_count` from name (fixed in Wave 4)
- No production bugs found — framework correctly handles all fault types

**Iteration**: 2026-03-10 Extended DST Campaign to 1003 Scenarios (Wave 4)

**Key Insight**: Fault injection + crash testing reveals different bugs than either alone. Graduated faults (write error + torn write + crash) test compound failure recovery.

## Key Files

- `rust/crates/faster-dst/src/fault_config.rs` — Fault configuration builder
- `rust/crates/faster-dst/src/scenarios/torn_write_varied.rs` — Torn write rates (5%, 10%, 25%)
- `rust/crates/faster-dst/src/scenarios/write_error_recovery.rs` — Write limits (100, 200, 500)
- `rust/crates/faster-dst/src/scenarios/graduated_fault_crash.rs` — Multi-fault + crash
- `rust/crates/faster-dst/src/device/faulty.rs` — Fault injection device implementation
