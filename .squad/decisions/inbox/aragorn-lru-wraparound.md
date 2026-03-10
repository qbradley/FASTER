# Decision: Lossy LRU Cache Wrap-Around Mode

**Author:** Aragorn (Rust Expert)
**Date:** 2025-01-20
**Branch:** `aragorn/lru-wraparound`
**Status:** Proposed

## Context

FASTER's hybrid log uses monotonically increasing 48-bit logical addresses.
When the in-memory page buffer fills, pages are evicted (flushed to disk or
discarded). Hash entries may still point to evicted/truncated addresses. Before
this change:

- **Reads** of truncated addresses correctly returned `NotFound`.
- **Upserts/RMW** of truncated addresses returned `Aborted`, causing infinite
  retry loops — effectively a crash for cache workloads.
- `evict_and_truncate()` and `invalidate_entries_in_range()` existed but were
  never called from `maintenance()`.

For lossy-cache use cases (e.g., page-cache sample), users need evicted data to
silently disappear rather than cause errors.

## Decision

Add a `lossy: bool` configuration flag to `FasterKvConfig` (default `false`)
that enables LRU cache semantics:

1. **Truncated upserts → copy-to-tail**: When a hash entry points to a
   truncated address, allocate a fresh record at the log tail (using existing
   `upsert_copy_to_tail()` with `old_value = None`).

2. **Truncated RMW → create-at-tail**: Use existing `rmw_create_at_tail()`
   which calls `rmw_need_initial_update` / `rmw_initial`.

3. **Lossy maintenance**: When `lossy = true`, `maintenance()` calls
   `evict_and_truncate()` (advances `begin_address` past head) followed by
   `invalidate_entries_in_range()` (CAS stale hash entries to EMPTY).

## Alternatives Considered

- **Always invalidate hash entries**: Rejected — adds overhead for non-lossy
  workloads that use disk-backed logs where truncated reads should go to disk.
- **Application-level retry with TTL**: Rejected — doesn't fix the `Aborted`
  return from upsert/RMW on truncated addresses, which is the root bug.
- **Automatic lossy detection**: Rejected — too magical; explicit opt-in is
  safer and clearer.

## Consequences

- Existing code is unaffected (`lossy: false` is the default and is set
  explicitly at all 28+ config construction sites).
- The page-cache sample now uses `lossy: true` and runs stably for 60+ seconds
  with concurrent readers/writers.
- 9 new integration tests validate eviction, re-upsert, RMW, delete, concurrent
  access, and begin_address advancement in lossy mode.
- Tests that need eviction write ~6M records to fill the 4-page buffer (each
  page = 32 MiB, each u64/u64 record = 24 bytes), so they take ~20–30s each.
