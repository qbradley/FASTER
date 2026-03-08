# Decision: Two-Level Prefetch Pipeline (P-06 / Task A4)

**Author:** legolas  
**Date:** 2026-03-08  
**Status:** Proposed  
**Branch:** `legolas/two-level-prefetch`

## Context

Every FASTER CRUD operation performs two serial memory accesses:
1. Hash bucket lookup (64-byte cache line)
2. Record access at the logical address from the bucket entry

With single-level prefetching (L1 only), only the hash bucket is prefetched ahead. The record access still incurs a full cache miss (~60-100ns), creating a pointer-chase bottleneck.

## Decision

Add a second prefetch stage (L2) that prefetches the record data after the hash bucket lookup resolves, plus a batch API with a sliding-window prefetch pipeline.

### Inline L2 Prefetch (All CRUD Paths)

After `find()`/`find_or_create()` returns but before `snapshot()` and `classify()`:
```rust
fn prefetch_record(allocator: &HybridLog, addr: LogicalAddress, is_write: bool) {
    if !addr.is_null() {
        let phys = allocator.get_physical_address(addr);
        if is_write { prefetch::prefetch_write(phys as *const u8); }
        else { prefetch::prefetch_read(phys as *const u8); }
    }
}
```

### Batch API with Sliding Window

- `PrefetchPipeline`: L1 window = 8 ops, L2 window = 4 ops
- Batch methods on `UnsafeContext` and `FasterKv`
- Epoch refresh every 256 operations

### prefetch_write Intrinsic

- x86_64: `_MM_HINT_ET0` (exclusive, all cache levels)
- aarch64: `PRFM PSTL1KEEP` (prefetch for store, L1, keep)
- Other: no-op fallback

## Alternatives Considered

1. **Software pipelining only (no inline L2):** Simpler but misses the low-hanging fruit of single-op L2 prefetch
2. **Larger L1 window instead:** Doesn't address the pointer-chase; hash buckets are already prefetched
3. **Async/coroutine-based prefetch:** Too complex for the current sync operation model

## Consequences

- ~+10-20% expected throughput improvement on cache-miss-heavy workloads
- Batch API enables further amortization of epoch enter/leave overhead
- `prefetch_record()` adds a `get_physical_address()` call (~5ns) per operation — negligible vs the ~60ns cache miss it avoids
- `UnsafeContext.session` field changed to `pub(super)` for batch module access
