# Decision: hash_layout_bench repair and OA prototype removal

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-09
**Branch:** `legolas/fix-hash-bench`

## Context

The `hash_layout_bench` was disabled (benches() commented out) because it took
hours to run. Root causes:

1. **Dead code**: ~136 lines of open-addressing (Robin Hood) prototype that is
   no longer needed — team decision settled on keeping multi-slot buckets.
2. **Oversized datasets**: 50K/200K/800K keys with 2^14/2^16/2^18 bucket tables.
   Setup cost alone for 800K entries was significant.
3. **Default criterion settings**: 100 samples × 5s measurement × 14 benchmarks.

## Decision

- **Removed all OA prototype code** (~206 lines total including OA benchmarks).
  This aligns with the settled team decision: keep multi-slot buckets.
- **Reduced dataset sizes** to 3K/12K/50K (spans L2→L3 cache boundary without
  excessive setup). Batched benchmark reduced from 800K to 50K.
- **Added criterion config**: `sample_size(10)`, `measurement_time(3s)` to cap
  wall-clock time for the full criterion suite.
- **Re-enabled benches()** call in main().

## Outcome

- File: 360 lines → 154 lines
- Smoke test: <0.2s (was hanging/hours)
- Full criterion run: estimated ~2-3 minutes (was hours)
- All precheckin checks pass (fmt, clippy, doctests, nextest)

## Impact

Benchmark still covers the three key regression scenarios:
- `faster_find` (raw lookup latency at 3 cache-pressure levels)
- `faster_prefetch_find` (prefetch benefit measurement)
- `faster_batch16` (realistic batched access pattern)
