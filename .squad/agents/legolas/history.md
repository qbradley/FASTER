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
- **Criterion baseline structure**: Criterion stores baselines *inside* each benchmark directory (e.g., `target/criterion/<bench_name>/<baseline_name>/estimates.json`), not in a flat directory. Scripts must traverse all benchmark dirs to check for baseline existence.
- **Criterion comparison output parsing**: Criterion prints `change: [lo% mid% hi%]` on a line following the benchmark time report. The next line indicates `Performance has regressed/improved` or `Change within noise threshold`. Parse these three lines as a unit.
- **`--load-baseline A --baseline B`**: To compare two named baselines without re-running benchmarks, use criterion's `--load-baseline` + `--baseline` flags together.
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

---

### Performance Regression Detection System (2026-03-09)
**Branch:** `legolas/perf-regression-detection`

Built the missing comparison and detection infrastructure for benchmark baselines.

**Artifacts:**
- `rust/scripts/bench-compare.sh` — Runs benchmarks against saved baseline, parses criterion comparison output, flags regressions exceeding configurable threshold (default 5%), outputs human-readable table or JSON. Exit code 1 on threshold violation.
- `rust/scripts/bench-baseline.sh` — Saves/lists/compares/deletes named baselines with metadata (git commit, timestamp, machine info). Supports `save`, `list`, `compare`, `info`, `delete` commands.
- `rust/docs/benchmarking.md` — Full benchmarking guide: suite descriptions, baseline management, regression detection workflow, criterion methodology, troubleshooting.
- Updated `rust/TESTING-ARCHITECTURE.md` — Added scripts to table, marked regression automation gap as resolved.

**Key design decisions:**
- 5% default threshold (configurable with `--threshold`) — balances noise tolerance with regression sensitivity.
- Metadata stored in `target/criterion/.baselines/*.meta.json` — not checked in, lives with benchmark data.
- Scripts designed for VM execution only — match existing project convention.
- `bench-compare.sh` saves results as `current` baseline, compares against named baseline (default `main`).
- JSON output mode (`--json`) for CI integration.

---

### Performance Regression Detection System — Wave 2 (2026-03-09T1817Z)
**Branch:** `legolas/perf-regression-detection` — merged to `squad`

Built the missing comparison and detection infrastructure for benchmark baselines.

**Artifacts:**
- `rust/scripts/bench-compare.sh` — Runs benchmarks against saved baseline, parses criterion comparison output, flags regressions exceeding configurable threshold (default 5%), outputs human-readable table or JSON. Exit code 1 on threshold violation.
- `rust/scripts/bench-baseline.sh` — Saves/lists/compares/deletes named baselines with metadata (git commit, timestamp, machine info). Supports `save`, `list`, `compare`, `info`, `delete` commands.
- `rust/docs/benchmarking.md` — Full benchmarking guide: suite descriptions, baseline management, regression detection workflow, criterion methodology, troubleshooting.
- Updated `rust/TESTING-ARCHITECTURE.md` — Added scripts to table, marked regression automation gap as resolved.

**Wave 2 team summary (2,067 tests pass in 58s):**
- Elrond: 29 tokio integration tests
- Saruman: 29 I/O error injection tests (FaultInjectingDevice)
- Éowyn: DST 5→108 scenarios (324 test cases in ~30s)
- Boromir: 20 recovery edge case tests; filed `"log."` prefix coupling decision

---

### Wave 3 — Benchmark Baseline Infrastructure (2026-03-09T1846Z)
**Branch:** `legolas/bench-baseline-infra` — merged to `squad`
**Agent ID:** agent-149

Built the release benchmarking layer on top of Wave 2's comparison scripts.

**Artifacts:**
- `rust/scripts/bench-record-baseline.sh` — record named baselines with git metadata (commit, version, timestamp, machine info). Stored in `rust/baselines/`. Usage: `bench-record-baseline.sh v0.1.0`
- `rust/scripts/bench-release-compare.sh` — compare current benchmark run against a stored named baseline. Flags regressions >5% (configurable). Exit code 1 on threshold violation. Generates markdown report.
- `rust/scripts/bench-ci-smoke.sh` — fast CI smoke test: verifies benchmark binaries compile and run without errors (not full criterion runs).
- `rust/baselines/README.md` — baseline storage format, usage, and release workflow documentation.
- `rust/baselines/.gitkeep` — tracked empty directory for baseline storage (actual baseline data not checked in).
- `rust/docs/benchmarking.md` — extended with release benchmarking section covering the new scripts.

**Release workflow integration:**
1. Before tagging: `bench-record-baseline.sh {version}` — saves performance baseline
2. After release: compare next dev cycle with `bench-release-compare.sh {version}` — gates on >5% regression
3. Tier 3 of `release-gate` (Boromir's script) calls `bench-release-compare.sh` automatically

**Wave 3 team context:**
- **Arwen (agent-147):** Documentation audit rated project 7.5/10. P0 gaps include missing sample crate READMEs.
- **Gandalf (agent-148):** Release pipeline live — tag format `rust-v{version}`, 5 publishable crates. semver-checks added to CI.
- **Boromir (agent-150):** `rust/scripts/release-gate` integrates your `bench-release-compare.sh` in Tier 3. Run `release-gate --tier 3` for deep validation including bench regression.
