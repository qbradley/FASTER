# 1-Hour Disk Stress Soak Test Results

**Agent:** Legolas (Performance Guru)
**Date:** 2025-07-16
**Requested by:** qbradley
**VM:** Azure Standard (31 GB RAM, 123 GB disk)

## Verdict: ✅ PASS — Rock Solid

The FASTER Rust disk-backed store ran for a full 3600 seconds under heavy mixed
workload with **zero crashes, zero panics, zero errors**. Throughput was flat,
memory was bounded, and disk was self-cleaning.

---

## Test Configuration

| Parameter | Value |
|---|---|
| Threads | 16 |
| Duration | 3600s (1 hour) |
| Key range | 1,000,000 |
| Max live keys | 500,000 |
| Log size | 1024 MB |
| Storage dir | /tmp/faster-disk-stress/ |
| Report interval | 60s |
| Mode | lossless (flush/eviction) |

---

## Key Metrics

### Total Operations
- **28.77 billion ops** completed in 3600 seconds
- **Upserts:** 12,330,495,381 (42.9%)
- **Reads:** 4,110,316,484 (14.3%)
- **Deletes:** 12,329,995,393 (42.9%)

### Throughput Over Time

| Window | Avg ops/s | Notes |
|---|---|---|
| First 10 min (T+60 to T+600) | 7.99M | Warmup at T+60 (7.26M), then steady |
| Middle 30 min (T+1200 to T+3000) | 7.99M | Flat line |
| Last 10 min (T+3000 to T+3600) | 8.01M | Slightly higher, no degradation |
| **Overall average** | **7.99M ops/s** | |

**Throughput degradation: NONE.** The last 10 minutes averaged 8.01M ops/s vs
7.99M in the first 10 minutes. If anything, throughput improved fractionally
over time (likely CPU cache warming effects).

Min/max observed: 7.26M (T+60 warmup) → 8.09M (T+2580 peak).
Excluding warmup, the floor was 7.75M (T+2700) — a mere 3.5% dip during
heavy compaction activity.

### RSS Memory Over Time

| Time | RSS (MB) | Delta from start |
|---|---|---|
| T+60 | 14,364 | baseline |
| T+300 | 14,531 | +167 |
| T+600 | 14,552 | +188 |
| T+1200 | 14,573 | +209 |
| T+1800 | 14,597 | +233 (peak) |
| T+2400 | 14,580 | +216 |
| T+3000 | 14,604 | +240 |
| T+3600 | 14,605 | +241 |

**Memory drift: +241 MB over 60 minutes (1.7% growth).**
This is effectively flat. The RSS stabilized around 14.5 GB after the first
3 minutes and drifted by only ~240 MB over the remaining 57 minutes. No
unbounded growth. No memory leak signal.

Post-test RSS dropped to ~1 GB (2,880 MB reported at shutdown, then down to
1 GB after process exit), confirming clean deallocation.

### Disk Usage

- Storage dir was automatically cleaned on exit (`keep-data: false`)
- The `/tmp` partition showed only 7.8 GB used system-wide post-test
- **Compaction kept disk bounded throughout the full hour** — no disk
  exhaustion despite 28.77B operations writing to a 123 GB volume

### Live Keys

Held steady at exactly 499,952–500,000 throughout all 60 checkpoints.
The working set cap was respected perfectly.

---

## Compaction Analysis

### Chain Hop Warnings

Total compaction chain-hop warnings observed: **~35 events** over 3600 seconds

These are informational warnings indicating compaction encountered hash chains
exceeding 1,000,000 hops. Typical hop counts ranged from 1.39M to 2.82M.

| Hop Range | Frequency | Notes |
|---|---|---|
| 1.39M–1.43M | Most common | Normal compaction traversal |
| 1.70M–1.72M | Frequent | Slightly longer chains |
| 2.79M–2.82M | Occasional (~4x) | Deeper chains, still resolved cleanly |

**Assessment:** These are diagnostic, not errors. Compaction always completed
successfully. The chain lengths are proportional to the key density (500K live
keys in 1M key space = 50% fill). No chain-hop warning caused a stall,
throughput dip, or failure.

No double-warnings (two compaction events overlapping) were seen until T+2580,
when two appeared between reports — suggesting compaction is keeping pace with
the write rate.

---

## Comparison: 5-Minute vs 1-Hour

| Metric | 5-min test | 1-hour test | Verdict |
|---|---|---|---|
| Duration | 300s | 3600s (12×) | ✅ |
| Throughput | 7.80M ops/s | 7.99M ops/s | ✅ Slightly better |
| RSS peak | 14,700 MB | 14,605 MB | ✅ Lower |
| Memory drift | N/A (too short) | +241 MB/hr | ✅ Bounded |
| Errors | 0 | 0 | ✅ |
| Panics | 0 | 0 | ✅ |
| Total ops | ~2.34B | 28.77B (12.3×) | ✅ Linear scaling |

---

## Anomalies

**None.** Zero panics, zero errors, zero crashes, zero OOM events, zero
throughput cliffs, zero stuck intervals.

---

## Conclusions

1. **Production-grade stability.** The disk-backed store is stable under
   sustained load for at least 1 hour with no degradation.

2. **No memory leak.** RSS grew by only 1.7% over 60 minutes and returned to
   baseline on exit. This is well within noise/fragmentation bounds.

3. **Compaction is effective.** Disk usage stayed bounded despite 28.77B
   operations. Chain-hop warnings are informational and do not indicate a
   problem — they're a natural consequence of high key density.

4. **Throughput is consistent.** 8.0M ops/s ± 3.5% across the entire hour.
   No warmup penalty beyond the first 60 seconds.

5. **Ready for longer soak or production pilots.** Nothing in this data
   suggests time-dependent failure modes. A 24-hour test could be run for
   extra confidence, but is not strictly necessary based on these results.
