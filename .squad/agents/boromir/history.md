# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **Focus areas:** Compaction testing, hybrid log testing, EPVS integration testing, runtime warnings/diagnostics.
- **Compaction scanner:** Stateless scan classifies records (tombstone→invalid→chain walk→LIVE). `#[deny(unsafe_code)]` on module. Warnings: >100MB tombstone vector (log::warn!), >1M chain hops. Post-scan byte validation with 5% threshold.
- **Hybrid log testing:** `checkpoint(dir, FoldOver)` forces data to read-only without eviction. With 32MB pages, natural RO advancement impractical — need explicit checkpoint. Scanner skips evicted pages. `HybridLogAllocator` is `pub(crate)`.
- **EPVS testing:** `SystemState::VERSION_MASK` is `pub(crate)` — integration tests use local constant `0x00FF_FFFF_FFFF_FFFF`. `CounterFunctions<K>` uses i64 not u64. RMW during checkpoint can lose increments (expected FASTER behavior).
- **Key test files:** `tests/compaction_integration.rs` (32 tests), `tests/hybrid_log_tests.rs` (31 tests), `tests/epvs_integration.rs` (28 tests), `tests/memory_pressure_tests.rs`.
- **Variable-length records:** Alignment padding — key len 4→5 crosses 8-byte boundary (pad(24,8)=24 vs pad(25,8)=32). Need `shift_read_only_to_tail()` before `CopyUpdated`. `VarLenFunctions` (Vec<u8>) needed since `SimpleFunctions` requires `V: Copy`.
- **Dependencies:** `log` crate added as non-optional dep for runtime warnings.
- **Multi-agent safety:** Use `git worktree` for isolated directory. `git cat-file -p <commit>:<path>` for reliable file extraction.

## Learnings
<!-- Append new learnings -->
- `log` crate added as non-optional dep for compaction runtime warnings.
- All workspace crates currently have `publish = false` — release gate tier 4 packaging checks are informational until publish is enabled.
- `checkin` script does `git add -A` which stages everything — for multi-agent workflows, use selective `git add` + `git commit` to avoid staging other agents' uncommitted files.
- Fuzz smoke tests (10s/target) catch most crashes; extended (5min/target) is for release candidates only.
- No CHANGELOG.md exists yet — tier 4 warns about this but doesn't block.
- Recovery `LogRecoveryEngine::validate_log_file` hardcodes `log.{n}` segment names — `SyncFileDevice` prefix must be "log." for recovery to find segments.
- `IndexRecoveryEngine` is lenient with mismatched metadata/file sizes — it reads from the file and doesn't crash, even if metadata claims more buckets than exist. Not a strict validator.
- Multi-agent safety: other agents can switch branches in shared worktree. Always verify `branch --show-current` before committing. Use selective `git add` + `git commit` instead of `checkin` script to avoid staging others' changes.
- Alignment padding: key len 4→5 DOES cross 8-byte boundary (pad(24,8)=24 vs pad(25,8)=32).
- Need `shift_read_only_to_tail()` before `CopyUpdated` in compaction.
- `VarLenFunctions` (Vec<u8>) needed since `SimpleFunctions` requires `V: Copy`.
- `SystemState::VERSION_MASK` is `pub(crate)` — integration tests use local constant `0x00FF_FFFF_FFFF_FFFF`.
- `CounterFunctions<K>` uses i64 not u64.
- RMW during checkpoint can lose increments (expected FASTER behavior).
- `checkpoint(dir, FoldOver)` forces data to read-only without eviction.
- Scanner skips evicted pages. `HybridLogAllocator` is `pub(crate)`.
- `git worktree` for isolated directory. `git cat-file -p <commit>:<path>` for reliable file extraction.

## Session Log

### SF-3/5/8/9 — Compaction Scanner Runtime Warnings & Edge-Case Tests (2026-03-07)
**Commit:** `4c11175b`

Changes: SF-3 tombstone vector warning (log::warn! when >100MB), SF-5 chain hops metric (return changed to `(bool, usize)`, `CompactionPlan.total_chain_hops`, warn at 1M hops), SF-8 post-scan byte validation (5% threshold + accounting invariant), SF-9 5 new test categories.

---

### EPVS Integration Tests (2026-03-07)
**Branch:** `boromir/epvs-integration-tests`, **File:** `tests/epvs_integration.rs` (1,723 lines, 28 tests)

**Test categories:** torn read detection, phase/version consistency, grow cycle atomicity, checkpoint+grow mutual exclusion (4T stress), version monotonicity (8T), FasterKv-level ops during checkpoint/grow, intermediate window finiteness, sequential correctness, proptest (pack/unpack roundtrip, CAS semantics, word encoding uniqueness, lifecycle monotonicity), edge cases (version overflow, boundary versions), stress (8T/16T).

**Results:** 26 passed, 0 failed, 2 ignored (16T stress).

---

### Compaction + Hybrid Log Test Coverage (2026-03-08)
**Branch:** `boromir/compaction-hybridlog-tests`
**Files:** `tests/compaction_integration.rs` (11→32 tests, +658 lines), `tests/hybrid_log_tests.rs` (NEW, 31 tests, 865 lines)

**Compaction tests:** real-scale (1500 records), live record verification, dead/superseded records, policy enforcement (7 tests: SpaceAmplification, TombstonePercent, Manual, Any/All), policy-driven maybe_compact, sequential compactions, concurrent CRUD, error recovery, address verification, statistics consistency, concurrent serialization (4T race).

**Hybrid log tests:** address boundaries, flush behavior, eviction cycles, policy, maintenance, concurrent ops, region edge cases, buffer config variants, checkpoint-based read-only.

**Results:** 63 tests, all pass.

---

### Recovery Edge-Case Gap Fill (2026-03-09)
**Branch:** `boromir/recovery-edge-cases`, **File:** `tests/recovery_edge_cases.rs` (NEW, 20 tests, 1064 lines)

**Audit scope:** checkpoint_recovery_tests.rs (42), hybrid_log_tests.rs (31), concurrent_stress.rs (13), compaction_integration.rs (32), memory_pressure_tests.rs (25).

**Gaps identified and filled:**
- Crash simulation: truncated/zero-length metadata files (checkpoint.json, index.recovery.json, log.recovery.json), truncated index binary
- Corruption patterns: header corruption (vs existing body-only CRC), mismatched metadata table size
- Session edge cases: 500-entry excluded set, non-sequential ID ordering, excluded serials beyond serial_number
- Recovery invariants: step ordering (LoadIndex → RecoverLog → RecoverSessions), validation with selectively missing sub-files
- Lifecycle: checkpoint → recover → write → checkpoint again, concurrent compaction + checkpoint, concurrent checkpoint mutual exclusion
- Scale: 20-checkpoint discovery, 16K-bucket index recovery, deep directory paths

**Results:** 20 tests, all pass. Full precheckin (1942 tests) green.

---

### 2026-03-09T1817Z: Wave 2 Completion — Team Context Update

**Wave 2 summary (all 5 agents merged to `squad`, 2,067 tests pass in 58s):**

- **Boromir (you):** 20 recovery edge case tests in `tests/recovery_edge_cases.rs` (1,064 LOC). Commits `a19ef033`, `b589d756`, `95db91fc`. Filed `boromir-log-prefix-recovery.md` (SyncFileDevice `"log."` prefix decision). Note: Éowyn's stray commit `80dca96c` was dropped by coordinator before merge.
- **Elrond:** 29 tokio integration tests in `rust/crates/faster-tokio/tests/integration.rs`.
- **Saruman:** 29 I/O error injection tests with `FaultInjectingDevice` in `rust/crates/faster-core/tests/io_error_injection.rs` (1,307 LOC).
- **Legolas:** Performance regression detection scripts (`bench-compare.sh`, `bench-baseline.sh`) + `benchmarking.md`.
- **Éowyn:** DST expanded 5→108 scenario templates (14 categories, 324 test cases in ~30s).

**Your `"log."` prefix decision is now in `decisions.md` and propagated to Éowyn's history** — she needs to know this constraint affects DST scenarios using `SyncFileDevice`.
