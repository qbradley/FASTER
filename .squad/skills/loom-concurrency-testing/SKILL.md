# Skill: Loom Concurrency Test Design

## When to Use

When testing lock-free concurrent data structures (epoch framework, hash buckets, CAS protocols, atomic state machines). Use loom to exhaustively explore all possible thread interleavings and find races, ABA bugs, and memory ordering violations.

## Pattern

### Algorithm Re-implementation Approach

Loom tests re-implement production algorithms using loom primitives when production code uses `std::sync::atomic` directly:

1. **Match exact bit layouts**
   - RecordInfo: 48-bit addr + 12-bit version + flag bits
   - HashBucketEntry: 48-bit addr + 14-bit tag + tentative bit
   - SystemState: 56-bit version + 8-bit phase with intermediate bit

2. **Match exact atomic orderings**
   - SeqCst for epoch bumps (global synchronization)
   - AcqRel/Acquire for CAS operations
   - Release for stores that publish data
   - Acquire for loads that consume published data

3. **Match exact protocols**
   - Two-phase tentative insert (write addr, set committed)
   - Intermediate-bit state transitions (mark + wait + commit)
   - Reentrant epoch guards (nested protect calls)

### Test Structure Pattern

```rust
#[test]
#[cfg(loom)]
fn subsystem_specific_invariant() {
    loom::model(|| {
        // Setup: Create shared state wrapped in Arc
        let state = Arc::new(AtomicU64::new(initial_value));
        
        // Spawn threads: Each tests one interleaving point
        let handles: Vec<_> = (0..THREAD_COUNT)
            .map(|i| {
                let state_clone = Arc::clone(&state);
                thread::spawn(move || {
                    // Execute protocol step
                    // Return observable outcome
                })
            })
            .collect();
        
        // Join and check invariants
        let results: Vec<_> = handles.into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        
        // Assert: Safety properties hold
        assert!(invariant_holds(&results));
    });
}
```

### Subsystem Coverage Checklist

- [ ] **Epoch Framework**: protect prevents reclamation, drain under concurrent access
- [ ] **Hash Bucket**: two-phase insert, find skips tentative entries, tag collision
- [ ] **RecordInfo**: seal visibility, revivify single winner, concurrent seal+tombstone
- [ ] **EPVS (State Machine)**: transition exclusive winner, wait_non_intermediate
- [ ] **Log Allocator**: no overlapping addresses, page boundary handling, sequential integrity
- [ ] **Combined**: multi-subsystem interactions (epoch+drain, protect+reclaim)

### Bug Classes Loom Catches

1. **Race conditions**: Unsynchronized access to shared state
2. **ABA problems**: CAS succeeds but value changed and changed back
3. **Lost updates**: Concurrent writes where one silently loses
4. **Memory ordering bugs**: Relaxed where Acquire/Release needed
5. **Partial visibility**: One thread sees torn/inconsistent multi-field update
6. **Liveness issues**: Deadlocks, livelocks (with `--max-preemptions` limit)

### Execution Pattern

```bash
# Run all loom tests (builds with loom cfg, runs exhaustive exploration)
RUSTFLAGS="--cfg loom" cargo test --features loom -p faster-core --test loom_tests

# Run single test with diagnostics
RUSTFLAGS="--cfg loom" cargo test --features loom -p faster-core --test loom_tests -- epoch_protect_prevents_reclaim --nocapture

# Increase exploration depth (default: 3 preemptions)
LOOM_MAX_PREEMPTIONS=5 RUSTFLAGS="--cfg loom" cargo test --features loom -p faster-core --test loom_tests
```

### Performance Expectations

- 2-thread tests: ~1-10 seconds (hundreds of interleavings)
- 3-thread tests: ~10-60 seconds (thousands of interleavings)
- 4+ threads: May timeout (combinatorial explosion)
- Complex protocols: Use `loom::model_max_branches` to limit exploration

### Future Improvement Path

When `crate::sync` shim is wired into production code, tests can use actual production types directly instead of re-implementing algorithms. Check if `#[cfg(loom)]` is active in production code paths before refactoring.

## Confidence: High

## Learned From

**Iteration**: 2026-03-10 Loom Concurrency Test Expansion (Wave 3)

**Context**: Expanded from 7 to 21 loom tests covering all 5 critical FASTER concurrency subsystems. Re-implementation approach chosen because production code uses `std::sync::atomic` directly, not the `crate::sync` loom shim.

**Key Insight**: Faithful re-implementation with exact bit layouts and orderings catches the same bugs production code would hit. Tests found no bugs (good news), but established baseline for regression detection.

## Key Files

- `rust/crates/faster-core/tests/loom_tests.rs` — All loom test implementations (1800 LOC, 21 tests)
- `rust/crates/faster-core/src/sync.rs` — Loom shim (not yet wired to production)
