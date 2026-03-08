# Feature Specification: Deterministic Simulation Testing

**Branch**: feature/deterministic-simulation-testing  |  **Created**: 2025-07-12  |  **Status**: Draft
**Input Brief**: Build a FoundationDB-style deterministic simulation testing framework for FASTER Rust

## Overview

FASTER Rust is a high-performance concurrent key-value store with complex internal state machines spanning epoch-protected concurrent access, hybrid log management, checkpoint/recovery, and compaction. These subsystems interact through lock-free atomic operations, background I/O threads, and callback-driven completion across 13+ thread spawn points. Today, the 1,456-test suite validates correctness primarily through integration tests and targeted loom/miri checks on isolated algorithms, but cannot systematically explore the vast space of concurrent interleavings, crash timings, and fault combinations that real deployments encounter.

Deterministic simulation testing (DST), pioneered by FoundationDB and adopted by systems like TigerBeetle and WarpStream, solves this by replacing all sources of nondeterminism with controlled abstractions. Every "thread" becomes a cooperative task on a single-threaded scheduler. Every time query returns simulator-controlled values. Every I/O operation passes through a fault-injectable device. A single seed number fully determines the entire execution, making any failure perfectly reproducible by re-running with that seed. This transforms the hardest class of bugs — race conditions, crash-recovery edge cases, and fault-timing sensitivities — from "impossible to reproduce" into "trivially replayable."

The existing `faster-dst` crate provides a foundation with `SimulatedDevice`, `FaultConfig`, seeded workload generation, and basic crash-recovery tests. This effort will enhance it into a comprehensive FoundationDB-style framework by adding a deterministic cooperative scheduler, time abstraction, systematic crash-point injection across all state machines, page-level checksums for torn-write detection, and a scenario exploration engine that can sweep thousands of seed/fault combinations efficiently. The framework must align with FASTER's test economics model: sophisticated code is free (AI writes and maintains it), but tests must execute fast and produce zero false positives.

## Objectives

- Enable deterministic reproduction of any concurrent bug by re-running a single seed value
- Provide systematic exploration of thread interleaving spaces that real-world testing cannot cover
- Detect crash-recovery correctness violations including torn writes, phantom reads, and lost committed data
- Catch fault-handling bugs by injecting I/O errors, partial writes, and resource exhaustion at controlled points
- Validate epoch protection invariants under adversarial scheduling (no stale pointer access, no premature reclamation)
- Achieve zero false positives — every simulation failure must represent a real bug (Rationale: false positives destroy trust and waste investigation time)
- Execute individual simulation scenarios in under 100ms to enable exploring thousands of seeds per minute
- Require zero runtime overhead in production builds via compile-time cfg gating

## User Scenarios & Testing

### User Story P1 – Deterministic Bug Reproduction

Narrative: A developer discovers a rare crash in production logs. They extract the operation sequence and run it through the simulation framework with seed exploration. The framework finds a seed that reproduces the exact failure. The developer re-runs that seed under a debugger, stepping through the deterministic execution to identify the root cause.

Independent Test: Run a simulation scenario with a known seed, observe it produces identical output on every run.

Acceptance Scenarios:
1. Given a simulation configured with seed=42 and a CRUD workload, When the scenario runs twice, Then every observable event (operation results, callback order, page states) is identical.
2. Given a bug that manifests as a lost record after concurrent upsert + compaction, When the developer runs seed exploration over 10,000 seeds, Then at least one seed reproduces the exact failure with a clear backtrace.
3. Given a failing seed, When the developer re-runs with `RUST_LOG=trace` and that seed, Then the execution trace shows every scheduler decision, I/O operation, and state transition in deterministic order.

### User Story P2 – Cooperative Task Scheduling

Narrative: A test author writes a simulation scenario that exercises concurrent CRUD operations across multiple simulated threads. The deterministic scheduler controls which task runs at each step, exploring different interleavings based on the seed. This catches races that would require millions of real-threaded runs to encounter.

Independent Test: Two tasks performing conflicting CAS operations produce different outcomes under different seeds but identical outcomes when the same seed is reused.

Acceptance Scenarios:
1. Given 4 simulated tasks each performing 100 upserts to overlapping keys, When run under the cooperative scheduler, Then exactly one task "wins" each CAS race (no lost updates), and the winner is deterministic per seed.
2. Given a scenario with epoch protection and a deferred callback, When the scheduler runs task A past the epoch boundary before task B, Then the callback executes before task B can observe stale data.
3. Given a scenario that yields at every scheduling point, When run with N different seeds, Then at least 3 distinct interleaving patterns are observed (scheduler provides meaningful diversity).

### User Story P3 – Crash-Point Injection

Narrative: A test scenario exercises the checkpoint state machine by injecting a simulated crash at each phase transition (Rest→Prepare, Prepare→InProgress, InProgress→WaitFlush, etc.). After each crash, recovery runs and invariants are verified: all committed data is present, no phantom data from incomplete operations.

Independent Test: Crash during WaitFlush phase, recover, verify all data committed before the checkpoint is present.

Acceptance Scenarios:
1. Given a store with 1,000 committed records and a checkpoint in WaitFlush phase, When a crash is injected and recovery runs, Then all 1,000 records are readable and no phantom records exist.
2. Given a store undergoing compaction at phase K3 (pointer swing), When a crash is injected mid-CAS, Then recovery produces a consistent hash index with no dangling pointers.
3. Given a crash during recovery itself (double-fault), When recovery re-runs from the same checkpoint, Then it completes successfully with correct data.

### User Story P4 – Torn Write Detection via Page Checksums

Narrative: The hybrid log gains page-level CRC checksums. When the simulation framework injects a partial write (torn page), the recovery process detects the corruption via checksum mismatch and handles it safely — either by discarding the torn page or falling back to the last known good state.

Independent Test: Write a page with valid CRC, corrupt half the page, attempt to read — checksum validation fails and reports the corruption.

Acceptance Scenarios:
1. Given a page written with a valid CRC-32C checksum, When the full page is read back, Then checksum validation succeeds.
2. Given a SimulatedDevice configured with partial_write_rate=1.0, When a page flush occurs, Then the written page has a mismatched CRC and recovery detects the torn write.
3. Given a checkpoint with one torn page, When recovery runs, Then it either uses the previous checkpoint's version of that page or reports a recoverable error — never serves corrupted data.

### User Story P5 – Seed Exploration Campaign

Narrative: A CI job runs a "simulation campaign" that sweeps a configured number of seeds (e.g., 10,000) across a set of scenario templates. Each scenario template defines a workload pattern and fault profile. The campaign runs in parallel across CPU cores and reports any failing seeds with full reproduction commands.

Independent Test: Run a 1,000-seed campaign, all scenarios pass, campaign completes in under 60 seconds.

Acceptance Scenarios:
1. Given a campaign with 10 scenario templates and 1,000 seeds each, When the campaign runs, Then it completes within the Tier 3 time budget (30 minutes) on a standard 8-core CI runner.
2. Given a campaign that finds 2 failing seeds, When the report is generated, Then each failure includes: seed, scenario template name, invariant violated, and a reproduction command.
3. Given a campaign on a 16-core machine, When parallelism is enabled, Then throughput scales near-linearly (each scenario is independent single-threaded).

### User Story P6 – Fault Injection Expansion

Narrative: Beyond I/O faults, the simulation framework supports injecting resource exhaustion (allocation failure), artificial latency spikes, and epoch drain timeouts. These faults are configured per-scenario and controlled deterministically by the seed.

Independent Test: Configure allocation failure after N allocations, observe the store handles it gracefully without corruption.

Acceptance Scenarios:
1. Given a FaultConfig with fail_after_n_writes=50 and a workload of 100 writes, When the scenario runs, Then the first 50 writes succeed, subsequent writes return an error, and recovery of the 50 committed writes succeeds.
2. Given an artificially delayed I/O completion (simulated 500ms latency), When the pending I/O manager's timeout fires, Then the timeout is handled correctly without data loss.
3. Given epoch drain failure (one simulated thread refuses to advance), When drain timeout fires, Then the checkpoint is aborted cleanly with an appropriate error.

### Edge Cases
- Crash at the exact byte boundary of a page write (worst-case torn write)
- Crash during the first-ever checkpoint (no prior checkpoint to fall back to)
- Recovery from a checkpoint taken during active compaction
- Zero-length workload followed by checkpoint (empty store edge case)
- Maximum number of simulated tasks exceeding epoch table capacity (256 slots)
- Seed value 0 and u64::MAX (boundary seeds)
- Scenario where every I/O operation fails (total storage failure)
- Double-fault: crash during recovery, then recover again

## Requirements

### Functional Requirements

- FR-001: Deterministic cooperative scheduler that executes tasks on a single OS thread with seed-controlled interleaving (Stories: P1, P2)
- FR-002: Task abstraction replacing `std::thread::spawn` under `#[cfg(feature = "simulation")]` flag, with yield points at all scheduling-relevant locations (Stories: P2)
- FR-003: Simulated time abstraction replacing `Instant::now()` and `SystemTime::now()` under simulation cfg, with scheduler-controlled advancement (Stories: P1, P2)
- FR-004: Deterministic random number generation seeded from scenario seed for all randomness sources including checkpoint tokens (Stories: P1)
- FR-005: Crash-point injection hooks in checkpoint state machine at every phase transition boundary (Stories: P3)
- FR-006: Crash-point injection hooks in compaction orchestrator between each phase (K1→K2→K3→K4) (Stories: P3)
- FR-007: Crash-point injection hooks in recovery flow at each stage boundary (Stories: P3)
- FR-008: Page-level CRC-32C checksums in hybrid log page headers for torn write detection (Stories: P4)
- FR-009: Checksum validation during page reads with configurable policy (reject, warn, or repair) (Stories: P4)
- FR-010: SimulatedDevice enhancement with deterministic callback ordering and controllable completion timing (Stories: P1, P6)
- FR-011: Expanded FaultConfig supporting allocation failures, latency injection, and epoch drain timeouts (Stories: P6)
- FR-012: Invariant checking framework with composable invariant combinators verified after each scenario (Stories: P1, P3, P5)
- FR-013: Scenario template system defining workload patterns, fault profiles, crash points, and expected invariants (Stories: P5)
- FR-014: Seed exploration engine running N seeds across M scenario templates with parallel execution (Stories: P5)
- FR-015: Campaign reporting with failing seed identification, invariant violation details, and reproduction commands (Stories: P5)
- FR-016: Integration with `cargo test` via standard `#[test]` functions for individual scenarios (Stories: P1, P2, P3)
- FR-017: Execution trace logging under simulation mode recording every scheduler decision, I/O operation, and state transition (Stories: P1)
- FR-018: Channel determinism — `mpsc::channel` operations under simulation must deliver in scheduler-controlled order (Stories: P2)
- FR-019: Zero production overhead — all simulation instrumentation gated behind `#[cfg(feature = "simulation")]` with zero cost when disabled (Stories: all)
- FR-020: Existing faster-dst tests (crash_recovery, seed_exploration) continue to pass after enhancement (Stories: all)

### Key Entities

- **SimTask**: A cooperative task representing a logical thread, with explicit yield points and scheduler-managed execution
- **DeterministicScheduler**: Single-threaded scheduler selecting which SimTask to run next based on seed-derived decisions
- **SimClock**: Controlled time source returning scheduler-determined values for `Instant::now()` and `SystemTime::now()`
- **CrashPoint**: Named injection site in a state machine where the simulator can trigger a crash (drop all state, run recovery)
- **ScenarioTemplate**: Declarative specification of workload + fault profile + crash configuration + invariants to check
- **Campaign**: A sweep of N seeds across M scenario templates, producing a pass/fail report
- **PageChecksum**: CRC-32C value stored in page header, computed over page payload

### Cross-Cutting / Non-Functional

- Individual scenario execution must complete in under 100ms for typical workloads (1,000 operations) to enable high-throughput seed exploration
- Simulation mode compilation must not increase production build time by more than 5% (cfg gating eliminates dead code)
- Framework must be safe to run in CI without special permissions (no ptrace, no kernel modules, no root access)
- All public APIs must have doc comments with examples
- Simulation failures must include enough context for reproduction without access to the original environment

## Success Criteria

- SC-001: Running the same scenario with the same seed produces byte-identical results across 100 consecutive runs (FR-001, FR-003, FR-004)
- SC-002: A synthetic bug (intentionally broken CAS in hash index) is caught by seed exploration within 10,000 seeds (FR-001, FR-014)
- SC-003: Crash injection at every checkpoint phase transition (6 transitions × 100 seeds = 600 scenarios) completes with all invariants passing or known-bug identification (FR-005, FR-012)
- SC-004: Page CRC validation catches 100% of simulated torn writes with zero false positives (FR-008, FR-009)
- SC-005: A 10,000-seed campaign across 5 scenario templates completes in under 30 minutes on a standard 8-core CI runner (FR-014)
- SC-006: Production build (`cargo build --release` without simulation feature) shows zero code size or performance difference from baseline (FR-019)
- SC-007: All existing faster-dst tests pass without modification after framework enhancement (FR-020)
- SC-008: Framework detects at least one previously unknown bug during initial deployment campaign (FR-001, FR-005, FR-006 — stretch goal)

## Assumptions

- CRC-32C is sufficient for torn write detection; cryptographic hashing not required (CRC-32C has hardware acceleration on x86_64 via SSE4.2)
- 256 epoch table slots is sufficient for simulation scenarios (real deployments rarely exceed 64 threads)
- Single-threaded cooperative scheduling faithfully models the concurrency bugs that multi-threaded execution would encounter (validated by FoundationDB's decade of production use)
- The existing `SimulatedDevice` and `SimulatedStorage` architecture is sound and should be extended, not replaced
- Page header space is available for checksum storage without breaking the on-disk format (or format version will be bumped)
- The `simulation` feature flag can coexist with `loom` and other existing feature flags without conflicts

## Scope

In Scope:
- Deterministic cooperative scheduler with seed-controlled interleaving
- Time and random abstraction layers under simulation cfg flag
- Crash-point injection in checkpoint, compaction, and recovery state machines
- Page-level CRC-32C checksums in hybrid log
- Enhanced fault injection (I/O errors, partial writes, allocation failures, timeouts)
- Invariant checking framework with standard invariants
- Scenario template and seed exploration campaign system
- Campaign reporting and reproduction support
- Production code instrumentation behind cfg flags (faster-core changes)
- Integration with existing faster-dst infrastructure
- Documentation and examples

Out of Scope:
- Network simulation (FASTER is single-process, not distributed)
- GUI or web dashboard for campaign results
- Automated bug-fix generation from simulation failures
- Integration with external frameworks (madsim, turmoil, Antithesis)
- Cross-process or multi-node simulation
- Formal verification or model checking (complementary but separate)
- Performance benchmarking under simulation mode (simulation trades speed for determinism)
- Changes to the C FFI layer (faster-ffi) for simulation support

## Dependencies

- `crc32fast` crate (or equivalent) for hardware-accelerated CRC-32C computation
- Existing `faster-dst` crate infrastructure (SimulatedDevice, FaultConfig, SimulationRuntime, harness)
- Existing `faster-core` trait abstractions (Device, Functions, CompactionPolicy)
- `#[cfg(feature = "simulation")]` support in faster-core Cargo.toml
- Nightly Rust toolchain (already required for miri tests)
- The `rand` crate with `SmallRng` for deterministic PRNG (already a dependency)

## Risks & Mitigations

- **Risk: Cooperative scheduling may not faithfully represent real threading bugs.** Impact: False confidence in correctness. Mitigation: Validate framework by injecting known bugs and confirming detection; complement with continued loom tests for memory-ordering-specific bugs.
- **Risk: Production code instrumentation (cfg hooks) introduces maintenance burden.** Impact: Cfg branches diverge over time. Mitigation: CI runs tests with AND without simulation feature; macro abstractions minimize boilerplate; code review enforces cfg hygiene.
- **Risk: Page checksums change the on-disk format, breaking compatibility.** Impact: Cannot read old checkpoints. Mitigation: Use format versioning (existing `format_version` field in recovery metadata); checksums optional via feature flag initially.
- **Risk: Single-threaded simulation is too slow for large workloads.** Impact: Campaign time budgets exceeded. Mitigation: Scenarios use small workloads (100-1000 ops); campaigns parallelize across seeds (each seed is independent); profile and optimize scheduler hot path.
- **Risk: Too many yield points slow simulation or reduce interleaving diversity.** Impact: Scenarios take longer, or scheduler choices become meaningless. Mitigation: Yield points categorized by granularity (coarse/fine); campaign configs select granularity level; profile yield-point density.
- **Risk: Existing faster-dst tests break during enhancement.** Impact: Regression. Mitigation: Existing tests are first CI gate; all changes must pass them before merging.

## References

- FoundationDB simulation testing: Will Wilson, "Testing Distributed Systems w/ Deterministic Simulation" (Strange Loop 2014)
- Antithesis: https://antithesis.com/resources/deterministic_simulation_testing/
- TigerBeetle simulation: https://tigerbeetle.com/blog/2023-07-11-we-put-a-distributed-database-in-the-browser
- Phil Eaton, "What's the big deal about Deterministic Simulation Testing?": https://notes.eatonphil.com/2024-08-20-deterministic-simulation-testing.html
- Amplify Partners, "A DST Primer for Unit Test Maxxers": https://www.amplifypartners.com/blog-posts/a-dst-primer-for-unit-test-maxxers
- Existing faster-dst crate: `rust/crates/faster-dst/`
- Loom pattern (cfg-based substitution): `rust/crates/faster-core/src/sync.rs`
- Device trait: `rust/crates/faster-core/src/device.rs:222`
- Checkpoint state machine: `rust/crates/faster-core/src/checkpoint/state_machine.rs`
- Compaction orchestrator: `rust/crates/faster-core/src/compaction/`
