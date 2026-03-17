# Disk-Backed Stress Test Results (5-Minute)

**Date:** 2026-03-17  
**Agent:** Legolas (Performance Guru)  
**Commit:** d621f59  
**VM:** Standard_F16s_v2 (16 vCPU, 32GB RAM, no swap)  
**Binary:** `stress_disk` example (lossless mode, disk-backed)

## Configuration

| Parameter | Value |
|-----------|-------|
| Threads | 16 |
| Duration target | 300s |
| Key range | 1,000,000 |
| Max live keys | 500,000 |
| Log segment size | 1024 MB |
| Storage dir | /tmp/faster-disk-stress/ |

## Result: 🔴 FAILED — OOM-killed at T+302s

The test **did not complete** the full 5 minutes. The process was killed by the Linux OOM-killer (exit code 137) at approximately T+302s after RSS spiked from 2.9 GB to 30.9 GB in the final 30 seconds.

## Throughput Summary

### Healthy Period (T+10 to T+270s)

| Metric | Value |
|--------|-------|
| **Average** | ~9.18M ops/s (2.48B ops / 270s) |
| **Peak** | 16.49M ops/s (T+30s, data still in buffer) |
| **Min** | 4.82M ops/s (T+60s, first flush stall) |
| **Steady-state avg** (T+40–T+270) | ~8.1M ops/s |

### Degenerate Period (T+280 to T+302s — OOM)

| Interval | ops/s | RSS |
|----------|-------|-----|
| T+280 | 1,904,409 | 27,144 MB ← 10x spike |
| T+290 | 27,980 | 28,949 MB |
| T+302 | 29,798 | 30,915 MB → OOM killed |

### Two Distinct Phases

1. **Warm-up (T+10 to T+30):** 15.3–16.5M ops/s — data fits in memory buffer, minimal disk I/O
2. **Disk-bound steady state (T+40 to T+270):** Oscillates 4.8–13.6M ops/s — flush/eviction pipeline active, ~2-3x throughput swings per cycle

## RSS Memory

- **T+10 to T+270:** Stable at 2.6–3.0 GB ✅ **Bounded** for 270 seconds
- **T+280:** Spiked to 27.1 GB ← **catastrophic, not gradual**
- **T+302:** 30.9 GB → OOM killed (32GB VM, no swap)

The memory spike is a **step function**, not gradual growth. Something broke in the flush/eviction pipeline at ~T+275s that caused runaway memory allocation.

## Disk Usage

- **48 log segment files** created (log.0 through log.47)
- **47 × 1GB + 1 × 128MB = ~47.1 GB** total on disk
- Segments created continuously from T+0 through T+302
- **No segment cleanup/truncation occurred** — old segments were never deleted

Disk storage grew unbounded at ~170 MB/s. For a 5-minute run, this would require ~50 GB. For a 1-hour run, ~600 GB.

## Working Set (Live Keys)

- Stable at 499,936–500,000 throughout ✅ **Correctly bounded**
- Delete rate ≈ upsert rate (maintaining ~500K cap)
- Operation mix: ~43% upsert, ~14% read, ~43% delete

## CLIFF Events

The binary flagged 14 out of 30 intervals as "⚠ CLIFF" (throughput drops). This is the flush/eviction cycle: throughput dips ~40-50% when the in-memory buffer fills and flush stalls the writers.

## Raw Output

```
  [T+   10s]  ops/s:   15284352  total:      152843520  live:   499984  RSS: 2633 MB
  [T+   20s]  ops/s:   16444236  total:      317285888  live:   500000  RSS: 2633 MB
  [T+   30s]  ops/s:   16489472  total:      482180608  live:   500000  RSS: 2674 MB
  [T+   40s]  ops/s:   13562496  total:      617805568  live:   500000  RSS: 2772 MB
  [T+   50s]  ops/s:    9450521  total:      712310784  live:   499968  RSS: 2802 MB
  [T+   60s]  ops/s:    4815667  total:      760467456  live:   500000  RSS: 2834 MB ⚠ CLIFF
  [T+   70s]  ops/s:    8436838  total:      844835840  live:   499936  RSS: 2834 MB
  [T+   80s]  ops/s:    5937945  total:      904215296  live:   500000  RSS: 2802 MB ⚠ CLIFF
  [T+   90s]  ops/s:    9013708  total:      994352384  live:   500000  RSS: 2859 MB
  [T+  100s]  ops/s:    8397542  total:     1078327808  live:   500000  RSS: 2891 MB
  [T+  110s]  ops/s:    8287872  total:     1161206528  live:   500000  RSS: 2860 MB
  [T+  120s]  ops/s:    8315008  total:     1244356608  live:   499952  RSS: 2732 MB
  [T+  130s]  ops/s:   11197516  total:     1356331776  live:   499936  RSS: 2924 MB
  [T+  140s]  ops/s:    6795187  total:     1424283648  live:   499936  RSS: 2924 MB ⚠ CLIFF
  [T+  150s]  ops/s:    9563289  total:     1519916544  live:   499984  RSS: 2892 MB
  [T+  160s]  ops/s:    7373644  total:     1593652992  live:   500000  RSS: 2828 MB ⚠ CLIFF
  [T+  170s]  ops/s:   11136870  total:     1705021696  live:   500000  RSS: 3049 MB
  [T+  180s]  ops/s:   11743078  total:     1822452480  live:   500000  RSS: 2875 MB
  [T+  190s]  ops/s:    5085824  total:     1873310720  live:   500000  RSS: 2878 MB ⚠ CLIFF
  [T+  200s]  ops/s:    5432140  total:     1927632128  live:   500000  RSS: 2814 MB ⚠ CLIFF
  [T+  210s]  ops/s:    8717798  total:     2014810112  live:   500000  RSS: 2910 MB
  [T+  220s]  ops/s:    7528089  total:     2090091008  live:   500000  RSS: 2846 MB ⚠ CLIFF
  [T+  230s]  ops/s:    6931584  total:     2159406848  live:   500000  RSS: 2942 MB ⚠ CLIFF
  [T+  240s]  ops/s:    7456870  total:     2233975552  live:   500000  RSS: 2910 MB ⚠ CLIFF
  [T+  250s]  ops/s:    8344371  total:     2317419264  live:   500000  RSS: 2878 MB
  [T+  260s]  ops/s:    8676454  total:     2404183808  live:   500000  RSS: 2846 MB
  [T+  270s]  ops/s:    7446656  total:     2478650368  live:   500000  RSS: 2878 MB ⚠ CLIFF
  [T+  280s]  ops/s:    1904409  total:     2497694464  live:   500000  RSS: 27144 MB ⚠ CLIFF
  [T+  290s]  ops/s:      27980  total:     2497974272  live:   500000  RSS: 28949 MB ⚠ CLIFF
  [T+  302s]  ops/s:      29798  total:     2498272256  live:   500000  RSS: 30915 MB ⚠ CLIFF
```

Process killed: exit code 137 (SIGKILL by OOM-killer)

## Findings

### 🔴 P0: Memory Explosion at T+280s

RSS jumped from 2.9 GB to 27.1 GB in a **single 10-second interval**. This is not gradual growth — something catastrophic happened in the flush/eviction pipeline that caused ~24 GB of allocation in ~10 seconds. Likely the disk device is buffering entire segments in memory when flush can't keep up, or `mmap`-based I/O is pulling file pages into RSS.

**Probable cause:** After 48 GB of log segments on disk (47 full segments), the eviction/flush pipeline stalled. Unable to advance the read-only address or truncate old segments, the system accumulated pending I/O and memory-mapped pages, spiking RSS past the 32 GB VM limit.

### 🟡 P1: Disk Storage Unbounded

48 GB of log segments accumulated with zero cleanup. The disk device needs a segment truncation/reclamation mechanism — old segments below the begin-address should be deleted. Without this, disk stress tests are limited by available disk space, not by the data structure's steady-state behavior.

### 🟡 P2: Throughput Oscillation (2-3x swings)

The flush/eviction cycle causes 40-50% throughput drops every ~20-30 seconds. While periodic dips are expected during flush, the magnitude suggests room for pipeline smoothing (e.g., background pre-flush, write-ahead buffering).

### 🟢 Working Set Correctly Bounded

Live keys stayed within 16 of the 500,000 target throughout. The upsert/delete balance works correctly.

### 🟢 No Panics or Data Corruption

The process was cleanly killed by OOM, not by a panic or assertion failure. All operations before the OOM spike were valid.

## Recommendations

1. **Fix P0:** Investigate the memory spike trigger at ~47 GB of accumulated log segments. The disk device likely needs:
   - Memory-bounded I/O buffering (cap pending writes in memory)
   - Segment-level memory unmapping after flush completes
   - Or the `stress_disk` binary may be memory-mapping files without releasing them

2. **Fix P1:** Implement log segment truncation — delete segments below the begin-address to keep disk usage bounded to `log_size_mb * N` where N is the active segment window.

3. **Rerun after fixes:** With bounded memory and disk, a 5-minute (and then 1-hour) test should be achievable on this 32GB VM.

## Comparison to Previous Tests

| Test | Device | Duration | Avg ops/s | Outcome |
|------|--------|----------|-----------|---------|
| stress_5min 16t non-lossy | InMemory | 300s | 38.85M | ✅ Completed (OOM at extended durations) |
| stress_5min 16t lossy | InMemory | 200s | ~40M | 🔴 Deadlocked (fixed in b65a0ba) |
| **stress_disk 16t** | **Disk** | **280s** | **~9.18M** | **🔴 OOM at T+280s** |

The disk-backed test runs at ~24% of the in-memory throughput (9.18M vs 38.85M), which is expected given real I/O overhead. The new failure mode is unbounded memory growth, not the old deadlock.
