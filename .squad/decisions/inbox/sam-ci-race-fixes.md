# Decision: CI Concurrency Test Stabilization Patterns

**Author:** Sam  
**Date:** 2025-07-18  
**Scope:** Testing conventions for concurrent/stress tests

## Context

Two CI tests were flaky due to scheduling assumptions:
1. `concurrent_maintenance_with_varlen` — assumed maintenance thread gets CPU time before `done` is set.
2. `checkpoint_grow_mutual_exclusion_stress` — assumed CAS race outcomes are statistically distributed across platforms.

## Decisions

### 1. Never rely on scheduling fairness for assertions

When a test needs to verify that a background thread *ran*, use an explicit synchronization signal (e.g., `AtomicBool` with Acquire/Release) rather than assuming the OS scheduler will be fair. The main thread must wait for the signal before tearing down.

**Pattern:**
```rust
let started = Arc::new(AtomicBool::new(false));
// Background thread sets started=true after first meaningful iteration
// Main thread spins: while !started.load(Acquire) { yield_now(); }
```

### 2. Hard-assert safety, soft-assert statistics

Stress tests that verify mutual exclusion via CAS should:
- **Hard-assert** safety invariants (e.g., "both sides must never win simultaneously")
- **Soft-assert** (warn, don't fail) statistical distribution (e.g., "both sides should win some races")
- Add scheduling jitter (asymmetric `yield_now()` counts per trial) to improve contention diversity

This prevents false failures on deterministic-scheduling platforms (macOS, single-core CI) while preserving correctness coverage.

## Commit

`89989f80` on branch `rust`
