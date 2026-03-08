# Hash Table Layout Analysis: Open Addressing vs Current Multi-Slot Buckets

**Author:** Gandalf (Lead / System Architect)
**Date:** 2026-03-06
**Status:** P2 Research — Investigation Complete
**Task:** A8 — Hash Table Layout Investigation (Open Addressing Prototype)

---

## Executive Summary

The Iteration 5 benchmarks showed Rust trailing C# by ~14% on Workload C (100% reads). The hypothesis was that C# uses "open addressing with inline keys/values" while Rust uses "separate chaining." **This hypothesis is incorrect.** Both implementations use the same fundamental architecture: 64-byte multi-slot hash buckets storing 8-byte logical addresses that point into the hybrid log. Neither stores keys or values inline in the hash table.

However, the performance gap is real. This document identifies the actual structural differences, analyzes what a true open-addressing-with-inline-data design would look like, and recommends a path forward.

---

## 1. Actual Layout Comparison

### 1.1 C# FASTER Hash Bucket (64 bytes, 1 cache line)

```
┌─────────────────────────────────────────────────────────────────────┐
│                     HashBucket (64 bytes)                           │
├──────────┬──────────┬──────────┬──────────┬──────────┬──────────┬──────────┬──────────┐
│ Entry[0] │ Entry[1] │ Entry[2] │ Entry[3] │ Entry[4] │ Entry[5] │ Entry[6] │ Entry[7] │
│  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │
└──────────┴──────────┴──────────┴──────────┴──────────┴──────────┴──────────┴──────────┘
                                                                              ▲
                                                                    Overflow + Latches
                                                                    (shared with Entry[7])

Entry[0..6] layout (each 8 bytes = 64 bits):
┌───────┬─────────┬────────────────┬─────────────────────────────────┐
│ Tent  │ Pending │   Tag (14)     │   Address (48)                  │
│  (1)  │   (1)   │                │   → points to hybrid log        │
└───────┴─────────┴────────────────┴─────────────────────────────────┘
 bit 63   bit 62    bits [61:48]          bits [47:0]

Entry[7] layout (overflow pointer + latches):
┌───────────┬─────────────────────┬───────────────────────────────────┐
│ Exclusive │ Shared Latch (15)   │   Overflow Address (48)           │
│  Latch(1) │                     │   → next overflow bucket          │
└───────────┴─────────────────────┴───────────────────────────────────┘
  bit 63      bits [62:48]              bits [47:0]
```

**Key properties:**
- `kEntriesPerBucket = 8` (1 << 3)
- `kOverflowBucketIndex = 7` (last slot serves double duty)
- 7 usable data slots per bucket
- Overflow pointer shares bits with reader-writer latch
- Uses shared/exclusive latches for bucket-level synchronization (CAS-based spin)

### 1.2 Rust FASTER Hash Bucket (64 bytes, 1 cache line)

```
┌─────────────────────────────────────────────────────────────────────┐
│                     HashBucket (64 bytes)                           │
├──────────┬──────────┬──────────┬──────────┬──────────┬──────────┬──────────┬──────────┐
│ Entry[0] │ Entry[1] │ Entry[2] │ Entry[3] │ Entry[4] │ Entry[5] │ Entry[6] │ Overflow │
│  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │  8 bytes │
└──────────┴──────────┴──────────┴──────────┴──────────┴──────────┴──────────┴──────────┘
                                                                              ▲
                                                                    Dedicated overflow
                                                                    address (no latches)

Entry[0..6] layout (each 8 bytes = 64 bits):
┌───────┬───────┬────────────────┬─────────────────────────────────┐
│ Tent  │ Rsvd  │   Tag (14)     │   Address (48)                  │
│  (1)  │  (1)  │                │   → points to hybrid log        │
└───────┴───────┴────────────────┴─────────────────────────────────┘
 bit 63  bit 62   bits [61:48]          bits [47:0]

Overflow layout (8 bytes):
┌─────────────────────────────────────────────────────────────────────┐
│                     AtomicLogicalAddress (48 bits used)              │
└─────────────────────────────────────────────────────────────────────┘
```

**Key properties:**
- `BUCKET_NUM_ENTRIES = 7` (7 data slots)
- Dedicated 8-byte overflow pointer (no bit sharing)
- **Lock-free**: no latches; relies on CAS + tentative bit + epoch protection
- 7 usable data slots per bucket (same as C# effective count)

### 1.3 Key Finding: Identical Architecture

| Aspect | C# | Rust |
|--------|-----|------|
| Bucket size | 64 bytes (1 cache line) | 64 bytes (1 cache line) |
| Data entries per bucket | 7 (slot 7 is overflow) | 7 (separate overflow field) |
| Entry size | 8 bytes | 8 bytes |
| Entry content | tag + logical address | tag + logical address |
| Data stored inline? | **NO** — address points to log | **NO** — address points to log |
| Overflow mechanism | Chained overflow buckets | Chained overflow buckets |
| Concurrency model | Shared/exclusive latches | Lock-free (CAS + tentative + epoch) |

**Both implementations are multi-slot hash buckets with overflow chaining. Neither is open addressing. Neither stores data inline.**

---

## 2. Cache Line Analysis: Reads

### 2.1 Read Path — Current Architecture (Both C# and Rust)

```
Step 1: Hash key → compute bucket index
        (arithmetic only, no memory access)

Step 2: Load bucket (64 bytes = 1 cache line)
        ← CACHE MISS #1 (unless prefetched)

Step 3: Scan 7 entries, compare tags
        (all within same cache line — free)

Step 4: Extract logical address from matching entry
        (still in same cache line — free)

Step 5: Translate logical address → physical pointer
        (depends on allocator page table — may be cached)

Step 6: Load record from hybrid log
        ← CACHE MISS #2 (key comparison)

Step 7: If key matches, load value
        ← possibly within same cache line if record is small,
           otherwise CACHE MISS #3
```

**Minimum cache misses per read: 2** (bucket + record)
**Maximum cache misses per read: 3+** (bucket + record header + value, if large records or overflow chain)

### 2.2 What True Open Addressing Would Look Like

A true open addressing design would store keys AND values directly in the hash table, eliminating Step 5-7:

```
Step 1: Hash key → compute slot index
Step 2: Load slot (contains key + value inline)
        ← CACHE MISS #1
Step 3: Compare key
        (may be in same cache line if record is small)
Step 4: If match, value is already loaded — done
        If no match, probe next slot
        ← may be in same cache line (linear probing)

Minimum cache misses per read: 1
```

But this fundamentally conflicts with FASTER's hybrid log architecture. See Section 4.

---

## 3. Actual Causes of the ~14% Read Gap

The gap is NOT from architectural differences in the hash index (they're identical). The likely causes are:

### 3.1 C# JIT Advantages on Hot Path
- .NET RyuJIT can inline aggressively and de-virtualize at runtime
- Struct-based generics in C# (`TKey`, `TValue`) generate specialized machine code per type
- Rust generic monomorphization is comparable, but C# may benefit from PGO (Profile-Guided Optimization) in recent .NET versions

### 3.2 Memory Ordering Differences
- **C# reads use plain loads** (no volatile/Interlocked) for bucket entry scanning on the hot path
- **Rust uses `Ordering::Acquire`** on every entry load, which on x86-64 is a no-op fence-wise but prevents compiler reordering and inhibits optimizations like vectorization
- On ARM/POWER, this would generate actual fence instructions

### 3.3 Overflow Prefetch Strategy
- **Rust aggressively prefetches**: loads overflow address and issues prefetch hint before scanning entries
- **C# does not prefetch**: relies on hardware prefetcher and cache coherence
- In read-heavy workloads with low collision rates, the Rust prefetch is mostly wasted work

### 3.4 Latch vs Lock-Free Trade-offs
- C# latches are acquisition-free on the read path for FindTag (no latch needed for reads)
- Rust's lock-free approach has no latch overhead either, but the tentative-bit check adds a branch per entry

### 3.5 Record Access After Hash Lookup
- The hybrid log record access path may differ in efficiency
- Page table translation, record layout, and key comparison code are separate from the hash index
- This is where most of the gap likely lives — not in the hash table itself

---

## 4. Open Addressing: Feasibility Analysis for FASTER

### 4.1 Fundamental Conflict with Hybrid Log

FASTER's hybrid log is the core innovation: records move through memory tiers (mutable → read-only → on-disk) without modifying the hash index. The hash index stores logical addresses precisely because:

1. **Mutable records are updated in-place** — the address stays stable
2. **Read-only records are immutable** — address is frozen
3. **On-disk records** — same address, but I/O needed to read
4. **Compaction** — records move, hash index entries are updated atomically

True open addressing (inline keys/values) would break this model:
- **Grow operations** become record copies, not just entry moves
- **Compaction** must touch hash table entries containing actual data
- **Variable-length keys/values** can't fit in fixed-size slots
- **Memory usage** explodes: slot size must accommodate the largest possible record
- **Delete** requires tombstones (which complicate scanning and waste space)

### 4.2 Hybrid Approach: Inline Small Records

A pragmatic compromise:

```
Slot (128 bytes):
┌──────────┬──────────┬──────────┬──────────────────────────────────────┐
│ Tag (2B) │ Flags(2B)│ KeyLen(4)│ Inline Key+Value (up to 120 bytes)   │
└──────────┴──────────┴──────────┴──────────────────────────────────────┘
                                   OR
┌──────────┬──────────┬──────────┬──────────────────────────────────────┐
│ Tag (2B) │ Flags(2B)│ Overflow │ LogicalAddress (8B) + padding        │
└──────────┴──────────┴──────────┴──────────────────────────────────────┘
```

**Problems:**
- 128-byte slots → 2 cache lines per slot → worse for probing
- Bucket array becomes 16× larger (128B vs 8B per entry)
- Small records benefit; large records get worse
- Concurrent CAS on 128-byte records requires 128-bit atomics or latching

### 4.3 Alternative: Fat Buckets with Inline Cache

A more FASTER-compatible optimization:

```
Fat Bucket (256 bytes = 4 cache lines):
┌─────────────────────────────────────────────────────────────────────┐
│ Cache Line 0: 7 standard entries (tag + address) + overflow ptr    │ ← existing
├─────────────────────────────────────────────────────────────────────┤
│ Cache Line 1: Inline cache — last N accessed records               │ ← new
│   Key hash + 8B key + 8B value for 2-3 most recent hits           │
├─────────────────────────────────────────────────────────────────────┤
│ Cache Lines 2-3: Extended inline cache or padding                  │
└─────────────────────────────────────────────────────────────────────┘
```

**Problems:**
- 4× memory usage per bucket
- Cache pollution on scan-heavy workloads
- Invalidation complexity when records are updated
- Marginal benefit for uniform-access workloads (YCSB-C is uniform random)

---

## 5. Prototype: Benchmarking What We CAN Optimize

Given that true open addressing is incompatible with FASTER's architecture, the prototype focuses on micro-optimizations to the existing hash index that could close the gap:

### 5.1 Prototype Design

The prototype (`rust/crates/faster-core/benches/hash_layout_bench.rs`) benchmarks:

1. **Current implementation** — baseline
2. **Relaxed-ordering variant** — uses `Ordering::Relaxed` for reads (safe on x86-64, matches C# behavior)
3. **No-prefetch variant** — skips overflow prefetch (matches C# behavior)
4. **Combined optimizations** — relaxed + no-prefetch

### 5.2 Expected Results

On x86-64 (TSO architecture), `Ordering::Acquire` compiles to the same instruction as `Ordering::Relaxed` (plain `mov`). The gap should therefore be minimal on x86-64 but significant on ARM.

The prefetch overhead analysis:
- At load factor < 1.0 (7 entries per bucket with low collision), overflow chains are rare
- Prefetching a non-existent overflow bucket wastes an instruction but costs no cache miss
- Net impact: near zero for read-heavy workloads

**The ~14% gap is likely NOT in the hash index code itself. It's in the record access path, memory allocator translation, or key comparison code downstream of the hash lookup.**

---

## 6. Impact Analysis: Operations Beyond Reads

| Operation | Current (Multi-Slot) | Open Addressing (Inline) | Impact |
|-----------|---------------------|--------------------------|--------|
| **Read** | 2 cache misses | 1 cache miss | ✅ Better |
| **Insert** | CAS 8-byte entry | CAS 128+ byte record | ❌ Much worse |
| **RMW** | Modify record in log | Modify inline record + CAS | ❌ Worse (larger CAS) |
| **Delete** | CAS entry to EMPTY | Write tombstone | ❌ Worse (tombstone management) |
| **Grow** | Move 8-byte entries | Move full records | ❌ Much worse |
| **Compaction** | Scan log, update entries | Scan table, move records | ❌ Fundamentally different |
| **Concurrent access** | Lock-free 8-byte CAS | Need latches for >8 bytes | ❌ Worse |

---

## 7. Recommendation

### Decision: Keep Multi-Slot Buckets. Investigate Record Access Path Instead.

**Rationale:**

1. **The hypothesis was wrong.** C# and Rust use the same hash table architecture. The gap isn't here.

2. **True open addressing is incompatible with FASTER's hybrid log.** The entire value proposition of FASTER (records flowing through mutable → read-only → disk tiers) requires the hash index to store pointers, not data.

3. **The 14% gap lives downstream.** Profile-guided investigation of the record access path (page table lookup, physical address translation, record header parsing, key comparison) is where the actual optimization opportunity exists.

4. **Micro-optimizations to try on the existing hash index:**
   - Consider `Ordering::Relaxed` for read-path loads on x86-64 (behind a cfg flag)
   - Evaluate removing the overflow prefetch on read path for low-collision workloads
   - Consider 8-entry buckets with shared overflow/latch bits (matching C# layout exactly)

### Next Steps (Priority Order)

1. **Profile the full read path** end-to-end with `perf stat` / `perf record` to identify where cycles are actually spent (A-priority)
2. **Benchmark record access isolation** — measure time from "have logical address" to "have value bytes" (B-priority)
3. **Evaluate Relaxed ordering on x86-64** behind `#[cfg(target_arch)]` (C-priority)
4. **Consider 8-entry bucket variant** with latch bits in overflow slot (C-priority)

---

## 8. Prototype Benchmark Results

### 8.1 Methodology

Standalone micro-benchmark comparing two pure hash index implementations:
- **Multi-slot**: FASTER-style 64-byte buckets with 7 entries + overflow chaining
- **Open-addr**: Linear probing with `(hash, address)` pairs at 50% load factor

Both store 8-byte logical addresses (not inline keys/values). The multi-slot table uses 14-bit tag comparison; open-addressing uses full 64-bit hash comparison. Tests run at three working set sizes to exercise L2, L3, and main memory.

Environment: Linux x86-64, `rustc -O` release build.

### 8.2 Results

```
=== 50,000 keys | multi-slot: 1.0 MB | open-addr: 2.0 MB ===
  (Fits in L2/L3 cache)
  Tag collisions: 3/50,000

  Individual lookups (random order):
    multi-slot find        10.8 ns/op
    open-addr find          5.2 ns/op   → open-addr 2.1× faster

  Batched lookups (batch=16 + prefetch):
    multi-slot batched-16   9.7 ns/op
    open-addr batched-16    4.9 ns/op   → open-addr 2.0× faster

=== 200,000 keys | multi-slot: 4.0 MB | open-addr: 8.0 MB ===
  (Fits in L3 cache)
  Tag collisions: 13/200,000

  Individual lookups (random order):
    multi-slot find        12.2 ns/op
    open-addr find          8.8 ns/op   → open-addr 1.4× faster

  Batched lookups (batch=16 + prefetch):
    multi-slot batched-16  10.0 ns/op
    open-addr batched-16    8.7 ns/op   → open-addr 1.2× faster

=== 800,000 keys | multi-slot: 16.0 MB | open-addr: 32.0 MB ===
  (Exceeds L3 — main memory bound)
  Tag collisions: 62/800,000

  Individual lookups (random order):
    multi-slot find        27.4 ns/op
    open-addr find         19.2 ns/op   → open-addr 1.4× faster

  Batched lookups (batch=16 + prefetch):
    multi-slot batched-16  17.9 ns/op
    open-addr batched-16   15.5 ns/op   → open-addr 1.2× faster
```

### 8.3 Analysis

**Why open addressing wins in micro-benchmarks:**
1. **Single 16-byte access** vs scanning a 64-byte bucket — even with all data in the same cache line, scanning 7×8-byte entries with branch per entry costs more than one comparison
2. **Full hash comparison** eliminates tag collisions entirely (14-bit tags collide at ~1 in 16K rate)
3. **50% load factor** keeps probe chains very short (avg ~1.5 probes)

**Why these numbers are misleading for FASTER:**
1. **2× memory overhead** — open-addr uses 32 MB vs 16 MB for 800K keys
2. **No concurrency** — real FASTER uses CAS; open-addr would need CAS on 16-byte entries (requires 128-bit atomics or latches)
3. **No variable-length records** — real FASTER stores keys/values in the hybrid log, not inline
4. **No deletions/tombstones** — open-addr deletion complexity is absent
5. **No epoch protection** or reclamation overhead
6. **Gap narrows with batching** — at 800K keys with prefetch, it's only 1.2×. FASTER already uses batched prefetch in its two-level pipeline

**Tag collisions scale as expected:** 3 → 13 → 62 collisions across 50K → 200K → 800K keys, consistent with the birthday paradox on 14-bit tags within same-bucket groups.

**Key takeaway:** Even under conditions maximally favorable to open addressing, the gap with batched prefetch at memory-bound working sets is only ~15%. Given the architectural incompatibility with FASTER's hybrid log (Section 4), this marginal gain cannot justify the fundamental redesign cost.

### 8.4 Benchmark Source

- Criterion-based: `rust/crates/faster-core/benches/hash_layout_bench.rs`
- Standalone timing (used for these results): compiled separately to avoid criterion output buffering

Run: `cd rust && cargo bench --bench hash_layout_bench`

---

*"A wizard is never late, nor is he early. He arrives precisely when he identifies the correct root cause." — Gandalf, probably*
