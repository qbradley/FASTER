# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **Role:** Architect, merge coordinator, hardening lead. Owns cross-cutting concerns: merge consolidation, hash table layout decisions, foundation hardening, retrospectives.
- **Merge strategy:** Feature branches per agent → consolidation merges to main integration branch. Resolve conflicts by understanding both sides' intent. See `.squad/skills/merge-consolidation/` for full process.
- **Hash table layout:** Cache-line aligned buckets (64 bytes). 7 entries per bucket + overflow pointer. Tag bits in entry for fast rejection. Prefetch-friendly linear probe within bucket.
- **EPVS coordination:** Epoch Protection Version Scheme spans multiple modules (state, checkpoint, grow). Two-phase intermediate CAS prevents ABA. Version bumps only Prepare→InProgress.
- **Foundation patterns:** `#[deny(unsafe_code)]` on safe modules. Unsafe confined to FFI, atomics, and allocator. All public API types implement Send+Sync where safe. Error types are non-exhaustive for forward compat.
- **Quality gates:** See `.squad/skills/quality-gate-verification/` for 4-tier validation model. Fast gate: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo nextest run`.
- **Key decisions log:** (1) No async runtime dependency — use `std::thread` + channels. (2) C FFI via `extern "C"` functions, not cbindgen auto-gen. (3) Single-writer session model (no interior mutability on hot path). (4) Epoch-based memory reclamation over hazard pointers.

## Critical Learnings

- **Architecture-first validated:** 12 binding decisions on Day 1 were never revisited. Spec survived 9 parallel agents.
- **Multi-layer verification wins:** 2,102 tests across unit/Miri/Loom/DST/mutation caught bugs no single modality finds.
- **Multi-writer deadlock (P0):** 3-point deadlock in flush/evict pipeline remains structural issue, not tuning.
- **Merge discipline:** Consolidate 5+ branches by dependency order. See merge-consolidation skill.
- **Publishing gotchas:** Path deps need `version = "x.y.z"`, crates.io index syncs in ~30s between publishes.
- **Surgical staging:** `checkin` script runs `git add -A` — stage explicitly when needed.

## Session Log (Compressed)

### Wave 0 Foundation (2026-03-06)
Established workspace layout, CI pipeline (`rust-ci.yml`), coding standards. Key: `rust/` directory structure, `#[deny(unsafe_code)]` default, feature flags for optional deps.

### Iteration 6 Merge (2026-03-06)
Merged 6 feature branches (EPVS implementation, sealed-bit, integration tests, benchmarks, compaction, unsafe context). 14 conflicts across 8 files. Post-merge: 1709 tests passing. See merge-consolidation skill for process.

### Session 2 Retrospective (2026-03-08)
Standardized `git worktree` for agents. Metrics: 8 branches merged, avg 45min merge time, +494 tests.

### A8 Hash Table Layout (2026-03-06)
Validated current 64-byte bucket design against alternatives (128-byte bucket, open addressing, cuckoo). Decision: keep current layout, optimize prefetch pipeline. Rationale documented in `hash-table-layout-analysis.md`.

### TESTING-ARCHITECTURE.md (2026-03-09)
Wrote comprehensive testing strategy (490 lines). Established 4-tier validation model. Inventory: ~1,950 tests across 9 categories. See quality-gate-verification skill.

### Wave 3 Release Automation (2026-03-09)
Branch: `gandalf/release-automation`. Created `release.toml`, `rust-release.yml`, `rust-ci.yml` semver checks, `docs/releasing.md`. Dependency-ordered publish with 30s index sync delays. See rust-release-pipeline skill.

### Wave 3 Integration (2026-03-09)
Integrated Legolas's benchmark baseline infrastructure and Boromir's release gate. Updated releasing.md to reference `bench-record-baseline.sh` and `release-gate` script.

## Learnings

### Deadlock Post-Mortem (2026-03-16)
Analyzed why two serious deadlocks (P1 flush-pipeline circular dependency, P0 lossy-mode InMemoryDevice lock starvation) escaped 2,100+ automated tests. Key findings:

1. **Loom tests are scoped correctly but limited by design.** Our 21 loom tests cover atomic primitives (Treiber stack, epoch, hash bucket, record info, state transitions, log alloc). They cannot model multi-component pipeline progress failures or scale-dependent behaviors. This is inherent — not a gap to fix.

2. **Both bugs share a meta-pattern: O(1)-at-test-scale → O(n)-at-production-scale.** Bug 1: retry loop deadlocks only when buffer fills under sustained load. Bug 2: `fill(0)` in truncate_until is O(total_bytes_flushed), taking 1-3s at ~9GB. Tests running <10s with small data never see this.

3. **Lossy mode is critically undertested.** Every test config helper sets `lossy: false`. Lossy mode activates `truncate_until` on every eviction cycle — a path no automated test exercises under load.

4. **InMemoryDevice hides pipeline stalls.** It completes I/O synchronously (`CompletedSync`), eliminating the timing gap between flush submission and callback that triggers Bug 1. Use `SlowDevice` for pipeline stress tests.

5. **Progress assertions > correctness assertions for concurrency.** "Did it finish?" misses throughput cliffs. "Did ops/s stay above a floor?" catches liveness bugs. Recommended: add throughput-over-time assertions to multi-writer tests.

6. **Deterministic simulation is the only tool that catches both bug categories.** Loom covers memory ordering, stress tests cover scale, but only a FoundationDB-style simulator covers progress + scale + timing together. Backlogged as R7.

7. **Immediate actions:** (R1) lossy-mode regression test, (R2) throughput-cliff detection in CI, (R3) "no O(n) under lock" code review rule, (R4) SlowDevice stress test in nightly. See `.squad/decisions/inbox/gandalf-deadlock-postmortem.md`.

### DST v2.0 Architecture (2026-03-16)
Authored the DST v2.0 architecture document (`gandalf-dst-v2-architecture.md`) — a FoundationDB-inspired deterministic simulation framework for catching time/scale-dependent concurrency bugs. Key decisions:

1. **Extend `faster-dst`, don't create a new crate.** Existing infrastructure (SimClock, DeterministicScheduler, SimTask, SeedCampaign, 22 crash scenarios) is reused directly. Concurrency simulation is a new scenario family, not a new framework.

2. **Run real FasterKv code, simulate only I/O and time.** The Device trait is the abstraction boundary — SimDeviceV2 with async completion queue, queue depth limits, and latency injection. Hash index, epoch system, hybrid log, and flush/evict pipeline all run as real production code.

3. **Two-phase thread model.** Phase 1 (4-5 weeks): real OS threads + simulated I/O timing. Not fully deterministic, but catches pipeline progress failures (Bug 1 class). Phase 2 (4-6 weeks): green threads via DeterministicScheduler for full determinism. Same seed = same execution = same outcome.

4. **`buggify!` macro follows `crash_point!` pattern.** Zero-cost without `simulation` feature. Seeded PRNG controls injection decisions for reproducibility.

5. **Scale simulation without real scale.** Virtual cost annotations (`annotate_cost`) model O(n) operations at small test data sizes. Catches Bug 2 pattern without 9GB of real memory.

6. **Key existing infrastructure reused:** SimulatedClock (clock.rs), DeterministicScheduler (scheduler.rs), SimTask/TaskAction/BlockReason (task.rs), SeedCampaign (campaign.rs), SimulatedStorage (device.rs), SimulationTrace (trace.rs), Workload trait (workload.rs), Invariant trait (invariant.rs), sync.rs cfg cascade, crash_point! macro pattern.

7. **CI integration:** PR gate <10 min (30 seeds), nightly <60 min (200 seeds), weekly exploration 10,000 seeds. Failing seed → auto-filed issue → developer reproduces in <30s.

8. **Identified risks:** FasterKv internal thread spawns in grow/checkpoint state machines (must intercept in Phase 2), `!Send` session types across yield points, cross-platform seed reproducibility.

### DST v2.0 Architecture (2026-03-16)
Produced gandalf-dst-v2-architecture.md (49KB) — FoundationDB-inspired deterministic simulation framework extending faster-dst for time/scale-dependent concurrency bugs:

1. **3-phase architecture:** Phase 1 (SimDeviceV2 + controlled multi-thread, 4-5 weeks) catches pipeline progress failures; Phase 2 (green threads/full determinism via DeterministicScheduler, 4-6 weeks) guarantees reproducibility; Phase 3 (buggify + fault campaigns, 2-3 weeks) exhaustive coverage.

2. **Device trait as simulation boundary.** SimDeviceV2 models async I/O with queue depth, latency injection, and completion callbacks. Hash index, epoch system, hybrid log, and flush/evict pipeline run as production code — only I/O and time are simulated (FoundationDB Future/Promise pattern).

3. **Release gate tiers designed:** PR gate <10 min (30 seeds), nightly <60 min (200 seeds), weekly exploration 10,000 seeds. Failing seed → auto-filed issue → developer reproduces in <30s.

4. **Reuses existing infrastructure:** SimulatedClock, DeterministicScheduler, SimTask/TaskAction/BlockReason, SeedCampaign, SimulatedStorage, SimulationTrace, Workload/Invariant traits, sync.rs cfg cascade, crash_point! macro. Also produced deadlock post-mortem (gandalf-deadlock-postmortem.md) identifying core infrastructure gaps.

Also investigated multi-writer deadlock root causes in earlier session (see gandalf-deadlock-postmortem.md).

### DST v2.0 Phase 1 Implementation Plan (2026-03-16 → 2026-03-17)
Produced `gandalf-dst-v2-implementation-plan.md` (597 lines) — detailed 7-phase breakdown for Phase 1 (SimDeviceV2 + controlled multi-thread scenarios). Critical path: 18 working days. Committed to git (branch: rust). Key decisions:

1. **7 phases, critical path ~18 working days.** P1 (sim_time/sim_hooks in faster-core) → P2 (SimDeviceV2) → P5 (DstRunner integration) → P6 (Bug 1/Bug 2 scenarios) → P7 (CI). P3 (cost model) and P4 (config/assertions) run in parallel.

2. **Agent assignment: Eowyn is primary implementer (5/7 phases), Sam owns faster-core changes (P1, P7).** Eowyn bottleneck mitigated by batching P2/P3/P4. Aragorn supports P6 test authoring. Legolas not needed until Phase 2.

3. **Integration milestone M3 is the risk gate:** First time real FasterKv runs against SimDeviceV2 (async completions vs CompletedSync). If flush pipeline assumptions break, debug before adding multi-writer complexity.

4. **Key risk: FasterKv flush pipeline has never been tested with in-process `Submitted` returns.** SimDeviceV2 defers callbacks unlike InMemoryDevice. Fallback: `sync_mode: bool` config for debugging.

5. **Phase 1 release gate:** SimDeviceV2 operational, virtual time wired, 4+ concurrency scenarios with canary seeds, deterministic I/O timing, no test regressions, CI integrated with < 10 min PR gate.

6. **Existing infrastructure inventory:** faster-dst has 4,086 lines across 17 modules + 1,215 lines in 22 crash-recovery scenarios. SimulatedClock already has `advance()`, `set()`, `now()` — needs only `charge()` and `now_nanos()` extensions. sync.rs 3-tier cascade (loom > simulation > std) already in place, simulation tier currently re-exports std — ready for swap.

7. **Open decisions resolved:** Completion scheduler as separate thread (not piggybacked on maintenance). SimDeviceV2 reads return CompletedSync initially (async reads deferred to Phase 2). StatsCollector records every op (ring buffer, bounded memory). Regression seeds live in `const` arrays in `seeds.rs`.

