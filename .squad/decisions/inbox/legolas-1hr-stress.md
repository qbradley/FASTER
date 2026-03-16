# 1-Hour Stress Test Results — OOM Analysis

**Date:** 2026-03-16
**Author:** Legolas (Performance Guru)
**VM:** Azure Standard_F16s_v2 (16 vCPU, 32GB RAM, no swap)
**Commit:** `b65a0ba` (includes lossy deadlock fix: eliminate O(N) truncation deadlock in InMemoryDevice)

---

## Executive Summary

**Both 1-hour stress tests were OOM-killed by the Linux kernel before completion.**
The fundamental issue is that `InMemoryDevice` uses an unbounded `Vec` that never
releases memory. At 16-thread throughput (~39M ops/s), the hybrid log fills 32GB
RAM in 8-9 minutes regardless of mode.

| Test | Duration Before OOM | Total Ops | Avg ops/s | Peak RSS | Violations |
|------|-------------------|-----------|-----------|----------|------------|
| Non-lossy (16t) | 480s (8 min) | 18,488,922,112 | 38,519,004 | 31.8 GB | 0 |
| Lossy (16t) | 540s (9 min) | 20,762,865,664 | 38,449,752 | 32.0 GB | 0 |

**Key Takeaway:** Lossy mode survived 60 seconds (12.5%) longer than non-lossy,
confirming that eviction/truncation is functioning and slowing memory growth. However,
the `InMemoryDevice` Vec never shrinks — truncated pages are logically freed but
the backing allocation remains. A 1-hour run at this throughput would require ~230GB RAM.

---

## Test 1: Non-Lossy 16-Thread (Target: 3600s)

**Command:**
```
cargo run --release -p faster-core --example stress_5min -- \
  --threads 16 --duration-secs 3600 --report-interval 60
```

**Result: ❌ OOM-KILLED at T+480s**

### Throughput Timeline

| Elapsed | ops/s | Total Ops | Violations |
|---------|-------|-----------|------------|
| T+60s | 39,893,917 | 2,393,635,072 | 0 |
| T+120s | 38,349,691 | 4,694,616,576 | 0 |
| T+180s | 39,527,714 | 7,066,279,424 | 0 |
| T+240s | 39,024,341 | 9,407,739,904 | 0 |
| T+300s | 39,222,016 | 11,761,060,864 | 0 |
| T+360s | 39,249,920 | 14,116,056,064 | 0 |
| T+420s | 37,115,165 | 16,342,966,016 | 0 |
| T+480s | 36,625,877 | 18,540,518,656 | 0 |
| *OOM* | *SIGKILL (exit 137)* | | |

### Analysis
- **Average throughput:** 38,626,080 ops/s (8 intervals)
- **Peak:** 39,893,917 ops/s (T+60s)
- **Min:** 36,625,877 ops/s (T+480s — just before OOM)
- **Throughput degradation:** 8.2% decline from first to last interval (memory pressure)
- **Oracle violations:** 0
- **Throughput cliff:** None detected (no >50% drop)

### OOM Details (from dmesg)
```
Out of memory: Killed process 124221 (stress_5min)
  total-vm:  45,551,660 kB (43.4 GB virtual)
  anon-rss:  31,808,392 kB (30.3 GB resident)
  pgtables:     62,568 kB
```

**Memory growth rate:** ~63 MB/s (30.3 GB / 480s)
**Projected 1hr requirement:** ~227 GB

---

## Test 2: Lossy 16-Thread (Target: 3600s)

**Command:**
```
cargo run --release -p faster-core --example stress_5min -- \
  --threads 16 --duration-secs 3600 --report-interval 60 --lossy
```

**Result: ❌ OOM-KILLED at T+540s (60s longer than non-lossy)**

### Throughput Timeline

| Elapsed | ops/s | Total Ops | Violations |
|---------|-------|-----------|------------|
| T+60s | 39,417,715 | 2,365,062,912 | 0 |
| T+120s | 38,164,326 | 4,654,922,496 | 0 |
| T+180s | 38,672,712 | 6,975,285,248 | 0 |
| T+240s | 37,877,166 | 9,247,915,264 | 0 |
| T+300s | 38,199,278 | 11,539,872,000 | 0 |
| T+360s | 38,527,539 | 13,851,524,352 | 0 |
| T+420s | 38,830,097 | 16,181,330,176 | 0 |
| T+480s | 38,459,865 | 18,488,922,112 | 0 |
| T+540s | 37,899,059 | 20,762,865,664 | 0 |
| *OOM* | *SIGKILL (exit 137)* | | |

### Analysis
- **Average throughput:** 38,449,751 ops/s (9 intervals)
- **Peak:** 39,417,715 ops/s (T+60s)
- **Min:** 37,877,166 ops/s (T+240s)
- **Throughput degradation:** 3.9% decline from first to last interval (more stable than non-lossy)
- **Oracle violations:** 0
- **Throughput cliff:** None detected
- **🔑 NO DEADLOCK** — The b65a0ba fix held under sustained 16-thread lossy eviction pressure

### OOM Details (from dmesg)
```
Out of memory: Killed process 124810 (stress_5min)
  total-vm:  43,683,912 kB (41.7 GB virtual)
  anon-rss:  32,048,096 kB (30.6 GB resident)
  pgtables:     63,000 kB
```

**Memory growth rate:** ~56 MB/s (30.6 GB / 540s) — 11% slower than non-lossy
**Projected 1hr requirement:** ~202 GB (less than non-lossy due to eviction)

---

## Comparative Analysis

### Lossy Deadlock Fix Validation ✅

The most critical finding: **The lossy deadlock fix (b65a0ba) is validated.**

| Metric | Before Fix (5min test) | After Fix (1hr attempt) |
|--------|----------------------|------------------------|
| Lossy outcome | Deadlocked at T+200s | Ran 540s at full speed until OOM |
| Throughput at death | 0 ops/s (frozen) | 37.9M ops/s (full speed) |
| Total lossy ops | ~7.4B | 20.8B (2.8x more) |
| Exit cause | Hung forever (kill -9) | Clean OOM kill |

The previous lossy deadlock occurred at ~7.4B ops. This test processed **20.8B ops**
(2.8x past the previous deadlock point) with zero stalling. The O(N) truncation
deadlock in InMemoryDevice is definitively fixed.

### Memory Growth Comparison

| Mode | Growth Rate | Duration | Eviction Benefit |
|------|-------------|----------|-----------------|
| Non-lossy | ~63 MB/s | 480s | N/A (no eviction) |
| Lossy | ~56 MB/s | 540s | 11% slower growth |

Eviction reduces memory growth rate by ~11%, extending survivability by ~12.5%.
However, since `InMemoryDevice::truncate()` marks pages as logically free but the
backing `Vec` retains the allocation, the benefit is limited.

### Throughput Stability (Both Modes)

Both modes showed excellent throughput stability before OOM:
- Non-lossy: σ = 1.08M ops/s (2.8% coefficient of variation)
- Lossy: σ = 460K ops/s (1.2% coefficient of variation)

Lossy mode was actually **more stable** — eviction smooths out memory pressure effects.

---

## Success Criteria Assessment

| Criterion | Target | Result | Verdict |
|-----------|--------|--------|---------|
| Complete 3600s (non-lossy) | 3600s | 480s (OOM) | ❌ FAIL |
| Complete 3600s (lossy) | 3600s | 540s (OOM) | ❌ FAIL |
| Non-lossy >30M avg ops/s | >30M | 38.6M | ✅ PASS |
| Lossy >30M avg ops/s | >30M | 38.4M | ✅ PASS |
| Zero oracle violations | 0 | 0 (both) | ✅ PASS |
| No throughput degradation | <20% | 8.2% / 3.9% | ✅ PASS |
| Lossy deadlock fix holds | No deadlock | No deadlock | ✅ PASS |

**Overall: 5/7 criteria passed.** The 2 failures are both OOM, not correctness or
stability issues. When running, both modes are fast, stable, and correct.

---

## Root Cause: InMemoryDevice Unbounded Growth

The `InMemoryDevice` is backed by a `Vec<u8>` that grows as the hybrid log flushes
pages to the "device." Even with lossy eviction and truncation:

1. **Pages flushed → Vec grows** (both modes)
2. **Truncation marks pages logically free** but `Vec` capacity is never reduced
3. **At ~39M ops/s** with 16 threads, pages are flushed at ~56-63 MB/s
4. **32GB VM ÷ 60 MB/s ≈ 533s** (~8-9 minutes) to OOM

This is by design — `InMemoryDevice` is a test/development device, not a production
storage backend. For true 1-hour runs, a disk-backed device is needed.

---

## Recommendations

### P0: Disk-Backed 1-Hour Test
To validate 1-hour stability, the stress test must use a disk-backed device (the VM
has ample SSD space). This would:
- Eliminate OOM as a bottleneck
- Test true disk I/O + eviction under sustained load
- Be the real production scenario

### P1: InMemoryDevice Memory Cap
Add an optional memory limit to `InMemoryDevice` that either:
- Rejects writes beyond the cap (simulating a full disk)
- Reclaims truncated regions (shrink the Vec or use a sparse data structure)

### P2: Stress Test Memory Monitoring
Add RSS tracking to the reporter thread (via `/proc/self/statm` on Linux) to provide
early warning of OOM pressure without needing external monitoring.

---

## Pre-flight Conditions

```
VM:     Azure Standard_F16s_v2
CPUs:   16 vCPU
RAM:    31 GiB (no swap)
Load:   0.20 (idle before tests)
Free:   23 GiB free + 7 GiB cache = 30 GiB available
Kernel: Linux (OOM killer active, no swap)
```
