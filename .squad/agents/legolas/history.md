# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

---

## 2026-03-05T18:33: Gandalf Rust FASTER Architecture Finalized

**What:** Gandalf completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Gandalf-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/gandalf/rust-faster-architecture.md`

**Sections:** Vision, Crates, Data Structures, Concurrency, Storage, Operations, Checkpoint, API, FFI, Async Integration, Testing, Phases, 12 Key Decisions, Risk Register (25 risks, 5 critical)

**12 Core Architectural Decisions:**
1. No async/await in core (user directive) — callback/completion model
2. Custom epoch (not crossbeam-epoch) — FASTER-specific semantics
3. Inline variable-length records (C++ model)
4. Rust atomic patterns for hash index — lock-free
5. 32 MB default page size — 512-byte sector alignment
6. Completion-based Device trait — runtime-agnostic
7. Opaque FFI handles (~15 function API surface)
8. Own checkpoint format — forward-compatible binary
9. Result<Status, Error> taxonomy with bitflag Status
10. Thread-affine sessions (!Send compile-time) — mono-threaded safety
11. Unified Key/Value traits for fixed+variable-length
12. Epoch-coordinated grow protocol (doubles, no shrink)

**What This Means For You:**
- **All:** You are now operating under this architecture. Decisions binding for implementation phases.
- **Elrond:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Aragorn:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Sam:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Éowyn:** Simulation framework must model callback completion ordering for testing.
- **Galadriel:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Frodo:** Phase roadmap and implementation sequence encoded in Part 3.
- **Saruman:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Éowyn lead), unsafe audit planning (Galadriel lead), Phase 1 sprint (Frodo lead), async adapter spike (Elrond lead).

---

## 2026-03-06: AI-4 Performance Analysis — Epoch Amortization Deep Dive

**What:** Comprehensive profiling of FASTER Rust to investigate the 10M ops/sec target gap. Built custom benchmark (`perf_analysis.rs`) and collected detailed component cost breakdown.

**Key Findings:**
- Per-op epoch protect/unprotect costs **~97–128 ns** (33–44% of upsert cost)
- Root cause: `compute_safe_epoch()` scans all 256 MAX_THREADS entries with Acquire loads on every unprotect (16KB cache scan)
- `UnsafeContext` (epoch amortization) already exists and provides **1.8× upsert speedup, 3.5× read speedup**
- L1 dcache miss rate: 43% (memory-bound workload, IPC only 0.73)
- Hash index cache misses are the dominant remaining bottleneck after epoch amortization (~80ns per lookup)

**Performance Numbers (local 20-CPU machine):**
- Single-thread amortized read: **12.7M ops/sec** (exceeds 10M target)
- Single-thread amortized upsert: **6.1–6.6M ops/sec**
- 4-thread amortized upsert: **10.0M total ops/sec** (achieves 10M target)
- 10M single-thread upsert requires hash prefetching (not achievable with current hash index)

**Artifacts:**
- Custom benchmark: `rust/crates/faster-core/benches/perf_analysis.rs`
- Analysis report: `.squad/decisions/inbox/legolas-perf-analysis-ai4.md`

**Architecture Insights:**
- `UnsafeContext` in `store/session.rs:593` is the fast-path API (matches C# `UnsafeContext`)
- `compute_safe_epoch()` in `epoch/table.rs:320` is the hot function in the epoch path
- Refresh sweet spot: every 64–128 operations
- Multi-thread scaling capped at ~54% efficiency due to hash index + log tail contention

**Optimization Priority Order:**
1. Promote `UnsafeContext` as primary API (+80–250%, Low effort)
2. Bound epoch scan to registered threads only (+5–10%, Low effort)
3. Hash index prefetching (+20–40%, Medium effort)
4. Per-thread log tail allocation (+5–10%, Medium effort)
5. Hash table layout optimization (+10–15%, High effort)


---

## 2026-03-06: Cross-Implementation Benchmark Suite Delivered

**What:** Built `cross-impl-bench` — a standalone YCSB benchmark binary in `rust/crates/samples/cross-impl-bench/` with clap CLI, 4 workloads (A/B/C/F), thread scaling (1-16T), uniform+zipfian distributions, 3 value sizes (8B/100B/1KB), latency histograms (P50/P99/P99.9), and CSV+table output.

**Performance Numbers (i9-10900K, 20 CPUs, 1M keys, 8B KV, UnsafeContext):**
- Single-thread: 3.4-3.5M ops/sec across all workloads (P50=300ns, P99=700ns)
- 4 threads: ~15.4M ops/sec (4.5× scaling, >100% efficiency)
- 8 threads: ~31M ops/sec (9.2× scaling)
- 16 threads: 56-60M ops/sec (16.5-17× scaling, super-linear due to cache effects)
- All workloads near-identical at in-memory scale; read-only (C) marginally fastest at 59.77M/16T

**Key Observations:**
- Super-linear scaling (>100% efficiency) from 1→8T due to aggregate L1/L2 cache coverage
- Tail latency well-controlled: P99.9 < 1.3μs under all conditions
- Workload type has minimal impact when dataset fits in memory (1M × 16B = 16MB << L3)
- UnsafeContext essential — epoch amortization eliminates ~100ns/op overhead

**C#/C++ Comparison Framework:**
- C# benchmark: `cs/benchmark/` with `FASTER.benchmark` binary, supports identical workloads via `--rumd` flags
- C++ benchmark: `cc/benchmark-dir/` with YCSB workloads 0-3, requires pre-generated data files
- Fair comparison requires: same hardware, same key count, same duration, same distribution, same refresh interval (64)

**Artifacts:**
- Benchmark crate: `rust/crates/samples/cross-impl-bench/`
- Performance report: `rust/PERFORMANCE.md`
- CSV results: generated via `--csv` flag

**Optimization Priority (unchanged from prior analysis):**
1. Hash index prefetching (+20-40% single-thread)
2. Epoch scan optimization (+5-10%)
3. Per-thread log partitioning (+5-10% multi-thread)

---

## 2026-03-07: Cross-Implementation YCSB Comparison Complete

**What:** Ran Rust vs C# vs C++ FASTER benchmarks on identical hardware (i9-10900K 20T, Azure Linux) with matched parameters (2.5M keys, 8B KV, uniform, 10s runs, in-memory).

**Results Summary:**
- **C# leads most workloads** — 51.65M ops/sec on 100% reads at 16T (highest absolute throughput)
- **C++ leads single-thread reads** — 4.07M ops/sec (77% faster than Rust's 2.30M)
- **Rust wins write-heavy 16T** — 39.21M on Workload A (50/50), beating C# 31.02M and C++ 38.91M
- **Rust has most consistent scaling** — 102% efficiency on Workload A at 16T; C# drops to 75%

**Key Performance Gap:** Rust single-thread hash index lookup ~40% slower than C#, ~77% slower than C++. Hash prefetching is the #1 optimization priority.

**Methodology Issues Found:**
- C++ benchmark has an inverted error check (`if (result == Status::Ok) { log_warn(...) }`) in benchmark.h:525 that destroys upsert performance via console I/O. Must fix locally for benchmarking.
- C++ benchmark uses FileSystemDisk by default (not NullDisk). Must switch to NullDisk for fair in-memory comparison.
- C# benchmark.csproj targets net7.0; needs retargeting to net8.0 for .NET 8 SDK.
- C# `--sd --synth` gives 2.5M init keys + 10M txn keys (SmallData synthetic mode).

**Artifacts:**
- Updated: `rust/PERFORMANCE.md` with full cross-implementation comparison tables
- Benchmark runner: `/tmp/run_benchmarks.sh` (ephemeral)
- C++ data generator: `/tmp/gen_ycsb_data` (ephemeral)

**Build Commands:**
- Rust: `cargo run -p cross-impl-bench --release -- --workloads A,B,C,F --threads 1,2,4,8,16 --num-keys 2500480`
- C#: `dotnet run --project cs/benchmark/FASTER.benchmark.csproj -c Release -- -b 0 -t N --synth --sd --rumd R,U,M,D --runsec 10`
- C++: Requires TBB + libaio + uuid headers; modify benchmark.h constants for key count; use NullDisk for in-memory.

---

## 2026-03-07: Disk I/O Benchmark Suite Delivered

**What:** Built `disk-io-bench` — a disk-bound benchmark binary comparing SyncFileDevice, UringDevice, and TokioFileDevice with sync and Tokio client modes. Includes 4 workloads (overcommit, write-heavy, mixed Zipfian, sequential scan), latency histograms, pending rate tracking, and JSON/CSV/table output.

**Artifacts:**
- Benchmark crate: `rust/crates/samples/disk-io-bench/`
- Design doc: `.squad/agents/legolas/disk-io-benchmark-design.md`
- Runner script: `rust/scripts/disk-io-bench-matrix.sh`
- Decision: `.squad/decisions/inbox/legolas-disk-io-bench.md`

**Benchmark Matrix:** 3 devices × 2 clients × 4 workloads × 4 thread counts = 96 configurations

**Key Design Decisions:**
- Used `buffer_size_pages` to control memory pressure (128 pages for overcommit = ~4MB buffer forcing disk reads)
- Tokio client uses per-thread `current_thread` runtime + `LocalSet` because `AsyncSession` is `!Send`
- TokioFileDevice requires Tokio runtime context even with sync client — handled with `rt.enter()` guard
- Track pending rate (ops returning `Pending` / total ops) as key metric for device comparison
- Sequential workload uses `Distribution::Sequential` (not in cross-impl-bench) for scan patterns

**Pre-existing Bugs Fixed:**
- `CompactionPlan` initializer in scanner.rs missing 3 fields (dead_bytes, tombstone_bytes, total_chain_hops)
- Duplicate `let mut drained` and `fetch_sub` lines in drain.rs
- Added `#[allow(dead_code)]` for `DrainList::has_pending()` not yet used

**Quick Smoke Test Results (i9-10900K, local WSL2, not representative):**
- sync+sync mixed 2T: 6.42M ops/sec (P50=200ns, no pending — dataset fits in memory)
- uring+sync write-heavy 2T: 8.58M ops/sec (P50=200ns)
- tokio+tokio overcommit 2T: 3.42M ops/sec (P50=500ns)
- Real VM results expected to show much higher pending rates and device differentiation

**Build Command:**
- `cargo build --release -p disk-io-bench`
- `./scripts/disk-io-bench-matrix.sh --data-dir /mnt/faster-bench`


---

### W2-01: Hash Index Software Prefetch (P-05) — Completed

**Date:** 2026-03-07
**Branch:** `legolas/hash-prefetch`
**Commit:** `feat(hash): add software prefetch hints on bucket lookups (P-05)`

**What was done:**
- Created `hash/prefetch.rs` — cross-platform prefetch module (x86_64 `_mm_prefetch`/`_MM_HINT_T0`, aarch64 `PRFM PLDL1KEEP`, no-op fallback)
- Added `HashTable::prefetch_bucket()` method mirroring `bucket()` safety invariants
- Added overflow chain prefetch in `find_entry_in_bucket_chain()` — eagerly loads overflow address and prefetches next bucket while scanning current 7-entry bucket
- Added `HashIndex::prefetch()` facade for store-level callers
- Inserted prefetch calls in all 4 CRUD hot paths (read, upsert, RMW, delete) between hash/layout computation and index lookup for ~10-20 cycles latency cover
- 11 new unit tests across prefetch.rs (4), table.rs (4), index.rs (3)
- Clippy clean, all 1654 tests pass

**Key design decisions:**
- L1 prefetch (`_MM_HINT_T0`) because bucket accessed within ~10 instructions
- Read-only prefetch — hash lookups are read-dominant, write prefetch would cause MESI invalidation traffic
- Single cache line — 64B bucket fits exactly in one line
- Overflow chain: moved overflow address load before entry scan to maximize prefetch lead time

## Learnings

- **Shared working directory hazard**: Multiple agents modify the same files concurrently. `git checkout -- <file>` restores to HEAD and wipes ALL uncommitted changes including yours. Always stage + commit immediately after editing.
- **Atomic commit workflow**: In a multi-agent environment, the only safe workflow is: edit → stage → commit in rapid succession. Never leave changes unstaged.
- **Unicode in Python string matching**: Box-drawing characters (──) and em-dashes (—) match fine when using proper Python Unicode escapes (\u2500, \u2014), not raw byte sequences.

---

## 2026-03-07: Wave 3 Comprehensive Benchmarking Suite

**What:** Merged all Wave 2 feature branches (hash-prefetch, sealed-bit, ffi-callbacks) into squad and ran full YCSB + disk I/O benchmark matrix.

**Key Results:**
- **A/F regressions recovered:** Workload A 16T: 40.93M → 47.47M (+16.0%), Workload F 16T: 43.70M → 48.03M (+9.9%)
- **Workload C new peak:** 45.98M → 49.17M (+6.9%) at 16T, total +114.5% from pre-epoch baseline (22.93M)
- **C# gap narrowing:** Workload C gap vs C# narrowed from -60% (pre-epoch) → -19.6% (epoch-fix) → -14.0% (Wave 3)
- **Hash prefetch impact:** +6-7% throughput on reads, +39% P99.9 latency improvement. Below predicted 20-40% — prefetch distance tuning needed.
- **Disk I/O baseline:** All workloads in-memory (0% pending rate) on this 32GB VM. Need more keys or less buffer for true disk benchmarks.

**Artifacts:**
- Wave 3 results: `.squad/agents/legolas/wave3-benchmark-results.md`
- CSV: `/tmp/wave3-ycsb-results.csv`

**Optimization Priorities (updated):**
1. Two-level prefetching (hash bucket + record) — expected +10-20%
2. Hash table layout optimization (open addressing) — expected +15-30%
3. Per-thread log tail allocation — expected +5-10% writes
4. True disk-bound benchmarks with 100M+ keys

---

## 2026-03-07: Disk I/O Benchmark Methodology Fix (A2+A11)

**What:** Overhauled the disk-io-bench sample to fix the W3-02 0% pending rate problem and add statistical rigor.

**Branch:** `legolas/disk-io-bench-fix`

**Problem:** The W3-02 disk I/O benchmark matrix showed 0% pending rate with buffer_pages=128 on a 32GB VM. All operations completed synchronously, meaning we measured in-memory ceiling performance, NOT actual disk I/O behavior.

**Changes:**
1. **Pending-rate validation gate:** After measurement, checks pending rate. If < 0.1%, prints a WARNING and tags results as "IN-MEMORY BASELINE". Results are classified as DISK-BOUND (>=5%), MIXED (0.1-5%), or IN-MEMORY BASELINE (<0.1%).
2. **--force-disk mode:** Auto-tunes buffer_pages starting at 16, halving until pending_rate > 0%. Reports final configuration that achieved disk-bound behavior.
3. **Pending rate in all output:** Every result row shows pending_rate and io_classification in table, CSV, and JSON output.
4. **Default parameters fixed:** buffer_pages reduced from 128-256 to 16-32. With 10M keys x 1KiB values on 32GB VM, this should produce actual disk pressure. Documented the memory budget relationship.
5. **Statistical rigor (--iterations):** Default 5 iterations with mean, stddev, 95% CI (t-distribution for small n). HIGH VARIANCE flag when stddev > 10% of mean.

**Key Design Decisions:**
- I/O classification thresholds: <0.1% = IN-MEMORY, 0.1-5% = MIXED, >=5% = DISK-BOUND
- Force-disk probes use short 5s runs to minimize auto-tune time
- Median run used for latency percentiles (more robust than mean for tail metrics)
- t-distribution critical values hardcoded for n=1..10 (no external stats dependency)

## Learnings

- **Concurrent agent /tmp file collision:** Multiple agents share `/tmp/commit-msg.txt` — files get overwritten between write and use. Use `git commit --amend` to fix after the fact.
- **Branch switching by concurrent agents:** Other agents can `git checkout` while you're working. Always verify branch immediately before commit and chain git operations atomically.
- **Memory budget for disk benchmarks:** `buffer_pages * 32KiB` is the FASTER in-memory window, but the OS page cache also matters. Even with tiny buffer_pages, the OS may cache the entire dataset. True disk-bound behavior requires either: (a) dataset >> RAM, (b) O_DIRECT, or (c) explicit cache flushing.
