# Decision: DST Scenarios Validated with Small-Pages Feature

**Author:** Éowyn (DST Expert)  
**Date:** 2026-03-18  
**Status:** Validated  

## Context

DST concurrency scenarios (pipeline_deadlock, lossy_truncate_starvation) never filled even a single 32 MB page (OFFSET_BITS=25). The small-pages feature flag (OFFSET_BITS=16, 64 KB pages) reduces page size to make buffer pressure testable with manageable workloads.

## Changes Made

1. **small-pages feature** added to `faster-core/Cargo.toml` and forwarded via `faster-dst/Cargo.toml`
2. **address.rs**: cfg-gated OFFSET_BITS (25 default, 16 for small-pages); fixed MAX_PAGE overflow for PAGE_BITS=32
3. **allocator.rs**: cfg-gated ITEMS_PER_PAGE_BITS (20 default, 16 for small-pages)
4. **scenarios.rs**: Tuned all 4 scenarios — reduced buffer_pool_pages to 4-8, changed key_dist to Uniform(500K) for near-zero in-place update rate, set ops to generate 15-37× buffer capacity
5. **assertions.rs**: Added `min_total_ops` assertion to detect degraded throughput from broken flush/eviction pipeline

## Scenario Configuration (with small-pages)

| Scenario | Writers | Ops/Writer | Pages | Buffer | Data Written | Ratio |
|----------|---------|-----------|-------|--------|-------------|-------|
| single_writer_baseline | 1 | 10K | 4 | 256 KB | ~216 KB | 0.84× |
| multi_writer_saturation | 16 | 20K | 8 | 512 KB | ~7.68 MB | 15× |
| pipeline_deadlock | 8 | 50K | 4 | 256 KB | ~9.6 MB | 37× |
| lossy_truncate_starvation | 4 | 50K | 4 | 256 KB | ~4.8 MB | 19× |

## Validation Results

### With fixes (production code):
- All 4 scenarios × 5 canary seeds = **20/20 PASS**
- pipeline_deadlock: ~400K total ops (100% success), ~5.3s
- lossy_truncate: ~200K total ops (100% success), ~2.6s

### With reverted deadlock fixes (eviction.rs + kv.rs):
- **pipeline_deadlock: 5/5 seeds FAIL** — 7% success rate (14K-19K ops vs 200K minimum)
- **lossy_truncate_starvation: 5/5 seeds FAIL** — similar degradation
- single_writer_baseline: PASS (fits in buffer, no flush needed)
- multi_writer_saturation: PASS (would need further tuning or min_total_ops)

### Failure mechanism:
Without `poll_completions()` in maintenance and without buffer-pressure detection, pages submitted for flush stay in Flushing state forever. The buffer fills, all new allocations fail (Aborted), and only ~7% of ops succeed (keys that happen to collide with existing mutable-region records get in-place updates).

## Key Learnings

1. **Record size matters**: Actual record size is 24 bytes (RecordInfo 8B + Key 8B + Value 8B), not 48 bytes. Buffer capacity calculations must use real record sizes.
2. **In-place updates bypass allocation**: Uniform key distribution with small range allows existing-key overwrites that don't need new allocation. Use large key ranges (>10× total ops) or Sequential keys to ensure allocation pressure.
3. **Bounded retry prevents hangs**: MAX_ALLOC_RETRIES=32 means writers never truly deadlock — they get Aborted status and move on. Detection requires throughput assertions (min_total_ops), not just watchdog stall detection.
4. **Scheduler thread masks deadlocks**: The DstRunner's scheduler thread provides an alternate maintenance path. Removing poll_completions from maintenance (the critical revert) prevents completions from firing in both writer and scheduler paths.

## Recommendation

Ship these scenario changes. The small-pages feature provides the right testing granularity for DST concurrency scenarios. The `min_total_ops` assertion is the key mechanism that distinguishes healthy (100% success) from broken (7% success) pipeline behavior.
