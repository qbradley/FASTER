# DST v2.0 Phase 1 Implementation Plan

**Author:** Gandalf (Lead / System Architect)  
**Date:** 2026-03-17  
**Status:** Implementation Plan — Ready for Execution  
**Requested by:** qbradley  
**Depends on:**  
- [Architecture Decisions](gandalf-dst-v2-architecture.md) — 3-phase plan, Device trait boundary, release gates  
- [Internals Inventory](sam-dst-internals-inventory.md) — 10 subsystems, 25 transitions, 9 invariants  
- [Technical Design](eowyn-dst-v2-technical-design.md) — SimClock, DstScheduler, SimDevice, Scenario framework  

---

## Executive Summary

Phase 1 delivers **SimDeviceV2 + controlled multi-thread concurrency scenarios** that deterministically catch pipeline progress failures (Bug 1 class) and O(n)-under-lock starvation (Bug 2 class). Real OS threads, simulated I/O timing, virtual time for cost modeling.

**7 implementation phases. 5 agents. ~4-5 weeks. Critical path: P1→P2→P3→P5→P6→P7.**

---

## Constraints (Standing Directives)

| Constraint | Enforcement |
|------------|-------------|
| Feature flag: `#[cfg(feature = "simulation")]` on `faster-core` | All `faster-core` changes gated; verified by `cargo build` without feature |
| Feature flag: `#[cfg(feature = "dst")]` on `faster-dst` | faster-dst is a test crate; always compiled with `simulation` via dependency |
| Must not break existing tests | CI gate: `cargo nextest run` (1720+), loom (25), DST crash-recovery (1003) |
| All work on `rust` branch of `qbradley/FASTER` | Branch per phase: `eowyn/dst-v2-p1-{phase-name}`, merge to `rust` |
| PR-based workflow with selective `git add` | No `git add -A`; stage only touched files |
| Same seed = identical execution | ChaChaRng everywhere; no SmallRng in new code; no entropy sources |
| Zero flakes | Deterministic I/O timing; bounded sim-time watchdog; no wall-clock dependencies in assertions |

---

## Phase Breakdown

### P1: SimClock Extensions + sim_hooks (faster-core)

**Goal:** Wire virtual time into `faster-core` so all `Instant::now()` calls under `simulation` feature route to the deterministic clock. Add simulation hook infrastructure.

**What to build:**
- `faster-core/src/sim_time.rs` — `SimInstant` struct (drop-in for `std::time::Instant`), thread-local `SimClock` installation/access
- `faster-core/src/sim_hooks.rs` — Thread-local yield/sleep hooks, `on_yield()`, `on_sleep()`, `take_yield_request()`, `take_sleep_request()`
- Modify `faster-core/src/sync.rs` — Simulation tier: swap `Instant` to `SimInstant`, wrap `thread::yield_now()` and `thread::sleep()` to route through `sim_hooks`
- Modify `faster-core/src/lib.rs` — Conditional module declarations for `sim_time` and `sim_hooks`

**Input files (read):**
- `rust/crates/faster-core/src/sync.rs` (lines 58-101 — simulation tier)
- `rust/crates/faster-core/src/lib.rs` (module declarations)
- `rust/crates/faster-dst/src/clock.rs` (existing `SimulatedClock` — reference, don't modify)

**Output files (create/modify):**
- CREATE: `rust/crates/faster-core/src/sim_time.rs`
- CREATE: `rust/crates/faster-core/src/sim_hooks.rs`
- MODIFY: `rust/crates/faster-core/src/sync.rs` (simulation tier time + thread blocks)
- MODIFY: `rust/crates/faster-core/src/lib.rs` (add conditional modules)

**Success criteria:**
1. `cargo build -p faster-core` (no features) compiles — zero-cost gate
2. `cargo build -p faster-core --features simulation` compiles with new modules
3. `cargo nextest run -p faster-core` — all existing tests pass
4. New unit tests: `SimInstant::now()` → `elapsed()` → correct virtual duration
5. New unit tests: `install_sim_clock()` → `SimInstant::now()` reads from installed clock
6. New unit tests: `on_yield()`/`take_yield_request()` round-trip
7. `thread::yield_now()` under `simulation` calls `on_yield()` (not `std::thread::yield_now()`)
8. `thread::sleep(d)` under `simulation` calls `on_sleep(d)` (not `std::thread::sleep()`)

**Assigned to:** Sam  
**Rationale:** Sam owns `sync.rs` and the 3-tier abstraction. He built it, he knows the cfg precedence rules, and he's the right person to extend it without breaking the loom tier.

**Estimated complexity:** M (Medium)  
**Estimated time:** 2-3 days

**Dependencies:** None (first phase, no blockers)

---

### P2: SimDeviceV2 (faster-dst)

**Goal:** Build the async-completion I/O device that replaces `CompletedSync` with deferred callbacks, bounded queue depth, and configurable latency.

**What to build:**
- `faster-dst/src/sim_device_v2.rs` — `SimDeviceV2` struct implementing `Device` trait
  - `SimIoConfig` — queue_depth, base_latency, latency_jitter, truncate_ns_per_byte, fault injection toggles
  - `PendingIo` — completion-time-ordered queue of deferred callbacks
  - `write_async()` → `QueueFull` when `pending.len() >= queue_depth`, otherwise enqueue with `complete_at = clock.now() + latency ± jitter`
  - `read_async()` → Read from backing `SimulatedStorage`, return `Submitted` with deferred callback
  - `poll_completions()` → Drain pending where `complete_at <= clock.now()`, fire callbacks, return count
  - `truncate_until()` → Charge O(n) virtual time via `clock.charge()` (virtual cost model)
  - All randomness via `ChaChaRng` seeded from scenario seed
  - Metrics: `total_submitted`, `total_completed`, `total_queue_full` (AtomicU64)
- Modify `faster-dst/src/lib.rs` — Add module declaration + re-export
- Modify `faster-dst/Cargo.toml` — Add `rand_chacha` dependency

**Input files (read):**
- `rust/crates/faster-core/src/device.rs` (Device trait definition, lines 229-302)
- `rust/crates/faster-dst/src/device.rs` (existing `SimulatedDevice` — reference pattern)
- `rust/crates/faster-dst/src/clock.rs` (SimulatedClock API)

**Output files (create/modify):**
- CREATE: `rust/crates/faster-dst/src/sim_device_v2.rs`
- MODIFY: `rust/crates/faster-dst/src/lib.rs`
- MODIFY: `rust/crates/faster-dst/Cargo.toml`

**Success criteria:**
1. `SimDeviceV2` implements `Device` trait — compiles cleanly
2. Unit test: `write_async()` with `queue_depth=4`, submit 4 writes → all `Submitted`, 5th → `QueueFull`
3. Unit test: `poll_completions()` fires callbacks in completion-time order
4. Unit test: Advance clock past completion times → callbacks fire → pages transition
5. Unit test: Same seed → same latency sequence → same completion order (determinism)
6. Unit test: `truncate_until()` charges virtual time proportional to offset
7. Unit test: Fault injection — configurable error rate on write completions
8. Metrics counters correct after mixed sequence of writes + polls
9. All existing `faster-dst` tests still pass (`cargo nextest run -p faster-dst`)

**Assigned to:** Eowyn  
**Rationale:** Eowyn designed the SimDevice v2 spec. She knows the completion-time model, the queue pressure mechanics, and the interaction with SimClock. This is her design.

**Estimated complexity:** L (Large)  
**Estimated time:** 4-5 days

**Dependencies:** P1 (needs `SimulatedClock::charge()` method — but `charge()` can be added to existing `clock.rs` in faster-dst, so P2 can start in parallel with P1 if Eowyn adds `charge()` locally first and Sam wires it into `faster-core` later)

**Parallelization note:** P2 can start immediately and run in parallel with P1. The `SimDeviceV2` uses `SimulatedClock` from `faster-dst/src/clock.rs` (already exists). The `faster-core` integration (P1) is needed only when we wire everything together in P5.

---

### P3: Virtual Cost Model + Clock Extensions (faster-dst)

**Goal:** Add the `charge()` method to `SimulatedClock` and define the virtual cost constant table for O(n) operation modeling.

**What to build:**
- Extend `faster-dst/src/clock.rs`:
  - `SimulatedClock::charge(cost: Duration)` — atomic add to nanos (models O(n) without real work)
  - `SimulatedClock::now_nanos() -> u64` — raw nanos accessor (avoids Duration construction in hot paths)
- CREATE `faster-dst/src/cost_model.rs`:
  - `pub mod cost` — Virtual cost constants:
    - `MEMZERO_NS_PER_BYTE: u64 = 1` (10× inflated for small-scale detection)
    - `TRUNCATE_NS_PER_BYTE: u64 = 1`
    - `HASH_INVALIDATE_NS_PER_BUCKET: u64 = 100`
    - `EPOCH_SCAN_NS_PER_ENTRY: u64 = 5`
  - `bytes_cost(bytes: u64, ns_per_byte: u64) -> Duration` helper
- Modify `faster-dst/src/lib.rs` — Module declaration

**Input files (read):**
- `rust/crates/faster-dst/src/clock.rs` (extend existing)
- Sam's inventory: Section 3D (eviction resource growth), Section 6D (device resource growth), Section 7D (epoch scan cost)

**Output files (create/modify):**
- MODIFY: `rust/crates/faster-dst/src/clock.rs` (add `charge()`, `now_nanos()`)
- CREATE: `rust/crates/faster-dst/src/cost_model.rs`
- MODIFY: `rust/crates/faster-dst/src/lib.rs`

**Success criteria:**
1. `charge(Duration::from_nanos(1000))` advances clock by exactly 1000ns
2. `now_nanos()` returns raw u64 matching `now().as_nanos()`
3. `bytes_cost(1_000_000, 1) == Duration::from_millis(1)` (1MB × 1ns/byte = 1ms)
4. Cost constants are documented with calibration rationale
5. All existing clock tests pass

**Assigned to:** Eowyn  
**Rationale:** This is part of Eowyn's SimClock design. She calibrated the cost constants.

**Estimated complexity:** S (Small)  
**Estimated time:** 1 day

**Dependencies:** None (extends faster-dst only, no cross-crate dependency)

**Parallelization note:** Can run fully in parallel with P1 and P2. Eowyn can batch P3 into the same PR as P2 if convenient.

---

### P4: Concurrency Configuration + Assertions Framework (faster-dst)

**Goal:** Build the scenario configuration, assertion, and statistics collection infrastructure that the runner will use.

**What to build:**
- CREATE `faster-dst/src/concurrency/` directory:
  - `mod.rs` — Module root, re-exports
  - `config.rs` — `ScenarioConfig`, `SimIoConfig`, `KeyDist` enum
  - `assertions.rs` — `ScenarioAssertions` (min_throughput, max_stall, memory_bound, max_maintenance_latency, all_writers_complete)
  - `stats.rs` — `ExecutionStats`, `StatsCollector` (thread-safe, collects ops timeline, throughput windows, stall detection)
    - `StatsCollector::record_op(sim_time)` — record successful operation
    - `StatsCollector::record_abort(sim_time)` — record aborted operation
    - `StatsCollector::finalize(sim_duration, wall_duration) -> ExecutionStats`
    - 1-second sliding window throughput calculation
    - Max stall detection (longest gap between consecutive ops)
  - `seeds.rs` — `regression::REGRESSION_SEEDS`, `canary::CANARY_SEEDS`, `exploration_seeds(root, count)`
- Modify `faster-dst/src/lib.rs` — Add `pub mod concurrency`
- Modify `faster-dst/Cargo.toml` — Ensure `rand_chacha` is present (may already be added in P2)

**Input files (read):**
- Eowyn's technical design: Sections 4.1 (ScenarioConfig), 4.2 (Assertions), 4.3 (DstRunner stats)

**Output files (create/modify):**
- CREATE: `rust/crates/faster-dst/src/concurrency/mod.rs`
- CREATE: `rust/crates/faster-dst/src/concurrency/config.rs`
- CREATE: `rust/crates/faster-dst/src/concurrency/assertions.rs`
- CREATE: `rust/crates/faster-dst/src/concurrency/stats.rs`
- CREATE: `rust/crates/faster-dst/src/concurrency/seeds.rs`
- MODIFY: `rust/crates/faster-dst/src/lib.rs`

**Success criteria:**
1. `ScenarioConfig::default()` produces valid config (4 writers, 16 buffer pages, 10K ops)
2. `ScenarioAssertions::default()` has conservative but non-trivial thresholds
3. `StatsCollector` is `Send + Sync` (shared across writer threads)
4. Unit test: feed known op timestamps → verify `min_throughput_1s` calculation
5. Unit test: feed ops with a 3-second gap → verify `max_stall == 3s`
6. Unit test: `exploration_seeds(42, 100)` produces 100 unique seeds, same result on re-run
7. `canary::CANARY_SEEDS` has at least 5 diverse seeds
8. All types implement `Clone + Debug`

**Assigned to:** Eowyn  
**Rationale:** Direct implementation of Eowyn's design. Configuration shapes match her spec exactly.

**Estimated complexity:** M (Medium)  
**Estimated time:** 2-3 days

**Dependencies:** None (pure data structures, no cross-crate dependency)

**Parallelization note:** Fully parallel with P1, P2, P3. Eowyn can batch P3 + P4 into one work stream while P2 is in review.

---

### P5: DstRunner — Scenario Execution Engine (faster-dst)

**Goal:** Build the test runner that wires FasterKv + SimDeviceV2 + SimClock + StatsCollector into an executable concurrency scenario with real OS threads and assertion checking.

**What to build:**
- CREATE `faster-dst/src/concurrency/runner.rs`:
  - `DstRunner` struct — holds `ScenarioConfig` + `ScenarioAssertions`
  - `DstRunner::run() -> RunOutcome` — the main execution method:
    1. Create `SimulatedClock` (shared `Arc`)
    2. Create `SimulatedStorage` + `SimDeviceV2` with config's I/O params
    3. Build `FasterKv` with SimDeviceV2 as log device
    4. Install `SimClock` on each writer thread via `install_sim_clock()` (from P1)
    5. Spawn N real OS writer threads, each with its own `FasterSession`
    6. Each writer: loop `ops_per_writer` times, doing `upsert()` + periodic `maintenance()`
    7. Spawn 1 completion scheduler thread: loop advancing clock + calling `device.poll_completions()`
    8. Watchdog: monitor progress via `StatsCollector`, abort on timeout
    9. Join all threads, collect `ExecutionStats`, check assertions
  - `RunOutcome` enum — `Pass { stats }`, `Fail { assertion, stats, trace_tail }`, `Deadlock { blocked_tasks, stats }`
  - `SimDeviceMetrics` — snapshot from `SimDeviceV2` (submitted, completed, queue_full counts)
- CREATE `faster-dst/src/concurrency/watchdog.rs`:
  - `Watchdog` — dedicated thread monitoring `StatsCollector` for progress
  - Configurable `max_stall: Duration` (wall-clock), `max_total: Duration` (wall-clock)
  - On stall detection: capture thread backtraces (if available), signal abort
- Modify `faster-dst/src/concurrency/mod.rs` — Add module declarations + re-exports

**Input files (read):**
- All P1-P4 outputs (sim_time, sim_hooks, SimDeviceV2, cost_model, config, assertions, stats)
- `rust/crates/faster-core/src/store/kv.rs` — FasterKv construction + session API
- `rust/crates/faster-dst/src/sim_store.rs` — existing SimulatedFasterKv pattern (reference)
- `rust/crates/faster-dst/src/harness.rs` — existing SimulationHarness (reference)

**Output files (create/modify):**
- CREATE: `rust/crates/faster-dst/src/concurrency/runner.rs`
- CREATE: `rust/crates/faster-dst/src/concurrency/watchdog.rs`
- MODIFY: `rust/crates/faster-dst/src/concurrency/mod.rs`

**Success criteria:**
1. `DstRunner::run()` with 2 writers, 1K ops, queue_depth=32 → `RunOutcome::Pass`
2. All writers complete within wall-clock timeout (default 60s real time)
3. `SimDeviceV2.poll_completions()` drains queue; pages transition Flushing→Flushed
4. `StatsCollector` records accurate ops timeline
5. Watchdog correctly detects and reports stalls (test with artificially stalled writer)
6. `SimClock` installed on all writer threads (SimInstant::now() returns virtual time)
7. Completion scheduler thread advances clock monotonically
8. All existing tests still pass

**Assigned to:** Eowyn (lead) + Sam (support for FasterKv construction patterns)  
**Rationale:** Eowyn designed the runner. Sam assists because the FasterKv builder, session lifecycle, and maintenance call patterns are his domain. Eowyn drives, Sam reviews.

**Estimated complexity:** XL (Extra Large) — This is the integration heart of Phase 1  
**Estimated time:** 5-7 days

**Dependencies:** P1 (sim_time installed in faster-core), P2 (SimDeviceV2), P3 (cost model), P4 (config/assertions/stats)

**Critical path:** This is the first phase that cannot start until P1 and P2 are complete and merged. It is the integration nexus.

---

### P6: Concurrency Scenarios — Bug 1 + Bug 2 Reproductions (faster-dst)

**Goal:** Write the specific concurrency scenarios that catch both known bug classes deterministically, plus campaign infrastructure.

**What to build:**
- CREATE `faster-dst/src/concurrency/scenarios.rs`:
  - `pipeline_deadlock_config(seed) -> (ScenarioConfig, ScenarioAssertions)` — Bug 1 reproduction
    - 8 writers, 16 buffer pages, queue_depth=4, latency=10ms, jitter=5ms
    - Assert: min_throughput >= 100, max_stall <= 10s, all_writers_complete
  - `lossy_truncate_starvation_config(seed) -> (ScenarioConfig, ScenarioAssertions)` — Bug 2 reproduction
    - 4 writers, 8 buffer pages, lossy=true, mutable_fraction=0.2, truncate_ns_per_byte=1
    - Assert: min_throughput >= 50, max_stall <= 15s, memory_bound <= 512MB, max_maintenance_latency <= 2s
  - `multi_writer_saturation_config(seed)` — High contention baseline
    - 16 writers, 32 buffer pages, queue_depth=8
  - `single_writer_baseline_config(seed)` — Correctness baseline (1 writer, no contention)
  - `ALL_SCENARIOS` — Static array of all scenario configs for campaign runner
- CREATE `faster-dst/src/concurrency/campaign.rs`:
  - `run_concurrency_campaign(scenarios, seeds) -> CampaignReport`
  - Parallel seed execution via `rayon` or manual thread pool
  - `CampaignReport` — total, passed, failures with seed + assertion detail
  - `CampaignFailure` — scenario name, seed, assertion message, stats snapshot
- CREATE `faster-dst/tests/dst_concurrency.rs`:
  - `#[test] fn dst_bug1_pipeline_deadlock()` — Run pipeline_deadlock across canary seeds
  - `#[test] fn dst_bug2_lossy_truncate_starvation()` — Run lossy_truncate across canary seeds
  - `#[test] fn dst_multi_writer_saturation()` — Run saturation across canary seeds
  - `#[test] fn dst_single_writer_baseline()` — Sanity check
  - Each test: verify QueueFull events > 0 (scenario actually exercised the path)
- CREATE `faster-dst/tests/dst_campaign.rs`:
  - `#[test] fn campaign_smoke_test()` — Run ALL_SCENARIOS × 5 canary seeds
  - Verify report.all_passed()

**Input files (read):**
- P5 outputs (DstRunner, RunOutcome)
- P4 outputs (ScenarioConfig, ScenarioAssertions, seeds)
- Eowyn's design: Sections 4.5 (Bug 1 scenario), 4.6 (Bug 2 scenario), 4.7 (Campaign)

**Output files (create/modify):**
- CREATE: `rust/crates/faster-dst/src/concurrency/scenarios.rs`
- CREATE: `rust/crates/faster-dst/src/concurrency/campaign.rs`
- CREATE: `rust/crates/faster-dst/tests/dst_concurrency.rs`
- CREATE: `rust/crates/faster-dst/tests/dst_campaign.rs`
- MODIFY: `rust/crates/faster-dst/src/concurrency/mod.rs`

**Success criteria:**
1. `dst_single_writer_baseline` passes on all canary seeds — basic correctness
2. `dst_multi_writer_saturation` passes on all canary seeds — contention handling works
3. `dst_bug1_pipeline_deadlock` either passes (bug fixed) or fails with clear diagnostic pointing to QueueFull cascade — the scenario correctly exercises the Bug 1 path
4. `dst_bug2_lossy_truncate_starvation` either passes (bug fixed) or fails with clear diagnostic pointing to O(n) maintenance latency — the scenario correctly exercises the Bug 2 path
5. `campaign_smoke_test` completes within 5 minutes wall-clock
6. Failing seeds produce reproduction commands: `--seed 0xDEADBEEF42`
7. All existing tests still pass

**Assigned to:** Eowyn (scenarios) + Aragorn (test authoring support)  
**Rationale:** Eowyn designed the scenarios. Aragorn provides Rust test ergonomics review and helps with the campaign runner's thread pool implementation.

**Estimated complexity:** L (Large)  
**Estimated time:** 4-5 days

**Dependencies:** P5 (DstRunner must be functional)

---

### P7: CI Integration + Release Gate Definition

**Goal:** Wire DST concurrency tests into CI pipeline. Define what "Phase 1 done" means. Establish the release gate.

**What to build:**
- MODIFY `rust-ci.yml` (or equivalent CI config):
  - PR gate job: `cargo test -p faster-dst --test dst_concurrency --test dst_campaign -- --canary-seeds` (< 10 min)
  - Nightly job: `cargo test -p faster-dst --test dst_concurrency -- --canary-seeds --explore 200` (< 60 min)
- CREATE `rust/crates/faster-dst/tests/README.md`:
  - Document how to run concurrency tests
  - How to reproduce a failing seed
  - How to add a regression seed
- Verify: all existing CI jobs still pass with the new test targets
- Verify: the `simulation` feature does not affect non-DST builds

**Input files (read):**
- CI configuration files (`.github/workflows/` or `azure-pipelines*.yml`)
- P6 test files

**Output files (create/modify):**
- MODIFY: CI config (add DST concurrency job)
- CREATE: `rust/crates/faster-dst/tests/README.md`

**Success criteria:**
1. PR gate runs DST concurrency tests with canary seeds in < 10 minutes
2. Nightly job runs 200 exploration seeds in < 60 minutes
3. A failing seed in nightly produces actionable output (seed + scenario + assertion + stats)
4. `cargo build -p faster-core` without `simulation` feature produces identical binary (zero-cost)
5. All 1720+ nextest, 25 loom, 1003 crash-recovery tests still pass
6. CI passes on `rust` branch

**Assigned to:** Sam (CI wiring) + Gandalf (release gate review)  
**Rationale:** Sam has CI experience from the sync.rs work. Gandalf reviews to verify the release gate meets the quality bar.

**Estimated complexity:** M (Medium)  
**Estimated time:** 2-3 days

**Dependencies:** P6 (tests must exist to integrate)

---

## Agent Assignment Strategy

### Parallel Execution Map

```
Week 1:
  Sam ────── P1: sim_time + sim_hooks (faster-core)
  Eowyn ──── P2: SimDeviceV2 (faster-dst)          ← parallel with P1
  Eowyn ──── P3: Cost model + clock extensions       ← batch with P2

Week 2:
  Eowyn ──── P4: Config + Assertions framework       ← parallel with P1 review
  Sam ────── P1 review/merge
  Eowyn ──── P2 review/merge

Week 2-3:
  Eowyn ──── P5: DstRunner (integration)             ← blocked on P1+P2 merge
  Sam ────── P5 support (FasterKv construction)

Week 3-4:
  Eowyn ──── P6: Scenarios + Campaign                ← blocked on P5
  Aragorn ── P6 support (test ergonomics)

Week 4-5:
  Sam ────── P7: CI integration
  Gandalf ── P7 review (release gate)
```

### Critical Path

```
P1 (Sam, 3d) ──→ P5 (Eowyn, 7d) ──→ P6 (Eowyn, 5d) ──→ P7 (Sam, 3d)
                   ↑
P2 (Eowyn, 5d) ──┘
```

**Total critical path: ~18 working days (~3.5 weeks)**  
**Total calendar with reviews and iteration: ~4-5 weeks**

### Who Does What

| Agent | Primary Phases | Role |
|-------|---------------|------|
| **Sam** | P1, P7 | sync.rs extension, CI wiring. Support on P5 (FasterKv patterns) |
| **Eowyn** | P2, P3, P4, P5, P6 | Primary implementer. This is her framework. She owns the most work. |
| **Aragorn** | P6 (support) | Rust idiom review, test ergonomics, campaign thread pool |
| **Gandalf** | P7 (review) | Release gate definition, architectural consistency review |
| **Legolas** | Standby | Not needed in Phase 1. Engages in Phase 2 (performance of green thread scheduler) |

### Workload Balance

- **Eowyn is the bottleneck.** She owns 5 of 7 phases. Mitigation: P2/P3/P4 can be batched into fewer PRs. Sam handles all `faster-core` changes (P1) so Eowyn stays in `faster-dst`.
- **Sam is underloaded.** After P1 (3 days), he has review duties until P7. He could backfill by writing additional unit tests for P2/P3 or by starting the `buggify!` macro groundwork (Phase 1.5 stretch goal).

---

## Integration Milestones

### M1: Virtual Time Round-Trip (end of Week 1)
**Verification:** Install SimClock on a thread → call `SimInstant::now()` → advance clock → call `elapsed()` → get correct virtual duration. No wall-clock involved.  
**Tests:** Unit tests from P1.  
**Gate:** P1 merge.

### M2: SimDeviceV2 Standalone (end of Week 1-2)
**Verification:** Create SimDeviceV2 → submit writes → advance clock → poll_completions → callbacks fire in correct order. `QueueFull` at queue depth limit.  
**Tests:** Unit tests from P2.  
**Gate:** P2 merge.

### M3: Single-Writer End-to-End (mid Week 3)
**Verification:** Build FasterKv with SimDeviceV2 → single writer thread doing 1K upserts → maintenance + poll_completions → all operations succeed → pages flush through the pipeline → SimClock advances.  
**This is the first time real FasterKv code runs against SimDeviceV2.**  
**Gate:** P5 first integration test passes.

### M4: Multi-Writer Pipeline Exercise (end of Week 3)
**Verification:** 4+ writers → buffer pressure → QueueFull events → retry loops → all writers complete. This exercises the flush pipeline under simulated I/O back-pressure.  
**Gate:** `dst_multi_writer_saturation` passes on canary seeds.

### M5: Bug Reproduction or Proof of Fix (end of Week 4)
**Verification:** Run Bug 1 and Bug 2 scenarios. Either:
- (a) The scenario catches the bug with a clear diagnostic → we have a regression test ready for when the fix lands, OR
- (b) The bug is already fixed → the scenario passes, confirming the fix holds under deterministic pressure.
**Gate:** `dst_bug1_pipeline_deadlock` and `dst_bug2_lossy_truncate_starvation` produce definitive results.

### M6: CI Green (end of Week 4-5)
**Verification:** PR gate passes with DST concurrency tests. Nightly runs 200 seeds. All existing tests still pass.  
**Gate:** P7 merge. Phase 1 complete.

---

## Release Gate Definition

### Phase 1 is "done" when ALL of the following hold:

1. **SimDeviceV2 operational:** `Device` trait implementation with async completion queue, configurable queue depth (2-1024), latency injection (seeded ChaChaRng), QueueFull back-pressure, and poll_completions draining.

2. **Virtual time wired:** `SimInstant::now()` under `simulation` feature reads from `SimulatedClock`. `thread::yield_now()` and `thread::sleep()` route through `sim_hooks`. Zero wall-clock dependency in simulation paths.

3. **Multi-writer scenarios passing:** At least 4 concurrency scenarios (single-writer baseline, multi-writer saturation, pipeline deadlock Bug 1, lossy starvation Bug 2) running with 5+ canary seeds each, all producing definitive pass/fail results.

4. **Deterministic I/O timing:** Same seed + same SimIoConfig → same I/O completion order → same QueueFull sequence → same flush pipeline behavior. (Thread interleaving is NOT deterministic in Phase 1 — that's Phase 2.)

5. **No existing test regressions:** 1720+ nextest, 25 loom, 1003 crash-recovery all pass. `cargo build` without `simulation` feature produces identical binary.

6. **CI integrated:** PR gate runs canary seeds (< 10 min). Nightly runs exploration seeds (< 60 min).

7. **Reproduction workflow:** Any failing seed can be reproduced with `cargo test -p faster-dst --test dst_concurrency -- --seed 0x{SEED}` in < 30 seconds.

### What Phase 1 does NOT include (deferred to Phase 2+):
- Green thread simulation (full determinism)
- SimMutex/SimRwLock (scheduler-aware locks)
- `buggify!` macro and chaos injection
- Thread spawn interception (scheduler-routed spawn)
- 10,000-seed weekly exploration campaigns

---

## Risk Assessment

### 1. FasterKv + SimDeviceV2 Integration (HIGH RISK)

**What:** FasterKv's flush pipeline assumes `CompletedSync` from InMemoryDevice. SimDeviceV2 returns `Submitted`, meaning callbacks fire later. The pipeline's retry loops, maintenance() calls, and session operations must all handle deferred completions correctly.

**Why risky:** The current codebase has never been tested with a device that returns `Submitted` in-process (only SyncFileDevice does this, and only in file-backed integration tests). There may be hidden assumptions about synchronous callback timing.

**Mitigation:** P5 starts with single-writer integration (M3). If callbacks don't fire correctly, we debug before adding multi-writer complexity. Sam reviews the flush.rs callback flow (flush_completion_callback) to verify it works when called from poll_completions on a different thread than the submit thread.

**Fallback:** If SimDeviceV2's `Submitted` mode causes cascading issues, we can add a `sync_mode: bool` config that falls back to `CompletedSync` for debugging, then selectively enable async mode path-by-path.

### 2. Completion Scheduler Thread Timing (MEDIUM RISK)

**What:** Phase 1 uses a dedicated completion scheduler thread that advances SimClock and calls poll_completions(). This thread must coordinate with writer threads (which call maintenance() → poll_completions() on their own). Two threads calling poll_completions() concurrently on the same SimDeviceV2 could race.

**Why risky:** The current Device trait's poll_completions() is not specified to be thread-safe for concurrent callers. SimDeviceV2 uses a Mutex on the pending queue, which makes it safe but potentially serializing.

**Mitigation:** Design decision: poll_completions() is always called under the pending_queue Mutex, so it's safe. The completion scheduler thread is the primary driver; maintenance() calls poll_completions() as a "help drain" optimization that's safe to no-op if the scheduler thread already drained.

### 3. SimInstant API Completeness (LOW RISK)

**What:** `SimInstant` must match `std::time::Instant`'s API surface wherever faster-core uses it. If faster-core code uses `Instant` methods we haven't implemented on `SimInstant`, compilation fails under `simulation` feature.

**Why risky:** We might miss edge-case methods (`saturating_duration_since`, `checked_add`, `Add<Duration>` impl, etc.).

**Mitigation:** Grep all `Instant::` and `.elapsed()` usages in faster-core before implementing SimInstant. Implement exactly the API surface used, not the full std::time::Instant surface.

### 4. Wall-Clock Leakage in Assertions (LOW RISK)

**What:** Assertions must use sim-time, not wall-clock time. If any assertion accidentally measures wall-clock duration, tests become flaky.

**Why risky:** Subtle — a developer might write `assert!(start.elapsed() < Duration::from_secs(5))` using `std::time::Instant` instead of `SimInstant`.

**Mitigation:** Code review rule: **No `std::time::Instant` or `std::time::SystemTime` in faster-dst concurrency code.** All time references go through `SimulatedClock`. The `StatsCollector` tracks both `sim_duration` and `wall_duration` explicitly.

### 5. ChaChaRng Cross-Platform Reproducibility (LOW RISK)

**What:** Design requires same seed → same execution across platforms. ChaChaRng is specified to be cross-platform deterministic. But if any non-deterministic source (HashMap iteration order, thread scheduling, system entropy) leaks into the seeded path, reproducibility breaks.

**Why risky:** In Phase 1, real OS threads mean thread interleaving is non-deterministic. We accept this — determinism is only for I/O timing. But we must ensure the I/O timing path has zero non-deterministic inputs.

**Mitigation:** SimDeviceV2's pending queue uses BTreeMap or sorted VecDeque (deterministic iteration order), not HashMap. All PRNG calls go through the single seeded ChaChaRng under Mutex.

### 6. Open Design Decisions

| Decision | Status | Recommendation |
|----------|--------|----------------|
| Should completion scheduler be a separate thread or piggyback on maintenance()? | **Open** | Separate thread in P5. Simpler to reason about. Can be collapsed in Phase 2 when green threads replace real threads. |
| Should SimDeviceV2 support read_async deferred completion? | **Open** | Start with reads returning CompletedSync (like InMemoryDevice). Reads are not on the critical path for Bug 1/Bug 2. Add async reads in Phase 2 if needed. |
| Should StatsCollector sample at fixed intervals or on every op? | **Open** | Every op for accuracy. Use a ring buffer with pre-allocated capacity to bound memory. 1M ops × 16 bytes/entry = 16MB — acceptable for tests. |
| How should the watchdog signal abort to writer threads? | **Open** | Atomic `abort` flag checked in writer loop + `std::process::abort()` as last resort. Prefer cooperative shutdown. |
| Where do regression seeds live long-term? | **Decided** | In `faster-dst/src/concurrency/seeds.rs` as `const` arrays. Compile-time verified. Never removed. |

---

## Appendix: File Inventory

### New Files (Phase 1)

| File | Phase | Crate | Lines (est.) |
|------|-------|-------|-------------|
| `faster-core/src/sim_time.rs` | P1 | faster-core | ~150 |
| `faster-core/src/sim_hooks.rs` | P1 | faster-core | ~60 |
| `faster-dst/src/sim_device_v2.rs` | P2 | faster-dst | ~400 |
| `faster-dst/src/cost_model.rs` | P3 | faster-dst | ~50 |
| `faster-dst/src/concurrency/mod.rs` | P4 | faster-dst | ~30 |
| `faster-dst/src/concurrency/config.rs` | P4 | faster-dst | ~100 |
| `faster-dst/src/concurrency/assertions.rs` | P4 | faster-dst | ~80 |
| `faster-dst/src/concurrency/stats.rs` | P4 | faster-dst | ~200 |
| `faster-dst/src/concurrency/seeds.rs` | P4 | faster-dst | ~50 |
| `faster-dst/src/concurrency/runner.rs` | P5 | faster-dst | ~350 |
| `faster-dst/src/concurrency/watchdog.rs` | P5 | faster-dst | ~100 |
| `faster-dst/src/concurrency/scenarios.rs` | P6 | faster-dst | ~200 |
| `faster-dst/src/concurrency/campaign.rs` | P6 | faster-dst | ~150 |
| `faster-dst/tests/dst_concurrency.rs` | P6 | faster-dst | ~200 |
| `faster-dst/tests/dst_campaign.rs` | P6 | faster-dst | ~80 |
| `faster-dst/tests/README.md` | P7 | faster-dst | ~50 |
| **Total** | | | **~2,250** |

### Modified Files (Phase 1)

| File | Phase | Change |
|------|-------|--------|
| `faster-core/src/sync.rs` | P1 | Simulation tier: swap Instant, wrap thread module |
| `faster-core/src/lib.rs` | P1 | Add conditional module declarations |
| `faster-dst/src/clock.rs` | P3 | Add `charge()`, `now_nanos()` |
| `faster-dst/src/lib.rs` | P2,P3,P4 | Add module declarations |
| `faster-dst/Cargo.toml` | P2 | Add `rand_chacha` dependency |
| CI config | P7 | Add DST concurrency job |

---

*— Gandalf, Lead / System Architect*  
*"A wizard is never late, nor is he early — he delivers exactly the implementation plan that is needed. Now go build it."*
