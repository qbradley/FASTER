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
