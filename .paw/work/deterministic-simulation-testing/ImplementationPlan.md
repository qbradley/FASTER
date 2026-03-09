# Deterministic Simulation Testing — Implementation Plan

## Overview

Enhance the existing `faster-dst` crate into a comprehensive FoundationDB-style deterministic simulation testing framework. The framework replaces all sources of nondeterminism (thread scheduling, time, channels, I/O completion) with seed-controlled abstractions, enabling perfect reproducibility of any concurrent execution. A cooperative scheduler runs all "threads" as tasks on a single OS thread, crash-point injection hooks instrument every state machine transition, and page-level CRC checksums detect torn writes. A campaign engine sweeps thousands of seed/fault combinations efficiently.

The implementation requires coordinated changes across two crates:
- **faster-core**: Simulation abstraction layer (cfg-gated imports for atomics, threads, time, channels) and crash-point/checksum instrumentation in production state machines
- **faster-dst**: Deterministic scheduler, campaign engine, scenario templates, and enhanced simulation infrastructure

## Current State Analysis

**Existing faster-dst** (1,882 lines): SimulatedClock (AtomicU64 nanos), SimulatedDevice (fault-injectable, sync callbacks), FaultConfig (write/read error rates, partial writes, N-write/N-read limits), SimulationHarness (factory for stores/devices), SimulationRuntime (seeded PRNG), Workload/Invariant traits. Two test suites: crash_recovery (357 lines) and seed_exploration (207 lines).

**Nondeterminism surface** (small): 2 production thread spawns (uring-io, sync-io workers), 2 mpsc channels, 5 critical Instant::now() calls (timeout enforcement), 2 SystemTime::now() calls (checkpoint metadata), 1 randomness source (checkpoint token from SystemTime).

**sync.rs abstraction pattern** (44 lines): cfg-based loom swap exists but is **entirely unused** — all 13 production files import directly from `std::sync::atomic`, bypassing sync.rs. A refactor is required before any cfg-based substitution can work.

**Page format**: No page header — records start at byte offset 0. No checksums on hybrid log pages. Recovery performs NO data integrity validation (file existence + size only). Format version exists at value 2 (IndexRecoveryInfo, LogRecoveryInfo). crc32fast 1.5.0 already a dependency (used for index checkpoint CRC).

**Key constraint**: 13 production files need import migration to sync.rs before simulation cfg hooks become effective. This is the critical-path prerequisite.

## Desired End State

A framework where:
1. `cargo test -p faster-dst` runs simulation scenarios deterministically — same seed = identical execution
2. Crash injection at all 18 state machine transition points with automatic recovery and invariant verification
3. Page CRC-32C checksums detect 100% of torn writes with zero false positives
4. A 10,000-seed campaign across 5 scenario templates completes in under 30 minutes on 8-core CI
5. Production builds (`cargo build --release` without `simulation` feature) have zero overhead — identical binary
6. All existing tests (1,456+) continue to pass without modification

**Verification approach**: Each phase has automated tests. The final validation is SC-001 through SC-008 from Spec.md, verified by the campaign engine itself.

## What We're NOT Doing

- Network simulation (FASTER is single-process)
- Replacing loom or miri — simulation testing is complementary
- Changing the C FFI layer (faster-ffi)
- GUI or web dashboard for campaign results
- Integration with external DST frameworks (madsim, turmoil, Antithesis)
- Cross-process or multi-node simulation
- Performance benchmarking under simulation mode
- Formal verification or model checking
- Making page checksums mandatory for reading old data (backward compatibility via format version detection)

## Phase Status

- [x] **Phase 1: Simulation Abstraction Layer** — cfg-gated imports in faster-core, sync.rs migration, feature flag
- [ ] **Phase 2: Deterministic Scheduler & Task Model** — Cooperative scheduler, SimTask, deterministic channels in faster-dst
- [x] **Phase 3: CRUD + Epoch Simulation Integration** — FasterKv under scheduler, yield points, epoch invariant tests
- [x] **Phase 4: Crash-Point Injection Framework** — 18 instrumentation sites, crash+recovery test scenarios
- [ ] **Phase 5: Page CRC-32C Checksums** — Flush-path CRC, recovery validation, torn write detection
- [ ] **Phase 6: Campaign Engine & Scenario Templates** — Parallel seed exploration, standard scenarios, expanded fault injection
- [ ] **Phase 7: Documentation** — Docs.md technical reference, project documentation updates

## Phase Candidates

- [ ] Shrinking / minimization (find minimal seed that reproduces a failure)
- [ ] Coverage-guided seed selection (prioritize seeds that explore new code paths)
- [ ] Snapshot/restore for long-running simulations (checkpoint simulator state)

---

## Phase 1: Simulation Abstraction Layer

**Depends on**: None (foundation phase)

**Objective**: Establish the cfg-gated abstraction layer in faster-core so that all synchronization, threading, time, and channel operations route through substitutable imports. This is the foundation every subsequent phase depends on.

### Changes Required

- **`rust/crates/faster-core/Cargo.toml`**: Add `simulation = []` feature flag alongside existing `loom`, `paranoid`, `metrics`, `tracing`

- **`rust/crates/faster-core/src/sync.rs`**: Extend the existing loom cfg pattern with a third tier for simulation. The module currently provides `#[cfg(loom)]` and `#[cfg(not(loom))]` branches for atomics, Arc, Mutex, and thread. Add `#[cfg(feature = "simulation")]` branches that re-export simulation-aware types. Add new abstractions for:
  - `Instant` and `SystemTime` (delegate to SimClock under simulation)
  - `mpsc::channel` (delegate to deterministic channel under simulation)
  - `thread::spawn` (delegate to scheduler task creation under simulation)
  - cfg precedence: `loom` > `simulation` > `std` (loom and simulation are mutually exclusive in practice, but loom takes priority if both enabled)

- **13 production files — import migration**: Replace direct `use std::sync::atomic::*` with `use crate::sync::*` in:
  - `sync_file_device.rs`, `allocator.rs`, `store/pending_io.rs`, `device.rs`, `hybrid_log/flush.rs`, `hybrid_log/eviction.rs`, `metrics.rs`, `compaction/orchestrator.rs`, `epoch/entry.rs`, `epoch/guard.rs`, `epoch/drain.rs`, `epoch/table.rs`, `store/kv.rs`

- **2 time-using files — import migration**: Replace `use std::time::Instant` with `use crate::sync::Instant` in:
  - `checkpoint/log_writer.rs`, `checkpoint/snapshot_writer.rs`

- **3 critical Instant::now() sites in faster-core**: The calls at `kv.rs:266,279` and `orchestrator.rs:273,278` will route through the abstraction after migration. 2 additional sites in `faster-uring/device.rs:456,478` do NOT need migration — UringDevice is replaced entirely by SimulatedDevice under simulation.

- **2 SystemTime::now() sites**: `orchestrator.rs:296`, `metadata_store.rs:355` — route through `crate::sync::SystemTime`. Note: `orchestrator.rs:296` derives the checkpoint token from SystemTime, making it deterministic under simulation (satisfies FR-004). A Phase 3 test will verify checkpoint tokens are deterministic across runs with the same seed.

- **Fully-qualified path migration** (call-site changes required): `checkpoint/orchestrator.rs` and `checkpoint/metadata_store.rs` use fully-qualified `std::time::Instant::now()`, `std::time::SystemTime::now()`, and `std::thread::sleep()` — NOT via imports. For these files:
  1. Add `use crate::sync::{Instant, SystemTime};` imports
  2. Replace `std::time::Instant::now()` → `Instant::now()` (2 sites in orchestrator)
  3. Replace `std::time::SystemTime::now()` → `SystemTime::now()` (1 site in orchestrator, 1 in metadata_store)
  4. Replace `std::thread::sleep()` → `crate::sync::thread::sleep()` (1 site in orchestrator:283)

- **1 mpsc channel site needs migration**: `sync_file_device.rs:26` — route through channel abstraction. (The second channel at `faster-uring/device.rs:55` does not need migration — UringDevice is replaced by SimulatedDevice under simulation.)

- **Tests**: Unit tests in sync.rs verifying that the cfg resolution works correctly for each tier (std, simulation). Compile-test that `simulation` feature enables successfully.

### Success Criteria

#### Automated Verification
- [ ] `cargo nextest run --workspace --all-targets` — all existing tests pass (no regressions from import migration)
- [ ] `cargo check -p faster-core --features simulation` — compiles cleanly
- [ ] `cargo check -p faster-core` — compiles cleanly without simulation feature
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — zero warnings
- [ ] Zero `use std::sync::atomic` imports remain in faster-core production code (all routed through sync.rs)
- [ ] **SC-006 binary diff baseline**: Build production binary (`cargo build --release -p faster-core`) both with and without `simulation` feature. Compare with `size` command — text/data/bss sections must be identical. Store baseline for CI regression.

#### Manual Verification
- [ ] sync.rs cfg resolution is clear and well-documented (loom > simulation > std precedence)
- [ ] Import migration is mechanical — no behavioral changes

---

## Phase 2: Deterministic Scheduler & Task Model

**Depends on**: Phase 1 (simulation feature flag and sync.rs abstractions must exist)

**Objective**: Implement the cooperative single-threaded scheduler in faster-dst that replaces OS thread scheduling with seed-controlled deterministic task selection. This is the core of the DST framework.

### Key Design Decisions

**Task model — closure-based cooperative tasks (not async/await):** Closures that explicitly yield at scheduling points are simpler to reason about for simulation, require no async runtime, and make yield points visible and deterministic. Async/await would add implicit yield points at every `.await` and require a custom executor — more machinery for no benefit since we need explicit control over scheduling. Generator/coroutine approaches are nightly-only and unstable. Trade-off: closure-based tasks are less composable than futures but provide the explicit control the scheduler requires.

### Changes Required

- **`rust/crates/faster-dst/src/scheduler.rs`** (new): `DeterministicScheduler` — single-threaded event loop that maintains a ready queue of `SimTask`s, selects the next task to run based on seed-derived PRNG, and advances simulated time. Key components:
  - Ready queue (tasks eligible to run)
  - Blocked queue (tasks waiting on I/O, channel, or time)
  - Seed-derived `SmallRng` for task selection
  - `run_until_complete()` — execute all tasks to completion or deadlock detection
  - `run_until_idle()` — execute until no task is ready (useful for controlled stepping)
  - Deadlock detection: if all tasks blocked and no pending I/O completions, report deadlock

- **`rust/crates/faster-dst/src/task.rs`** (new): `SimTask` — cooperative task wrapping a closure or future-like state machine. Yield points:
  - `sim_yield()` — voluntary yield, task returns to ready queue
  - `sim_park()` — block until explicitly unparked (for channel recv, I/O wait)
  - Tasks identified by `TaskId` for deterministic scheduling decisions
  - Task-local storage for epoch thread identity

- **`rust/crates/faster-dst/src/channel.rs`** (new): `SimChannel<T>` — deterministic mpsc channel. `send()` enqueues message and unparks any blocked receiver. `recv()` returns message if available or parks the task. Delivery order controlled by scheduler (not OS).

- **`rust/crates/faster-dst/src/device.rs`** (modify): Enhance `SimulatedDevice` to queue I/O completion callbacks instead of calling them synchronously. Completions are delivered by the scheduler as events, allowing control over callback ordering. Add `IoCompletionEvent` struct and scheduler integration point.

- **`rust/crates/faster-dst/src/clock.rs`** (modify): Enhance `SimulatedClock` to support both `Instant`-style monotonic time and `SystemTime`-style wall clock. Add `sim_instant_now()` and `sim_system_time_now()` methods. Scheduler advances time when all tasks are blocked on time-based waits.

- **`rust/crates/faster-dst/src/trace.rs`** (new): `SimulationTrace` — structured execution trace logger (FR-017). Records every scheduler decision (task selected, yield reason), I/O operation (read/write issued, completion delivered), and time advancement. Output is deterministic per seed. Enabled via `RUST_LOG=trace` or programmatic configuration. Uses a `Vec<TraceEvent>` buffer for in-memory collection with optional streaming output.

- **`rust/crates/faster-dst/Cargo.toml`**: Add dependency on faster-core with `simulation` feature enabled

- **Tests** (`rust/crates/faster-dst/tests/scheduler_tests.rs`): 
  - Same seed produces identical task interleaving across runs
  - Different seeds produce different interleavings  
  - Channel send/recv is deterministic per seed
  - I/O completion ordering is deterministic per seed
  - Deadlock detection triggers when all tasks block
  - Time advancement is deterministic
  - At least 3 distinct interleaving patterns observed across 10 seeds (scheduler diversity)
  - Trace output from a simulation run shows every scheduler decision and I/O operation in deterministic order (FR-017, P1-AS3)

### Success Criteria

#### Automated Verification
- [ ] `cargo nextest run -p faster-dst` — all new scheduler tests pass
- [ ] `cargo nextest run -p faster-dst` — all existing crash_recovery and seed_exploration tests still pass
- [ ] Determinism test: run same scenario 100 times with same seed, assert byte-identical results (SC-001)

#### Manual Verification
- [ ] Scheduler selection algorithm provides meaningful interleaving diversity (not degenerate FIFO)
- [ ] Yield point API is ergonomic for test authors

---

## Phase 3: CRUD + Epoch Simulation Integration

**Depends on**: Phase 2 (scheduler and task model must exist)

**Objective**: Make FasterKv operations run under the deterministic scheduler. Insert yield points at scheduling-relevant locations so the scheduler can explore different interleavings of concurrent CRUD operations under epoch protection.

### Changes Required

- **`rust/crates/faster-core/src/store/kv.rs`**: Add `sim_yield()` calls (gated behind `#[cfg(feature = "simulation")]`) at:
  - Entry to each CRUD operation (`read`, `upsert`, `rmw`, `delete`) — after epoch protect, before hash lookup
  - After hash index CAS (upsert/rmw completion) — before returning to caller
  - In the blocking poll loop (`kv.rs:266-284`) — yield instead of `thread::sleep`
  - In the checkpoint flush-wait loop (`checkpoint/orchestrator.rs:283`) — yield instead of `thread::sleep(1ms)` to prevent blocking the cooperative scheduler

- **`rust/crates/faster-core/src/epoch/table.rs`**: Add yield points at:
  - After `protect()` (line ~198) — between announcing epoch and proceeding
  - After `bump_current_epoch()` (line ~268) — allow other tasks to observe new epoch
  - During safe-epoch scan (line ~355) — yield between reading each thread's epoch

- **`rust/crates/faster-core/src/hybrid_log/flush.rs`**: Add yield point:
  - After issuing async write (`flush.rs:252`) — allow scheduler to reorder completions

- **`rust/crates/faster-dst/src/sim_store.rs`** (new): `SimulatedFasterKv` factory — convenience wrapper that creates a `FasterKv` configured for simulation (SimulatedDevice, SimClock, etc.) and spawns CRUD operations as SimTasks on the scheduler. Each CRUD operation emits trace events (FR-017) via the `SimulationTrace` infrastructure from Phase 2.

- **Tests** (`rust/crates/faster-dst/tests/crud_simulation.rs`):
  - 4 simulated tasks × 100 upserts to overlapping keys → deterministic per seed, all CAS races resolved correctly (P2-AS1)
  - Epoch protection invariant: no task observes stale data after epoch advance (P2-AS2)
  - Read-after-write consistency under concurrent upserts
  - RMW correctness under concurrent modification
  - Delete followed by read returns NotFound under all interleavings
  - Checkpoint token determinism: two runs with same seed produce identical checkpoint tokens (FR-004)

### Success Criteria

#### Automated Verification
- [ ] `cargo nextest run -p faster-dst --test crud_simulation` — all concurrent CRUD tests pass
- [ ] `cargo nextest run --workspace --all-targets` — no regressions
- [ ] Determinism: each CRUD scenario produces identical results across 100 runs with same seed

#### Manual Verification
- [ ] Yield points are at meaningful scheduling boundaries (not too sparse, not too dense)
- [ ] SimulatedFasterKv API is clean and easy to use in new scenarios

---

## Phase 4: Crash-Point Injection Framework

**Depends on**: Phase 3 (SimulatedFasterKv and yield points must exist for crash scenarios)

**Parallelizable with**: Phase 5 (CRC checksums have no dependency on crash injection)

**Objective**: Instrument all state machine transition points with crash-point hooks that the simulation framework can trigger. When a crash fires, all in-flight state is dropped, a fresh FasterKv is created from the persisted SimulatedStorage snapshot, recovery runs, and invariants are verified.

### Key Design Decisions

**Crash mechanism — controlled panic with catch_unwind (not longjmp):** The scheduler wraps each task's execution in `std::panic::catch_unwind()`. When a crash point triggers, it panics with a sentinel type (`SimulatedCrash`). The scheduler catches the unwind, drops all task state (including FasterKv), and proceeds to recovery. Trade-off: panic unwinding runs Drop impls which may flush partial state — but this is actually desirable since it tests whether FASTER's Drop behavior is crash-safe. longjmp would skip destructors entirely, which doesn't model real crash behavior (the OS does run cleanup on some resources). Panic-based is both safer (no UB) and more realistic.

### Changes Required

- **`rust/crates/faster-core/src/sim_hooks.rs`** (new): `crash_point!()` macro gated behind `#[cfg(feature = "simulation")]`. When simulation is active, the macro checks a thread-local/task-local crash schedule. If the current crash point is scheduled, it panics with a `SimulatedCrash` sentinel that the scheduler catches via `catch_unwind`. Each crash point also emits a trace event (FR-017) recording the crash point name and whether it fired. When simulation is not active, the macro expands to nothing.

- **`rust/crates/faster-core/src/checkpoint/state_machine.rs`**: Insert `crash_point!(CrashPoint::Checkpoint(phase))` at each of the 6 phase transition sites identified in CodeResearch.md (lines 236, 381, 411, 415, 419, 424).

- **`rust/crates/faster-core/src/compaction/orchestrator.rs`**: Insert `crash_point!(CrashPoint::Compaction(phase))` at each of the 6 boundary points (before K1, K1→K2, K2→K3, mid-K3, K3→K4, after K4).

- **`rust/crates/faster-core/src/recovery/mod.rs`** and **`index_recovery.rs`**, **`log_recovery.rs`**: Insert `crash_point!(CrashPoint::Recovery(stage))` at each of the 6 boundary points (before discovery, before index recovery, after CRC check, before log recovery, during chain verification, after completion).

- **`rust/crates/faster-dst/src/fault.rs`** (modify): Enhance `CrashPoint` enum from documentation markers to active injection sites. Add `CrashSchedule` struct that specifies which crash points to trigger and when (e.g., "crash at checkpoint WaitFlush on the 2nd checkpoint").

- **`rust/crates/faster-dst/src/crash.rs`** (new): Crash-recovery test orchestration. `CrashRecoveryRunner`:
  1. Run workload under scheduler until crash point triggers
  2. Drop all FasterKv state (simulating process crash)
  3. Create fresh FasterKv from SimulatedStorage (persists across drops)
  4. Run recovery
  5. Verify invariants

- **Tests** (`rust/crates/faster-dst/tests/crash_injection.rs`):
  - Crash at each of 6 checkpoint phases → recover → all committed data present, no phantoms (P3-AS1)
  - Crash at compaction K3 (pointer swing) → recover → consistent hash index (P3-AS2)
  - Double-fault: crash during recovery → re-recover → success (P3-AS3)
  - Crash during first-ever checkpoint (no prior checkpoint to fall back to)
  - Crash at every compaction phase → recover → all live records present
  - Recovery from checkpoint taken during active compaction (spec edge case #3)
  - Zero-length workload followed by checkpoint — empty store edge case (spec edge case #4)
  - 6 checkpoint transitions × 100 seeds = 600 scenarios (SC-003)

### Success Criteria

#### Automated Verification
- [ ] `cargo nextest run -p faster-dst --test crash_injection` — all crash+recovery scenarios pass
- [ ] 600 checkpoint crash scenarios pass (6 transitions × 100 seeds) per SC-003
- [ ] `cargo nextest run --workspace --all-targets` — no regressions
- [ ] `cargo build --release -p faster-core` — zero-cost: crash_point!() compiles to nothing without simulation feature

#### Manual Verification
- [ ] crash_point!() macro is clean — minimal boilerplate at each injection site
- [ ] Crash-recovery orchestration correctly simulates process crash (full state drop, only SimulatedStorage survives)

---

## Phase 5: Page CRC-32C Checksums

**Depends on**: Phase 1 (simulation feature flag for testing). Independent of Phases 3-4.

**Parallelizable with**: Phase 4 (crash injection is independent of checksum logic)

**Objective**: Add page-level CRC-32C integrity checksums to the hybrid log. Pages are checksummed before flush and validated during recovery. This enables torn write detection in simulation and improves production durability.

### Key Design Decisions

**Checksums are always-on (not feature-gated):** CRC-32C checksums benefit production builds directly — they catch torn writes, silent corruption, and storage errors. The `crc32fast` crate uses hardware SIMD (SSE4.2 CRC32 instructions on x86_64), making the overhead negligible for 32MB pages. Gating behind a feature flag would create two on-disk format variants, complicating recovery and deployment. Instead: checksums are always computed and written. Format version bumps from 2 to 3. Recovery detects version 2 pages (no checksum) and skips validation for backward compatibility. This is a production durability improvement that happens to also enable torn-write testing in simulation.

### Changes Required

- **`rust/crates/faster-core/src/hybrid_log/page.rs`**: Add `PageChecksum` struct (CRC-32C value + valid_bytes). Define a page trailer format: the last 8 bytes of the sector-aligned write region store `[valid_bytes: u32, crc32: u32]`. This uses existing sector padding rather than changing the record start offset, preserving backward compatibility of the address encoding. When valid_bytes is already sector-aligned, extend the write by one additional sector to accommodate the trailer.

- **`rust/crates/faster-core/src/hybrid_log/flush.rs`**: Before `device.write_async()` at line 252 and `device.write_sync()` at line 322:
  1. Compute CRC-32C over `frame[0..valid_bytes]` using `crc32fast::hash()`
  2. Write trailer (valid_bytes, crc32) at `write_size - 8` offset in the page buffer
  3. Proceed with device write as before

- **`rust/crates/faster-core/src/recovery/log_recovery.rs`**: In `validate_log_file()` (line 325+):
  1. After loading each page, read trailer from last 8 bytes of sector-aligned region
  2. Compute CRC-32C over `page[0..trailer.valid_bytes]`
  3. Compare with `trailer.crc32`
  4. On mismatch: apply `ChecksumValidationPolicy` — an enum with variants `Reject` (return error, halt recovery), `Warn` (log corruption details + continue with best-effort), `Repair` (use last good checkpoint state). Default: `Reject`. Policy is set via `FasterKvConfig` and passed to recovery. Each mode requires explicit test coverage.

- **`rust/crates/faster-core/src/checkpoint/metadata.rs`**: Bump `FORMAT_VERSION_CURRENT` from 2 to 3. Add logic to detect version 2 pages (no checksum) and skip validation for backward compatibility.

- **`rust/crates/faster-dst/src/device.rs`**: Verify SimulatedDevice partial write mode produces correctly torn CRC mismatches. Add test helper `verify_page_checksum()`.

- **Tests** (`rust/crates/faster-dst/tests/checksum_tests.rs` and `rust/crates/faster-core` unit tests):
  - Valid page write → read → CRC passes (P4-AS1)
  - SimulatedDevice with `partial_write_rate=1.0` → CRC mismatch detected (P4-AS2)
  - Checkpoint with one torn page → recovery detects and handles safely (P4-AS3)
  - Version 2 format (no checksum) pages read without error (backward compat)
  - 100% of simulated torn writes detected, zero false positives (SC-004)
  - ChecksumValidationPolicy::Reject — torn page causes recovery to return error (FR-009)
  - ChecksumValidationPolicy::Warn — torn page logged, recovery continues, data accessible (FR-009)
  - ChecksumValidationPolicy::Repair — torn page triggers fallback to last good checkpoint (FR-009)

### Success Criteria

#### Automated Verification
- [ ] `cargo nextest run --workspace --all-targets` — all tests pass (including existing crash_recovery with new checksum code)
- [ ] CRC validation catches 100% of injected torn writes with zero false positives (SC-004)
- [ ] `cargo build --release -p faster-core` — CRC overhead negligible (crc32fast uses SIMD; always-on, not feature-gated)
- [ ] Format version 2 backward compatibility preserved

#### Manual Verification
- [ ] Trailer approach does not require changing LogicalAddress encoding or record start offset
- [ ] CRC computation overhead is negligible for 32MB pages (crc32fast uses SIMD)

---

## Phase 6: Campaign Engine & Scenario Templates

**Depends on**: Phases 2, 3, 4, 5 (needs scheduler, CRUD integration, crash injection, and checksums for full scenario coverage)

**Objective**: Build the seed exploration campaign system that sweeps thousands of seed/fault combinations across scenario templates, executing in parallel and reporting failures with reproduction commands.

### Changes Required

- **`rust/crates/faster-dst/src/scenario.rs`** (new): `ScenarioTemplate` — declarative specification combining:
  - Workload: which operations (CRUD mix, key distribution, operation count)
  - Fault profile: FaultConfig settings (error rates, partial writes, limits)
  - Crash schedule: which crash points to trigger and when
  - Invariants: which invariant checks to run post-scenario (using composable combinators)
  - Builder pattern for ergonomic construction

- **`rust/crates/faster-dst/src/invariant.rs`** (modify): Extend the existing `Invariant` trait with composable combinators (FR-012): `And<A, B>`, `Or<A, B>`, `All(Vec<Box<dyn Invariant>>)`, `Not<I>`. Add standard invariant library: `AllCommittedRecoverable` (existing), `NoPhantomRecords`, `MonotonicAddresses`, `ConsistentHashIndex`, `ValidPageChecksums`. ScenarioTemplate consumes composed invariants via `invariants: Vec<Box<dyn Invariant>>`.

- **`rust/crates/faster-dst/src/campaign.rs`** (new): `SeedCampaign` — execution engine:
  - Accepts Vec<ScenarioTemplate> and seed range
  - Runs each (template, seed) pair as an independent single-threaded scenario
  - Parallel execution across seeds using `std::thread` (each scenario is independent and single-threaded)
  - Thread pool sized to available cores (configurable)
  - Collects results: pass/fail, invariant violations, reproduction info

- **`rust/crates/faster-dst/src/report.rs`** (new): `CampaignReport` — structured output:
  - Summary: total scenarios, passed, failed, elapsed time
  - Per-failure: seed, scenario template name, invariant violated, backtrace excerpt
  - Reproduction command: `cargo test -p faster-dst --test campaign -- --seed=N --scenario=name`
  - Machine-readable (JSON) and human-readable (text) output formats

- **`rust/crates/faster-dst/src/fault.rs`** (modify): Expand `FaultConfig` with:
  - `allocation_failure_after: Option<u64>` — fail allocations after N operations (P6)
  - `io_latency_ms: Option<u64>` — simulated I/O latency for timeout testing (P6)
  - `epoch_drain_timeout: bool` — simulate a task that refuses to advance epoch (P6)

- **`rust/crates/faster-dst/src/scenarios/`** (new directory): Standard scenario templates:
  - `crud_stress.rs` — high-contention concurrent CRUD
  - `checkpoint_crash.rs` — crash at every checkpoint phase
  - `compaction_crash.rs` — crash at every compaction phase
  - `recovery_stress.rs` — crash during recovery + re-recover
  - `torn_write.rs` — partial write injection + CRC detection

- **Tests** (`rust/crates/faster-dst/tests/campaign_tests.rs`):
  - 1,000-seed campaign completes in under 60 seconds (P5-AS1 scaled)
  - Campaign with intentionally broken invariant reports correct failing seeds (P5-AS2)
  - Parallel execution scales near-linearly on available cores (P5-AS3)
  - All standard scenarios pass across 1,000 seeds each
  - Reproduction command actually reproduces the failure
  - SC-002: synthetic bug (intentionally broken CAS) caught within 10,000 seeds
  - SC-005: 10,000-seed campaign across 5 templates completes in under 30 minutes
  - Boundary seed values (0 and u64::MAX) produce valid deterministic runs (spec edge case #6)
  - Total storage failure scenario — FaultConfig with 100% error rates handled gracefully (spec edge case #7)
  - Epoch table capacity stress — 256+ simulated tasks approach/exceed MAX_THREADS limit (spec edge case #5)
  - Per-scenario execution time assertion: no scenario exceeds 100ms for 1,000-op workload (NFR validation)

### Success Criteria

#### Automated Verification
- [ ] `cargo nextest run -p faster-dst --test campaign_tests` — all campaign tests pass
- [ ] 10,000-seed campaign across 5 templates < 30 minutes on 8-core CI (SC-005)
- [ ] Synthetic bug injection detected within 10,000 seeds (SC-002)
- [ ] `cargo nextest run --workspace --all-targets` — no regressions

#### Manual Verification
- [ ] ScenarioTemplate API is intuitive — easy to define new scenarios
- [ ] Campaign report provides actionable reproduction information
- [ ] Standard scenarios cover the most critical state machine paths

---

## Phase 7: Documentation

**Depends on**: Phases 1-6 (documents the completed framework)

**Objective**: Create comprehensive documentation covering the simulation testing framework's architecture, usage, and integration with CI.

### Changes Required

- **`.paw/work/deterministic-simulation-testing/Docs.md`**: Technical reference covering:
  - Architecture overview (scheduler, task model, cfg abstraction layer)
  - API reference for key types (DeterministicScheduler, SimTask, ScenarioTemplate, SeedCampaign)
  - Writing simulation scenarios (step-by-step guide with examples)
  - Crash-point injection usage
  - Campaign configuration and execution
  - Debugging simulation failures (seed reproduction, trace logging)
  - Integration with CI (recommended configurations, time budgets)

- **`rust/crates/faster-dst/README.md`** (create or update): Crate-level documentation with quick-start examples

- **SC-008 Deployment Campaign**: After Phase 6 completion, run full campaign engine against real workload patterns (not synthetic). Investigate any failures to determine if they represent previously unknown bugs. Document findings. This is a stretch goal (SC-008) but the activity must be explicitly scheduled to have any chance of evaluation.

- **`rust/crates/faster-dst/src/lib.rs`**: Module-level documentation (`//!`) and public API doc comments with examples

- **`rust/README.md`** (update): Add section on simulation testing under testing methodology

- **Doc comments**: All public types and functions in new modules have `///` documentation with usage examples

### Success Criteria

- [ ] `cargo doc --workspace --no-deps` — builds without warnings
- [ ] All public APIs have doc comments with examples
- [ ] Docs.md provides complete technical reference
- [ ] A developer unfamiliar with the codebase can write a new scenario template using only the documentation

---

## References

- Issue: none
- Spec: `.paw/work/deterministic-simulation-testing/Spec.md`
- Research: `.paw/work/deterministic-simulation-testing/CodeResearch.md`
- FoundationDB DST: Will Wilson, "Testing Distributed Systems w/ Deterministic Simulation" (Strange Loop 2014)
- Existing faster-dst crate: `rust/crates/faster-dst/`
- sync.rs loom pattern: `rust/crates/faster-core/src/sync.rs`
- Checkpoint state machine: `rust/crates/faster-core/src/checkpoint/state_machine.rs`
- Compaction orchestrator: `rust/crates/faster-core/src/compaction/orchestrator.rs`
- Recovery flow: `rust/crates/faster-core/src/recovery/`
- Page flush path: `rust/crates/faster-core/src/hybrid_log/flush.rs`
- Epoch framework: `rust/crates/faster-core/src/epoch/`
