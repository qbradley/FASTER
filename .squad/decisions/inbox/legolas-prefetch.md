# Decision: Hash Index Software Prefetch (P-05)

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Branch:** `legolas/hash-prefetch`
**Status:** Implemented — ready for review

## Summary

Added cross-platform software prefetch hints on hash bucket lookups across
all four CRUD hot paths (read, upsert, RMW, delete). Expected to reduce
L1/L2 cache-miss stalls by 20-40% on single-thread throughput benchmarks.

## Architecture

```
key.hash() → prefetch(bucket_ptr) → layout computation → find()/find_or_create()
                                     ^^^^^^^^^^^^^^^^
                                     ~10-20 cycle latency cover
```

The overflow chain also prefetches the next bucket while scanning the
current 7-entry bucket, hiding pointer-chase latency.

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| L1 prefetch (`_MM_HINT_T0`) | Bucket accessed within ~10 instructions |
| Read-only prefetch | Hash lookups are read-dominant; write prefetch would cause MESI invalidation |
| Single cache line | `HashBucket` is exactly 64 bytes (`#[repr(C, align(64))]`) |
| aarch64 `PRFM PLDL1KEEP` | Equivalent L1 read prefetch for ARM targets |
| No-op fallback | Graceful degradation on unsupported architectures |

## Team Impact

- **Gandalf**: This implements P-05 from the feature proposals. The API
  (`HashIndex::prefetch(key_hash)`) is available for any future hot-path
  optimizations.
- **Sam**: No interaction with epoch system — prefetch is purely advisory.
- **Faramir**: The prefetch calls in operations.rs are adjacent to but
  independent of the operation info struct parameters.
- **Benchmarking**: Need `cargo bench` or `disk-io-bench` to measure actual
  throughput improvement.

## Files Changed

- `hash/prefetch.rs` (NEW) — cross-platform prefetch intrinsics
- `hash/mod.rs` — module declaration
- `hash/table.rs` — `prefetch_bucket()` + overflow chain prefetch + 4 tests
- `hash/index.rs` — `prefetch()` facade + 3 tests
- `store/operations.rs` — 4 prefetch call sites
