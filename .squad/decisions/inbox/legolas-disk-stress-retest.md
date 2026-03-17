# Disk Stress Retest — Compaction Fix

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-17
**Requested by:** qbradley
**Branch:** `rust` @ `5caa018` (qbradley/FASTER)
**VM:** Azure Standard_D8s_v3 (32 GB RAM), Ubuntu

## Verdict: ❌ FAIL — Crashed at ~T+30s with SIGSEGV

The compaction fix did **not** survive 5 minutes. The process segfaulted (signal 11, exit code 139) approximately 30 seconds into the run. This is worse than the previous OOM at T+280s in terms of wall-clock survival time, though the failure mode is fundamentally different: this is a **code bug**, not a resource exhaustion problem.

## What happened

1. **T+0–10s** — Blazing start at 14.5M ops/s. RSS settled at 3.2 GB. Working set hit 500K as expected.
2. **T+10–20s** — First compaction cycle kicks in. Throughput drops 80% to 2.96M ops/s (⚠ CLIFF). RSS rises to 3.75 GB.
3. **T+20–30s** — Compaction continues blocking workers. Throughput craters to 770K ops/s (95% drop from peak). RSS holds at 3.72 GB.
4. **After T+30s** — No more reporter heartbeats. Compaction log spam (~170K lines of `tombstone_records` messages) floods stdout. Chain hop warnings escalate (up to 41M hops). Process eventually segfaults.

## Performance Data (3 intervals before crash)

| Time | ops/sec | Total ops | Live keys | RSS (MB) | Notes |
|------|---------|-----------|-----------|----------|-------|
| T+10 | 14,566,758 | 145,667,584 | 500,000 | 3,206 | Clean burst |
| T+20 | 2,955,852 | 175,226,112 | 499,984 | 3,752 | ⚠ CLIFF (compaction started) |
| T+30 | 770,560 | 182,931,712 | 499,984 | 3,724 | ⚠ CLIFF (compaction blocking) |

## Comparison with previous run

| Metric | Previous (no compaction) | This run (with compaction) |
|--------|--------------------------|---------------------------|
| Duration | 280s (OOM at 93%) | ~30s then SIGSEGV |
| Throughput (avg) | 9.18M ops/s | 6.1M ops/s (30s avg) |
| Throughput (peak) | ~10M ops/s | 14.5M ops/s |
| Peak RSS | 27+ GB (OOM kill) | 3.75 GB ✅ |
| Disk at crash | 48 GB (unbounded) | 3.4 GB ✅ |
| Failure mode | OOM kill | SIGSEGV (compaction bug) |
| Working set | 500K | 500K ✅ |

## The good news

The compaction wiring **does** fundamentally work for resource bounding:
- **RSS stayed at ~3.7 GB** vs 27+ GB — memory is bounded ✅
- **Disk was 3.4 GB** (4 segment files) vs 48 GB — segment cleanup works ✅
- **Working set held at 500K** — deletion logic is correct ✅
- **`auto_compact: true` + `LogSizeBudgetPolicy`** correctly triggered compaction

## The bad news — Three compaction bugs

### Bug 1: SIGSEGV in compaction (CRITICAL)
The compaction scanner or address updater has a memory safety violation. The process dumped core with signal 11. This is likely an unsafe block in the log-scanning code that dereferences an invalid address during concurrent access. The crash happened after scanning ~2.1 GB of a 3.6 GB range.

### Bug 2: Compaction starves workers (SEVERE)
When compaction runs, throughput drops 80–95%. The compaction scanner appears to hold locks that block worker threads from making progress. This turns the store into a stop-the-world collector. The previous run without compaction maintained steady 9M ops/s — so compaction is the bottleneck, not I/O.

### Bug 3: Verbose debug logging in hot loop (MINOR)
The scanner prints `tombstone_records ~N MB (M entries)` every 1024 tombstones. With 7M tombstone entries, this produced **170,087 log lines**. The logging itself adds overhead and makes output unusable. The `chain hops exceeds threshold` warning also fires repeatedly (10 times). These should be rate-limited or gated behind a debug flag.

## Compaction telemetry

- **Tombstone records collected**: ~7M entries (~106 MB)
- **Chain hops**: up to 41M (threshold 1M) — hash index chains are pathologically long
- **Scan coverage**: only 2.1 GB of 3.6 GB range scanned (58%) before crash
- **Compaction cycles attempted**: ~13 (based on `scanned=` messages)
- **Disk at crash**: 3.4 GB across 4 segment files (budget ≈ 2 GB, so slightly over but in the right ballpark)

## Recommendations

1. **Fix the SIGSEGV first** — review all `unsafe` blocks in `scanner.rs`, `copier.rs`, and `address_update.rs`. The crash likely involves a stale address reference into a log segment that was concurrently truncated or unmapped. This is the #1 priority.

2. **Make compaction non-blocking** — the scanner should not hold exclusive locks during its full scan. Consider read-copy-update (RCU) or epoch-based approaches so workers can continue while compaction reads.

3. **Rate-limit compaction logging** — gate `tombstone_records` prints behind a verbosity flag or log at most once per second. The `chain hops` warning should use exponential backoff.

4. **Re-examine hash index sizing** — 41M chain hops across 7M tombstones means average chain length ≈ 6. With `hash_index_size_log2 = ceil(log2(2M)) = 21` (2M buckets) and 500K live keys, the load factor should be 0.25. The long chains suggest many hash collisions or stale index entries that compaction should be cleaning.

## Next steps

This test is **not ready to pass**. The compaction infrastructure is promising — it successfully bounded both memory and disk — but it has a segfault that kills the process within 30 seconds under load.

**Recommended path:**
1. Sam: Fix the segfault in compaction (likely in `scanner.rs` unsafe code)
2. Sam: Add a compaction lock timeout or make it non-blocking
3. Legolas: Re-run this exact test after fix
4. If it survives 5 min, extend to 30 min and 1 hour runs

---
*Test executed on Azure VM 20.59.58.117 | 16 threads | 1M key range | 500K live keys | 2 GB compaction budget*
