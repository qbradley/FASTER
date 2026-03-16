# Post-Mortem: Why Automated Tests Missed Two Production Deadlocks

**Author:** Gandalf (Lead Architect)  
**Date:** 2026-03-16  
**Status:** Post-Mortem Analysis  
**Requested by:** qbradley  

---

## Executive Summary

Two P0/P1 deadlocks — a flush-pipeline circular dependency and a lossy-mode lock starvation bug — survived our 2,100+ test suite and only surfaced in stress testing at scale. This wasn't a gap in test *quantity*; it was a structural mismatch between what our validation tools *can* model and what these bugs *require* to manifest. Both bugs belong to categories that are fundamentally invisible to our current tooling: multi-component pipeline stalls and scale-dependent resource exhaustion.

---

## Section 1: The Bugs

### Bug 1: P1 Multi-Writer Flush Pipeline Deadlock (Iteration 7)

| Attribute | Detail |
|-----------|--------|
| **Root cause** | Three-point circular dependency: writers fill buffer → eviction needs contiguously flushed pages → flush pipeline hits I/O back-pressure and reverts pages to Sealed → head can't advance → writers permanently blocked |
| **Category** | **Pipeline coordination deadlock** — not a lock ordering bug, not a data race. It's a *progress* failure across three cooperating subsystems. |
| **Trigger conditions** | ≥2 writer threads, linear key distribution (forces page allocation), small circular buffer, any I/O back-pressure |
| **Time to manifest** | ~70 seconds under 16-thread stress |
| **Fix** | Three-fix protocol: break-on-QueueFull, poll-completions in retry loop, maintenance integration |
| **Key files** | `flush.rs`, `operations.rs`, `kv.rs` (maintenance) |

### Bug 2: P0 Lossy Mode InMemoryDevice Deadlock (This Session)

| Attribute | Detail |
|-----------|--------|
| **Root cause** | `InMemoryDevice::truncate_until()` called `guard[..offset].fill(0)` under `RwLock` write lock. In lossy mode, the Vec grew unboundedly (~9GB after 200s). Each `truncate_until` held the write lock for 1-3 seconds, starving ALL concurrent `write_async` calls (flush pipeline blocked → 0 ops/s) |
| **Category** | **Scale-dependent resource exhaustion** — the `fill(0)` operation is O(total_bytes_flushed), which grows monotonically over time. It's correct at small scale, catastrophic at large scale. |
| **Trigger conditions** | Lossy mode + sustained write load + >100 seconds runtime + InMemoryDevice (not file-backed) |
| **Time to manifest** | ~200 seconds under 16 threads |
| **Fix** | `truncate_until()` → no-op (data below begin_address is never read) |
| **Key files** | `device.rs:557` (InMemoryDevice::truncate_until) |

---

## Section 2: Why Loom Didn't Catch These

### What Loom Actually Tests (21 tests in `loom_tests.rs`)

Our loom tests are well-written and cover critical low-level concurrency primitives:

| Test Category | Count | What It Verifies |
|---------------|-------|-----------------|
| B2: Treiber stack (free-list) | 1 | ABA prevention, no lost/duplicated items |
| B3: Hash bucket CAS | 1 | Concurrent slot insertion, no lost updates |
| B4/D1: Epoch protect/unprotect | 3 | Safe-epoch invariant, premature reclamation prevention |
| B5: Bump pool allocator | 1 | Distinct address allocation |
| B6: Epoch-gated free list | 1 | Deferred push correctness |
| C0: Drain list | 2 | Concurrent push + atomic-swap drain |
| D2: Hash bucket two-phase insert | 3 | Tentative bit protocol, concurrent find |
| D3: RecordInfo seal/revivify | 3 | Seal visibility, single-winner revivify |
| D4: State transitions | 2 | Exclusive winner CAS, non-intermediate wait |
| D5: Log allocator | 3 | Non-overlapping addresses, page boundary crossing |

### Why Loom *Cannot* Catch These Deadlocks

**Fundamental limitation #1: Loom tests re-implement minimal algorithms, not the real system.**

Every loom test creates a standalone micro-model using `loom::sync` primitives. They don't (and can't) use our actual `FasterKv`, `HybridLog`, `Flusher`, or `Evictor` types. This is by design — loom's state-space exploration is exponential in the number of atomic operations.

- **Bug 1** requires the *interaction* of three subsystems: allocator retry loop + flush pipeline + eviction head advancement. No single loom model covers this pipeline. You'd need to model the entire flush-evict-allocate cycle, which would have millions of interleavings.
- **Bug 2** requires the *interaction* of `InMemoryDevice::truncate_until()` (write lock + O(n) fill) with `write_async()` (write lock for data copy). This is an `RwLock` contention issue — loom models lock-free atomics, not `RwLock` starvation.

**Fundamental limitation #2: Loom can't model time or scale.**

Loom explores all *orderings* of a fixed set of operations. It cannot model:
- Operations that become expensive at scale (Bug 2: `fill(0)` is O(1) for small data, O(n) for 9GB)
- Time-dependent starvation (Bug 2: lock held for 1-3 seconds)
- Pipeline throughput cliffs (Bug 1: only manifests when buffer fills, requiring sustained load)
- Back-pressure cascades (Bug 1: QueueFull only occurs under I/O saturation)

**Fundamental limitation #3: State space explosion.**

Even our smallest loom tests use 2 threads with 2-4 operations each. Modeling the flush pipeline would require:
- 16 writer threads × N operations each
- Flush thread scanning pages
- Eviction thread checking page states
- I/O callback threads
- Page state machine (Open → Sealed → Flushing → Flushed → Evicted)

The state space would be astronomically large — loom would never complete.

### Verdict: Loom is the right tool for primitive correctness, but wrong tool for system-level progress.

---

## Section 3: Why Existing Tests Didn't Catch These

### Test Coverage Analysis

| Test Category | Count | Concurrency | Scale | Duration | Catches Bug 1? | Catches Bug 2? |
|---------------|-------|-------------|-------|----------|-----------------|-----------------|
| Loom tests | 21 | Modeled (2 threads) | Tiny (2-4 ops) | Exhaustive | ❌ | ❌ |
| Deadlock regression tests | 15 | Real (2-4 threads) | Small (4-8 pages) | <10s | ✅ (post-fix) | ❌ |
| Unit tests | ~1,700+ | Single-thread | Small | <1s each | ❌ | ❌ |
| Integration tests | ~200+ | 1-2 threads | Medium | <5s each | ❌ | ❌ |

### What's Missing in the Test Suite

**1. No sustained multi-writer stress tests in CI:**
The deadlock regression tests (`deadlock_tests.rs`) use 2-4 threads for 5-10 seconds with tiny buffers. Bug 1 needed 16 threads for ~70 seconds; Bug 2 needed 16 threads for ~200 seconds. The tests are designed for fast CI, not for catching timing-sensitive stalls.

**2. No lossy mode stress tests:**
The `deadlock_config()` helper explicitly sets `lossy: false`. There are zero automated tests that exercise lossy eviction under sustained multi-writer load. Bug 2 only manifests in lossy mode where `truncate_until` is called on every eviction cycle.

**3. No throughput-over-time regression detection:**
Our tests check "did it complete?" (correctness) but not "did throughput collapse?" (liveness). Bug 1 manifested as a throughput cliff from 200K ops/s to 14 ops/s — the system was technically making progress (14 ops/s), just catastrophically slowly.

**4. InMemoryDevice hides scale effects:**
All integration tests use `InMemoryDevice` which completes I/O synchronously (`CompletedSync`). This means:
- No I/O back-pressure → no QueueFull → Bug 1's trigger is absent
- No real latency gap → no timing-dependent starvation
- Vec grows without bound, but tests are too short to notice (Bug 2)

**5. No tests assert `truncate_until` performance:**
The existing test for `truncate_until` (device.rs:698) just calls it and checks correctness — never checks that it doesn't hold a lock for seconds.

---

## Section 4: Root Cause Classification

| Dimension | Bug 1: Pipeline Deadlock | Bug 2: Lock Starvation |
|-----------|--------------------------|------------------------|
| **Lock ordering** | ❌ No — no circular lock acquisition | ❌ No — single lock, no ordering issue |
| **Data race** | ❌ No — all accesses are synchronized | ❌ No — RwLock is correctly used |
| **Progress failure** | ✅ Yes — circular dependency prevents forward progress | ✅ Yes — write lock held for O(n) time starves readers/writers |
| **Scale-dependent** | ✅ Yes — needs buffer to fill (sustained load) | ✅ Yes — O(n) zeroing grows with total data flushed |
| **Time-dependent** | ✅ Yes — needs I/O latency gap | ✅ Yes — needs 200s+ to accumulate enough data |
| **Multi-component** | ✅ Yes — allocator × flush × eviction | ⚠️ Partial — device lock × flush pipeline |
| **Reproducible at small scale** | ❌ No (fixed before: maybe with SlowDevice) | ❌ No — O(n) only dominates at large n |

### Which Tools Catch Which Categories?

| Tool | Lock Ordering | Data Race | Progress Failure | Scale-Dependent | Time-Dependent |
|------|--------------|-----------|------------------|-----------------|----------------|
| Loom | ❌ | ✅ (memory ordering) | ❌ | ❌ | ❌ |
| Miri | ❌ | ✅ (UB detection) | ❌ | ❌ | ❌ |
| ThreadSanitizer | ✅ (partial) | ✅ | ❌ | ❌ | ❌ |
| Property tests (proptest) | ❌ | ❌ | ⚠️ (if model checks progress) | ❌ | ❌ |
| Deterministic simulation | ✅ | ✅ | ✅ | ✅ (if modeled) | ✅ (virtual time) |
| Stress tests | ❌ | ❌ | ✅ | ✅ | ✅ |
| Static lock-order analysis | ✅ | ❌ | ❌ | ❌ | ❌ |
| Code review (structural rules) | ⚠️ | ❌ | ⚠️ | ✅ (O(n) under lock) | ❌ |

---

## Section 5: Gap Analysis — What Would Have Caught Each Bug

### 5.1 Loom with Different Test Designs

**Bug 1:** Could loom catch the pipeline deadlock with a bigger model?
- **Theoretically yes**, if you modeled the full allocator-flush-evict loop with QueueFull injection. But the state space would be massive (16+ threads, page state machine, I/O queue). Practically infeasible.
- **Verdict:** ❌ Wrong tool for the job.

**Bug 2:** Could loom catch the lock starvation?
- **No.** Loom explores orderings, not durations. A `fill(0)` on a 1-byte Vec and a 9GB Vec are the same to loom — both are "one write lock acquisition." Loom can't model that one takes 3 seconds.
- **Verdict:** ❌ Fundamentally impossible.

### 5.2 Property-Based Tests (proptest with Stateful Model)

**Bug 1:** A stateful model test that tracks page states (Open, Sealed, Flushing, Flushed, Evicted) and asserts "eventually all pages reach Flushed" could detect the pipeline stall — IF the model included QueueFull back-pressure.
- **Verdict:** ⚠️ Possible but requires careful model design. Would need `FaultInjectingDevice` with QueueFull injection.

**Bug 2:** A property test asserting "time-per-maintenance-cycle stays bounded" would catch this immediately.
- **Verdict:** ⚠️ Would need to measure wall-clock time, which property tests don't naturally do.

### 5.3 Deterministic Simulation (FoundationDB-style)

**Bug 1:** ✅ **Yes.** A deterministic simulator with virtual time, modeled I/O latency, and injected QueueFull would reproduce this reliably. The simulator could explore: "what happens when flush hits QueueFull and writers are waiting for buffer space?"

**Bug 2:** ✅ **Yes.** A simulator modeling `truncate_until` as an O(n) operation with virtual time would detect that maintenance latency grows without bound, eventually starving the flush pipeline.

**Verdict:** Most powerful tool, but highest implementation cost. This is the gold standard for distributed/concurrent systems.

### 5.4 Static Analysis (Lock-Order / Deadlock Detection)

**Bug 1:** ❌ No locks are involved — it's a pipeline progress failure, not a lock cycle.

**Bug 2:** ⚠️ Partially. A rule like "no O(n) operations under lock" would flag `guard[..offset].fill(0)` inside a write lock. Tools like `cargo-lockorder` don't exist for Rust, but a custom lint could catch "method call on lock guard that's O(n) in data size."

### 5.5 Code Review Structural Rules

**Bug 1:** ⚠️ A review checklist item "does this pipeline have a circular dependency?" would require system-level thinking. Not easily automated.

**Bug 2:** ✅ A review rule "**never hold a write lock while iterating over unbounded data**" would have caught this immediately. The `guard[..offset].fill(0)` under write lock is a textbook violation.

---

## Section 6: Recommendations (Prioritized)

### Tier 1: Fast automated checks (<60s, run in CI)

| # | Recommendation | Catches | Effort | Bug 1? | Bug 2? |
|---|---------------|---------|--------|--------|--------|
| R1 | **Add lossy-mode deadlock regression test**: 4 threads, 8 pages, lossy=true, 30-second timeout with throughput assertion (must sustain >100 ops/s) | Lock starvation, lossy mode regressions | Low (1 day) | ❌ | ✅ |
| R2 | **Add throughput-cliff detection to existing multi-writer test**: Assert that ops/s in last 5s ≥ 10% of ops/s in first 5s | Pipeline stalls, throughput regressions | Low (0.5 day) | ✅ | ✅ |
| R3 | **Add "O(n) under lock" clippy lint / code review checklist item**: Flag any `fill()`, `iter()`, `copy_from_slice()` call on a lock guard where the size is unbounded | Scale-dependent lock starvation | Low (0.5 day) | ❌ | ✅ |

### Tier 2: Medium-cost infrastructure (run nightly or per-PR)

| # | Recommendation | Catches | Effort | Bug 1? | Bug 2? |
|---|---------------|---------|--------|--------|--------|
| R4 | **Add `SlowDevice`-based stress test**: 8 threads, 50ms I/O latency, 60-second duration, assert forward progress | Pipeline deadlocks under I/O back-pressure | Medium (2 days) | ✅ | ❌ |
| R5 | **Property-based pipeline model test**: Use proptest to generate sequences of (write, flush, evict, QueueFull) events, assert "buffer never stays full for >N cycles" | Pipeline progress invariant violations | Medium (3 days) | ✅ | ❌ |
| R6 | **Maintenance latency assertion**: Instrument `maintenance()` with elapsed time measurement, assert <100ms per call in stress tests | O(n) regressions in maintenance path | Low (1 day) | ❌ | ✅ |

### Tier 3: High-value strategic investments

| # | Recommendation | Catches | Effort | Bug 1? | Bug 2? |
|---|---------------|---------|--------|--------|--------|
| R7 | **Deterministic simulation harness**: Model virtual time, I/O latency injection, QueueFull injection, and assert pipeline progress invariants | All progress failures, all timing bugs, all scale bugs | High (2-4 weeks) | ✅ | ✅ |
| R8 | **Chaos/fault-injection test mode**: Runtime flag that randomly injects QueueFull, slow I/O, and write lock contention into any Device impl | Fragile pipeline assumptions | Medium (1 week) | ✅ | ⚠️ |

### Immediate Actions

1. **R1 + R2 this week** — cheapest, catches both bug categories, prevents regression
2. **R3 as a code review rule** — add to `.squad/skills/quality-gate-verification/`
3. **R4 next sprint** — the `SlowDevice` already exists in `deadlock_tests.rs`, just needs a longer-running test
4. **R7 backlog** — aspirational but transformative; revisit after Wave 4

---

## Section 7: Key Insights

1. **Our loom tests are excellent — for what loom does.** They catch memory ordering bugs, ABA races, and atomic protocol violations. They were never designed to catch pipeline progress failures or scale-dependent starvation, and they shouldn't be.

2. **The deadlock regression tests were written after Bug 1 was found** (iteration 7). They prevent *that specific* regression but don't generalize to new deadlock patterns. They need a "throughput over time" assertion, not just "did it complete."

3. **InMemoryDevice is a correctness tool, not a stress tool.** Its synchronous completion model (`CompletedSync`) hides the timing gaps that real I/O devices create. Any test that needs to find pipeline stalls should use `SlowDevice`.

4. **Lossy mode is undertested.** Every config helper in the test suite sets `lossy: false`. This is the single biggest gap. Lossy mode enables eviction paths that non-lossy mode never touches, including `truncate_until`.

5. **The two bugs share a common meta-pattern: "operation that's O(1) at test scale becomes O(n) at production scale."** Bug 1: retry loop that spins at test scale but deadlocks when the buffer fills at production scale. Bug 2: `fill(0)` that's instant at test scale but takes seconds at production scale. We need tests that deliberately push past test scale.

6. **Progress assertions beat correctness assertions for concurrency bugs.** "Did it finish?" is necessary but not sufficient. "Did throughput stay above a floor?" catches a whole class of liveness bugs that correctness checks miss.

---

## Appendix: Test Tool Capabilities Matrix

```
                    Memory   Lock    Pipeline   Scale    Time
                    Order    Order   Progress   Depend.  Depend.
                    ──────   ──────  ────────   ──────   ──────
Loom                 ✅       ❌       ❌         ❌       ❌
Miri                 ✅       ❌       ❌         ❌       ❌
ThreadSanitizer      ⚠️       ⚠️       ❌         ❌       ❌
Property tests       ❌       ❌       ⚠️         ❌       ❌
Deterministic sim    ✅       ✅       ✅         ✅       ✅
Stress tests         ❌       ❌       ✅         ✅       ✅
Static analysis      ❌       ⚠️       ❌         ⚠️       ❌
Code review          ❌       ⚠️       ⚠️         ✅       ❌
```

This matrix should guide tool selection for future concurrency work. No single tool covers all categories — the portfolio approach (loom + stress + property + structural rules) is the right strategy.

---

*— Gandalf, Lead Architect*
