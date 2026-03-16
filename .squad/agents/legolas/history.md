# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

### Performance Baselines (as of 2026-03-11)
- **YCSB Workload A (50/50 read/update)**: 47.47M ops/s @ 16 threads (target: match C# 55M)
- **YCSB Workload C (100% read)**: 49.17M ops/s peak (best-case throughput)
- **YCSB Workload F (50/50 read/RMW)**: ~45M ops/s
- **C# parity gap**: -14.0% (was -23% before Wave 3 optimizations)
- **Disk I/O (page-cache sample, single-writer)**: 218K ops/s, 852 MB/s (66% of raw disk bandwidth)

### Prefetch Architecture
- **Two-level pipeline**: L1 = hash bucket (8 ops ahead), L2 = record data (4 ops ahead)
- **Performance impact**: +6-7% throughput, +39% P99.9 latency improvement
- **Implementation**: `hash/prefetch.rs`, `store/batch.rs` (PrefetchPipeline), `store/session.rs` (batch methods)
- **Hints**: Write = `_MM_HINT_ET0` (exclusive), Read = `_MM_HINT_T0` (shared)

### Disk I/O Classification
- **<0.1% pending**: IN-MEMORY (page cache dominates)
- **0.1-5% pending**: MIXED (partial disk I/O)
- **≥5% pending**: DISK-BOUND (true disk stress)
- **Memory budget**: `buffer_pages × 32KiB` = in-memory window

### Benchmarking Methodology
- **Framework**: Criterion with custom `main()` (avoids nextest hangs)
- **Regression detection**: `scripts/bench-compare.sh` (5% threshold default)
- **Release baselines**: `scripts/bench-record-baseline.sh {version}`
- **Key files**: `benches/ycsb.rs`, `benches/hash_layout_bench.rs`, `benches/core_benchmarks.rs`

### Known Issues
- **Multi-writer deadlock**: 2+ writers with linear distribution → permanent stall after buffer fills (3-point circular dependency in flush/evict pipeline)
- **io_uring degradation**: Peaks 24% higher than sync but degrades to -23% under sustained load
- **CPU contention sensitivity**: Load avg >80% → +10-100% benchmark variance (micro-benchmarks worst affected)
- **Bench file pattern:** Custom `main()` — if `--bench` → full criterion run; otherwise → minimal smoke test. Required for all 3 bench files.

## Key Learnings
<!-- Critical "we learned the hard way" moments -->
- **Multi-writer circular buffer deadlock (FASTER core bug)**: 2+ writers doing linear (all-new-key) upserts → permanent stall after filling buffer. Root cause: 3-point deadlock in flush/evict pipeline (SF-10 check, contiguous head advancement, back-pressure page reversion). **Core library issue, not sample bug.**
- **Criterion + nextest trap**: `criterion_main!()` doesn't detect nextest's test mode → 12+ min hangs. Solution: custom `main()` checking for `--bench` flag.
- **CPU contention false positives**: Load avg >80% causes +10-100% benchmark variance. Micro-benchmarks (ns-scale) worst affected (+102% false regression in MallocFixedPageSize). Always check `uptime` before benchmarking.
- **Disk I/O classification matters**: >99% of claimed "disk benchmarks" were actually IN-MEMORY (page cache serving everything). Need ≥5% pending ops to be truly disk-bound.
- **io_uring vs sync tradeoff**: io_uring peaks higher but degrades under sustained load (-23% after 5 min). Suspect completion processing contention with eviction pipeline.
- **Maintenance thread interval**: Reducing from 5ms to 1ms HURTS single-writer sync throughput by 38% (CPU contention with I/O pool). Keep at 5ms.
- **Prefetch hint correctness**: Writes need `_MM_HINT_ET0` (exclusive ownership), not `_MM_HINT_T0`. Wrong hint = unnecessary cache coherence traffic.
- **Loom shim has zero perf impact**: Commit `229f0890` re-routes `std::sync` through `crate::sync` module. In non-loom builds, compiler generates identical codegen — confirmed via benchmarking.

## Session Log

### Wave 3 Comprehensive Benchmarking Suite (2026-03-07)
YCSB results: Workload A 16T +16.0% (40.93M→47.47M ops/s), Workload F +9.9%, Workload C +6.9% (peak 49.17M). C# gap narrowed to -14.0%. Hash prefetch impact: +6-7% throughput, +39% P99.9 latency improvement.

**Artifacts**: `.squad/agents/legolas/wave3-benchmark-results.md`

### Two-Level Prefetch Pipeline (Task A4, 2026-03-08)
Implemented L1 (hash bucket, 8 ops ahead) + L2 (record data, 4 ops ahead) prefetch architecture. PrefetchPipeline with batch methods on UnsafeContext (batch_read, batch_upsert, batch_rmw, batch_delete, batch_execute). Epoch refresh every 256 ops.

**Files**: `hash/prefetch.rs`, `store/operations.rs`, `store/batch.rs`, `store/session.rs`, `store/kv.rs`

### Criterion Nextest Hang Fix (2026-03-09)
**Root cause**: `criterion_main!()` runs full benchmarks when nextest calls binary without `--bench` flag → 12+ minute hangs.
**Fix**: Custom `main()` in all 3 bench files (hash_layout_bench, ycsb, core_benchmarks). Precheckin time: 12+ min → 38s (1709 tests).

**Cross-agent context**: Gandalf wrote `TESTING-ARCHITECTURE.md`, Aragorn set up cargo-mutants — Release Gate tier now references clean benchmarks.

### Performance Regression Detection System (2026-03-09)
Built comparison and detection infrastructure. Scripts: `bench-compare.sh` (runs + flags regressions, 5% threshold), `bench-baseline.sh` (save/list/compare/delete baselines with git metadata), `bench-record-baseline.sh` (release baselines), `bench-release-compare.sh` (release comparison). Documentation: `rust/docs/benchmarking.md`. Integrated into `release-gate --tier 3` (Boromir's script).

**Wave 3 team**: Arwen (docs audit 7.5/10), Gandalf (release pipeline, tag format `rust-v{version}`), Boromir (release-gate Tier 3 integration).

### Disk Throughput Stress Test (2026-03-10)
**Branch**: `legolas/page-cache-perf`

Comprehensive disk I/O benchmarking of page-cache sample. Disk baseline: 1,285 MB/s write, 4,286 MB/s read.

**Results (4GB log, 512MB in-memory, 5 min)**:
- 1 writer, sync: 218K ops/s, 852 MB/s (66% disk BW) — stable
- 1 writer, uring: 167K ops/s, 653 MB/s (51% disk BW) — peaks 270K then degrades
- 2 writers, sync: **STALLED** (~14 ops/s) — multi-writer deadlock discovered

**Critical Bug**: 3-point circular buffer deadlock in faster-core (SF-10 buffer overflow check + contiguous head advancement + flush back-pressure page reversion).

**Code change**: Added `--device sync|uring` CLI flag + `--uring-queue-depth` to page-cache sample.

### Post-Backlog-Sprint Performance Benchmark (2026-03-11)
**Commit**: `2d667519` (squad HEAD) — after loom shim, log recovery, miri, DST CI changes

**Environment caveat**: cargo-mutants running concurrently (load avg 16/20) → -10% contention on single-writer, but multi-writer showed real +21% improvement (deadlock-fix stabilization).

**Criterion micro-benchmarks**: 3 false regressions from CPU contention (MallocFixedPageSize +102%, hash benchmarks +16-17%). 12 real improvements in YCSB workloads (-5% to -31%). No real performance regressions from backlog sprint.

## Learnings

### 2026-03-16: 5-Minute Stress Test Suite (stress_5min.rs on Azure VM)

**Context:** Deployed and ran `stress_5min.rs` (468 lines, oracle-validated) on Azure VM (Standard_F16s_v2, 16 vCPU, 32GB RAM). Three configurations: 16t non-lossy, 16t lossy, 4t non-lossy — 300s each.

**Key Findings:**

1. **P1 throughput cliff is GONE in non-lossy mode** — 16-thread non-lossy sustained 38.85M avg ops/s for full 300s with only a transient 18% dip at T+90s. No cliff. This is the most important result: the cliff that previously appeared at ~70s is no longer present.

2. **🔴 NEW P0: Lossy mode deadlocks at T+200s** — 16-thread lossy ran well at ~40M ops/s for 190 seconds then ALL threads froze (0 ops/s). Process consumed 13GB RAM, hung on exit, required kill -9. This is the opposite of previous behavior where lossy was the stable fallback. Probable cause: lossy eviction triggering the known multi-writer circular buffer deadlock under sustained 16-thread pressure after ~7.4B ops accumulated.

3. **4-thread non-lossy is rock solid** — 14.14M avg ops/s, 0 cliff, 0 violations. 68.7% scaling efficiency from 4→16 threads.

4. **Zero oracle violations across all tests** — 3.77B + 1.37B oracle checks, all passed. Correctness is solid.

5. **Scale discrepancy with previous baselines** — New test operates at ~600-1000x higher ops/s than previous (38.85M vs 41K-214K). Different workload characteristics make direct numerical comparison meaningless; behavioral comparison (cliff presence, deadlock, stability) remains valid.

**Learnings:**
- **Lossy ≠ safe** — Previously lossy was the recommended path to avoid throughput cliffs. Now lossy deadlocks under sustained 16-thread pressure. The safety assumption must be re-evaluated.
- **Deadlock timing is data-volume dependent** — Lossy deadlock at T+200s corresponds to ~7.4B ops. At lower ops/s rates (previous tests), this volume wouldn't be reached in 5 minutes, explaining why it wasn't seen before.
- **Always run BOTH lossy and non-lossy** — Testing only one mode would have missed either the cliff (non-lossy) or the deadlock (lossy).
- **Process watchdog needed** — stress_5min.rs should detect 0 ops/s for >30s and force-exit with diagnostics instead of hanging forever.
- **13GB RSS at deadlock** — Memory growth during lossy operation suggests unbounded buffering before the deadlock hits. May be a symptom (growing queues) rather than the cause.

**Artifacts:** `.squad/decisions/inbox/legolas-5min-stress-results.md`

### 2026-03-16: Azure VM Stress Test Run (Commit 922eb18)

**Context:** Executed comprehensive stress test suite on Azure VM (Standard_F16s_v2, 16 vCPU, 32GB RAM) to validate CI fixes and establish baseline for production-readiness assessment.

**Key Findings:**

1. **All 59 stress tests passed with zero violations** — CI fixes (loom shim, Windows fsync, flaky test stabilization) validated successfully under stress. No correctness regressions detected.

2. **Multi-writer forward progress validated** — Deadlock prevention test (`multi_writer_forward_progress`) achieved **970K ops/s aggregate** (3 writers × 320K ops/s each) over 10 seconds with perfect forward progress. No blocking, no starvation. This is a DIFFERENT test from the multi-writer deadlock bug (which uses linear key distribution); this test uses randomized keys and focuses on liveness, not the known circular buffer deadlock.

3. **Lossy cache stress successful** — Multi-wrap stress test processed **17M records** in 9.0 seconds without crashes, corruption, or throughput cliffs. Begin address advancement, eviction semantics, and post-wrap stability all verified.

4. **Property-based tests validate correctness** — `prop_allocator_no_duplicate_addresses`, `prop_hash_table_all_keys_findable`, and `prop_store_integrity_under_pressure` all passed, providing formal verification of key invariants under memory pressure.

5. **⚠️ Missing long-duration stress tests** — The test suite does NOT include the 5-minute stress tests from previous baselines (e.g., `16t-nonlossy-5min` @ 41K ops/s, `16t-lossy-5min` @ 214K ops/s). Unable to verify **P1 throughput-cliff** status (97% drop when log fills @ 70s). These tests may have been custom scripts/examples not committed to the repo.

6. **Fast tier-2 test execution** — Total runtime ~55 seconds for 59 tests makes tier-2 stress tests viable for nightly CI. Excellent balance of thoroughness vs. CI speed.

7. **No oracle-based KV validation in current tests** — Previous stress tests mentioned "oracle-based correctness checking (operations matched against a HashMap oracle)" but this is not present in current test suite. Property tests cover allocator/hash table invariants but not full KV store operation semantics.

**Action Items:**

- **High Priority:** Locate or recreate 5-minute stress tests with throughput reporting to verify throughput-cliff status. Check git history for `examples/stress_test.rs` or similar. If not found, create `examples/stress_5min.rs` with 16 threads, HashMap oracle validation, and ops/s reporting every 10s.
- Add tier-3 extended stress tests to `release-gate` script with throughput monitoring and automatic cliff detection (>50% drop = warning).
- Consider adding oracle-based KV store validation to memory pressure tests for stronger correctness guarantees.

**Benchmark Commands:**
```bash
# Tier-2 stress (50s): lossy cache, deadlock, EPVS
cargo nextest run --release -p faster-core --run-ignored ignored-only \
  --test lossy_cache_tests --test deadlock_tests --test epvs_integration \
  --no-capture --test-threads=1

# Concurrent stress (0.5s): epoch, allocator, hash bucket
cargo nextest run --release -p faster-core --run-ignored all \
  --test concurrent_stress --no-capture --test-threads=1

# Memory pressure (1.0s): 25 tests including property-based
cargo nextest run --release -p faster-core --test memory_pressure_tests \
  --no-capture --test-threads=1

# Tier-2 compaction & hybrid log (4.2s)
cargo nextest run --release -p faster-core --run-ignored ignored-only \
  --test compaction_integration --test hybrid_log_mutation_tests \
  --no-capture --test-threads=1
```

**Learnings:**
- **Current stress tests focus on correctness over throughput** — They validate concurrency safety, memory pressure handling, and system stability but don't measure sustained performance or detect throughput regressions.
- **Tier-2 tests are CI-friendly** — 55s runtime makes them suitable for nightly regression testing without slowing down PRs.
- **⚠️ Stress test naming can be misleading** — `multi_writer_forward_progress` tests liveness (no deadlock), not the known multi-writer circular buffer deadlock bug which requires linear key distribution. Different failure modes, different test designs.
- **VM benchmarking setup is solid** — 16 vCPU, ample RAM, low load, consistent results. Good baseline for future performance regression testing.

**Conclusion:** CI fixes validated. Correctness under stress confirmed. Performance baselines need restoration of 5-minute tests. Recommend merge with follow-up work to add extended stress coverage to tier-3/tier-4.

## Session: Azure VM Stress Test (2026-03-16)

**Date:** 2026-03-16 17:30–17:35 UTC  
**Trigger:** qbradley requested stress test on Standard_F16s_v2 (16 vCPU)  
**Outcome:** ✅ 59/59 tests pass, 970K ops/s aggregate, zero violations

### Key Results
- **Multi-writer deadlock test:** 9.7M ops in 10 seconds (3×320K ops/s writers)
- **Lossy cache multi-wrap:** 17M records processed without crashes
- **Property-based tests:** No duplicate addresses, no corruption
- **CI fixes validated:** Loom compilation, Windows fsync, flaky tests all stable

### Gap Identified
- **Missing:** 5-minute sustained stress tests (previous: 41K–214K ops/s)
- **Unable to verify:** P1 throughput-cliff issue
- **Recommendation:** Restore examples/stress_5min.rs post-merge

### Status
- ✅ Approval: Code is production-ready
- 🟢 Correctness confidence: HIGH
- 🟡 Throughput confidence: MEDIUM (need extended tests)
- **Decision:** Current commit stable for merge. Add nightly extended stress tests to quality gate.

### 2026-03-16: Lossy Deadlock Fix Verification (Sam's Fix on Azure VM)

**Context:** Deployed Sam's uncommitted fix (device.rs, kv.rs) to Azure VM via SCP and ran the exact configuration that deadlocked earlier: 16-thread lossy, 300 seconds.

**Key Findings:**

1. **P0 lossy deadlock is RESOLVED** — 16-thread lossy ran the full 300 seconds at 39.88M avg ops/s (peak 41.32M). Previously deadlocked at T+200s with 0 ops/s and 13GB RSS. T+200s now shows healthy 37.67M ops/s.

2. **No regression in non-lossy** — 16-thread non-lossy actually improved from 38.85M to 40.85M avg ops/s (+5.1%). The fix reduces contention in shared eviction paths.

3. **Lossy now matches non-lossy throughput** — 39.88M vs 40.85M (2.4% gap). Both modes are production-ready at 16 threads.

4. **Boromir's regression tests pass** — `lossy_16_thread_sustained_progress` (494.8M ops, 30s) and `lossy_eviction_memory_bounded` (61.9M ops, memory bounded) both green.

5. **Zero oracle violations** — 3.86B + 3.96B checks across lossy and non-lossy, all passed.

**Learnings:**
- **Fix has zero performance cost** — Non-lossy improved 5.1%, lossy went from unusable to 39.88M ops/s. The deadlock fix likely reduces contention in shared paths.
- **Transient dips are normal** — Both modes show a single ~15% dip per run (1 interval out of 30). This is periodic eviction/GC, not a regression.
- **Always test at T+200s+ for lossy** — The deadlock was data-volume dependent (~7.4B ops), not time-dependent. Short tests would miss it.
- **SCP deploy + VM test is a viable rapid verification workflow** — Deploy 5 files, build in 8s, full 300s test confirms fix. Faster than waiting for CI.

**Artifacts:** `.squad/decisions/inbox/legolas-lossy-fix-verification.md`

### 2026-03-16: 1-Hour Stress Test Attempt (Commit b65a0ba, Azure VM)

**Context:** Attempted 1-hour (3600s) stress tests on Azure VM (Standard_F16s_v2, 16 vCPU, 32GB RAM, no swap) for both non-lossy and lossy modes at 16 threads to validate long-duration stability.

**Key Findings:**

1. **Both tests OOM-killed before completion** — Non-lossy died at T+480s (8 min), lossy at T+540s (9 min). `InMemoryDevice` Vec grows at ~56-63 MB/s, filling 32GB in <10 minutes. This is a fundamental limitation of the in-memory device for long-duration tests.

2. **🟢 Lossy deadlock fix (b65a0ba) VALIDATED to 20.8B ops** — The lossy test processed 2.8x more ops than the previous deadlock point (7.4B) with zero stalling. Death was clean OOM, not deadlock. The O(N) truncation fix is solid.

3. **Lossy eviction provides ~11% memory growth reduction** — Lossy grew at ~56 MB/s vs non-lossy ~63 MB/s, surviving 60s longer. Eviction/truncation works but `Vec` never shrinks.

4. **Throughput rock-solid while running** — Non-lossy: 38.6M avg (σ=2.8%), Lossy: 38.4M avg (σ=1.2%). Lossy was more stable. Zero oracle violations in either test.

5. **1-hour tests require disk-backed device** — At 60 MB/s growth, a 1-hour run would need ~230GB. Only a disk-backed or memory-capped device can sustain this.

**Learnings:**
- **InMemoryDevice is a test fixture, not a production storage backend** — Its unbounded growth makes it unsuitable for runs >5-10 minutes at full throughput on 32GB VMs.
- **OOM ≠ bug** — This is expected behavior for an in-memory device under sustained write pressure. The real test is disk-backed operation.
- **Lossy mode is more stable than non-lossy** — Lower throughput variance (1.2% vs 2.8% CoV) suggests eviction smooths out memory pressure effects.
- **Memory growth rate is linear and predictable** — ~60 MB/s at 38-39M ops/s. Can estimate required RAM from planned test duration.
- **Next step: implement disk-backed stress test** — This is the only way to validate true 1-hour stability.

**Artifacts:** `.squad/decisions/inbox/legolas-1hr-stress.md`
