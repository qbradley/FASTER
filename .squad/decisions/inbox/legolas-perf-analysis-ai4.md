# AI-4 Performance Analysis: Epoch Amortization & 10M ops/sec Target

**Author:** Legolas (Performance Guru)  
**Date:** 2026-03-06  
**Status:** Analysis Complete  
**Machine:** Standard_D4ds_v5 equivalent (20 vCPU dev machine, 3.9 GHz)

---

## Executive Summary

Per-operation epoch protection is the single largest bottleneck in FASTER Rust, consuming **~128 ns (33–44%) of every operation's cost**. The existing `UnsafeContext` API already eliminates this overhead — it just isn't used by the default API paths. With epoch amortization:

- **Single-thread reads already exceed 10M ops/sec** (12.7M measured)
- **4-thread amortized upserts hit 10M total ops/sec** (measured)
- **Single-thread upsert reaches 6.1–6.6M ops/sec** (limited by hash index cache misses)
- **10M single-thread upserts are NOT achievable** without hash table prefetching or batched operations

---

## 1. Measured Component Costs (ns/op)

### 1.1 Isolated Components

| Component | Cost (ns) | Notes |
|-----------|-----------|-------|
| Hash computation (u64 key) | **0.9** | Negligible — FASTER's multiply-rotate hash |
| Bare epoch protect/unprotect | **97–100** | Dominated by `compute_safe_epoch` scanning 256 entries |
| Epoch with drain list active | **97–102** | Drain list not a significant overhead |
| Pipeline insert (epoch+hash+alloc+write) | **118** | Criterion benchmark, single bucket |
| Pipeline lookup (epoch+find+read) | **123** | Criterion benchmark, 7-entry bucket |

### 1.2 Full Store Operations (Per-Op Epoch — Current Behavior)

| Operation | ns/op | M ops/sec |
|-----------|-------|-----------|
| Upsert insert (sequential) | **293** | 3.4 |
| Upsert update (key exists) | **277** | 3.6 |
| Point read (key exists) | **276** | 3.6 |
| RMW (from YCSB criterion) | ~170 | 5.9 |

### 1.3 Full Store Operations (Amortized Epoch — UnsafeContext)

| Operation | ns/op | M ops/sec | Speedup |
|-----------|-------|-----------|---------|
| Upsert insert (sequential) | **165** | 6.1 | **1.8×** |
| Upsert update (key exists) | **151** | 6.6 | **1.8×** |
| Point read (key exists) | **79** | 12.7 | **3.5×** |

### 1.4 Derived Cost Breakdown (per upsert insert, 293 ns total)

| Component | Estimated Cost | % of Total | Method |
|-----------|---------------|------------|--------|
| **Epoch protect/unprotect** | **~128 ns** | **44%** | 293 - 165 = 128 |
| Hash index traversal (cold) | **~80 ns** | **27%** | 165 - 118 + 97 (pipeline has epoch too) |
| Hash computation | **~1 ns** | **<1%** | Direct measurement |
| Allocate at tail | **~20 ns** | **7%** | Atomic bump + page math |
| Record write + CAS commit | **~20 ns** | **7%** | memcpy + CAS |
| Overhead / misc | **~44 ns** | **15%** | Version chain walk, snapshot, layout |

---

## 2. Why Epoch Is So Expensive: The `compute_safe_epoch` Scan

The 97–128 ns epoch cost is NOT from the `protect()` or `unprotect()` atomics themselves (those are ~5 ns each). It's from `try_drain()` → `compute_safe_epoch()`:

```rust
fn compute_safe_epoch(&self) -> u64 {
    let current = self.current_epoch.load(Ordering::SeqCst);  // ← SeqCst fence
    let mut min = current;
    for entry in self.table.iter() {                           // ← 256 iterations!
        let epoch = entry.local_current_epoch.load(Ordering::Acquire);
        // ...
    }
    min.saturating_sub(1)
}
```

Every `unprotect()` (when reentrant goes 1→0) triggers this scan of **all 256 epoch table entries** (256 × 64-byte cache lines = 16 KB). The `Acquire` loads on each entry create a pipeline stall.

### Hardware Evidence (perf stat)

| Counter | Value | Assessment |
|---------|-------|------------|
| IPC (instructions per cycle) | **0.73** | Very low — memory bound |
| L1 dcache miss rate | **43%** | Extremely high |
| LLC cache miss rate | **17%** | Significant DRAM traffic |
| Branch miss rate | **0.39%** | Excellent (not a branch predictor problem) |

The 43% L1 dcache miss rate confirms this is a **memory-access-bound** workload, not a compute-bound one.

---

## 3. Multi-Thread Scaling Analysis

### 3.1 Per-Op Epoch (Current)

| Threads | Total M ops/sec | Per-thread M ops/sec | Scaling efficiency |
|---------|----------------|---------------------|-------------------|
| 1 | 2.8–3.0 | 2.8–3.0 | 100% (baseline) |
| 2 | 4.2–4.5 | 2.1–2.3 | 75% |
| 4 | 6.3 | 1.6 | **53%** |

### 3.2 Amortized Epoch (UnsafeContext)

| Threads | Total M ops/sec | Per-thread M ops/sec | Scaling efficiency |
|---------|----------------|---------------------|-------------------|
| 1 | 4.6–4.7 | 4.6–4.7 | 100% (baseline) |
| 2 | 7.1–7.7 | 3.6–3.9 | 78% |
| 4 | **10.0** | 2.5 | **54%** |

### 3.3 Why Scaling Is Poor (Both Paths)

Even with amortized epoch, scaling is ~54% efficient at 4 threads. Remaining contention sources:

1. **Hash index CAS contention** — `find_or_create` uses CAS on bucket entries. Different keys in the same bucket contend.
2. **Log tail allocation** — all threads compete for the atomic tail pointer. Even with cache-line padding, the `fetch_add` on the tail address bounces between cores.
3. **CPU cache coherence** — adjacent hash buckets in the same cache line cause false sharing.

### 3.4 Epoch Contention (Criterion Numbers)

| Background threads | Epoch ns/op | Slowdown |
|-------------------|-------------|----------|
| 0 | 99 | 1.0× |
| 2 | 252 | 2.5× |
| 4 | 378 | 3.8× |

This explains why per-op epoch scaling is so bad — the epoch table entries bounce between cores. Amortized epoch eliminates this per-operation cost entirely.

---

## 4. Refresh Interval Tuning

| Refresh Interval | ns/op (update) | M ops/sec |
|-----------------|----------------|-----------|
| Every 32 ops | 197 | 5.1 |
| Every 64 ops | 179 | 5.6 |
| **Every 128 ops** | **176** | **5.7** |
| Every 256 ops | 196 | 5.1 |
| Every 512 ops | 202 | 5.0 |
| Every 1024 ops | 193 | 5.2 |

**Sweet spot: 64–128 operations between refreshes.** Too frequent → refresh overhead (epoch scan); too infrequent → cache pollution from stale epoch entries.

The U-shaped curve at 256+ is surprising — likely due to increased memory pressure from deferred drain callbacks accumulating without timely cleanup.

---

## 5. Projected Throughput with Optimizations

### 5.1 Optimization Impact Estimates

| Optimization | Single-thread Upsert | Single-thread Read | Effort |
|-------------|---------------------|-------------------|--------|
| **Baseline (current per-op)** | **3.4M** | **3.6M** | — |
| + Epoch amortization (UnsafeContext) | **6.1M (+80%)** | **12.7M (+253%)** | Low — API already exists |
| + Hash prefetching (batch lookahead) | ~8–9M (+30%) | ~15M+ | Medium |
| + Reduce epoch table to registered-only scan | ~6.5M (+7%) | ~13M (+3%) | Low |
| + Smaller hash entries (48-bit packed) | ~7M (+15%) | ~14M (+10%) | Medium |
| + All combined | ~9–10M | ~16M+ | High |

### 5.2 Can We Reach 10M ops/sec Single-Thread?

| Workload | Achievable? | Path |
|----------|-------------|------|
| Point reads | **YES — already done** | 12.7M with UnsafeContext |
| Read-heavy (95/5) | **YES** | Projected ~11M with UnsafeContext |
| Mixed (50/50) | **MAYBE** | ~8M projected, 10M with prefetching |
| Pure upsert (insert) | **NO** without hash prefetching | 6.1M ceiling with current hash index |
| Pure upsert (update) | **UNLIKELY** | 6.6M, maybe 8M with prefetching |

**Key insight:** Reads are 2× faster than upserts because reads only do `find()` (no CAS, no allocation, no record write). The hash index find is the dominant cost for reads at 79ns — this is almost entirely cache-miss latency on the bucket lookup.

---

## 6. Optimization Priority Recommendations

### Priority 1: Promote UnsafeContext as Primary API (Impact: +80–250%)
**Effort: Low (1–2 days)**

The `UnsafeContext` API already exists and works. The gains are massive:
- Upsert: 293→165 ns (1.8× faster)
- Read: 276→79 ns (3.5× faster)

**Actions:**
- Add `unsafe_context` examples to all documentation
- Consider making amortized epoch the default in YCSB benchmarks
- Add batch operation helpers that auto-refresh every N ops
- Consider a `BatchSession` wrapper that handles refresh automatically

### Priority 2: Reduce `compute_safe_epoch` Scan (Impact: +5–10%)
**Effort: Low (1 day)**

Currently scans all 256 `MAX_THREADS` entries even if only 1–4 are registered. Track a `max_registered_index` to bound the scan.

```rust
// Instead of scanning all 256 entries:
for entry in &self.table[..self.max_registered_index.load(Relaxed) + 1] {
```

### Priority 3: Hash Index Prefetching (Impact: +20–40%)
**Effort: Medium (3–5 days)**

The hash index bucket lookup causes an L1 cache miss on every operation (43% miss rate confirms this). Prefetching the next key's bucket while processing the current key can hide memory latency.

**Design:** Add a `prefetch_bucket(key_hash)` method and batch operation APIs:
```rust
// Pseudo-code for batched upsert
for window in keys.windows(LOOKAHEAD) {
    store.prefetch_for_upsert(&window[LOOKAHEAD-1]);
    store.upsert_internal(&window[0], ...);
}
```

### Priority 4: Log Tail Allocation Optimization (Impact: +5–10%)
**Effort: Medium (2–3 days)**

Per-thread page-level allocation pools could eliminate atomic contention on the global tail. Each thread gets a "slab" from the tail, then allocates locally within it.

### Priority 5: Hash Table Layout Optimization (Impact: +10–15%)
**Effort: High (1–2 weeks)**

Current buckets: 7 entries × 8 bytes + 8 bytes overflow = 64 bytes (1 cache line). Could experiment with:
- 2-cache-line buckets (14 entries, fewer overflow chains)
- Separate tag array (better SIMD potential)
- Cuckoo hashing (no overflow chains)

### Priority 6: Multi-Thread Tail Contention (Impact: better scaling)
**Effort: Medium (3–5 days)**

Partition the log tail per-thread (each thread has its own allocation region within the same page or across pages). This eliminates the `fetch_add` contention on the global tail pointer.

---

## 7. Summary Table

| Metric | Current (Per-Op) | With UnsafeContext | Projected (All Opts) |
|--------|------------------|--------------------|---------------------|
| Single-thread upsert | 3.4M | **6.1M** | ~9–10M |
| Single-thread read | 3.6M | **12.7M** | ~15M+ |
| 4-thread upsert | 6.3M | **10.0M** | ~15M+ |
| 4-thread read | ~8M est. | **~30M+ est.** | ~40M+ |

---

## 8. Key Architectural Observations

1. **Epoch amortization is the C# `UnsafeContext` equivalent** — Rust FASTER already has this API. The current per-operation API (`FasterKv::upsert` etc.) is the simple/safe path; `UnsafeContext` is the fast path. This matches the C# FASTER design exactly.

2. **The 10M single-thread upsert target requires hash prefetching** — after epoch amortization, the hash index cache miss (~80ns per lookup) is the dominant remaining cost. Without prefetching, the CPU stalls on every operation waiting for the L1 miss to resolve.

3. **Read performance is excellent with amortization** — 12.7M single-thread reads exceeds the 10M target by 27%. Reads have a lower cache-miss cost because they don't write (no store buffer pressure, no cache line invalidation).

4. **Multi-thread scaling is limited by cache coherence** — even with perfect epoch amortization, shared data structures (hash index, log tail) create cross-core cache traffic. Partitioned approaches (per-thread allocation, sharded hash buckets) would improve scaling.

---

## Appendix: Benchmark Artifacts

- **Custom benchmark:** `rust/crates/faster-core/benches/perf_analysis.rs`
- **Criterion pipeline benchmarks:** `rust/crates/faster-bench/benches/main.rs`
- **YCSB benchmarks:** `rust/crates/faster-core/benches/ycsb.rs`
- **Run command:** `cd rust && cargo bench --bench perf_analysis -p faster-core -- --nocapture`

### Raw Benchmark Output (Representative Run)

```
Bare epoch:          97.4 ns/op  (10.3M ops/sec)
Hash only:            0.9 ns/op  (1153M ops/sec)
Per-op upsert ins:  293.2 ns/op  (3.4M ops/sec)
Per-op upsert upd:  277.0 ns/op  (3.6M ops/sec)
Amortized ins:      165.2 ns/op  (6.1M ops/sec)
Amortized upd:      151.4 ns/op  (6.6M ops/sec)
Per-op read:        275.5 ns/op  (3.6M ops/sec)
Amortized read:      78.8 ns/op  (12.7M ops/sec)
4T per-op upsert:     6.3M total ops/sec
4T amortized upsert: 10.0M total ops/sec
```
