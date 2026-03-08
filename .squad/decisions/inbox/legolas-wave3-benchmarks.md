# Decision: Wave 3 Benchmark Baseline Established

**Author:** Legolas
**Date:** 2026-03-07
**Status:** Informational

## Context

Wave 3 merged all Wave 2 feature branches and ran comprehensive benchmarks to establish the post-optimization baseline.

## Key Findings

1. **All regressions recovered.** The A (-14.7%) and F (-8.1%) regressions observed during epoch-fix validation were caused by intervening commits, NOT the epoch fix itself. Wave 2 work restored and exceeded prior peaks.

2. **Hash prefetching delivers +6-7%, not +20-40%.** The software prefetch hints in `hash/prefetch.rs` provide measurable but modest improvement. Two-level prefetching (bucket + record) and hash table layout changes are needed for the predicted 20-40% gain.

3. **C# gap at -14% on reads.** Down from -60% pre-epoch, -20% post-epoch. Closing the remaining gap requires structural changes (open addressing hash table), not incremental tuning.

4. **Disk I/O benchmarks need redesign.** The 32GB VM with 128 buffer pages and 10M keys produces 0% pending rate — all data stays in memory. True disk benchmarks need 100M+ keys or buffer_pages ≤ 16.

## Decisions

- **Benchmark baseline is set.** All future performance changes should compare against these Wave 3 numbers.
- **Prefetch distance tuning is P1.** Low effort, expected +2-5% additional improvement.
- **Hash table layout investigation is P2.** High effort but highest remaining impact (+15-30%).
- **Disk I/O benchmarks deferred to Wave 4** with proper memory pressure configuration.

## Impact

- **Aragorn:** No action needed. FFI callbacks merge clean, no perf impact.
- **Sam:** Sealed bit merge clean. Epoch + sealed bit cumulative impact confirmed positive.
- **All:** Use `.squad/agents/legolas/wave3-benchmark-results.md` as the reference baseline.
