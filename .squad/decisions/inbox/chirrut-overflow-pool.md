# Decision: Overflow Bucket Pool Design

**Agent:** Chirrut (Systems Programming Expert)
**Date:** 2026-03-07
**Status:** DECIDED — Implemented
**Task:** 2b (Overflow Bucket Pool)

## Decision

Overflow bucket allocation, chaining, and traversal implemented as:
1. `OverflowBucketPool` — thin wrapper around `MallocFixedPageSize<HashBucket>` in `overflow.rs`
2. Chain operations — methods on `HashBucket` in `hash_bucket.rs`

## Key Design Choices

### 1. Re-zeroing on allocate (not on free)

The pool re-zeroes buckets in `allocate()` rather than in `free()`. Rationale: fresh bump-allocated pages are already zeroed, so only free-list reuse needs re-zeroing. Zeroing at allocate-time means we zero unconditionally (safe) rather than relying on callers to free cleanly. Cost: ~7 relaxed stores per allocate — negligible compared to the allocation itself and the subsequent cold cache line fetch.

### 2. Overflow CAS: AcqRel/Acquire ordering

`get_or_create_overflow()` uses `compare_exchange(ZERO, new_addr, AcqRel, Acquire)`:
- **AcqRel on success:** Release publishes the zeroed bucket content (entries + overflow pointer) before the pointer becomes visible. Acquire ensures we see any prior writes to this overflow slot.
- **Acquire on failure:** We must see the winner thread's fully-initialized bucket.

This matches Architecture §3.1.4: "Write overflow pointer: Release" / "Read overflow pointer: Acquire".

### 3. Chain methods on HashBucket, pool in separate module

Chain traversal, search, and insert live on `HashBucket` because they access the private `overflow_address` field and are core bucket semantics. The `OverflowBucketPool` lives in `overflow.rs` to avoid bloating `hash_bucket.rs` (which was already 1591 lines).

### 4. Iterative traversal, not recursive

`for_each_bucket()` and `find_entry_in_chain()` use iterative loops rather than recursion. Overflow chains are typically short (load factor < 1.0 means most chains are depth 0), but correctness shouldn't depend on stack depth assumptions.

`insert_in_chain()` has one recursive tail-call for the rare case where a freshly-allocated overflow is immediately filled by concurrent threads. This is bounded by the number of concurrent inserters.

## Implications

- **Tarkin (storage):** Checkpoint must serialize overflow buckets. `OverflowBucketPool` addresses are `LogicalAddress`es that can be persisted.
- **Mando (API):** Hash table (task 2d) can now use `insert_in_chain` and `find_entry_in_chain` directly — no manual overflow management needed.
- **Ahsoka (perf):** Benchmarks added for chain traversal at depth 0/1/2/5. Primary bucket lookup (depth=0) is the hot path and has zero overhead from overflow machinery.
