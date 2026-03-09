# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **Benchmarking stack:** YCSB workloads (A/C/F), Criterion framework with custom `main()` to avoid nextest hangs. CSV output for tracking. C# comparison baseline.
- **Prefetch architecture:** Two-level pipeline — L1 = `HashIndex::prefetch(key_hash)` (hash bucket), L2 = `prefetch_record(allocator, addr, is_write)` (record data). L2 issued after find()/find_or_create() returns. Writes: `_MM_HINT_ET0`, Reads: `_MM_HINT_T0`.
- **PrefetchPipeline:** Sliding window — L1=8 ops ahead, L2=4 ops ahead. Batch methods on UnsafeContext: `batch_read`, `batch_upsert`, `batch_rmw`, `batch_delete`, `batch_execute`. Epoch refresh every 256 ops.
- **Key files:** `hash/prefetch.rs` (prefetch intrinsics), `store/operations.rs` (prefetch_record), `store/batch.rs` (PrefetchPipeline, BatchResult, BatchOp), `store/session.rs` (batch methods), `store/kv.rs` (convenience methods), `benches/ycsb.rs`, `benches/hash_layout_bench.rs`, `benches/core_benchmarks.rs`.
- **Disk I/O classification:** <0.1% pending = IN-MEMORY, 0.1-5% = MIXED, >=5% = DISK-BOUND. `--force-disk` auto-tunes buffer_pages. Memory budget: `buffer_pages * 32KiB` in-memory window.
- **Performance baselines:** Workload A 16T: 47.47M ops/s, Workload C peak: 49.17M, C# gap: -14.0%. Hash prefetch: +6-7% throughput, +39% P99.9 improvement.
- **Bench file pattern:** Custom `main()` — if `--bench` → full criterion run; otherwise → minimal smoke test. Required for all 3 bench files.

## Learnings
<!-- Append new learnings -->
- **Shared working directory hazard**: Multiple agents modify the same files. `git checkout -- <file>` wipes ALL uncommitted changes. Always stage + commit immediately.
- **Atomic commit workflow**: edit → stage → commit in rapid succession. Never leave changes unstaged.
- **Unicode in Python**: Box-drawing characters and em-dashes match fine with proper Python Unicode escapes.
- **Concurrent agent /tmp file collision**: Use unique temp file names, not shared `/tmp/commit-msg.txt`.
- **Branch switching by concurrent agents**: Always verify branch immediately before commit.
- **Memory budget for disk benchmarks**: `buffer_pages * 32KiB` is the in-memory window, but OS page cache also matters. True disk-bound requires dataset >> RAM, O_DIRECT, or cache flushing.
- **Criterion + nextest `harness = false` trap**: `criterion_main!()` does NOT detect nextest's test mode. Always use custom `main()` that checks for `--bench`.
- **Multi-agent workspace safety**: `checkin` script does `git add -A` — dangerous in multi-agent workspace. Use selective `git add`.

## Session Log

### Wave 3 Comprehensive Benchmarking Suite (2026-03-07)
**Results:** Workload A 16T +16.0% (40.93M→47.47M), Workload F +9.9%, Workload C +6.9% (peak 49.17M), C# gap narrowed to -14.0%.

Hash prefetch impact: +6-7% throughput, +39% P99.9 latency improvement (below predicted 20-40%).

**Artifacts:** `.squad/agents/legolas/wave3-benchmark-results.md`, CSV at `/tmp/wave3-ycsb-results.csv`
**Optimization Priorities:** (1) Two-level prefetching, (2) Hash table layout, (3) Per-thread log tail, (4) True disk-bound benchmarks (100M+ keys)

---

### Disk I/O Benchmark Methodology Fix (A2+A11) (2026-03-07)
**Branch:** `legolas/disk-io-bench-fix`

Changes: Pending-rate validation gate, `--force-disk` mode (auto-tunes buffer_pages), pending rate in all output, default buffer_pages reduced 128-256→16-32, statistical rigor with 5 iterations (mean/stddev/95% CI).

**I/O classification:** <0.1% = IN-MEMORY, 0.1-5% = MIXED, >=5% = DISK-BOUND.

---

### Two-Level Prefetch Pipeline (Task A4) (2026-03-08)
**Architecture:** L1 = `HashIndex::prefetch(key_hash)` (hash bucket), L2 = `prefetch_record(allocator, addr, is_write)` (record data). L2 issued after find()/find_or_create() returns. Writes: `prefetch_write()` (_MM_HINT_ET0), Reads: `prefetch_read()` (_MM_HINT_T0).

**PrefetchPipeline:** Sliding window L1=8 ops ahead, L2=4 ops ahead. Batch methods: batch_read, batch_upsert, batch_rmw, batch_delete, batch_execute on UnsafeContext. Epoch refresh every 256 ops.

**Files:** `hash/prefetch.rs`, `store/operations.rs`, `store/batch.rs` (NEW: PrefetchPipeline, BatchResult, BatchOp), `store/session.rs` (6 batch methods), `store/kv.rs` (4 convenience methods).

---

### Fix hash_layout_bench 12+ Minute Nextest Hang (2026-03-09)
**Root cause:** `criterion_main!()` runs full benchmarks when no `--bench` flag received.

**Fix:** Replaced `criterion_main!(benches)` with custom `main()` in all 3 bench files: if `--bench` → full criterion run; otherwise → minimal smoke test.

**Files:** `benches/hash_layout_bench.rs`, `benches/ycsb.rs`, `benches/core_benchmarks.rs`.
**Impact:** Precheckin 12+ min hang → 38 seconds (1709 tests).

---

### Fix hash_layout_bench — Remove Dead OA Prototype Code (2026-03-09)
**Branch:** `legolas/fix-hash-bench` → merged to `squad`

**Root cause:** 206 lines of dead Open Addressing prototype code never removed from `hash_layout_bench.rs`. The OA prototype ran with oversized datasets through Criterion's full harness even in smoke mode — causing >1 hour runtime in debug.

**Changes:**
- Removed 206 lines of dead OA prototype code
- Reduced dataset sizes to match realistic benchmark scope
- Added Criterion tuning (`measurement_time`, `warm_up_time`, `sample_size`)
- Re-enabled `benches()` call (had been commented out by user)

**Outcome:** Smoke test 0.15s (was >1 hour in debug). Bench re-enabled and healthy.

**Cross-agent context:**
- Gandalf wrote `rust/TESTING-ARCHITECTURE.md` this same session — the Release Gate tier now references a clean, working benchmark file.
- Aragorn set up cargo-mutants with `rust/docs/mutation-testing.md` — mutation testing gap in testing architecture is now resolved.
